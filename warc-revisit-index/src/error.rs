//! Errors produced by the crawl-state index.

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

/// An error opening the crawl-state database.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// SQLite could not open, configure, or initialize the database.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// The database was created by an incompatible schema version.
    #[error("unsupported crawl-state schema version {found}; expected {expected}")]
    SchemaVersion {
        /// The version understood by this crate.
        expected: u32,
        /// The version stored in SQLite.
        found: u32,
    },
}

/// An error querying or ingesting into the crawl-state database.
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
    /// An archived HTTP response head is malformed.
    #[error("malformed archived HTTP response: {0}")]
    MalformedHttpResponse(&'static str),
    /// A WARC record's declared payload could not be extracted.
    #[error("malformed WARC payload: {0}")]
    MalformedWarcPayload(#[source] archivindex_warc::record::payload::Error),
}
