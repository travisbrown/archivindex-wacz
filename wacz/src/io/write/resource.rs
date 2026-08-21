//! ZIP member layout, path validation, and manifest resource tracking.

use std::io::{Read, Seek, Write};

use sha2::Digest as _;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::{Error, WaczWriter};
use crate::digest::Sha256Digest;
use crate::frictionless::resource::Resource;
use crate::{
    ARCHIVE_PREFIX, DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, GZIP_EXTENSION, INDEXES_PREFIX,
    PAGES_PREFIX,
};

impl<W: Write + Seek> WaczWriter<W> {
    /// Add a custom resource and track its manifest digest and size.
    pub fn add_resource<R: Read>(&mut self, path: &str, mut reader: R) -> Result<(), Error> {
        if path.starts_with(ARCHIVE_PREFIX)
            || path.starts_with(INDEXES_PREFIX)
            || path.starts_with(PAGES_PREFIX)
        {
            return Err(Error::ReservedResourcePath(path.to_owned()));
        }
        self.add_typed_resource(path, &mut reader)
    }

    pub(super) fn add_typed_resource<R: Read>(
        &mut self,
        path: &str,
        mut reader: R,
    ) -> Result<(), Error> {
        let options = options_for(path, self.config.zip_compression_level)?;
        self.add_member(path, options, |writer| {
            std::io::copy(&mut reader, writer)?;
            Ok(())
        })
    }

    pub(super) fn add_member<F>(
        &mut self,
        path: &str,
        options: SimpleFileOptions,
        write: F,
    ) -> Result<(), Error>
    where
        F: FnOnce(&mut HashingWriter<&mut ZipWriter<W>>) -> Result<(), Error>,
    {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        self.validate_path(path)?;
        self.poisoned = true;
        self.zip.start_file(path, options)?;
        let mut writer = HashingWriter::new(&mut self.zip);
        write(&mut writer)?;
        let (hash, bytes) = writer.finish();
        self.resources.push(Resource::new(
            resource_name(path).to_owned(),
            path.to_owned(),
            hash,
            bytes,
        ));
        self.poisoned = false;
        Ok(())
    }

    pub(super) fn validate_path(&self, path: &str) -> Result<(), Error> {
        if !crate::paths::is_safe(path) {
            return Err(Error::InvalidMemberPath(path.to_owned()));
        }
        if path == DATA_PACKAGE_PATH
            || path == DATA_PACKAGE_DIGEST_PATH
            || self.resources.iter().any(|resource| resource.path == path)
        {
            return Err(Error::DuplicateMemberPath(path.to_owned()));
        }
        let name = resource_name(path);
        if !crate::paths::valid_resource_name(name)
            || self.resources.iter().any(|resource| resource.name == name)
        {
            return Err(Error::InvalidResourceName(name.to_owned()));
        }
        Ok(())
    }
}

pub(super) struct HashingWriter<W> {
    underlying: W,
    hasher: sha2::Sha256,
    bytes: u64,
}

impl<W> HashingWriter<W> {
    pub(super) fn new(underlying: W) -> Self {
        Self {
            underlying,
            hasher: sha2::Sha256::new(),
            bytes: 0,
        }
    }

    pub(super) fn finish(self) -> (Sha256Digest, u64) {
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

pub(super) fn options_for(
    path: &str,
    compression_level: Option<u32>,
) -> Result<SimpleFileOptions, Error> {
    if let Some(level) = compression_level
        && !(super::MIN_ZIP_COMPRESSION_LEVEL..=super::MAX_ZIP_COMPRESSION_LEVEL).contains(&level)
    {
        return Err(Error::InvalidZipCompressionLevel(level));
    }
    let method = if path.starts_with(ARCHIVE_PREFIX) || path.ends_with(GZIP_EXTENSION) {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    };
    let mut options = SimpleFileOptions::default()
        .compression_method(method)
        .large_file(true);
    if method == CompressionMethod::Deflated {
        options = options.compression_level(compression_level.map(i64::from));
    }
    Ok(options)
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Return the manifest identifier for a resource path.
///
/// The conventional WACZ path uses camel case, while Data Package resource names permit only
/// lowercase ASCII. The path remains `pages/extraPages.jsonl`; only its manifest identifier is
/// normalized.
pub(super) fn resource_name(path: &str) -> &str {
    match file_name(path) {
        "extraPages.jsonl" => "extra-pages.jsonl",
        name => name,
    }
}
