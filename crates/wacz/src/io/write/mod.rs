//! Assembling WACZ files and configuring compression and indexes.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use zip::ZipWriter;

use crate::PAGES_PREFIX;
use crate::frictionless::resource::Resource;
use crate::pages::{self, Page, PageListHeader};

mod file;
pub mod index;
mod manifest;

pub use file::WaczFileWriter;
mod resource;
pub mod warc;

use resource::options_for;
const DEFAULT_PAGE_ID_LENGTH: NonZeroUsize = NonZeroUsize::new(24).unwrap();
const DEFAULT_ZIPNUM_LINES: NonZeroUsize = NonZeroUsize::new(1024).unwrap();
/// Minimum ZIP DEFLATE compression level accepted by this crate.
pub const MIN_ZIP_COMPRESSION_LEVEL: u32 = 1;
/// Maximum ZIP DEFLATE compression level accepted by this crate.
pub const MAX_ZIP_COMPRESSION_LEVEL: u32 = 264;

/// The format of the CDXJ index written by [`WaczWriter::add_index`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexFormat {
    /// One plain-text CDXJ member.
    Plain,
    /// Independently compressed CDXJ blocks plus a searchable `.idx` summary.
    ZipNum {
        /// Maximum CDX lines per gzip block.
        lines: NonZeroUsize,
    },
}

impl IndexFormat {
    /// Create a `ZipNum` configuration with at most 1024 lines per block, matching `py-wacz`.
    #[must_use]
    pub const fn zipnum() -> Self {
        Self::ZipNum {
            lines: DEFAULT_ZIPNUM_LINES,
        }
    }

    /// Create a `ZipNum` configuration with an explicit nonzero block size.
    #[must_use]
    pub const fn zipnum_with_lines(lines: NonZeroUsize) -> Self {
        Self::ZipNum { lines }
    }
}

/// Configuration for WACZ creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterConfig {
    /// Length of synthetic page identifiers.
    page_id_length: NonZeroUsize,
    /// CDXJ index representation.
    index_format: IndexFormat,
    /// ZIP DEFLATE compression level, or `None` for the encoder default.
    zip_compression_level: Option<u32>,
    /// Gzip compression level for WARC records and `ZipNum` blocks.
    gzip_compression_level: u32,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            page_id_length: DEFAULT_PAGE_ID_LENGTH,
            index_format: IndexFormat::Plain,
            zip_compression_level: None,
            gzip_compression_level: archivindex_warc::io::write::DEFAULT_GZIP_COMPRESSION_LEVEL,
        }
    }
}

impl WriterConfig {
    /// Set the length of generated page identifiers.
    ///
    /// Defaults to 24 hexadecimal characters. Values above 64 produce the full SHA-256 digest.
    #[must_use]
    pub const fn page_id_length(mut self, length: NonZeroUsize) -> Self {
        self.page_id_length = length;
        self
    }

    /// Select the plain or compressed CDXJ representation.
    #[must_use]
    pub const fn index_format(mut self, format: IndexFormat) -> Self {
        self.index_format = format;
        self
    }

    /// Set the ZIP DEFLATE compression level for compressible members.
    ///
    /// Levels range from 1 through 264 and default to 6. Levels up to 9 use `miniz_oxide`; higher
    /// levels use Zopfli. WARC and gzip-compressed members use ZIP `STORE` and are unaffected.
    pub fn zip_compression_level(mut self, level: u32) -> Result<Self, Error> {
        if !(MIN_ZIP_COMPRESSION_LEVEL..=MAX_ZIP_COMPRESSION_LEVEL).contains(&level) {
            return Err(Error::InvalidZipCompressionLevel(level));
        }
        self.zip_compression_level = Some(level);
        Ok(self)
    }

    /// Set the gzip compression level for streaming WARC and `ZipNum` members.
    ///
    /// Levels range from 0 (no compression) through 9 (best compression) and default to 6.
    pub const fn gzip_compression_level(mut self, level: u32) -> Result<Self, Error> {
        if level > archivindex_warc::io::write::MAX_GZIP_COMPRESSION_LEVEL {
            return Err(Error::InvalidGzipCompressionLevel(level));
        }
        self.gzip_compression_level = level;
        Ok(self)
    }
}

