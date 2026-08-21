//! Streaming WARC-member construction.

use std::io::{Seek, Write};

use archivindex_warc::io::write::{WarcWriter, Written};
use archivindex_warc::parse::raw;
use zip::ZipWriter;

use super::resource::{HashingWriter, options_for, resource_name};
use super::{Error, WaczWriter};
use crate::ARCHIVE_PREFIX;
use crate::frictionless::resource::Resource;

/// A WARC member being written directly into a WACZ package.
///
/// Call [`finish`](Self::finish) after writing every record. Dropping an unfinished sink leaves
/// its parent WACZ writer poisoned, since the ZIP member may be incomplete.
pub struct WarcSink<'a, W: Write + Seek> {
    writer: WarcWriter<HashingWriter<&'a mut ZipWriter<W>>>,
    resources: &'a mut Vec<Resource<'static>>,
    poisoned: &'a mut bool,
    path: String,
    gzip: bool,
    gzip_compression_level: u32,
}

impl<W: Write + Seek> WaczWriter<W> {
    /// Start a WARC member that accepts records directly without an intermediate WARC file.
    pub fn start_warc(&mut self, name: &str) -> Result<WarcSink<'_, W>, Error> {
        let Some(gzip) = crate::paths::warc_gzip(name) else {
            return Err(Error::InvalidWarcName(name.to_owned()));
        };
        let path = format!("{ARCHIVE_PREFIX}{name}");
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        self.validate_path(&path)?;
        self.poisoned = true;
        self.zip
            .start_file(&path, options_for(&path, self.config.zip_compression_level))?;
        let writer = WarcWriter::new(HashingWriter::new(&mut self.zip)).with_digests();
        Ok(WarcSink {
            writer,
            resources: &mut self.resources,
            poisoned: &mut self.poisoned,
            path,
            gzip,
            gzip_compression_level: self.config.gzip_compression_level,
        })
    }
}

impl<W: Write + Seek> WarcSink<'_, W> {
    /// Write one validated raw WARC record and return its member-relative frame.
    pub fn write(&mut self, record: &raw::Record) -> Result<Written, Error> {
        if self.gzip {
            Ok(self
                .writer
                .write_gzip_with_level(record, self.gzip_compression_level)?)
        } else {
            Ok(self.writer.write(record)?)
        }
    }

    /// Finish the WARC member and register its digest and size in the package manifest.
    pub fn finish(mut self) -> Result<(), Error> {
        self.writer.flush()?;
        let hashing = self.writer.into_inner();
        let (hash, bytes) = hashing.finish();
        self.resources.push(Resource::new(
            resource_name(&self.path).to_owned(),
            self.path,
            hash,
            bytes,
        ));
        *self.poisoned = false;
        Ok(())
    }
}
