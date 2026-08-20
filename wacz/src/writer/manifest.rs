//! Final data-package manifest and digest writing.

use std::borrow::Cow;
use std::io::{Seek, Write};

use chrono::Utc;

use super::resource::options_for;
use super::{Error, PackageMetadata, WaczWriter};
use crate::digest::Sha256Digest;
use crate::frictionless::{DataPackage, DataPackageDigest, PROFILE, WACZ_VERSION};
use crate::{DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, ExtraProperties};

const SOFTWARE: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

impl<W: Write + Seek> WaczWriter<W> {
    /// Write the manifest and digest files and finish the ZIP.
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
}
