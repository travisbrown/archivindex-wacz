//! The public archiving client facade and outcome types.

use std::io::Write;
use std::path::Path;

use archivindex_warc::record::BlockError;
use archivindex_warc::recorder::Recorder;
use archivindex_warc_revisit_index::db::Index as RevisitIndex;
use chrono::{DateTime, Utc};
use http::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};

use crate::config::Config;

pub(crate) mod capture;
pub(crate) mod collection;
mod pool;
mod warc_fields;
mod warc_mapping;

use capture::CaptureOutcome;
use collection::Collection;
use warc_fields::WarcinfoOptions;

const WARC_NAME: &str = "data.warc";
const GZIP_WARC_NAME: &str = "data.warc.gz";

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
    InvalidUserAgent(#[from] InvalidUserAgent),
    /// A session identifier is empty or contains a non-URI-unreserved character.
    #[error(transparent)]
    InvalidSessionId(#[from] crate::session::InvalidSessionId),
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

/// The configured `User-Agent` cannot be sent or recorded safely.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid User-Agent header value: {0:?}")]
pub struct InvalidUserAgent(String);

/// The outcome of an archiving run.
#[derive(Debug, Default)]
pub struct ArchiveSummary {
    /// URLs archived successfully, in request order.
    pub captures: Vec<CaptureSummary>,
    /// URLs that could not be captured.
    pub failures: Vec<Failure>,
    /// Whether an event sink requested a clean stop before all input was dispatched.
    pub cancelled: bool,
}

impl ArchiveSummary {
    /// Whether every URL was captured.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failures.is_empty() && !self.cancelled
    }
}

/// A live capture lifecycle notification.
#[derive(Clone, Copy, Debug)]
pub enum CaptureEvent<'a> {
    /// A URL capture attempt is starting.
    Started {
        /// Requested URL.
        url: &'a str,
        /// One-based attempt number.
        attempt: usize,
    },
    /// A transient failure will be retried after a delay.
    Retrying {
        /// Requested URL.
        url: &'a str,
        /// One-based number of the upcoming attempt.
        attempt: usize,
        /// Delay before that attempt.
        delay: std::time::Duration,
    },
    /// A URL produced a final HTTP response.
    Captured {
        /// Requested URL.
        url: &'a str,
        /// Final HTTP status.
        status: u16,
    },
    /// A URL could not be captured.
    Failed {
        /// Requested URL.
        url: &'a str,
        /// Final capture error.
        error: &'a Error,
    },
    /// The URL's records were written to the pending collection.
    Written {
        /// Requested URL.
        url: &'a str,
    },
}

/// Decision returned by a [`CaptureEventSink`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureControl {
    /// Continue capturing.
    Continue,
    /// Stop dispatching work and finalize what has already completed.
    Cancel,
}

/// Observer that can report progress or request clean cancellation.
pub trait CaptureEventSink {
    /// Observe one event and decide whether capture should continue.
    fn event(&mut self, event: CaptureEvent<'_>) -> CaptureControl;

    /// Report that a URL capture attempt is starting and return whether it should be cancelled.
    fn started(&mut self, url: &str, attempt: usize) -> bool {
        self.event(CaptureEvent::Started { url, attempt }) == CaptureControl::Cancel
    }
}

impl<F> CaptureEventSink for F
where
    F: for<'a> FnMut(CaptureEvent<'a>) -> CaptureControl,
{
    fn event(&mut self, event: CaptureEvent<'_>) -> CaptureControl {
        self(event)
    }
}

struct IgnoreEvents;

impl CaptureEventSink for IgnoreEvents {
    fn event(&mut self, _event: CaptureEvent<'_>) -> CaptureControl {
        CaptureControl::Continue
    }
}

/// The outcome of capturing one URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSummary {
    /// The requested URL.
    pub url: String,
    /// When capture of the final response began.
    pub date: DateTime<Utc>,
    /// The final response status.
    pub status: u16,
    /// The decoded entity-body length.
    pub size: u64,
    /// The number of redirects followed.
    pub redirects: usize,
}

/// A URL that could not be captured.
#[derive(Debug)]
pub struct Failure {
    /// The requested URL.
    pub url: String,
    /// The capture failure.
    pub error: Error,
}

/// An HTTP client that captures lists of URLs in WARC files.
///
/// Each URL is fetched synchronously over HTTP/1.1. Redirect hops, wire-format messages, capture
/// metadata are retained. One-shot lists request every URL unconditionally; only crawl sessions
/// revalidate earlier captures.
#[derive(Clone, Debug)]
pub struct Archiver {
    recorder: Recorder,
    headers: HeaderMap,
    config: Config,
}

