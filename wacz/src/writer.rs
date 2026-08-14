//! Assembling a new WACZ file.

use std::borrow::Cow;
// The anonymous import makes `write!` work on `String` without shadowing `std::io::Write`.
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::Digest as _;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::cdxj;
use crate::digest::Sha256Digest;
use crate::frictionless::{DataPackage, DataPackageDigest, PROFILE, Resource, WACZ_VERSION};
use crate::pages::{self, Page, PageListHeader};
use crate::{
    ARCHIVE_PREFIX, DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, ExtraProperties, GZIP_EXTENSION,
    INDEXES_PREFIX, PAGES_PREFIX,
};

/// The default `software` manifest property written by this crate.
const SOFTWARE: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// The default number of characters in synthetic page identifiers.
const DEFAULT_PAGE_ID_LENGTH: usize = 24;

/// The default number of CDX lines per gzip block in a `ZipNum` index, matching `py-wacz`.
const DEFAULT_ZIPNUM_LINES: usize = 1024;

/// The format identifier in the `!meta` header line of a `ZipNum` summary file.
const ZIPNUM_FORMAT: &str = "cdxj-gzip-1.0";

/// The format of the CDXJ index written by [`WaczWriter::add_index`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexFormat {
    /// A plain-text CDXJ file.
    Plain,
    /// A `ZipNum` compressed index, as written by `py-wacz`: the sorted CDX lines are grouped into
    /// blocks, each block is compressed as an independent gzip member in a `.cdx.gz` data file, and
    /// a plain-text `.idx` summary file locates each block by offset, length, and digest, allowing
    /// binary search over the compressed index.
    ZipNum {
        /// The number of CDX lines per gzip block.
        lines: usize,
    },
}

impl IndexFormat {
    /// The `ZipNum` format with the standard block size of 1024 lines, matching `py-wacz`.
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
    /// The number of characters in the synthetic identifiers given to pages written without one
    /// (see [`pages::synthetic_id`]).
    pub page_id_length: usize,
    /// The format of the CDXJ index written by [`WaczWriter::add_index`].
    pub index_format: IndexFormat,
}

impl Default for WriterConfig {
    /// The default configuration: 24-character synthetic page identifiers and a plain-text index.
    fn default() -> Self {
        Self {
            page_id_length: DEFAULT_PAGE_ID_LENGTH,
            index_format: IndexFormat::Plain,
        }
    }
}

/// An error type for WACZ writing.
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
    /// A file to be added under `archive/` does not have a usable UTF-8 file name.
    #[error("invalid WARC file name: {}", .0.display())]
    InvalidFileName(PathBuf),
    /// A file path contains backslashes or empty, `.`, or `..` segments (which covers absolute and
    /// directory paths).
    #[error("invalid member path: {0}")]
    InvalidMemberPath(String),
    /// A file path repeats an already-written file or collides with a manifest file written by
    /// [`WaczWriter::finish`].
    #[error("duplicate member path: {0}")]
    DuplicateMemberPath(String),
}

/// The contextual manifest properties written by [`WaczWriter::finish`].
///
/// All fields are optional; `created` defaults to the current time and `software` to this crate's
/// name and version.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PackageMetadata {
    /// A short description of the collection.
    pub title: Option<String>,
    /// A longer, possibly Markdown-formatted, description of the collection.
    pub description: Option<String>,
    /// When the WACZ file was created.
    pub created: Option<DateTime<Utc>>,
    /// When the WACZ file was last modified.
    pub modified: Option<DateTime<Utc>>,
    /// A description of the software that created the WACZ file.
    pub software: Option<String>,
    /// The URL of the primary entry page for replay.
    pub main_page_url: Option<String>,
    /// The capture date to use when replaying the primary entry page.
    pub main_page_date: Option<DateTime<Utc>>,
}

/// A writer that assembles a WACZ and tracks each file's digest and size for the manifest written
/// by [`finish`](Self::finish).
pub struct WaczWriter<W: Write + Seek> {
    zip: ZipWriter<W>,
    resources: Vec<Resource<'static>>,
    config: WriterConfig,
}

