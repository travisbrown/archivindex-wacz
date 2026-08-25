//! Queue-driven crawl sessions written to a single WARC file.
//!
//! A processor may inspect successful responses, propose page titles, discover deduplicated URLs,
//! and deliberately request recaptures. A recapture of a URL whose earlier response carried an
//! `ETag` or `Last-Modified` validator is requested conditionally, so that the server may answer
//! `304 Not Modified` instead of repeating the payload. Sessions retry transient failures and
//! preserve completed work when a later recording failure ends the crawl.

use std::borrow::Cow;
use std::path::PathBuf;
use std::time::Duration;

use crate::client::{
    Archiver, CaptureControl, CaptureEvent, CaptureEventSink, CaptureSummary, Error, Failure,
};

mod run;

/// A session identifier is empty or contains a non-URI-unreserved character.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid session identifier: {0:?}")]
pub struct InvalidSessionId(String);

/// The operator named in a session's `warcinfo` record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Operator {
    /// The operator's name.
    pub name: String,
    /// The operator's email address.
    pub email: Option<String>,
}

/// Crawling software named in a session's `warcinfo` record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Software {
    /// Software name.
    pub name: String,
    /// Software version.
    pub version: String,
}

impl Default for Software {
    /// This crate's own name and version.
    fn default() -> Self {
        Self {
            name: env!("CARGO_PKG_NAME").to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// A successfully captured page shown to a [`CaptureProcessor`].
#[derive(Clone, Debug)]
pub struct Capture<'a> {
    /// The seed or discovered URL as requested.
    pub url: &'a str,
    /// The final URL after redirects.
    pub final_url: &'a str,
    /// The final HTTP status: `304` when the server revalidated a recapture instead of repeating
    /// its payload.
    pub status: u16,
    /// The decoded entity body, or stored body bytes when decoding fails. Empty for a revalidated
    /// recapture, whose unchanged payload the earlier capture holds.
    pub payload: &'a [u8],
    /// The complete recorded HTTP response.
    pub response: &'a [u8],
    pub(crate) response_metadata: Cow<'a, archivindex_warc::record::http::ResponseMetadata>,
}

impl<'a> Capture<'a> {
    /// Build a capture from recorded response bytes, for driving a [`CaptureProcessor`] outside a
    /// session.
    ///
    /// The status and headers are read from `response`, so a processor is shown exactly what a
    /// session would show it for that recording. Returns `None` if `response` does not begin with
    /// a complete HTTP response head.
    #[must_use]
    pub fn new(
        url: &'a str,
        final_url: &'a str,
        payload: &'a [u8],
        response: &'a [u8],
    ) -> Option<Self> {
        let response_metadata = archivindex_warc::record::http::ResponseMetadata::parse(response)?;

        Some(Self {
            url,
            final_url,
            status: response_metadata.status,
            payload,
            response,
            response_metadata: Cow::Owned(response_metadata),
        })
    }

    /// Return the first value of a response header as readable text.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.response_metadata
            .header(name)
            .and_then(|value| std::str::from_utf8(value).ok())
    }
}

/// Discoveries, deliberate recaptures, and a page title produced by a processor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Inspection {
    /// Deduplicated URLs appended to the session queue.
    pub links: Vec<String>,
    /// URLs appended without deduplication for validation workflows.
    ///
    /// A recapture is requested conditionally on the validators of the URL's earlier response, and
    /// a `304 Not Modified` answer reaches the processor with an empty payload. A processor that
    /// returns recaptures forever creates an infinite crawl.
    pub recaptures: Vec<String>,
    /// A proposed title, retained in WARC metadata when title recording is enabled.
    pub title: Option<String>,
    /// A failure that makes the traversal incomplete and stops the session after recording this
    /// capture.
    pub error: Option<String>,
}

impl Inspection {
    /// Stop traversal with a processor error after recording the current capture.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }

    /// Request a deliberate recapture of `url`.
    #[must_use]
    pub fn recapture(url: impl Into<String>) -> Self {
        Self {
            recaptures: vec![url.into()],
            ..Self::default()
        }
    }
}

/// Inspect successful captures to discover URLs, request recaptures, and supply titles.
pub trait CaptureProcessor {
    /// Inspect one successful capture.
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection;
}

/// Retry policy for transient network failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryConfig {
    /// Total attempts, including the first. Zero is treated as one.
    pub attempts: usize,
    /// Delay before the first retry.
    pub initial_backoff: Duration,
    /// Maximum retry delay.
    pub max_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: 3,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// The outcome of a session run.
