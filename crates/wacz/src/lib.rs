//! Reading and writing web archive collections in the [WACZ
//! format](https://specs.webrecorder.net/wacz/1.1.1/).
//!
//! A WACZ file is a ZIP file containing WARC data and the metadata needed for replay: a
//! [Frictionless Data Package](https://specs.frictionlessdata.io/data-package/) manifest, a page
//! list, and CDXJ indexes.
//!
//! # Modules
//!
//! - [`cdxj`]: Reading CDXJ streams with source diagnostics
//! - [`frictionless`]: The `datapackage.json` manifest and `datapackage-digest.json` formats
//! - [`digest`]: SHA-256 digests in the `sha256:<hex>` encoding used by WACZ manifests
//! - [`pages`]: The `pages/pages.jsonl` page list format
//! - [`io::read`]: Reading the files in an existing WACZ
//! - [`io::write`]: Assembling a new WACZ file
#![cfg_attr(docsrs, feature(doc_cfg))]

mod attributes;
pub mod cdxj;
pub mod digest;
pub mod frictionless;
pub mod io;
pub mod pages;
mod paths;
mod zipnum;

#[cfg(test)]
mod strategies;

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
