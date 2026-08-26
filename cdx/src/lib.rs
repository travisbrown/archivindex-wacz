//! Data models for web archive capture indexes.
//!
//! The crate covers the three CDX representations commonly encountered in web archiving:
//!
//! - [`classic`] header-described, delimiter-separated CDX records;
//! - [`cdxj`] records with a searchable-key and timestamp prefix followed by a JSON object;
//! - [`json`] CDX Server documents with a header row and JSON-array records.
//!
//! These modules model and convert individual values and records. Reading files, sorting indexes,
//! looking up captures, and resolving WARC byte ranges belong to higher-level crates.
//!
//! The models follow the [IIPC CDX description], the [CDXJ 0.1.0 specification], and the
//! header-driven JSON output of the [Wayback CDX Server].
//!
//! [IIPC CDX description]: https://iipc.github.io/warc-specifications/specifications/cdx-format/cdx-2015/
//! [CDXJ 0.1.0 specification]: https://specs.webrecorder.net/cdxj/0.1.0/
//! [Wayback CDX Server]: https://github.com/internetarchive/wayback/tree/master/wayback-cdx-server

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod attributes;
pub mod capture;
pub mod cdxj;
pub mod classic;
pub mod field;
pub mod json;
pub mod properties;
pub mod timestamp;

#[cfg(test)]
mod strategies;
