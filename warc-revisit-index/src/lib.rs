//! A revisit index derived from WARC records.
//!
//! Everything here exists to support revisit records: to decide whether a capture may be written
//! as one, and to name the record it refers back to. Two deliberately separate tables in one
//! SQLite database answer those two questions:
//!
//! - The payload table maps a digest to the canonical payload-bearing WARC record: the
//!   `WARC-Refers-To` target of any revisit, whether an `identical-payload-digest` revisit found
//!   the digest again or a `server-not-modified` revisit confirmed it unchanged.
//! - The resource-state table maps a resource/request identity to its HTTP validators and the
//!   digest of its prior representation. The validators drive conditional requests; the digest
//!   leads, through the payload table, to the record a `server-not-modified` revisit refers to.
//!
//! The index is derived, rebuildable state; WARC files remain the source of truth. A database
//! written by an incompatible schema version is not migrated: delete it and index the records
//! again. This crate is intentionally unaware of WACZ. A caller may ingest records from standalone
//! WARC files, WARC streams extracted from WACZ packages, or any other source.
//!
//! Resource identity is currently one canonical GET representation per target URI. The
//! [`resource::ResourceKey`] wrapper leaves room for explicitly representing method, authorization
//! context, cookies, and `Vary`-selected request headers in a future schema. The same payload may
//! be linked to any number of independent resource keys.
//!
//! # Example
//!
//! ```
//! use archivindex_warc::value::{Algorithm, LabelledDigest};
//! use archivindex_warc_revisit_index::Index;
//! use archivindex_warc_revisit_index::resource::ResourceKey;
//! use fluent_uri::Uri;
//!
//! let index = Index::open_in_memory()?;
//! let key = ResourceKey::new(Uri::parse("https://example.com/")?.to_owned());
//! let digest = LabelledDigest::from_digest(Algorithm::Sha256, &[0; 32]);
//!
//! assert!(index.lookup_payload(&digest)?.is_none());
//! assert!(index.lookup_resource(&key)?.is_none());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Modules
//!
//! - [`payload`] describes the canonical payload-bearing records a revisit can refer to.
//! - [`resource`] describes the conditional-request state a revisit is produced from.
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![forbid(unsafe_code)]

mod db;
mod ingest;
pub mod payload;
pub mod resource;

#[cfg(test)]
mod strategies;

use rusqlite::Connection;

/// A database handle shared by the persistent index and its bulk transactions.
///
/// The type parameter is either an open SQLite connection ([`Index`]) or a transaction on one
/// ([`Transaction`]); the crate seals it, so there is no third instantiation.
pub struct Store<C> {
    connection: C,
}

/// A revisit index held in a SQLite database.
pub type Index = Store<Connection>;

/// A bulk indexing transaction.
pub type Transaction<'connection> = Store<rusqlite::Transaction<'connection>>;

/// The changes made while indexing one WARC record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexRecordOutcome {
    /// A new canonical payload source was inserted.
    pub payload_inserted: bool,
    /// Resource state was inserted or updated.
    pub resource_updated: bool,
}

/// A SQLite operation failed.
#[derive(Debug, thiserror::Error)]
#[error("SQLite operation `{operation}` failed: {source}")]
pub struct DatabaseError {
    operation: &'static str,
    #[source]
    source: rusqlite::Error,
}

impl DatabaseError {
    /// Wrap a SQLite error with the operation it interrupted.
    pub(crate) const fn during(operation: &'static str) -> impl FnOnce(rusqlite::Error) -> Self {
        move |source| Self { operation, source }
    }
}

/// An error opening the revisit index.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// SQLite could not open, configure, or initialize the database.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// The database was created by an incompatible schema version.
    ///
    /// The index is derived state and is never migrated; delete the database and rebuild it.
    #[error(
        "unsupported revisit index schema version {found}; expected {expected}. The index is \
         derived state: delete the database and index the records again."
    )]
    SchemaVersion {
        /// The version understood by this crate.
        expected: u32,
        /// The version stored in SQLite.
        found: u32,
    },
}

/// An error querying or updating the revisit index.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A SQLite operation failed.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// A digest uses an algorithm this index cannot safely normalize.
    #[error("unsupported digest algorithm `{0}`")]
    UnsupportedDigestAlgorithm(String),
    /// Digest bytes do not have the length required by their algorithm.
    #[error("invalid {algorithm} digest length {actual}; expected {expected}")]
    InvalidDigestLength {
        /// The stable algorithm label.
        algorithm: String,
        /// The required byte length.
        expected: usize,
        /// The provided byte length.
        actual: usize,
    },
    /// A labelled digest's byte encoding is ambiguous or malformed.
    #[error("cannot decode labelled digest `{0}`")]
    UndecodableDigest(String),
    /// A persisted URI is malformed.
    #[error("malformed persisted {field} URI `{value}`: {source}")]
    MalformedUri {
        /// The database field containing the URI.
        field: &'static str,
        /// The malformed value.
        value: String,
        /// The URI parse error.
        #[source]
        source: fluent_uri::ParseError,
    },
    /// A persisted WARC date is malformed.
    #[error("malformed persisted {field} WARC date `{value}`")]
    MalformedDate {
        /// The database field containing the date.
        field: &'static str,
        /// The malformed value.
        value: String,
    },
    /// An unsigned Rust value cannot be represented by SQLite's signed integer type.
    #[error("{field} value {value} is outside SQLite's integer range")]
    IntegerOutOfRange {
        /// The value's meaning.
        field: &'static str,
        /// The out-of-range value.
        value: u64,
    },
    /// A persisted integer is invalid for its Rust representation.
    #[error("malformed persisted {field} integer `{value}`")]
    MalformedInteger {
        /// The database field containing the integer.
        field: &'static str,
        /// The invalid value.
        value: i64,
    },
    /// A persisted optional digest has only one of its required columns populated.
    #[error("malformed persisted resource digest: algorithm and bytes must both be present")]
    IncompleteDigest,
    /// A persisted representation variance is malformed.
    #[error("malformed persisted representation variance `{value}`")]
    MalformedVariance {
        /// The malformed value.
        value: String,
    },
}

/// An error indexing a WARC record.
///
/// Only [`Store::index_record`] reads WARC and HTTP metadata, so only it can report the two
/// malformed-input cases below; every other operation fails with [`Error`] alone.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// Reading or updating the index failed.
    #[error(transparent)]
    Index(#[from] Error),
    /// An archived HTTP response head is malformed.
    #[error("malformed archived HTTP response: {0}")]
    MalformedHttpResponse(&'static str),
    /// A WARC record's declared payload could not be extracted.
    #[error("malformed WARC payload: {0}")]
    MalformedWarcPayload(#[source] archivindex_warc::record::payload::Error),
}