/// An error produced while writing a WACZ.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Publication failed; directory sync failures retain the published output.
    #[error(transparent)]
    Publication(#[from] archivindex_publication::Error),
    /// The underlying stream could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The ZIP container could not be written.
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    /// A raw WARC record could not be written into a streaming member.
    #[error(transparent)]
    Warc(#[from] archivindex_warc::io::write::Error),
    /// A page list could not be written.
    #[error(transparent)]
    Pages(#[from] crate::pages::Error),
    /// The data package manifest could not be serialized.
    #[error("invalid data package manifest")]
    Manifest(#[source] serde_json::Error),
    /// A pre-rendered CDXJ index is invalid.
    #[error("invalid CDXJ index")]
    InvalidIndex(#[source] crate::cdxj::Error),
    /// A pre-rendered CDXJ index is not sorted by search key and timestamp.
    #[error("CDXJ index line {line} sorts before the line preceding it")]
    UnsortedIndex {
        /// One-based number of the first line that sorts before its predecessor.
        line: usize,
    },
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
    /// A WARC member name does not have a conforming suffix.
    #[error("WARC member must end in .warc or .warc.gz: {0}")]
    InvalidWarcName(String),
    /// An index name is not a direct `.cdx` member name.
    #[error("index name must be a direct .cdx file name: {0}")]
    InvalidIndexName(String),
    /// The requested gzip compression level is outside the supported range.
    #[error("gzip compression level must be between 0 and 9, got {0}")]
    InvalidGzipCompressionLevel(u32),
    /// A path-based WACZ output does not use the required extension.
    #[error("WACZ output path must end in .wacz: {}", .0.display())]
    InvalidWaczExtension(PathBuf),
    /// A custom resource attempts to use a WACZ-reserved directory.
    #[error("custom resource may not use a reserved directory: {0}")]
    ReservedResourcePath(String),
    /// A manifest resource name is invalid or duplicates an existing name.
    #[error("invalid or duplicate resource name: {0}")]
    InvalidResourceName(String),
    /// The package is missing one or more member classes required by WACZ.
    #[error("missing required WACZ members: {}", .0.join(", "))]
    MissingRequiredMembers(Vec<&'static str>),
    /// A pre-rendered CDXJ line has missing or colliding properties.
    #[error("CDXJ index line {line} does not conform")]
    NonConformingIndex {
        /// One-based number of the offending line.
        line: usize,
        /// The properties the line is missing, or its property collision.
        #[source]
        source: archivindex_cdx::format::cdxj::ConformanceError,
    },
    /// A previous member write failed after mutating the ZIP stream.
    #[error("WACZ writer is unusable after a member write failure")]
    Poisoned,
}

/// A WACZ assembler that tracks member digests and sizes for its final manifest.
pub struct WaczWriter<W: Write + Seek> {
    zip: ZipWriter<W>,
    resources: Vec<Resource<'static>>,
    config: WriterConfig,
    poisoned: bool,
}

impl WaczWriter<BufWriter<File>> {
    /// Stage a WACZ file for publication without overwriting an existing destination.
    ///
    /// This convenience constructor returns [`WaczFileWriter`]; generic sink writers created with
    /// [`Self::new`] only encode and flush their supplied sink.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<WaczFileWriter, Error> {
        WaczFileWriter::create(path)
    }

    /// Stage a configured WACZ file for publication without overwriting an existing destination.
    pub fn create_with_config<P: AsRef<Path>>(
        path: P,
        config: WriterConfig,
    ) -> Result<WaczFileWriter, Error> {
        WaczFileWriter::create_with_config(path, config)
    }
}

impl<W: Write + Seek> WaczWriter<W> {
    /// Create a writer with default configuration.
    #[must_use]
    pub fn new(writer: W) -> Self {
        Self::from_config(writer, WriterConfig::default())
    }

    /// Create a writer with explicit configuration.
    #[must_use]
    pub fn with_config(writer: W, config: WriterConfig) -> Self {
        Self::from_config(writer, config)
    }

    fn from_config(writer: W, config: WriterConfig) -> Self {
        Self {
            zip: ZipWriter::new(writer),
            resources: Vec::new(),
            config,
            poisoned: false,
        }
    }

    /// Stream WARC data under `archive/` using ZIP `STORE`.
    ///
    /// The bytes are copied without parsing or content-level decompression. Use
    /// [`WaczReader::validate`](crate::io::read::WaczReader::validate) with content validation to
    /// check WARC framing and gzip encoding.
    pub fn add_warc<R: Read>(&mut self, name: &str, mut reader: R) -> Result<(), Error> {
        let mut sink = self.start_warc(name)?;
        std::io::copy(&mut reader, &mut sink)?;
        sink.finish()
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
    ///
    /// Pages without identifiers receive synthetic ones of the configured length.
    pub fn add_page_list<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        name: &str,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        let id_length = self.config.page_id_length.get();
        let compression_level = self.config.zip_compression_level;
        let path = format!("{PAGES_PREFIX}{name}");
        self.add_member(&path, options_for(&path, compression_level), |writer| {
            Ok(pages::write_page_list_with_synthetic_ids(
                writer, header, pages, id_length,
            )?)
        })
    }

    /// Add a pre-rendered page-list file after validating it in a first pass.
    pub fn add_page_list_file<R: BufRead + Seek>(
        &mut self,
        name: &str,
        mut reader: R,
    ) -> Result<(), Error> {
        let mut parsed = pages::PageListReader::new(&mut reader)?;
        for page in &mut parsed {
            page?;
        }
        reader.rewind()?;
        let path = format!("{PAGES_PREFIX}{name}");
        self.add_typed_resource(&path, reader)
    }
}
