//! Staging and publishing WACZ files.

use std::fs::File;
use std::io::{BufWriter, IntoInnerError};
use std::ops::{Deref, DerefMut};
use std::path::Path;

use archivindex_publication::{Policy, Publication};

use super::{Error, WaczWriter, WriterConfig};
use crate::frictionless::DataPackageBuilder;

/// A WACZ file staged beside its destination until [`Self::finish`] publishes it.
///
/// Member-writing methods are provided by the underlying [`WaczWriter`] through dereferencing.
/// Dropping this writer before finishing removes its temporary file on a best-effort basis. No
/// destination placeholder is created.
pub struct WaczFileWriter {
    writer: WaczWriter<BufWriter<Publication>>,
}

impl WaczFileWriter {
    /// Create a WACZ file without overwriting an existing destination.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::create_with_config(path, WriterConfig::default())
    }

    /// Create a configured WACZ file without overwriting an existing destination.
    pub fn create_with_config(path: impl AsRef<Path>, config: WriterConfig) -> Result<Self, Error> {
        let path = path.as_ref();
        if path.extension().and_then(|extension| extension.to_str()) != Some("wacz") {
            return Err(Error::InvalidWaczExtension(path.to_owned()));
        }
        let publication = Publication::new(path, Policy::CreateNew)?;
        Ok(Self {
            writer: WaczWriter::with_config(BufWriter::new(publication), config),
        })
    }

    /// Finish and flush the ZIP, then sync and publish the completed file.
    ///
    /// Returns the published file handle. Publication refuses an existing destination; a
    /// directory-sync error means the file is already visible and is retained. Directory
    /// synchronization is supported on Unix, as documented by [`Publication`].
    pub fn finish(self, metadata: DataPackageBuilder) -> Result<File, Error> {
        let buffered = self.writer.finish(metadata)?;
        let publication = buffered.into_inner().map_err(IntoInnerError::into_error)?;
        Ok(publication.publish()?)
    }
}

impl Deref for WaczFileWriter {
    type Target = WaczWriter<BufWriter<Publication>>;

    fn deref(&self) -> &Self::Target {
        &self.writer
    }
}

impl DerefMut for WaczFileWriter {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.writer
    }
}
