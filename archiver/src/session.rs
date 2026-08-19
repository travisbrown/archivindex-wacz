//! Crawl sessions: queue-driven captures written to a single WARC file.
//!
//! A [`Session`] downloads a queue of URLs seeded at construction and grown by an optional
//! [`CaptureProcessor`], which inspects each successful response, discovers further URLs to
//! capture, and may title its page entry. Every exchange of the session is recorded into a single
//! WARC file, named after the session identifier, inside a WACZ written at the path given on
//! construction. The seed URLs receive entries in the required `pages/pages.jsonl` page list; URLs
//! discovered during the crawl are listed in `pages/extraPages.jsonl` instead.
//!
//! The session's `warcinfo` record carries the identifier (as `isPartOf`), the `User-Agent` sent
//! with every request (as `http-header-user-agent`), the [`Operator`] who ran the crawl (required
//! on construction), and the crawling software's name and version (this crate unless
//! [`Session::software`] names other software built on it). A discovered URL's first exchange
//! carries the URI of the page it was discovered on as the `via` field of its `metadata` record, so
//! that the archive itself records how the crawl reached each page.
//!
//! Transient network failures (connection, timeout, and other I/O errors) are retried with
//! exponential backoff up to a configurable number of attempts; malformed responses and rejected
//! URLs are not, since repeating the request cannot change the outcome. A URL whose retries are
//! exhausted is reported as a failure without ending the session. When recording a capture fails (a
//! spool write error, which no retry can help), the session stops crawling but still attempts to
//! write everything it has captured so far.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use archivindex_warc::record::payload;

use crate::client::{Archiver, CaptureSummary, Error, Exchange, Failure};
use crate::response;

/// The operator running a crawl session, named in the `warcinfo` record's `operator` field (as
/// `name` or `name <email>`).
///
/// Neither value may contain a control character (a line break in particular), which the
/// `warc-fields` grammar rejects; [`Session::run`] fails on such a value before any network
/// activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Operator {
    /// The operator's name.
    pub name: String,
    /// The operator's email address.
    pub email: Option<String>,
}

/// A successfully captured page, as shown to a [`CaptureProcessor`].
///
/// The payload is the entity body of the final response (any chunked transfer coding removed); when
/// that body cannot be decoded, the stored body bytes are passed as they crossed the wire instead,
/// so that a decoding problem does not hide the response from the processor entirely.
#[derive(Clone, Copy, Debug)]
pub struct Capture<'a> {
    /// The URL as requested (a seed or a discovered URL).
    pub url: &'a str,
    /// The URL of the final response, after any redirects.
    pub final_url: &'a str,
    /// The status code of the final response.
    pub status: u16,
    /// The payload of the final response.
    pub payload: &'a [u8],
}

/// The discoveries and page title produced by inspecting a successful capture.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Inspection {
    /// The URLs discovered in the capture, appended to the session queue in this order after URLs
    /// already captured or queued are removed.
    pub links: Vec<String>,
    /// The title written into the capture's page list entry, when present.
    pub title: Option<String>,
}

/// Inspect successful captures to discover links and supply page titles.
///
/// A processor is called once for every successfully captured requested URL, with the final
/// response in its redirect chain. Returning links grows the session queue; returning a title adds
/// it to that capture's page list entry. The processor may keep mutable state across calls.
pub trait CaptureProcessor {
    /// Inspect a successful capture.
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection;
}

/// How transient network failures are retried.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryConfig {
    /// The number of times a URL is attempted in total, including the first attempt.
    ///
    /// A value of zero is treated as one.
    pub attempts: usize,
    /// The delay before the first retry; each further retry doubles it.
    pub initial_backoff: Duration,
    /// The ceiling the doubling backoff cannot exceed.
    pub max_backoff: Duration,
}

impl Default for RetryConfig {
    /// The default retry behavior: three attempts, backing off for one second and then two.
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
    /// The seed URLs captured successfully, in request order.
    pub seed_captures: Vec<CaptureSummary>,
    /// The discovered URLs captured successfully, in request order.
    pub extra_captures: Vec<CaptureSummary>,
    /// The URLs (seed or discovered) that could not be captured after all retries, with the reason
    /// for each.
    pub failures: Vec<Failure>,
    /// The error that ended the crawl early, if any.
    ///
    /// When set, the archive was still written and holds everything captured up to the error, but
    /// URLs remaining in the queue were never attempted.
    pub fatal_error: Option<Error>,
}

impl SessionSummary {
    /// Whether no URL failed and no fatal error ended the crawl.
    ///
    /// A session that stops at its configured capture limit is complete even though URLs may remain
    /// queued intentionally.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failures.is_empty() && self.fatal_error.is_none()
    }
}

