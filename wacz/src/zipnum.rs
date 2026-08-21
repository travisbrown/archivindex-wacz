//! Shared wire types for the `ZipNum` summary format.
//!
//! Both the index writer and the reader go through these types, so the two sides of the format
//! cannot drift. Summary lines are serialized with `serde_json`'s default compact formatting;
//! every known reader parses the JSON object rather than matching its spacing.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::digest::Sha256Digest;

/// The `format` value of a `!meta 0` header written by this crate.
pub const FORMAT: &str = "cdxj-gzip-1.0";

/// The JSON object of the `!meta 0` header line naming the compressed data member.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SummaryHeader<'a> {
    /// The summary format identifier; see [`FORMAT`].
    #[serde(borrow)]
    pub format: Cow<'a, str>,
    /// The data member's file name, relative to the summary.
    #[serde(borrow)]
    pub filename: Cow<'a, str>,
}

/// The JSON object of one block line locating a gzip member within the data member.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct SummaryEntry {
    /// Byte offset of the gzip member within the data member.
    pub offset: u64,
    /// Byte length of the gzip member.
    pub length: u64,
    /// SHA-256 digest of the compressed gzip member.
    pub digest: Sha256Digest,
}
