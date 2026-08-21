//! Reading files from an existing WACZ.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use archivindex_warc::io::read::WarcReader;
use flate2::read::MultiGzDecoder;
use zip::ZipArchive;
use zip::result::ZipError;

use bounded_static::IntoBoundedStatic;

use crate::cdxj::IndexReader;
use crate::digest::Sha256Digest;
use crate::frictionless::{DataPackage, DataPackageDigest};
use crate::pages::PageListReader;
use crate::{
    ARCHIVE_PREFIX, DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, GZIP_EXTENSION, INDEXES_PREFIX,
    PAGES_PATH,
};

pub mod random;
pub mod validate;

/// An error type for WACZ reading.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying stream could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The ZIP container is invalid.
    #[error(transparent)]
    Zip(#[from] ZipError),
    /// A requested file is absent from the WACZ.
    #[error("missing member: {0}")]
    MissingMember(String),
    /// The `datapackage.json` manifest could not be parsed.
    #[error("invalid data package manifest")]
    InvalidDataPackage(#[source] serde_json::Error),
    /// The `datapackage-digest.json` file could not be parsed.
    #[error("invalid data package digest")]
    InvalidDataPackageDigest(#[source] serde_json::Error),
    /// A page list could not be read.
    #[error(transparent)]
    Pages(#[from] crate::pages::Error),
    /// A CDXJ index entry or search key is invalid.
    #[error(transparent)]
    Cdxj(#[from] crate::cdxj::Error),
    /// A WARC record could not be parsed.
    #[error(transparent)]
    Warc(#[from] archivindex_warc::io::read::Error),
    /// A text index is not UTF-8.
    #[error("index is not UTF-8: {0}")]
    InvalidIndexEncoding(String),
    /// A `ZipNum` summary is malformed or names an unsupported format.
    #[error("invalid ZipNum summary: {0}")]
    InvalidZipNum(String),
    /// A byte range cannot be read because the ZIP member is compressed.
    #[error("random access requires a stored ZIP member: {0}")]
    CompressedMember(String),
    /// A byte range falls outside its member or overflows.
    #[error("range {offset}..{end} is outside member {path} ({size} bytes)")]
    RangeOutOfBounds {
        /// The member path.
        path: String,
        /// The requested starting offset.
        offset: u64,
        /// The requested exclusive end offset.
        end: u64,
        /// The member's uncompressed size.
        size: u64,
    },
    /// A required CDXJ capture field is absent.
    #[error("capture is missing CDXJ field `{0}`")]
    MissingCaptureField(&'static str),
    /// Bytes located by an index do not match their declared digest.
    #[error("digest mismatch for {path}: expected {expected}, computed {actual}")]
    DigestMismatch {
        /// The member or range being checked.
        path: String,
        /// The declared digest.
        expected: Sha256Digest,
        /// The computed digest.
        actual: Sha256Digest,
    },
    /// A manifest resource does not match its declared size or digest.
    #[error(
        "resource mismatch for {path}: expected {expected_size} bytes and {expected_hash}, \
         found {actual_size} bytes and {actual_hash}"
    )]
    ResourceMismatch {
        /// The resource path.
        path: String,
        /// The declared size.
        expected_size: u64,
        /// The observed size.
        actual_size: u64,
        /// The declared digest.
        expected_hash: Sha256Digest,
        /// The observed digest.
        actual_hash: Sha256Digest,
    },
    /// A capture range does not contain exactly one WARC record.
    #[error("capture range contains {0} WARC records; expected exactly one")]
    CaptureRecordCount(usize),
    /// A requested resource is not listed in the data package manifest.
    #[error("path is not a manifest resource: {0}")]
    UnlistedResource(String),
}

/// A buffered stream over one file in a WACZ, with gzip data decompressed.
pub type MemberReader<'a> = BufReader<Box<dyn Read + 'a>>;

/// Metadata needed to choose between sequential and random member access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberMetadata {
    /// The member path.
    pub path: String,
    /// The ZIP compression method applied to the member.
    pub compression: zip::CompressionMethod,
    /// The number of bytes stored in the ZIP container.
    pub compressed_size: u64,
    /// The size after ZIP decompression. For stored members this equals `compressed_size`.
    pub size: u64,
    /// The member's ZIP CRC-32.
    pub crc32: u32,
}

/// The outcome of checking a WACZ's declared fixity: whether each file listed in the manifest
/// matches its declared byte length and SHA-256 digest.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct Fixity {
    /// Paths whose digests and sizes match the manifest. Includes the manifest itself when a digest
    /// file is present and matches.
    pub verified: Vec<String>,
    /// Paths whose contents do not match the manifest.
    pub mismatched: Vec<String>,
    /// Paths listed in the manifest but absent from the WACZ.
    pub missing: Vec<String>,
}

impl Fixity {
    /// Whether every declared digest and size matched.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.mismatched.is_empty() && self.missing.is_empty()
    }
}

/// A reader over the files in a WACZ.
///
/// The underlying ZIP reader yields one decompressed stream at a time, so the file accessors borrow
/// this reader mutably and only one file can be read at once.
pub struct WaczReader<R> {
    archive: ZipArchive<R>,
    duplicate_members: Vec<String>,
}

impl WaczReader<BufReader<File>> {
    /// Open a WACZ file for reading.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        Self::new(BufReader::new(File::open(path)?))
    }
}

