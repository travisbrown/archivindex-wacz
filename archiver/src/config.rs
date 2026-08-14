//! Configuration for the archiving client.

use std::time::Duration;

// Re-exported so callers can construct `Config` without depending on `archivindex-wacz`.
pub use archivindex_wacz::writer::IndexFormat;

/// The default `User-Agent` header value, identifying this crate and its version.
pub const DEFAULT_USER_AGENT: &str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Configuration for the archiving client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    /// The `User-Agent` header value sent with every request.
    ///
    /// The value must be a valid HTTP header value (in particular, it cannot contain line breaks);
    /// [`Archiver::new`](crate::client::Archiver::new) rejects anything else.
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
    /// that the index offsets frame complete members that replay tools can decompress without
    /// reading the rest of the file.
    pub gzip_warc: bool,
    /// The CDXJ index format: a plain-text `index.cdx`, or a `ZipNum` compressed `index.cdx.gz` and
    /// `index.idx` pair.
    pub index_format: IndexFormat,
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

impl Default for Config {
    /// The default configuration: this crate's `User-Agent`, a 30-second timeout, at most ten
    /// redirects per URL, one download at a time, unbounded response sizes, a gzip-compressed WARC
    /// file, and a plain-text index.
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            timeout: Duration::from_secs(30),
            max_redirects: 10,
            gzip_warc: true,
            index_format: IndexFormat::Plain,
            concurrency: 1,
            max_response_length: None,
        }
    }
}
