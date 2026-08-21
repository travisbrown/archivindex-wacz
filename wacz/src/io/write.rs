//! WACZ writer facade and configuration.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use zip::ZipWriter;

use crate::frictionless::resource::Resource;
use crate::pages::{self, Page, PageListHeader};
use crate::{ARCHIVE_PREFIX, PAGES_PREFIX};

mod index;
mod manifest;
mod resource;
pub mod warc;

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
    /// A gzip-named WARC is not a valid gzip stream.
    #[error("invalid gzip WARC: {0}")]
    InvalidGzip(#[source] std::io::Error),
    /// A WARC-record sink was configured with an invalid gzip compression level.
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
    /// An index entry omits normative CDXJ fields.
    #[error(transparent)]
    NonconformingIndex(#[from] crate::cdxj::ConformanceError),
    /// An extension property duplicates a modeled JSON property.
    #[error(transparent)]
    ExtraProperty(#[from] crate::ExtraPropertyError),
    /// A previous member write failed after mutating the ZIP stream.
    #[error("WACZ writer is unusable after a member write failure")]
    Poisoned,
}

struct AtomicPublication {
    temporary: tempfile::NamedTempFile,
    target: PathBuf,
}

/// A WACZ assembler that tracks member digests and sizes for its final manifest.
pub struct WaczWriter<W: Write + Seek> {
    zip: ZipWriter<W>,
    resources: Vec<Resource<'static>>,
    config: WriterConfig,
    publication: Option<AtomicPublication>,
    poisoned: bool,
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
        let path = path.as_ref();
        if path.extension().and_then(|extension| extension.to_str()) != Some("wacz") {
            return Err(Error::InvalidWaczExtension(path.to_owned()));
        }
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("output already exists: {}", path.display()),
            )
            .into());
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temporary = tempfile::NamedTempFile::new_in(parent)?;
        let writer = BufWriter::new(temporary.reopen()?);
        let mut wacz = Self::with_config(writer, config);
        wacz.publication = Some(AtomicPublication {
            temporary,
            target: path.to_owned(),
        });
        Ok(wacz)
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
            publication: None,
            poisoned: false,
        }
    }

    /// Add WARC data under `archive/` using ZIP `STORE`.
    pub fn add_warc<R: Read>(&mut self, name: &str, mut reader: R) -> Result<(), Error> {
        let gzip = name.strip_suffix(".warc.gz").is_some();
        if !(gzip || name.strip_suffix(".warc").is_some()) {
            return Err(Error::InvalidWarcName(name.to_owned()));
        }
        let path = format!("{ARCHIVE_PREFIX}{name}");
        if gzip {
            let mut spool = tempfile::tempfile()?;
            std::io::copy(&mut reader, &mut spool)?;
            spool.seek(SeekFrom::Start(0))?;
            std::io::copy(
                &mut flate2::read::MultiGzDecoder::new(&mut spool),
                &mut std::io::sink(),
            )
            .map_err(Error::InvalidGzip)?;
            spool.seek(SeekFrom::Start(0))?;
            self.add_typed_resource(&path, spool)
        } else {
            self.add_typed_resource(&path, reader)
        }
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
