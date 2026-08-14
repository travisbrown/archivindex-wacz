//! Reading and writing web archive collections in the [WACZ
//! format](https://specs.webrecorder.net/wacz/1.1.1/).
//!
//! A WACZ file is a ZIP file containing WARC data and the metadata needed for replay: a
//! [Frictionless Data Package](https://specs.frictionlessdata.io/data-package/) manifest, a page
//! list, and CDXJ indexes.
//!
//! # Modules
//!
//! - [`cdxj`]: CDXJ index lines mapping searchable URL keys to WARC records
//! - [`frictionless`]: The `datapackage.json` manifest and `datapackage-digest.json` formats
//! - [`digest`]: SHA-256 digests in the `sha256:<hex>` encoding used by WACZ manifests
//! - [`pages`]: The `pages/pages.jsonl` page list format
//! - [`reader`]: Reading the files in an existing WACZ
//! - [`writer`]: Assembling a new WACZ file
#![warn(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    rust_2018_idioms
)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

mod attributes;
pub mod cdxj;
pub mod digest;
pub mod frictionless;
mod lines;
pub mod pages;
pub mod reader;
pub mod writer;

/// Additional JSON properties preserved verbatim for round-tripping.
///
/// A [`serde_json::Map`] wrapper that implements the `bounded_static` traits required by the
/// structs containing it.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct ExtraProperties(serde_json::Map<String, serde_json::Value>);

impl bounded_static::ToBoundedStatic for ExtraProperties {
    type Static = Self;

    fn to_static(&self) -> Self {
        self.clone()
    }
}

impl bounded_static::IntoBoundedStatic for ExtraProperties {
    type Static = Self;

    fn into_static(self) -> Self {
        self
    }
}

impl From<serde_json::Map<String, serde_json::Value>> for ExtraProperties {
    fn from(map: serde_json::Map<String, serde_json::Value>) -> Self {
        Self(map)
    }
}

impl std::ops::Deref for ExtraProperties {
    type Target = serde_json::Map<String, serde_json::Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ExtraProperties {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// The path of the data package manifest within a WACZ file.
pub const DATA_PACKAGE_PATH: &str = "datapackage.json";

/// The path of the data package digest within a WACZ file.
pub const DATA_PACKAGE_DIGEST_PATH: &str = "datapackage-digest.json";

/// The path of the required page list within a WACZ file.
pub const PAGES_PATH: &str = "pages/pages.jsonl";

/// The directory prefix for WARC files.
pub const ARCHIVE_PREFIX: &str = "archive/";

/// The directory prefix for index files.
pub const INDEXES_PREFIX: &str = "indexes/";

/// The directory prefix for page lists.
pub const PAGES_PREFIX: &str = "pages/";

/// Files with this extension contain gzip data and are stored without ZIP compression.
const GZIP_EXTENSION: &str = ".gz";