impl<R: Read + Seek> WaczReader<R> {
    /// Create a new reader, parsing the ZIP central directory.
    pub fn new(reader: R) -> Result<Self, Error> {
        let archive = ZipArchive::new(reader)?;
        let central_directory_start = archive.central_directory_start();
        let mut reader = archive.into_inner();
        let duplicate_members = duplicate_member_names(&mut reader, central_directory_start)?;

        Ok(Self {
            archive: ZipArchive::new(reader)?,
            duplicate_members,
        })
    }

    /// Read and parse the `datapackage.json` manifest.
    pub fn data_package(&mut self) -> Result<DataPackage<'static>, Error> {
        let bytes = self.member_bytes(DATA_PACKAGE_PATH)?;

        Ok(parse_data_package(&bytes)?.into_static())
    }

    /// Read and parse the `datapackage-digest.json` file.
    ///
    /// Returns `None` when the file is absent, since the specification only recommends it.
    pub fn data_package_digest(&mut self) -> Result<Option<DataPackageDigest<'static>>, Error> {
        match self.member_bytes(DATA_PACKAGE_DIGEST_PATH) {
            Ok(bytes) => serde_json::from_slice::<DataPackageDigest<'_>>(&bytes)
                .map(|digest| Some(digest.into_static()))
                .map_err(Error::InvalidDataPackageDigest),
            Err(Error::MissingMember(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Read the required `pages/pages.jsonl` page list.
    pub fn pages(&mut self) -> Result<PageListReader<MemberReader<'_>>, Error> {
        self.page_list(PAGES_PATH)
    }

    /// Read a page list by path (for example `pages/extraPages.jsonl`).
    pub fn page_list(&mut self, path: &str) -> Result<PageListReader<MemberReader<'_>>, Error> {
        let member = self.member_stream(path)?;

        Ok(PageListReader::with_source(member, path)?)
    }

    /// The paths of the WARC files, in unspecified order.
    pub fn warc_paths(&self) -> impl Iterator<Item = &str> {
        self.paths_under(ARCHIVE_PREFIX)
    }

    /// The paths of the index files, in unspecified order.
    pub fn index_paths(&self) -> impl Iterator<Item = &str> {
        self.paths_under(INDEXES_PREFIX)
    }

    /// All file paths in the WACZ, in unspecified order and excluding ZIP directory entries.
    pub fn member_paths(&self) -> impl Iterator<Item = &str> {
        self.archive
            .file_names()
            .filter(|name| !name.ends_with('/'))
    }

    /// File paths under a directory prefix, excluding ZIP directory entries.
    fn paths_under<'s>(&'s self, prefix: &'s str) -> impl Iterator<Item = &'s str> {
        self.archive
            .file_names()
            .filter(move |name| name.starts_with(prefix) && !name.ends_with('/'))
    }

    /// Read a CDXJ index by path, decompressing files with a `.gz` extension.
    pub fn index(&mut self, path: &str) -> Result<IndexReader<MemberReader<'_>>, Error> {
        Ok(IndexReader::with_source(self.member_stream(path)?, path))
    }

    /// Read a WARC file by path, decompressing files with a `.gz` extension.
    pub fn warc(&mut self, path: &str) -> Result<WarcReader<MemberReader<'_>>, Error> {
        Ok(WarcReader::new(self.member_stream(path)?))
    }

    /// Check the WACZ files against the digests and sizes declared by the manifest, and the
    /// manifest against the digest file if one is present.
    ///
    /// Missing, corrupt, and mismatched files are reported in the result rather than treated as
    /// errors, as is a digest file that cannot be parsed or that does not name the manifest.
    ///
    /// This checks declared fixity only: members that the manifest does not list are ignored, and
    /// conformance with the WACZ specification is not examined. Use [`validate`](Self::validate)
    /// for layered conformance checking, which can include this check as its fixity layer.
    pub fn verify_fixity(&mut self) -> Result<Fixity, Error> {
        let manifest_bytes = self.member_bytes(DATA_PACKAGE_PATH)?;
        let package = parse_data_package(&manifest_bytes)?;

        let mut fixity = Fixity::default();

        match self.data_package_digest() {
            Ok(Some(digest)) => {
                if digest.path == DATA_PACKAGE_PATH
                    && digest.hash == Sha256Digest::compute(&manifest_bytes)
                {
                    fixity.verified.push(DATA_PACKAGE_PATH.to_owned());
                } else {
                    fixity.mismatched.push(DATA_PACKAGE_PATH.to_owned());
                }
            }
            Ok(None) => {}
            // A digest file that cannot be parsed cannot corroborate the manifest.
            Err(Error::InvalidDataPackageDigest(_)) => {
                fixity.mismatched.push(DATA_PACKAGE_PATH.to_owned());
            }
            Err(error) => return Err(error),
        }

        for resource in &package.resources {
            match self.member(&resource.path) {
                Ok(member) => match Sha256Digest::from_reader(member) {
                    Ok((hash, bytes)) if hash == resource.hash && bytes == resource.bytes => {
                        fixity.verified.push(resource.path.to_string());
                    }
                    Ok(_) => fixity.mismatched.push(resource.path.to_string()),
                    // The ZIP layer reports a corrupt entry (a CRC or decompression failure) as
                    // `InvalidData` once the stream has been read to its end.
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        fixity.mismatched.push(resource.path.to_string());
                    }
                    Err(error) => return Err(error.into()),
                },
                Err(Error::MissingMember(path)) => fixity.missing.push(path),
                Err(error) => return Err(error),
            }
        }

        Ok(fixity)
    }

    /// Open a ZIP entry by path, mapping the ZIP crate's not-found error to a dedicated variant.
    /// Open a member's content without interpreting a `.gz` suffix.
    ///
    /// ZIP compression is decoded by the ZIP layer; gzip content remains compressed. This is the
    /// byte representation whose offsets and lengths are used by WACZ indexes.
    pub fn member(&mut self, path: &str) -> Result<zip::read::ZipFile<'_, R>, Error> {
        match self.archive.by_name(path) {
            Err(ZipError::FileNotFound) => Err(Error::MissingMember(path.to_owned())),
            result => Ok(result?),
        }
    }

    /// Return metadata for a member without reading its content.
    pub fn member_metadata(&mut self, path: &str) -> Result<MemberMetadata, Error> {
        let member = self.member(path)?;

        Ok(MemberMetadata {
            path: path.to_owned(),
            compression: member.compression(),
            compressed_size: member.compressed_size(),
            size: member.size(),
            crc32: member.crc32(),
        })
    }

    /// Open the bytes physically stored for a ZIP member, without ZIP or gzip decompression.
    pub fn raw_member(&mut self, path: &str) -> Result<zip::read::ZipFile<'_, R>, Error> {
        let index = self
            .archive
            .index_for_name(path)
            .ok_or_else(|| Error::MissingMember(path.to_owned()))?;

        Ok(self.archive.by_index_raw(index)?)
    }

    /// Open a file by path as a buffered stream, decompressing files with a `.gz` extension.
    pub fn member_stream(&mut self, path: &str) -> Result<MemberReader<'_>, Error> {
        let is_gzip = path.ends_with(GZIP_EXTENSION);
        let member = self.member(path)?;

        // The buffering lives on the decoded side (the `BufReader` below); the compressed side
        // reads from the archive's own reader, which is buffered when opened from a path.
        let stream: Box<dyn Read + '_> = if is_gzip {
            // The gzip header is parsed on the first read, so invalid gzip data produces an error
            // from the returned stream.
            Box::new(MultiGzDecoder::new(member))
        } else {
            Box::new(member)
        };

        Ok(BufReader::new(stream))
    }

    /// Open a member as a stream, decoding both ZIP compression and gzip content.
    ///
    /// This is an explicit-name alias for [`member_stream`](Self::member_stream).
    pub fn decoded_member(&mut self, path: &str) -> Result<MemberReader<'_>, Error> {
        self.member_stream(path)
    }

    /// Read the full contents of a file by path.
    pub fn member_bytes(&mut self, path: &str) -> Result<Vec<u8>, Error> {
        let mut member = self.member(path)?;
        // The declared size is untrusted input, so the preallocation it drives is capped.
        let capacity = usize::try_from(member.size())
            .unwrap_or(usize::MAX)
            .min(MAX_PREALLOCATION);
        let mut bytes = Vec::with_capacity(capacity);
        member.read_to_end(&mut bytes)?;

        Ok(bytes)
    }

    /// Read a complete member, additionally decoding gzip content when its name ends in `.gz`.
    pub fn decoded_member_bytes(&mut self, path: &str) -> Result<Vec<u8>, Error> {
        let mut member = self.member_stream(path)?;
        let mut bytes = Vec::new();
        member.read_to_end(&mut bytes)?;

        Ok(bytes)
    }

    /// Read and verify an arbitrary resource listed in `datapackage.json`.
    ///
    /// The returned bytes have ZIP compression removed but retain any content-level compression,
    /// such as gzip. Both the manifest size and SHA-256 digest are checked.
    pub fn resource_bytes(&mut self, path: &str) -> Result<Vec<u8>, Error> {
        let package = self.data_package()?;
        let resource = package
            .resources
            .iter()
            .find(|resource| resource.path == path)
            .ok_or_else(|| Error::UnlistedResource(path.to_owned()))?;
        let expected_hash = resource.hash;
        let expected_size = resource.bytes;
        let bytes = self.member_bytes(path)?;
        let actual_hash = Sha256Digest::compute(&bytes);
        let actual_size = bytes.len() as u64;

        if actual_hash != expected_hash || actual_size != expected_size {
            return Err(Error::ResourceMismatch {
                path: path.to_owned(),
                expected_size,
                actual_size,
                expected_hash,
                actual_hash,
            });
        }

        Ok(bytes)
    }
}