impl Archiver {
    /// Create a new archiving client.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidUserAgent`] if the configured `User-Agent` is not a valid header value.
    pub fn new(config: Config) -> Result<Self, InvalidUserAgent> {
        let user_agent = HeaderValue::from_str(&config.user_agent)
            .map_err(|_| InvalidUserAgent(config.user_agent.clone()))?;
        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(USER_AGENT, user_agent);

        let mut recorder = Recorder::new()
            .connect_timeout(config.timeout)
            .io_timeout(config.timeout);
        if let Some(length) = config.max_response_length {
            recorder = recorder.max_response_length(length);
        }

        Ok(Self {
            recorder,
            headers,
            config,
        })
    }

    /// Download URLs and atomically publish a new WARC at `path`, refusing to overwrite it.
    pub fn archive_to_path<P: AsRef<Path>, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        path: P,
    ) -> Result<ArchiveSummary, Error> {
        self.archive_to_path_with_events(urls, path, &mut IgnoreEvents)
    }

    /// Download URLs with live events and atomically publish a new WARC at `path`.
    pub fn archive_to_path_with_events<P: AsRef<Path>, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        path: P,
        events: &mut impl CaptureEventSink,
    ) -> Result<ArchiveSummary, Error> {
        let path = path.as_ref();
        let warc_name =
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(if self.config.gzip_warc {
                    GZIP_WARC_NAME
                } else {
                    WARC_NAME
                });
        let (collection, cancelled) =
            self.archive_collection(urls, warc_name, Some(path), events)?;
        let mut summary = collection.finish_to_path(path)?;
        summary.cancelled = cancelled;
        Ok(summary)
    }

    /// Download URLs and write a WARC stream to `writer`.
    pub fn archive<W: Write, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        writer: W,
    ) -> Result<ArchiveSummary, Error> {
        self.archive_with_events(urls, writer, &mut IgnoreEvents)
    }

    /// Download URLs with live events and write a WARC stream to `writer`.
    pub fn archive_with_events<W: Write, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        writer: W,
        events: &mut impl CaptureEventSink,
    ) -> Result<ArchiveSummary, Error> {
        let warc_name = if self.config.gzip_warc {
            GZIP_WARC_NAME
        } else {
            WARC_NAME
        };
        let (collection, cancelled) = self.archive_collection(urls, warc_name, None, events)?;
        let mut summary = collection.finish(writer)?;
        summary.cancelled = cancelled;
        Ok(summary)
    }

    /// Start the collection used by a crawl session.
    pub(crate) fn session_collection(
        &self,
        id: &str,
        software: &crate::session::Software,
        operator: &crate::session::Operator,
        title: Option<&str>,
        output: &Path,
        persistent_index: Option<RevisitIndex>,
    ) -> Result<Collection, Error> {
        let gzip = self.config.gzip_warc;
        let suffix = if gzip { ".warc.gz" } else { ".warc" };

        Collection::new_for_path(
            output,
            &format!("{id}{suffix}"),
            gzip,
            &WarcinfoOptions {
                user_agent: &self.config.user_agent,
                software: Some(software),
                operator: Some(operator),
                session_id: Some(id),
                title,
            },
            persistent_index,
        )
    }

    fn archive_collection<I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        warc_name: &str,
        output: Option<&Path>,
        events: &mut impl CaptureEventSink,
    ) -> Result<(Collection, bool), Error> {
        let gzip = self.config.gzip_warc;
        let warcinfo = WarcinfoOptions::archiver(&self.config.user_agent);
        let mut collection = if let Some(output) = output {
            Collection::new_for_path(output, warc_name, gzip, &warcinfo, None)?
        } else {
            Collection::new(warc_name, gzip, &warcinfo, None)?
        };

        let concurrency = self.config.concurrency.max(1);
        let mut cancelled = false;
        if concurrency == 1 {
            for url in urls {
                let url = url.as_ref();
                if events.started(url, 1) {
                    cancelled = true;
                    break;
                }
                let outcome = self.capture(url, None);
                cancelled |= notify_outcome(events, url, &outcome);
                collection.record(url.to_owned(), outcome, None, None)?;
                cancelled |= events.event(CaptureEvent::Written { url }) == CaptureControl::Cancel;
                if cancelled {
                    break;
                }
            }
        } else {
            cancelled = self.capture_concurrently(urls, concurrency, &mut collection, events)?;
        }

        Ok((collection, cancelled))
    }
}

pub(crate) fn notify_outcome(
    events: &mut (impl CaptureEventSink + ?Sized),
    url: &str,
    outcome: &CaptureOutcome,
) -> bool {
    let event = match outcome {
        CaptureOutcome::Captured(exchanges) => CaptureEvent::Captured {
            url,
            status: exchanges
                .last()
                .expect("successful capture has an exchange")
                .status,
        },
        CaptureOutcome::Failed { error, .. } => CaptureEvent::Failed { url, error },
    };
    events.event(event) == CaptureControl::Cancel
}
