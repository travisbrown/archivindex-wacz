//! WACZ writer facade and configuration.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use zip::ZipWriter;

use crate::frictionless::Resource;
use crate::pages::{self, Page, PageListHeader};
use crate::{ARCHIVE_PREFIX, PAGES_PREFIX};

mod index;
mod manifest;
mod resource;

use resource::options_for;

const DEFAULT_PAGE_ID_LENGTH: usize = 24;
const DEFAULT_ZIPNUM_LINES: usize = 1024;
/// Least ZIP DEFLATE compression level supported by the enabled encoders.
pub const MIN_ZIP_COMPRESSION_LEVEL: u32 = 1;
/// Greatest ZIP DEFLATE compression level supported by the enabled encoders.
pub const MAX_ZIP_COMPRESSION_LEVEL: u32 = 264;

/// The format of the CDXJ index written by [`WaczWriter::add_index`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexFormat {
    /// One plain-text CDXJ member.
    Plain,
    /// Independently compressed CDXJ blocks plus a searchable `.idx` summary.
    ZipNum {
        /// Maximum CDX lines per gzip block.
        lines: usize,
    },
}

impl IndexFormat {
    /// Standard `ZipNum` configuration: 1024 lines per block, matching `py-wacz`.
    #[must_use]
    pub const fn zipnum() -> Self {
        Self::ZipNum {
            lines: DEFAULT_ZIPNUM_LINES,
        }
    }
}

/// Configuration for WACZ creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterConfig {
    /// Length of synthetic page identifiers.
    pub page_id_length: usize,
    /// CDXJ index representation.
    pub index_format: IndexFormat,
    /// ZIP DEFLATE compression level, or `None` for the encoder default (currently 6).
    ///
    /// Levels 1 through 9 use `miniz_oxide`; levels 10 through 264 use Zopfli. This setting does
    /// not affect members stored with ZIP `STORE`, including WARC and gzip-compressed members.
    pub zip_compression_level: Option<u32>,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            page_id_length: DEFAULT_PAGE_ID_LENGTH,
            index_format: IndexFormat::Plain,
            zip_compression_level: None,
        }
    }
}

/// An error produced while writing a WACZ.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying stream could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The ZIP container could not be written.
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    /// A page list could not be written.
    #[error(transparent)]
    Pages(#[from] crate::pages::Error),
    /// The data package manifest could not be serialized.
    #[error("invalid data package manifest")]
    Manifest(#[source] serde_json::Error),
    /// A WARC path has no usable UTF-8 file name.
    #[error("invalid WARC file name: {}", .0.display())]
    InvalidFileName(PathBuf),
    /// A member path is not safely relative.
    #[error("invalid member path: {0}")]
    InvalidMemberPath(String),
    /// A member path was already written or is reserved for a generated manifest.
    #[error("duplicate member path: {0}")]
    DuplicateMemberPath(String),
    /// The requested ZIP DEFLATE compression level is outside the supported range.
    #[error("ZIP compression level must be between 1 and 264, got {0}")]
    InvalidZipCompressionLevel(u32),
}

/// Contextual manifest properties supplied when finishing a WACZ.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PackageMetadata {
    /// Short collection description.
    pub title: Option<String>,
    /// Longer, optionally Markdown-formatted description.
    pub description: Option<String>,
    /// Creation time; defaults to the current time.
    pub created: Option<DateTime<Utc>>,
    /// Last modification time.
    pub modified: Option<DateTime<Utc>>,
    /// Creating software; defaults to this crate and version.
    pub software: Option<String>,
    /// Primary replay URL.
    pub main_page_url: Option<String>,
    /// Primary replay capture date.
    pub main_page_date: Option<DateTime<Utc>>,
}

/// A WACZ assembler that tracks member digests and sizes for its final manifest.
pub struct WaczWriter<W: Write + Seek> {
    zip: ZipWriter<W>,
    resources: Vec<Resource<'static>>,
    config: WriterConfig,
}

impl WaczWriter<BufWriter<File>> {
    /// Create a WACZ path without overwriting an existing file.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Self::create_with_config(path, WriterConfig::default())
    }

    /// Create a configured WACZ path without overwriting an existing file.
    pub fn create_with_config<P: AsRef<Path>>(
        path: P,
        config: WriterConfig,
    ) -> Result<Self, Error> {
        Ok(Self::with_config(
            BufWriter::new(File::create_new(path)?),
            config,
        ))
    }
}

impl<W: Write + Seek> WaczWriter<W> {
    /// Create a writer with default configuration.
    #[must_use]
    pub fn new(writer: W) -> Self {
        Self::with_config(writer, WriterConfig::default())
    }

    /// Create a writer with explicit configuration.
    #[must_use]
    pub fn with_config(writer: W, config: WriterConfig) -> Self {
        Self {
            zip: ZipWriter::new(writer),
            resources: Vec::new(),
            config,
        }
    }

    /// Add WARC data under `archive/` using ZIP `STORE`.
    pub fn add_warc<R: Read>(&mut self, name: &str, reader: R) -> Result<(), Error> {
        self.add_resource(&format!("{ARCHIVE_PREFIX}{name}"), reader)
    }

    /// Add a WARC file under `archive/`, using its file name.
    pub fn add_warc_from_path<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::InvalidFileName(path.to_path_buf()))?;
        self.add_warc(name, BufReader::new(File::open(path)?))
    }

    /// Write the required `pages/pages.jsonl` list.
    pub fn add_pages<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        self.add_page_list("pages.jsonl", header, pages)
    }

    /// Write a named page list under `pages/`.
    pub fn add_page_list<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        name: &str,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        let id_length = self.config.page_id_length;
        let compression_level = self.config.zip_compression_level;
        let path = format!("{PAGES_PREFIX}{name}");
        self.add_member(&path, options_for(&path, compression_level)?, |writer| {
            Ok(pages::write_page_list_with_synthetic_ids(
                writer, header, pages, id_length,
            )?)
        })
    }
}