impl WaczWriter<BufWriter<File>> {
    /// Create a WACZ file at the given path, refusing to overwrite an existing file.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Self::create_with_config(path, WriterConfig::default())
    }

    /// Create a WACZ file at the given path with the given configuration, refusing to overwrite an
    /// existing file.
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
    /// Create a new writer with the default configuration.
    #[must_use]
    pub fn new(writer: W) -> Self {
        Self::with_config(writer, WriterConfig::default())
    }

    /// Create a new writer with the given configuration.
    #[must_use]
    pub fn with_config(writer: W, config: WriterConfig) -> Self {
        Self {
            zip: ZipWriter::new(writer),
            resources: Vec::new(),
            config,
        }
    }

    /// Add WARC data under `archive/` with the given file name.
    ///
    /// Files under `archive/` are stored without ZIP compression (`STORE`), as the specification
    /// recommends, so that readers can seek to CDX offsets within them. Names ending in `.gz` must
    /// hold gzip data.
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
        let file = File::open(path)?;

        self.add_warc(name, BufReader::new(file))
    }

    /// Write the CDXJ index under `indexes/`, in the configured
    /// [`index_format`](WriterConfig::index_format), sorted as required for binary search.
    ///
    /// `name` is the base file name and should not end in `.gz` (conventionally `index.cdx`). With
    /// [`IndexFormat::Plain`], a single plain-text file is written with that name. With
    /// [`IndexFormat::ZipNum`], a `{name}.gz` data file and an `.idx` summary file (named by
    /// replacing a `.cdx` suffix, so conventionally `index.idx`) are written following the
    /// `py-wacz` layout.
    pub fn add_index<'a, I: IntoIterator<Item = &'a cdxj::Item<'a>>>(
        &mut self,
        name: &str,
        items: I,
    ) -> Result<(), Error> {
        // py-wacz sorts and deduplicates the rendered lines themselves, which orders items by key
        // and then timestamp; both formats share the behavior so that identical input produces an
        // identically-ordered index either way.
        let mut rendered = items
            .into_iter()
            .map(|item| format!("{item}\n"))
            .collect::<Vec<_>>();
        rendered.sort_unstable();
        rendered.dedup();

        match self.config.index_format {
            IndexFormat::Plain => {
                let path = format!("{INDEXES_PREFIX}{name}");

                self.add_member(&path, options_for(&path), |writer| {
                    for line in &rendered {
                        writer.write_all(line.as_bytes())?;
                    }

                    Ok(())
                })
            }
            IndexFormat::ZipNum { lines } => self.add_zipnum_index(name, &rendered, lines),
        }
    }

    /// Write a `ZipNum` index pair: blocks of `lines` gzipped CDX lines in `{name}.gz`, located by
    /// a plain-text summary file.
    fn add_zipnum_index(
        &mut self,
        name: &str,
        rendered: &[String],
        lines: usize,
    ) -> Result<(), Error> {
        let data_name = format!("{name}{GZIP_EXTENSION}");
        let idx_name = format!("{}.idx", name.strip_suffix(".cdx").unwrap_or(name));
        let data_path = format!("{INDEXES_PREFIX}{data_name}");
        let idx_path = format!("{INDEXES_PREFIX}{idx_name}");

        // The file name is JSON-escaped into the header line; serializing a string to a `String`
        // cannot fail.
        let escaped_data_name =
            serde_json::to_string(&data_name).expect("string serialization cannot fail");

        let mut summary = String::new();
        writeln!(
            summary,
            "!meta 0 {{\"format\": \"{ZIPNUM_FORMAT}\", \"filename\": {escaped_data_name}}}"
        )
        .expect("writing to a String cannot fail");

        self.add_member(&data_path, options_for(&data_path), |writer| {
            let mut offset: u64 = 0;

            for block in rendered.chunks(lines.max(1)) {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                for line in block {
                    encoder.write_all(line.as_bytes())?;
                }
                let compressed = encoder.finish()?;

                let length = compressed.len();
                let digest = Sha256Digest::compute(&compressed);
                writeln!(
                    summary,
                    "{} {{\"offset\": {offset}, \"length\": {length}, \"digest\": \"{digest}\"}}",
                    line_prefix(&block[0])
                )
                .expect("writing to a String cannot fail");

                writer.write_all(&compressed)?;
                offset += length as u64;
            }

            if offset == 0 {
                // Keep the ZipNum data file valid gzip even when the index is empty.
                let compressed = GzEncoder::new(Vec::new(), Compression::default()).finish()?;
                writer.write_all(&compressed)?;
            }

            Ok(())
        })?;

        self.add_member(&idx_path, options_for(&idx_path), |writer| {
            writer.write_all(summary.as_bytes())?;

            Ok(())
        })
    }

    /// Write the required page list at `pages/pages.jsonl`.
    ///
    /// Pages without an identifier are given a synthetic one derived from their timestamp and URL
    /// (see [`pages::synthetic_id`]), truncated to the configured
    /// [`page_id_length`](WriterConfig::page_id_length).
    pub fn add_pages<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        self.add_page_list("pages.jsonl", header, pages)
    }

    /// Write a page list under `pages/` with the given file name (for example `extraPages.jsonl`).
    ///
    /// Pages without an identifier are given a synthetic one derived from their timestamp and URL
    /// (see [`pages::synthetic_id`]), truncated to the configured
    /// [`page_id_length`](WriterConfig::page_id_length).
    pub fn add_page_list<'a, I: IntoIterator<Item = &'a Page<'a>>>(
        &mut self,
        name: &str,
        header: &PageListHeader<'_>,
        pages: I,
    ) -> Result<(), Error> {
        let id_length = self.config.page_id_length;
        let path = format!("{PAGES_PREFIX}{name}");

        self.add_member(&path, options_for(&path), |writer| {
            Ok(pages::write_page_list_with_synthetic_ids(
                writer, header, pages, id_length,
            )?)
        })
    }

    /// Add a file at the given path and record it in the manifest.
    ///
    /// Files under `archive/` and paths ending in `.gz` (which must contain gzip data) use `STORE`;
    /// other files use `DEFLATE`. The specification permits custom files anywhere outside of the
    /// `archive/`, `indexes/`, and `pages/` directories, which are reserved for the dedicated
    /// methods.
    pub fn add_resource<R: Read>(&mut self, path: &str, mut reader: R) -> Result<(), Error> {
        self.add_member(path, options_for(path), |writer| {
            std::io::copy(&mut reader, writer)?;

            Ok(())
        })
    }

    /// Write the manifest and digest files and finish the ZIP, returning the underlying writer.
    pub fn finish(self, metadata: PackageMetadata) -> Result<W, Error> {
        let Self {
            mut zip,
            resources,
            config: _,
        } = self;

        let package = DataPackage {
            profile: Cow::Borrowed(PROFILE),
            wacz_version: Cow::Borrowed(WACZ_VERSION),
            resources,
            name: None,
            id: None,
            title: metadata.title.map(Cow::Owned),
            description: metadata.description.map(Cow::Owned),
            keywords: Vec::new(),
            homepage: None,
            image: None,
            version: None,
            sources: Vec::new(),
            licenses: Vec::new(),
            contributors: Vec::new(),
            created: Some(metadata.created.unwrap_or_else(Utc::now)),
            modified: metadata.modified,
            software: Some(
                metadata
                    .software
                    .map_or(Cow::Borrowed(SOFTWARE), Cow::Owned),
            ),
            main_page_url: metadata.main_page_url.map(Cow::Owned),
            main_page_date: metadata.main_page_date,
            extra: ExtraProperties::default(),
        };

        let manifest = serde_json::to_vec_pretty(&package).map_err(Error::Manifest)?;
        zip.start_file(DATA_PACKAGE_PATH, options_for(DATA_PACKAGE_PATH))?;
        zip.write_all(&manifest)?;

        let digest = DataPackageDigest {
            path: Cow::Borrowed(DATA_PACKAGE_PATH),
            hash: Sha256Digest::compute(&manifest),
            signed_data: None,
        };

        let digest_bytes = serde_json::to_vec_pretty(&digest).map_err(Error::Manifest)?;
        zip.start_file(
            DATA_PACKAGE_DIGEST_PATH,
            options_for(DATA_PACKAGE_DIGEST_PATH),
        )?;
        zip.write_all(&digest_bytes)?;

        Ok(zip.finish()?)
    }

    /// Start a ZIP entry, write its contents through a hashing writer, and record the resulting
    /// resource. All file writes use this path so that the manifest matches the ZIP contents.
    fn add_member<F>(
        &mut self,
        path: &str,
        options: SimpleFileOptions,
        write: F,
    ) -> Result<(), Error>
    where
        F: FnOnce(&mut HashingWriter<&mut ZipWriter<W>>) -> Result<(), Error>,
    {
        self.validate_path(path)?;
        self.zip.start_file(path, options)?;

        let mut writer = HashingWriter::new(&mut self.zip);
        write(&mut writer)?;
        let (hash, bytes) = writer.finish();

        self.resources.push(Resource::new(
            file_name(path).to_owned(),
            path.to_owned(),
            hash,
            bytes,
        ));

        Ok(())
    }

    /// Check that a file path is safely relative and not already used.
    ///
    /// Backslashes and empty, `.`, or `..` segments are rejected; the empty-segment check also
    /// covers the empty path, absolute paths (whose leading slash yields an empty first segment),
    /// and directory paths (whose trailing slash yields an empty last segment).
    fn validate_path(&self, path: &str) -> Result<(), Error> {
        if path.contains('\\')
            || path
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        {
            return Err(Error::InvalidMemberPath(path.to_owned()));
        }

        if path == DATA_PACKAGE_PATH
            || path == DATA_PACKAGE_DIGEST_PATH
            || self.resources.iter().any(|resource| resource.path == path)
        {
            return Err(Error::DuplicateMemberPath(path.to_owned()));
        }

        Ok(())
    }
}

