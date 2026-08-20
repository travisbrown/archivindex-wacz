//! Final data-package manifest and digest writing.

use std::borrow::Cow;
use std::io::{Seek, Write};

use chrono::Utc;

use super::resource::options_for;
use super::{Error, WaczWriter};
use crate::digest::Sha256Digest;
use crate::frictionless::{DataPackageBuilder, DataPackageDigest};
use crate::{DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH};

const SOFTWARE: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

impl<W: Write + Seek> WaczWriter<W> {
    /// Write the manifest and digest files and finish the ZIP.
    pub fn finish(self, metadata: DataPackageBuilder) -> Result<W, Error> {
        let mut missing = Vec::new();
        if !self
            .resources
            .iter()
            .any(|resource| resource.path.starts_with(crate::ARCHIVE_PREFIX))
        {
            missing.push("archive/*.warc[.gz]");
        }
        if !self
            .resources
            .iter()
            .any(|resource| resource.path.starts_with(crate::INDEXES_PREFIX))
        {
            missing.push("indexes/*");
        }
        if !self
            .resources
            .iter()
            .any(|resource| resource.path == crate::PAGES_PATH)
        {
            missing.push("pages/pages.jsonl");
        }
        if !missing.is_empty() {
            return Err(Error::MissingRequiredMembers(missing));
        }
        self.finish_unchecked(metadata)
    }

    /// Finish a package without checking the required WACZ member classes.
    ///
    /// This is intended for malformed-archive fixtures and compatibility tooling. Normal package
    /// construction should use [`Self::finish`]. Member paths and resource names are still checked
    /// when they are inserted.
    pub fn finish_unchecked(self, metadata: DataPackageBuilder) -> Result<W, Error> {
        let Self {
            mut zip,
            resources,
            config,
        } = self;
        let mut package = metadata.into_data_package(resources);
        package.created.get_or_insert_with(Utc::now);
        package.software.get_or_insert(Cow::Borrowed(SOFTWARE));

        let manifest = serde_json::to_vec_pretty(&package).map_err(Error::Manifest)?;
        zip.start_file(
            DATA_PACKAGE_PATH,
            options_for(DATA_PACKAGE_PATH, config.zip_compression_level)?,
        )?;
        zip.write_all(&manifest)?;
        let digest = DataPackageDigest {
            path: Cow::Borrowed(DATA_PACKAGE_PATH),
            hash: Sha256Digest::compute(&manifest),
            signed_data: None,
        };
        let digest_bytes = serde_json::to_vec_pretty(&digest).map_err(Error::Manifest)?;
        zip.start_file(
            DATA_PACKAGE_DIGEST_PATH,
            options_for(DATA_PACKAGE_DIGEST_PATH, config.zip_compression_level)?,
        )?;
        zip.write_all(&digest_bytes)?;
        Ok(zip.finish()?)
    }
}