/// A crawl session that captures a seeded, dynamically grown queue of URLs in one WACZ.
///
/// URLs are captured strictly in queue order, one at a time: the seeds first, then discovered URLs
/// in the order the capture processor returned them. A URL is captured at most once; a discovered
/// URL that repeats a seed or an earlier discovery is dropped rather than requeued.
///
/// # Examples
///
/// ```no_run
/// use archivindex_archiver::client::Archiver;
/// use archivindex_archiver::config::Config;
/// use archivindex_archiver::session::{
///     Capture, CaptureProcessor, Inspection, Operator, Session,
/// };
///
/// struct HtmlProcessor;
///
/// impl CaptureProcessor for HtmlProcessor {
///     fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
///         Inspection {
///             links: Vec::new(), // parse links here
///             title: capture.payload.is_empty().then(|| "Empty".to_owned()),
///         }
///     }
/// }
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let summary = Session::new(
///     Archiver::new(Config::default())?,
///     "example-crawl",
///     Operator {
///         name: "A. Archivist".to_owned(),
///         email: Some("archivist@example.com".to_owned()),
///     },
///     ["https://www.example.com/"],
///     "example-crawl.wacz",
/// )?
/// .processor(HtmlProcessor)
/// .run()?;
///
/// assert!(summary.is_complete());
/// # Ok(())
/// # }
/// ```
pub struct Session<'a> {
    archiver: Archiver,
    id: String,
    operator: Operator,
    software: (String, String),
    seeds: Vec<String>,
    output: PathBuf,
    processor: Option<Box<dyn CaptureProcessor + 'a>>,
    retry: RetryConfig,
    limit: Option<usize>,
}

