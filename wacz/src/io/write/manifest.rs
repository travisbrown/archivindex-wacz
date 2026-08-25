//! Final data-package manifest and digest writing.

use std::borrow::Cow;
use std::io::{Seek, Write};

use chrono::SubsecRound as _;
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
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let mut missing = Vec::new();
        if !self
            .resources
            .iter()
            .any(|resource| crate::paths::is_warc(&resource.path))
        {
            missing.push("archive/*.warc[.gz]");
        }
        if !self
            .resources
            .iter()
            .any(|resource| crate::paths::is_cdxj_index(&resource.path))
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
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        metadata.validate()?;
        for resource in &self.resources {
            resource.validate()?;
        }
        let Self {
            mut zip,
            resources,
            config,
            publication,
            poisoned: _,
        } = self;
        let mut package = metadata.into_data_package(resources);
        let created = *package
            .created
            .get_or_insert_with(|| Utc::now().trunc_subsecs(3));
        package.modified.get_or_insert(created);
        package.software.get_or_insert(Cow::Borrowed(SOFTWARE));

        let manifest = serde_json::to_vec_pretty(&package).map_err(Error::Manifest)?;
        // Both manifests are small and fully buffered, so neither needs ZIP64 extra fields.
        zip.start_file(
            DATA_PACKAGE_PATH,
            options_for(DATA_PACKAGE_PATH, config.zip_compression_level).large_file(false),
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
            options_for(DATA_PACKAGE_DIGEST_PATH, config.zip_compression_level).large_file(false),
        )?;
        zip.write_all(&digest_bytes)?;
        let mut output = zip.finish()?;
        output.flush()?;

        if let Some(publication) = publication {
            publication.temporary.as_file().sync_all()?;
            publication
                .temporary
                .persist_noclobber(publication.target)
                .map_err(|error| error.error)?;
        }

        Ok(output)
    }
}