/// A writer that computes the digest and size of the bytes passing through it.
struct HashingWriter<W> {
    underlying: W,
    hasher: sha2::Sha256,
    bytes: u64,
}

impl<W> HashingWriter<W> {
    fn new(underlying: W) -> Self {
        Self {
            underlying,
            hasher: sha2::Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (Sha256Digest, u64) {
        (Sha256Digest(self.hasher.finalize().into()), self.bytes)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.underlying.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes += written as u64;

        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.underlying.flush()
    }
}

/// The ZIP entry options for a file.
///
/// Files under `archive/` use `STORE` so that CDX offsets remain seekable, as recommended by the
/// specification. Gzip files use `STORE` because the specification prohibits recompressing
/// already-compressed data; other files use `DEFLATE`.
fn options_for(path: &str) -> SimpleFileOptions {
    let method = if path.starts_with(ARCHIVE_PREFIX) || path.ends_with(GZIP_EXTENSION) {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    };

    // `large_file` permits entries over the ZIP64 threshold, at a cost of a few bytes of header
    // overhead per ZIP entry.
    SimpleFileOptions::default()
        .compression_method(method)
        .large_file(true)
}

/// The prefix of a rendered CDX line locating a `ZipNum` block: its search key and timestamp, i.e.
/// everything before the third space-separated field (the JSON block, which may itself contain
/// spaces or braces, as may a search key holding a `{` from a query string).
fn line_prefix(line: &str) -> &str {
    line.match_indices(' ')
        .nth(1)
        .map_or_else(|| line.trim_end(), |(index, _)| &line[..index])
}

/// The final segment of a file path, used as its resource name in the manifest.
fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}