/// Read raw central-directory entries because `zip` indexes them by name and therefore hides
/// earlier entries when an archive contains duplicate names.
fn duplicate_member_names<R: Read + Seek>(
    reader: &mut R,
    central_directory_start: u64,
) -> Result<Vec<String>, std::io::Error> {
    const CENTRAL_HEADER_SIZE: usize = 46;
    const CENTRAL_HEADER_SIGNATURE: [u8; 4] = *b"PK\x01\x02";

    reader.seek(SeekFrom::Start(central_directory_start))?;
    let mut names = HashSet::<Vec<u8>>::new();
    let mut reported = HashSet::<Vec<u8>>::new();
    let mut duplicates = Vec::new();

    loop {
        let mut header = [0_u8; CENTRAL_HEADER_SIZE];
        reader.read_exact(&mut header[..4])?;
        if header[..4] != CENTRAL_HEADER_SIGNATURE {
            break;
        }
        reader.read_exact(&mut header[4..])?;

        let name_len = u16::from_le_bytes([header[28], header[29]]) as usize;
        let extra_len = i64::from(u16::from_le_bytes([header[30], header[31]]));
        let comment_len = i64::from(u16::from_le_bytes([header[32], header[33]]));
        let mut name = vec![0_u8; name_len];
        reader.read_exact(&mut name)?;
        reader.seek(SeekFrom::Current(extra_len + comment_len))?;

        if !names.insert(name.clone()) && reported.insert(name.clone()) {
            duplicates.push(String::from_utf8_lossy(&name).into_owned());
        }
    }

    Ok(duplicates)
}

/// The maximum capacity preallocated from an untrusted ZIP entry size.
const MAX_PREALLOCATION: usize = 1 << 20;

/// Parse manifest bytes, mapping parse failures to the dedicated variant.
fn parse_data_package(bytes: &[u8]) -> Result<DataPackage<'_>, Error> {
    serde_json::from_slice(bytes).map_err(Error::InvalidDataPackage)
}
