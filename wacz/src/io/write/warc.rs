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
/// Pre-rendered WARC bytes can be streamed through [`Write`], while
/// [`write_record`](Self::write_record) validates and renders one raw record. Call
/// [`finish`](Self::finish) after writing all content. Dropping an unfinished sink leaves its
/// parent WACZ writer poisoned, since the ZIP member may be incomplete.
pub struct WarcSink<'a, W: Write + Seek> {
    writer: HashingWriter<&'a mut ZipWriter<W>>,
    resources: &'a mut Vec<Resource<'static>>,
    poisoned: &'a mut bool,
    path: String,
    gzip: bool,
    gzip_compression_level: u32,
}

impl<W: Write + Seek> WaczWriter<W> {
    /// Start a WARC member that accepts pre-rendered bytes or records without an intermediate file.
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
        let writer = HashingWriter::new(&mut self.zip);
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
    pub fn write_record(&mut self, record: &raw::Record) -> Result<Written, Error> {
        let offset = self.writer.bytes();
        let mut writer = WarcWriter::new(&mut self.writer).with_digests();
        let mut written = if self.gzip {
            writer.write_gzip_with_level(record, self.gzip_compression_level)?
        } else {
            writer.write(record)?
        };
        written.offset = offset;
        Ok(written)
    }

    /// Finish the WARC member and register its digest and size in the package manifest.
    pub fn finish(mut self) -> Result<(), Error> {
        self.writer.flush()?;
        let (hash, bytes) = self.writer.finish();
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

impl<W: Write + Seek> Write for WarcSink<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.writer.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}