#[derive(Debug, Default)]
pub struct SessionSummary {
    /// Successful seed captures in request order.
    pub seed_captures: Vec<CaptureSummary>,
    /// Successful discovered captures in request order.
    pub extra_captures: Vec<CaptureSummary>,
    /// URLs that exhausted capture attempts.
    pub failures: Vec<Failure>,
    /// The error that ended crawling early, if the partial WARC could still be written.
    pub fatal_error: Option<Error>,
    /// Whether an event sink requested a clean stop.
    pub cancelled: bool,
}

impl SessionSummary {
    /// Whether no URL failed and no fatal error ended the crawl.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failures.is_empty() && self.fatal_error.is_none() && !self.cancelled
    }
}

/// A seeded crawl whose processor may grow its queue.
pub struct Session<'a> {
    archiver: Archiver,
    id: String,
    operator: Operator,
    software: Software,
    seeds: Vec<String>,
    output: PathBuf,
    processor: Option<Box<dyn CaptureProcessor + 'a>>,
    retry: RetryConfig,
    request_delay: Duration,
    limit: Option<usize>,
    revisit_index: Option<PathBuf>,
    events: Option<Box<dyn CaptureEventSink + 'a>>,
    titles: bool,
}

impl<'a> Session<'a> {
    /// Create a session, validating its URI-unreserved identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSessionId`] if `id` is empty or contains a character outside the URI
    /// unreserved set.
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>, P: Into<PathBuf>>(
        archiver: Archiver,
        id: &str,
        operator: Operator,
        seeds: I,
        output: P,
    ) -> Result<Self, InvalidSessionId> {
        if id.is_empty()
            || !id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
            })
        {
            return Err(InvalidSessionId(id.to_owned()));
        }

        Ok(Self {
            archiver,
            id: id.to_owned(),
            operator,
            software: Software::default(),
            seeds: seeds
                .into_iter()
                .map(|seed| seed.as_ref().to_owned())
                .collect(),
            output: output.into(),
            processor: None,
            retry: RetryConfig::default(),
            request_delay: Duration::ZERO,
            limit: None,
            revisit_index: None,
            events: None,
            titles: false,
        })
    }

    /// Override the crawling software name and version recorded in `warcinfo`.
    #[must_use]
    pub fn software(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.software = Software {
            name: name.into(),
            version: version.into(),
        };
        self
    }

    /// Set the processor called for every successful capture.
    #[must_use]
    pub fn processor<P: CaptureProcessor + 'a>(mut self, processor: P) -> Self {
        self.processor = Some(Box::new(processor));
        self
    }

    /// Record the session identifier as the `warcinfo` title and processor titles in metadata.
    #[must_use]
    pub const fn titles(mut self) -> Self {
        self.titles = true;
        self
    }

    /// Observe capture lifecycle events and optionally request clean cancellation.
    #[must_use]
    pub fn events<E: CaptureEventSink + 'a>(mut self, events: E) -> Self {
        self.events = Some(Box::new(events));
        self
    }

    fn event(&mut self, event: CaptureEvent<'_>) -> CaptureControl {
        self.events
            .as_mut()
            .map_or(CaptureControl::Continue, |sink| sink.event(event))
    }

    /// Set the transient-failure retry policy.
    #[must_use]
    pub const fn retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Wait for `delay` between successive queued capture requests.
    ///
    /// Retry attempts continue to use [`RetryConfig`]'s backoff instead of this delay.
    #[must_use]
    pub const fn request_delay(mut self, delay: Duration) -> Self {
        self.request_delay = delay;
        self
    }

    /// Limit successful requested-URL captures; failures do not count toward the limit.
    #[must_use]
    pub const fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Use the persistent revisit and resource-state database at `path`.
    ///
    /// Existing payload entries make matching responses revisits, and existing resource state
    /// supplies conditional request headers. New records enter a private in-memory overlay during
    /// the crawl, so later captures in the same session can use them without exposing records that
    /// are not durable yet. After the WARC is atomically published, it is indexed into this
    /// database in one transaction. Without this option, the in-memory index lasts for the run.
    #[must_use]
    pub fn revisit_index(mut self, path: impl Into<PathBuf>) -> Self {
        self.revisit_index = Some(path.into());
        self
    }
}