impl<'a> Session<'a> {
    /// Create a session using the given archiver to capture seed URLs into a WACZ file at the given
    /// path, run by the given operator.
    ///
    /// The identifier must be non-empty and hold only URI unreserved characters (ASCII letters,
    /// digits, `-`, `.`, `_`, and `~`), so that it can appear verbatim in the WARC file name and
    /// the `warcinfo` record. A session captures one URL at a time so that processing can grow the
    /// queue between captures; the archiver's concurrency setting therefore applies only to its
    /// one-shot [`Archiver::archive`](crate::client::Archiver::archive) methods.
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>, P: Into<PathBuf>>(
        archiver: Archiver,
        id: &str,
        operator: Operator,
        seeds: I,
        output: P,
    ) -> Result<Self, Error> {
        if id.is_empty()
            || !id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
            })
        {
            return Err(Error::InvalidSessionId(id.to_owned()));
        }

        Ok(Self {
            archiver,
            id: id.to_owned(),
            operator,
            software: (
                env!("CARGO_PKG_NAME").to_owned(),
                env!("CARGO_PKG_VERSION").to_owned(),
            ),
            seeds: seeds
                .into_iter()
                .map(|seed| seed.as_ref().to_owned())
                .collect(),
            output: output.into(),
            processor: None,
            retry: RetryConfig::default(),
            limit: None,
        })
    }

    /// Name the crawling software written into the `warcinfo` record's `software` field (as
    /// `name/version`), replacing the default: this crate's name and version.
    ///
    /// Neither value may contain a control character, which the `warc-fields` grammar rejects;
    /// [`run`](Self::run) fails on such a value before any network activity.
    #[must_use]
    pub fn software(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.software = (name.into(), version.into());

        self
    }

    /// Set the processor called once with every successful capture.
    ///
    /// Its discovered links are appended to the queue (skipping any already captured or queued),
    /// and its title is written into the capture's page list entry.
    #[must_use]
    pub fn processor<P: CaptureProcessor + 'a>(mut self, processor: P) -> Self {
        self.processor = Some(Box::new(processor));

        self
    }

    /// Set how transient network failures are retried.
    #[must_use]
    pub const fn retry(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;

        self
    }

    /// Limit the number of successfully captured requested URLs written by the session.
    ///
    /// Failed URLs do not count toward the limit. A limit of zero writes the collection metadata
    /// without attempting any queued URL. Reaching the limit is a successful end to the session;
    /// URLs left in the queue are intentionally not attempted.
    #[must_use]
    pub const fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);

        self
    }

    /// Run the crawl to the end of its queue and write the WACZ file, refusing to overwrite an
    /// existing file.
    ///
    /// The `warcinfo` fields are validated and the output file is created up front, so invalid
    /// metadata or an unusable path fails the session before any network activity. An error return
    /// means the archive could not be written; a crawl that ended early for any other reason
    /// instead reports the cause in the summary's [`fatal_error`](SessionSummary::fatal_error),
    /// with the archive holding everything captured before the error.
    pub fn run(mut self) -> Result<SessionSummary, Error> {
        // The collection is opened first so that a `warcinfo` value the field grammar rejects fails
        // the session before the output file is created.
        let mut collection = self.archiver.session_collection(
            &self.id,
            (&self.software.0, &self.software.1),
            (&self.operator.name, self.operator.email.as_deref()),
        )?;
        let wacz = self.archiver.wacz_to_path(&self.output)?;

        // The seen set covers everything ever queued, so a URL is captured at most once even when
        // the processor rediscovers it. A seed snapshot lets the summary separate seed and
        // discovered captures. Each queue entry carries the URI of the page on which it was
        // discovered (its `via` referrer); seeds have no referrer.
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();

        for seed in std::mem::take(&mut self.seeds) {
            if seen.insert(seed.clone()) {
                queue.push_back((seed, None));
            }
        }

        let seeds = seen.clone();
        let mut fatal_error = None;
        let mut capture_count = 0;

        while self.limit.is_none_or(|limit| capture_count < limit) {
            let Some((url, via)) = queue.pop_front() else {
                break;
            };
            let (exchanges, error) = self.capture_with_retry(&url);
            let captured = error.is_none();

            let title = if captured {
                self.process_capture(&url, &exchanges, &mut seen, &mut queue)
            } else {
                None
            };

            let extra = !seeds.contains(&url);

            if let Err(error) =
                collection.record(url, exchanges, error, title, extra, via.as_deref())
            {
                fatal_error = Some(error);
                break;
            }

            capture_count += usize::from(captured);
        }

        // The archive is written even after a fatal error, so that nothing captured is lost; when
        // writing fails too, the error that ended the crawl is the one reported, since the write
        // failure is almost always its consequence.
        match collection.finish(wacz, Some(self.id)) {
            Ok(summary) => {
                let (seed_captures, extra_captures) = summary
                    .captures
                    .into_iter()
                    .partition(|capture| seeds.contains(&capture.url));

                Ok(SessionSummary {
                    seed_captures,
                    extra_captures,
                    failures: summary.failures,
                    fatal_error,
                })
            }
            Err(error) => Err(fatal_error.unwrap_or(error)),
        }
    }

    /// Show a successful capture to the processor, queue its discoveries, and return its title.
    ///
    /// A discovered URL uses this capture's final URL as its referrer because the processor
    /// inspected the final response, not the originally requested URL before any redirects.
    fn process_capture(
        &mut self,
        url: &str,
        exchanges: &[Exchange],
        seen: &mut HashSet<String>,
        queue: &mut VecDeque<(String, Option<String>)>,
    ) -> Option<String> {
        let processor = self.processor.as_mut()?;

        let last = exchanges
            .last()
            .expect("a capture without an error has at least one exchange");

        // The entity body mirrors what the archive records as the payload; a body that cannot be
        // decoded falls back to the stored bytes (see `Capture`).
        let payload = payload::entity_body(&last.captured.response).unwrap_or_else(|_| {
            let body_offset = response::head(&last.captured.response)
                .expect("invariant violation: the recorder stores a well-formed response head")
                .body_offset;

            Cow::Borrowed(&last.captured.response[body_offset..])
        });

        let capture = Capture {
            url,
            final_url: last.captured.target_uri.as_str(),
            status: last.status,
            payload: &payload,
        };

        let Inspection { links, title } = processor.inspect(&capture);

        for discovered in links {
            if seen.insert(discovered.clone()) {
                queue.push_back((discovered, Some(capture.final_url.to_owned())));
            }
        }

        title
    }

    /// Capture a URL, retrying with exponential backoff when the failure is transient.
    ///
    /// Hops captured by an attempt that is retried are discarded, so that the archive does not hold
    /// a partial redirect chain for every attempt; the exchanges of the last attempt are kept
    /// either way, matching the archiver's behavior of recording hops captured before a failure.
    fn capture_with_retry(&self, url: &str) -> (Vec<Exchange>, Option<Error>) {
        let attempts = self.retry.attempts.max(1);
        let mut backoff = self.retry.initial_backoff;

        for _ in 1..attempts {
            let (exchanges, error) = self.archiver.capture(url);

            match error {
                Some(error) if is_transient(&error) => {
                    drop((exchanges, error));
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(self.retry.max_backoff);
                }
                error => return (exchanges, error),
            }
        }

        self.archiver.capture(url)
    }
}

/// Whether a capture failure could plausibly succeed on retry.
///
/// Network I/O failures (a refused or dropped connection, a timeout) are transient, as is a
/// response that ended (by disconnect or read timeout) before a complete header section arrived,
/// which is how the recorder reports a server that accepted the connection but never answered. A
/// URL rejected before any request and a response the recorder cannot frame are not, since
/// repeating the request cannot change the outcome. TLS failures are likewise treated as permanent:
/// a handshake rejection almost always reflects configuration rather than a transient fault.
const fn is_transient(error: &Error) -> bool {
    matches!(
        error,
        Error::Fetch(
            archivindex_warc::recorder::Error::Io(_)
                | archivindex_warc::recorder::Error::Response(
                    archivindex_warc::recorder::ResponseError::IncompleteHeaderSection
                )
        )
    )
}
