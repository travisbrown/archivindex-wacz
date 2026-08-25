//! Archiving web pages over HTTP into WARC files.
//!
//! This crate provides a small client that captures URLs in WARC files, recording the exact wire
//! bytes of every HTTP request and response, including redirect hops. A response whose payload
//! duplicates an earlier capture is stored as a `revisit` record referencing the original instead
//! of repeating the payload. WARC files can subsequently be packaged as WACZ distributions with
//! the `archivindex-packager` crate.
//!
//! # Examples
//!
//! ```no_run
//! use archivindex_archiver::{Archiver, Config};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let archiver = Archiver::new(Config::default())?;
//! let summary = archiver.archive_to_path(["https://www.example.com/"], "example.warc")?;
//!
//! assert!(summary.is_complete());
//! # Ok(())
//! # }
//! ```
//!
//! Beyond one-shot lists, the [`session`] module offers crawl sessions: a queue of seed URLs grown
//! by a user-supplied capture processor inspecting each response, captured (with retries for
//! transient network failures) into a single WARC file named after the session identifier. The
//! processor may also propose titles that an explicitly configured session records in metadata. A
//! session recapturing a URL asks the server to revalidate the earlier response, storing a `304 Not
//! Modified` answer as a `revisit` record under the `server-not-modified` profile. A session may
//! use a persistent revisit index to deduplicate against earlier WARC captures and reuse their HTTP
//! validators across runs. The `archivindex-wordpress` crate provides one such processor, which
//! crawls the comments of a `WordPress` site.
//!
//! # Modules
//!
//! * [`capture`]: what a capture run reports and observes
//! * [`session`]: queue-driven crawl sessions
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

pub mod capture;
mod client;
mod config;
mod http_date;
pub mod session;

#[cfg(test)]
mod strategies;

use std::time::Duration;

use archivindex_warc::record::BlockError;
use archivindex_warc::recorder::Recorder;
use http::header::HeaderMap;

/// An HTTP client that captures lists of URLs in WARC files.
///
/// Each URL is fetched synchronously over HTTP/1.1. Redirect hops, wire-format messages, and
/// capture metadata are retained. One-shot lists request every URL unconditionally; only crawl
/// sessions revalidate earlier captures.
#[derive(Clone, Debug)]
pub struct Archiver {
    recorder: Recorder,
    headers: HeaderMap,
    config: Config,
}

/// Configuration for the archiving client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// The `User-Agent` header value sent with every request.
    ///
    /// [`Archiver::new`] rejects values that cannot be used as HTTP field values.
    pub user_agent: String,
    /// The network timeout, applied to connecting and to each socket read and write.
    ///
    /// A fetch fails when connecting, sending the request, or reading the response header section
    /// times out. A read timing out after the header section instead truncates the response, which
    /// is recorded with a `WARC-Truncated` reason of `time`.
    pub timeout: Duration,
    /// The maximum number of redirects followed for each URL.
    ///
    /// Every hop is captured; when a response still redirects after this many follows, it is
    /// recorded as the final response for its URL rather than treated as an error.
    pub max_redirects: usize,
    /// Whether to gzip the WARC file (as `data.warc.gz`).
    ///
    /// Each record is compressed as an independent gzip member, following the WARC convention, so
    /// that individual records can be decompressed without reading the rest of the file.
    pub gzip_warc: bool,
    /// The number of URLs downloaded concurrently.
    ///
    /// Captures are always written to the archive in input order; raising this only allows up to
    /// this many downloads (each including its full redirect chain) to be in flight at once. A
    /// value of zero is treated as one.
    pub concurrency: usize,
    /// The maximum number of response bytes stored for one fetch, when set.
    ///
    /// A response reaching the limit is truncated rather than failed: its record holds the bytes
    /// received up to the limit and carries a `WARC-Truncated` reason of `length`. Response size is
    /// unbounded when unset.
    pub max_response_length: Option<u64>,
}

/// An error type for archiving.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The archive could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An exchange could not be completed.
    #[error(transparent)]
    Fetch(#[from] archivindex_warc::recorder::Error),
    /// A URL to be archived could not be parsed.
    #[error(transparent)]
    InvalidUrl(#[from] url::ParseError),
    /// A URL to be archived contains credentials. The displayed URL has them removed.
    #[error("URL contains credentials: {0}")]
    CredentialedUrl(String),
    /// A URL to be archived does not have a host.
    #[error("URL has no host: {0}")]
    MissingHost(String),
    /// A parsed URL cannot be represented by the HTTP request URI grammar.
    #[error("URL is not a valid URI: {url}")]
    InvalidUri {
        /// The URL as requested.
        url: String,
        /// Where the URL departs from the URI grammar.
        #[source]
        source: http::uri::InvalidUri,
    },
    /// An HTTP response status remained retryable after the configured attempts were exhausted.
    #[error("HTTP status {status} after retries for {url}")]
    HttpStatus {
        /// The URL whose response remained unsuccessful.
        url: String,
        /// The final HTTP response status.
        status: u16,
    },
    /// A capture processor could not complete its traversal.
    #[error("capture processor failed for {url}: {message}")]
    Processor {
        /// The URL being inspected.
        url: String,
        /// The processor's description of the failure.
        message: String,
    },
    /// The configured `User-Agent` cannot be sent or recorded safely.
    #[error(transparent)]
    InvalidUserAgent(#[from] UserAgentError),
    /// A session identifier is empty or contains a non-URI-unreserved character.
    #[error(transparent)]
    InvalidSessionId(#[from] crate::session::SessionIdError),
    /// A revisit index could not be opened.
    #[error(transparent)]
    RevisitIndexOpen(#[from] archivindex_warc_revisit_index::error::OpenError),
    /// A revisit index could not be queried or updated.
    #[error(transparent)]
    RevisitIndex(#[from] archivindex_warc_revisit_index::error::Error),
    /// A WARC content block could not be attached to its record.
    #[error(transparent)]
    WarcBlock(#[from] BlockError),
    /// A `warc-fields` value could not be written.
    #[error(transparent)]
    WarcFields(#[from] archivindex_warc::record::fields::Error),
    /// A WARC record could not be rendered.
    #[error(transparent)]
    WarcRender(#[from] archivindex_warc::record::RenderError),
    /// A WARC record could not be written.
    #[error(transparent)]
    WarcWrite(#[from] archivindex_warc::io::write::Error),
}

impl From<archivindex_warc_revisit_index::error::DatabaseError> for Error {
    fn from(error: archivindex_warc_revisit_index::error::DatabaseError) -> Self {
        Self::RevisitIndex(error.into())
    }
}

/// The configured `User-Agent` is not a valid HTTP field value.
///
/// Control characters are rejected, apart from the horizontal tab; a carriage return or line feed
/// would end the field early, both in the request and in the `warcinfo` record. Everything else is
/// accepted, including non-ASCII text, which RFC 9110 carries as opaque bytes without giving it a
/// meaning. The error message includes the rejected value.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid User-Agent header value: {0:?}")]
pub struct UserAgentError(String);
