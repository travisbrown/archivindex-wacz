//! ZIP member layout, path validation, and manifest resource tracking.

use std::io::{Read, Seek, Write};

use sha2::Digest as _;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::{Error, WaczWriter};
use crate::digest::Sha256Digest;
use crate::frictionless::Resource;
use crate::{ARCHIVE_PREFIX, DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, GZIP_EXTENSION};

impl<W: Write + Seek> WaczWriter<W> {
    /// Add a custom resource and track its manifest digest and size.
    pub fn add_resource<R: Read>(&mut self, path: &str, mut reader: R) -> Result<(), Error> {
        self.add_member(path, options_for(path), |writer| {
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
        self.validate_path(path)?;
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
        Ok(())
    }

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

pub(super) struct HashingWriter<W> {
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

pub(super) fn options_for(path: &str) -> SimpleFileOptions {
    let method = if path.starts_with(ARCHIVE_PREFIX) || path.ends_with(GZIP_EXTENSION) {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    };
    SimpleFileOptions::default()
        .compression_method(method)
        .large_file(true)
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Return the manifest identifier for a resource path.
///
/// The conventional WACZ path uses camel case, while Data Package resource names permit only
/// lowercase ASCII. The path remains `pages/extraPages.jsonl`; only its manifest identifier is
/// normalized.
fn resource_name(path: &str) -> &str {
    match file_name(path) {
        "extraPages.jsonl" => "extra-pages.jsonl",
        name => name,
    }
}
