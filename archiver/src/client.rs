//! The archiving client.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use std::sync::{Mutex, mpsc};
use std::thread;

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::pages::{Page, PageListHeader};
use archivindex_wacz::writer::{PackageMetadata, WaczWriter, WriterConfig};
use archivindex_warc::io::write::{WarcWriter, Written};
use archivindex_warc::record::capture::CaptureEvent;
use archivindex_warc::record::fields::metadata::MetadataField;
use archivindex_warc::record::{BlockError, FieldsBlock, Record, payload};
use archivindex_warc::recorder::{CapturedExchange, Recorder};
use archivindex_warc::value::{DigestAlgorithm, LabelledDigest, WarcDate, WarcDatePrecision};
use chrono::{DateTime, Utc};
use fluent_uri::Uri;
use http::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use url::Url;

use crate::config::{Config, DEFAULT_USER_AGENT};
use crate::response;

/// The name of the uncompressed WARC file in the WACZ.
const WARC_NAME: &str = "data.warc";
/// The name of the gzip-compressed WARC file in the WACZ.
const GZIP_WARC_NAME: &str = "data.warc.gz";
/// The name of the CDXJ index file.
const INDEX_NAME: &str = "index.cdx";
/// The page list for URLs discovered during a crawl, alongside the required `pages.jsonl` seed list
/// in the `pages/` directory.
const EXTRA_PAGES_NAME: &str = "extraPages.jsonl";

/// The precision at which `WARC-Date` fields are recorded.
///
/// WARC 1.1 admits up to nine fractional-second digits, but the system clock is read at whatever
/// resolution the host happens to offer, so digits past the sixth would record precision the
/// timestamps do not have.
const DATE_PRECISION: WarcDatePrecision = WarcDatePrecision::Fraction(6);

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
    /// A URL to be archived contains credentials.
    ///
    /// Credentials are rejected rather than archived: an HTTP client would send them as an
    /// `Authorization` header, so capturing the exchange would either leak the secret into the
    /// archive or misrepresent what was sent. The URL carried here has its credentials removed so
    /// that the error is safe to log.
    #[error("URL contains credentials: {0}")]
    CredentialedUrl(String),
    /// A URL to be archived does not have a host.
    #[error("URL has no host: {0}")]
    MissingHost(String),
    /// A URL to be archived cannot be used as an HTTP request target.
    ///
    /// The recorder takes its target in the `http` crate's URI form, whose grammar is stricter than
    /// the URL parser's, so a URL that parses is not necessarily one that can be fetched.
    #[error("URL is not a valid URI: {url}")]
    InvalidUri {
        /// The URL as requested.
        url: String,
        /// Where the URL departs from the URI grammar.
        #[source]
        source: http::uri::InvalidUri,
    },
    /// The configured `User-Agent` cannot be sent as an HTTP header value or recorded as a
    /// `warcinfo` field value (both reject control characters, line breaks in particular).
    #[error("invalid User-Agent header value: {0:?}")]
    InvalidUserAgent(String),
    /// A session identifier is empty or holds a character outside the URL-safe set.
    ///
    /// Session identifiers are restricted to the URI unreserved characters (ASCII letters, digits,
    /// `-`, `.`, `_`, and `~`) so that they can appear verbatim in WARC file names and `warcinfo`
    /// fields.
    #[error("invalid session identifier: {0:?}")]
    InvalidSessionId(String),
    /// A CDXJ search key could not be derived for a URL.
    #[error(transparent)]
    Index(#[from] cdxj::Error),
    /// A WARC content block could not be attached to its record.
    #[error(transparent)]
    WarcBlock(#[from] BlockError),
    /// A `warc-fields` value (such as a `via` referrer) could not be written.
    #[error(transparent)]
    WarcFields(#[from] archivindex_warc::record::fields::Error),
    /// A WARC record could not be rendered into its written form.
    #[error(transparent)]
    WarcRender(#[from] archivindex_warc::record::RenderError),
    /// A WARC record could not be written.
    #[error(transparent)]
    WarcWrite(#[from] archivindex_warc::io::write::Error),
    /// The WACZ file could not be written.
    #[error(transparent)]
    Wacz(#[from] archivindex_wacz::writer::Error),
}

/// The outcome of an archiving run.
///
/// Individual URLs that could not be downloaded are reported here rather than treated as errors, so
/// that one unreachable URL does not lose the rest of the collection.
#[derive(Debug, Default)]
pub struct ArchiveSummary {
    /// The URLs archived successfully, in request order.
    pub captures: Vec<CaptureSummary>,
    /// The URLs that could not be captured, with the reason for each.
    pub failures: Vec<Failure>,
}

impl ArchiveSummary {
    /// Whether every URL was captured.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The outcome of capturing a single URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSummary {
    /// The URL as requested.
    pub url: String,
    /// When the capture of the final response began (the `WARC-Date` of its records).
    pub date: DateTime<Utc>,
    /// The status code of the final response.
    pub status: u16,
    /// The payload length in bytes of the final response: its entity body, with any chunked
    /// transfer coding removed.
    pub size: u64,
    /// The number of redirects followed (each hop is recorded in the archive).
    pub redirects: usize,
}

/// A URL that could not be captured.
///
/// Hops of a redirect chain captured before the failure are still recorded in the archive and its
/// index; only the page entry, which describes a final response that was never received, is
/// omitted.
#[derive(Debug)]
pub struct Failure {
    /// The URL as requested.
    pub url: String,
    /// The reason the capture failed.
    pub error: Error,
}

/// A single captured exchange, indexed but not yet written.
pub(crate) struct Exchange {
    key: String,
    /// The capture date at the recorded precision, shared by the WARC records (via the capture
    /// event) and the index entry, so that the index timestamp cannot drift from the records.
    date: WarcDate,
    pub(crate) status: u16,
    /// The digest of the response entity body, absent when the body cannot be decoded (see
    /// [`Archiver::fetch`]).
    payload_digest: Option<Sha256Digest>,
    payload_length: u64,
    pub(crate) captured: CapturedExchange,
}

/// A completed capture sent from a worker to the writer: its input position, URL, exchanges, and
/// any error that ended the redirect chain.
type CaptureOutcome = (usize, String, Vec<Exchange>, Option<Error>);

/// Information recorded in the WARC file's initial `warcinfo` record.
pub(crate) struct WarcinfoOptions<'a> {
    /// The `User-Agent` header value sent with every request.
    pub(crate) user_agent: &'a str,
    /// The crawling software's name and version, overriding this crate's.
    pub(crate) software: Option<(&'a str, &'a str)>,
    /// The operator's name and, optionally, email address.
    pub(crate) operator: Option<(&'a str, Option<&'a str>)>,
    /// The identifier of the crawl session containing the WARC file.
    pub(crate) session_id: Option<&'a str>,
}

impl<'a> WarcinfoOptions<'a> {
    /// The options for a one-shot archiving run: this crate as the software, no operator, and no
    /// session.
    pub(crate) const fn archiver(user_agent: &'a str) -> Self {
        Self {
            user_agent,
            software: None,
            operator: None,
            session_id: None,
        }
    }
}

/// Files accumulated while captures are written to a spooled WARC file.
pub(crate) struct Collection {
    /// The WARC writer over the spool tracks record offsets and digests the stored bytes, so the
    /// index entries take their framing straight from each write.
    warc: WarcWriter<BufWriter<File>>,
    warcinfo_id: Uri<String>,
    warc_name: String,
    gzip: bool,
    summary: ArchiveSummary,
    items: Vec<cdxj::Item<'static>>,
    page_list: Vec<Page<'static>>,
    extra_page_list: Vec<Page<'static>>,
}

impl Collection {
    /// Start a collection by writing a `warcinfo` record to a temporary, spooled WARC file.
    pub(crate) fn new(
        warc_name: String,
        gzip: bool,
        warcinfo: &WarcinfoOptions<'_>,
    ) -> Result<Self, Error> {
        let mut warc = WarcWriter::new(BufWriter::new(tempfile::tempfile()?)).with_digests();

        let warcinfo = warcinfo_record(&warc_name, warcinfo)?;
        let warcinfo_id = warcinfo.core().record_id.clone();
        write_record(&mut warc, warcinfo, gzip)?;

        Ok(Self {
            warc,
            warcinfo_id,
            warc_name,
            gzip,
            summary: ArchiveSummary::default(),
            items: Vec::new(),
            page_list: Vec::new(),
            extra_page_list: Vec::new(),
        })
    }

    /// Record the outcome of capturing one URL: write and index every captured hop, then add a page
    /// entry and capture summary on success, or a failure entry otherwise.
    ///
    /// The page entry carries the given title and is written to the extra page list rather than the
    /// main one when `extra` is set. When a referring URI is given (the page a crawl session
    /// discovered this URL on), the metadata record of the first hop carries it as `via`; later
    /// hops were reached by redirect rather than discovery, so they do not repeat it.
    pub(crate) fn record(
        &mut self,
        url: String,
        exchanges: Vec<Exchange>,
        error: Option<Error>,
        title: Option<String>,
        extra: bool,
        via: Option<&str>,
    ) -> Result<(), Error> {
        let redirects = exchanges.len().saturating_sub(1);
        let mut last = None;

        for (hop, exchange) in exchanges.into_iter().enumerate() {
            last = Some((
                exchange.date.date_time(),
                exchange.status,
                exchange.payload_length,
            ));

            let item = write_exchange(
                &mut self.warc,
                exchange,
                &self.warcinfo_id,
                &self.warc_name,
                self.gzip,
                via.filter(|_| hop == 0),
            )?;
            self.items.push(item);
        }

        if let Some(error) = error {
            self.summary.failures.push(Failure { url, error });
        } else {
            let (date, status, size) =
                last.expect("a capture without an error has at least one exchange");

            let page = Page {
                url: Cow::Owned(url.clone()),
                ts: date,
                id: None,
                title: title.map(Cow::Owned),
                text: None,
                size: Some(size),
                extra: ExtraProperties::default(),
            };

            if extra {
                self.extra_page_list.push(page);
            } else {
                self.page_list.push(page);
            }

            self.summary.captures.push(CaptureSummary {
                url,
                date,
                status,
                size,
                redirects,
            });
        }

        Ok(())
    }

    /// Add the spooled WARC and supporting files to the WACZ and finish it.
    ///
    /// The extra page list (`extraPages.jsonl`) is written only when it has entries, so that
    /// collections without discovered pages keep the conventional layout. The main page of the
    /// manifest is the first entry of the main page list.
    pub(crate) fn finish<W: Write + Seek>(
        self,
        mut wacz: WaczWriter<W>,
        title: Option<String>,
    ) -> Result<ArchiveSummary, Error> {
        let Self {
            warc,
            warc_name,
            summary,
            items,
            page_list,
            extra_page_list,
            ..
        } = self;

        let mut file = warc.finish().map_err(std::io::IntoInnerError::into_error)?;
        file.rewind()?;

        wacz.add_warc(&warc_name, file)?;
        wacz.add_index(INDEX_NAME, &items)?;
        wacz.add_pages(&PageListHeader::default(), &page_list)?;

        if !extra_page_list.is_empty() {
            wacz.add_page_list(
                EXTRA_PAGES_NAME,
                &extra_page_list_header(),
                &extra_page_list,
            )?;
        }

        let metadata = PackageMetadata {
            title,
            software: Some(DEFAULT_USER_AGENT.to_owned()),
            main_page_url: page_list.first().map(|page| page.url.clone().into_owned()),
            main_page_date: page_list.first().map(|page| page.ts),
            ..PackageMetadata::default()
        };

        wacz.finish(metadata)?.flush()?;

        Ok(summary)
    }
}

/// The conventional header of the `pages/extraPages.jsonl` list holding pages discovered during a
/// crawl, matching the identifier and title `py-wacz` writes.
fn extra_page_list_header() -> PageListHeader<'static> {
    PageListHeader {
        format: Cow::Borrowed(archivindex_wacz::pages::FORMAT),
        id: Some(Cow::Borrowed("extra-pages")),
        title: Some(Cow::Borrowed("Extra Pages")),
        extra: ExtraProperties::default(),
    }
}

/// An HTTP client that captures lists of URLs in WACZ files.
///
/// Each URL is fetched with a `GET` request over a connection of its own, following redirects up to
/// the configured limit; every hop is recorded in the WARC file as a request record and a response
/// record holding the HTTP messages exactly as they crossed the wire (chunked transfer coding,
/// header spelling, and reason phrase included), followed by a `metadata` record giving the
/// response capture duration. A CDXJ entry indexes each response, and a page list entry describes
/// each requested URL's final response. When a download fails partway through a redirect chain, the
/// hops already captured are still recorded and the URL is reported as a failure; a response cut
/// short by a size limit, disconnect, or read timeout is recorded as truncated rather than failed.
///
/// The client is blocking: each fetch performs synchronous network I/O on the calling thread.
#[derive(Clone, Debug)]
pub struct Archiver {
    recorder: Recorder,
    headers: HeaderMap,
    config: Config,
}

impl Archiver {
    /// Create a new archiving client.
    ///
    /// Requests are made over HTTP/1.1 only, so that the recorded messages match the wire format,
    /// and redirects are followed (and captured) by the archiver itself.
    pub fn new(config: Config) -> Result<Self, Error> {
        // Building the header value up front validates the configured `User-Agent`: a value with
        // embedded line breaks would otherwise forge header lines in the serialized request and
        // break every request sent.
        let user_agent = HeaderValue::from_str(&config.user_agent)
            .map_err(|_| Error::InvalidUserAgent(config.user_agent.clone()))?;

        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(USER_AGENT, user_agent);

        // The configured timeout bounds connecting and each socket operation; the recorder adds the
        // `host` and closing `connection` headers around the pair prepared above.
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

    /// Download each URL and write a WACZ file at the given path, refusing to overwrite an existing
    /// file.
    pub fn archive_to_path<P: AsRef<Path>, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        path: P,
    ) -> Result<ArchiveSummary, Error> {
        self.archive_into(
            urls,
            WaczWriter::create_with_config(path, self.writer_config())?,
        )
    }

    /// Download each URL and write a WACZ file to the given writer.
    pub fn archive<W: Write + Seek, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        writer: W,
    ) -> Result<ArchiveSummary, Error> {
        self.archive_into(urls, WaczWriter::with_config(writer, self.writer_config()))
    }

    /// Start the collection used by a crawl session.
    ///
    /// Keeping this construction here makes the archiver the single owner of capture and archive
    /// format configuration. A session supplies only the crawl metadata that is unique to it.
    pub(crate) fn session_collection(
        &self,
        id: &str,
        software: (&str, &str),
        operator: (&str, Option<&str>),
    ) -> Result<Collection, Error> {
        let gzip = self.config.gzip_warc;
        let suffix = if gzip { ".warc.gz" } else { ".warc" };

        Collection::new(
            format!("{id}{suffix}"),
            gzip,
            &WarcinfoOptions {
                user_agent: &self.config.user_agent,
                software: Some(software),
                operator: Some(operator),
                session_id: Some(id),
            },
        )
    }

    /// Create a WACZ writer using this archiver's output configuration.
    pub(crate) fn wacz_to_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<WaczWriter<BufWriter<File>>, Error> {
        Ok(WaczWriter::create_with_config(path, self.writer_config())?)
    }

    /// The WACZ writer configuration derived from this client's configuration.
    pub(crate) fn writer_config(&self) -> WriterConfig {
        WriterConfig {
            index_format: self.config.index_format,
            ..WriterConfig::default()
        }
    }

    /// Capture each URL in a spooled WARC file, then assemble the WACZ.
    fn archive_into<W: Write + Seek, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        wacz: WaczWriter<W>,
    ) -> Result<ArchiveSummary, Error> {
        let gzip = self.config.gzip_warc;
        let warc_name = if gzip { GZIP_WARC_NAME } else { WARC_NAME };

        let mut collection = Collection::new(
            warc_name.to_owned(),
            gzip,
            &WarcinfoOptions::archiver(&self.config.user_agent),
        )?;

        let concurrency = self.config.concurrency.max(1);

        if concurrency == 1 {
            for url in urls {
                let url = url.as_ref();
                let (exchanges, error) = self.capture(url);

                collection.record(url.to_owned(), exchanges, error, None, false, None)?;
            }
        } else {
            self.capture_concurrently(urls, concurrency, &mut collection)?;
        }

        collection.finish(wacz, None)
    }

    /// Capture URLs with a pool of worker threads, recording the outcomes in input order.
    ///
    /// At most `concurrency` downloads are in flight at a time, and completed captures are buffered
    /// only until their turn comes to be written, so memory use is proportional to the concurrency
    /// rather than to the number of URLs.
    fn capture_concurrently<I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        concurrency: usize,
        collection: &mut Collection,
    ) -> Result<(), Error> {
        let mut urls = urls.into_iter();

        // The channels live outside the thread scope so that the workers may borrow them; the task
        // sender is moved into the scope body and dropped there, closing the task channel so that
        // idle workers exit before the scope joins them.
        let (task_sender, task_receiver) = mpsc::channel::<(usize, String)>();
        let task_receiver = Mutex::new(task_receiver);
        let (outcome_sender, outcome_receiver) = mpsc::sync_channel::<CaptureOutcome>(concurrency);

        thread::scope(|scope| {
            for _ in 0..concurrency {
                let task_receiver = &task_receiver;
                let outcome_sender = outcome_sender.clone();

                scope.spawn(move || {
                    loop {
                        // The lock is held only to take the next task, never while downloading.
                        let task = task_receiver
                            .lock()
                            .ok()
                            .and_then(|receiver| receiver.recv().ok());

                        let Some((index, url)) = task else { return };
                        let (exchanges, error) = self.capture(&url);

                        // The writer hanging up means the run ended early; stop working.
                        if outcome_sender.send((index, url, exchanges, error)).is_err() {
                            return;
                        }
                    }
                });
            }

            drop(outcome_sender);

            let mut dispatched = 0;

            for (index, url) in urls.by_ref().take(concurrency).enumerate() {
                let _ = task_sender.send((index, url.as_ref().to_owned()));
                dispatched += 1;
            }

            let mut result = Ok(());
            let mut completed = 0;
            let mut next_to_record = 0;
            let mut pending = BTreeMap::new();

            // Every dispatched task is drained even after a write error, so that no worker is left
            // blocked on the outcome channel when the scope joins the pool.
            while completed < dispatched {
                let (index, url, exchanges, error) = outcome_receiver
                    .recv()
                    .expect("workers always report an outcome before exiting");
                completed += 1;

                if result.is_ok() {
                    // Refill the pool so that `concurrency` downloads stay in flight.
                    if let Some(url) = urls.next() {
                        let _ = task_sender.send((dispatched, url.as_ref().to_owned()));
                        dispatched += 1;
                    }

                    // Outcomes are recorded strictly in input order, so completions that arrive
                    // early wait in the reorder buffer for their turn.
                    pending.insert(index, (url, exchanges, error));

                    while let Some((url, exchanges, error)) = pending.remove(&next_to_record) {
                        if let Err(error) =
                            collection.record(url, exchanges, error, None, false, None)
                        {
                            result = Err(error);
                            break;
                        }

                        next_to_record += 1;
                    }
                }
            }

            drop(task_sender);

            result
        })
    }

    /// Fetch a URL and every hop of its redirect chain, in order.
    ///
    /// The returned list holds every hop captured. When the error is set, the request for the next
    /// hop (or, if the list is empty, the first request) failed; otherwise the list ends with the
    /// final response. A response that still redirects after the configured limit (or whose target
    /// is unusable) is recorded as final rather than followed.
    pub(crate) fn capture(&self, url: &str) -> (Vec<Exchange>, Option<Error>) {
        let mut exchanges = Vec::new();

        let mut current = match Url::parse(url) {
            Ok(url) => url,
            Err(error) => return (exchanges, Some(error.into())),
        };

        loop {
            match self.fetch(&current) {
                Ok((exchange, location)) => {
                    exchanges.push(exchange);

                    match location {
                        Some(next) if exchanges.len() <= self.config.max_redirects => {
                            current = next;
                        }
                        _ => return (exchanges, None),
                    }
                }
                Err(error) => return (exchanges, Some(error)),
            }
        }
    }

    /// Perform one `GET` request, recording the exchange's wire bytes and returning its redirect
    /// target, if any.
    fn fetch(&self, url: &Url) -> Result<(Exchange, Option<Url>), Error> {
        // A URL with credentials cannot be archived faithfully: an HTTP client would turn them into
        // an `Authorization` header, so recording the exchange would either leak the secret into
        // the archive or misrepresent what was sent.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::CredentialedUrl(redact_credentials(url)));
        }

        if url.host_str().is_none() {
            return Err(Error::MissingHost(url.to_string()));
        }

        let key = cdxj::search_key(url.as_str())?;
        let target = url
            .as_str()
            .parse::<http::Uri>()
            .map_err(|source| Error::InvalidUri {
                url: url.to_string(),
                source,
            })?;

        let captured = self
            .recorder
            .fetch(&http::Method::GET, &target, &self.headers, None)?;

        let head = response::head(&captured.response)
            .expect("invariant violation: the recorder stores a well-formed response head");
        let location = next_location(url, head.status, head.location.as_deref());

        // The WARC payload is the entity body: the message body with its chunk framing removed. A
        // body that cannot be decoded (cut short inside its chunk framing, or delivered under a
        // transfer coding the WARC crate cannot remove) is still archived verbatim, but with no
        // payload digest declared, and its stored length stands in for the payload length.
        let (payload_digest, payload_length) = match payload::entity_body(&captured.response) {
            Ok(payload) => (Some(Sha256Digest::compute(&payload)), payload.len() as u64),
            Err(_) => (None, (captured.response.len() - head.body_offset) as u64),
        };

        Ok((
            Exchange {
                key,
                // The recorder reads the clock when network activity begins; store it at the
                // configured WARC-Date precision.
                date: WarcDate::new(captured.date, DATE_PRECISION),
                status: head.status,
                payload_digest,
                payload_length,
                captured,
            },
            location,
        ))
    }
}

/// The redirect target of a response, when present and followable over HTTP.
fn next_location(current: &Url, status: u16, location: Option<&str>) -> Option<Url> {
    // Only the redirect statuses that denote a fetchable alternate location are followed: `300
    // Multiple Choices` and `304 Not Modified` are redirection-class but are final responses in
    // their own right.
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }

    let next = current.join(location?).ok()?;

    // A target with credentials could not be archived faithfully (see `Error::CredentialedUrl`), so
    // it is treated as unusable and the redirecting response is recorded as the final hop.
    (matches!(next.scheme(), "http" | "https")
        && next.username().is_empty()
        && next.password().is_none())
    .then_some(next)
}

/// The URL rendered with its credentials removed, safe for error messages and logs.
fn redact_credentials(url: &Url) -> String {
    let mut redacted = url.clone();

    // Removing credentials only fails for URLs that cannot carry them, which cannot get here.
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);

    redacted.to_string()
}

/// The current instant at the precision `WARC-Date` fields are recorded with.
fn record_date() -> WarcDate {
    WarcDate::new(Utc::now(), DATE_PRECISION)
}

/// The writer's digest of a record's stored bytes, converted into the WACZ digest type.
fn stored_digest(written: &Written) -> Sha256Digest {
    written
        .digest
        .as_ref()
        .and_then(LabelledDigest::decoded)
        .and_then(|bytes| bytes.try_into().ok())
        .map(Sha256Digest)
        .expect("invariant violation: a digesting writer reports a 32-byte SHA-256 digest")
}

/// Render one record and write it to the spooled WARC file, returning where its stored bytes landed
/// and their digest.
///
/// Rendering the record supplies its `WARC-Block-Digest`. When `gzip` is set, the record is written
/// as an independent gzip member, so that the reported length (and therefore the index offsets
/// derived from it) frames a complete member that can be decompressed on its own; the digest
/// likewise covers the stored (compressed) bytes, so that it describes exactly the framed range.
fn write_record<W: Write>(
    writer: &mut WarcWriter<W>,
    record: Record,
    gzip: bool,
) -> Result<Written, Error> {
    let record = record.into_raw()?;

    if gzip {
        writer.write_gzip(&record)
    } else {
        writer.write(&record)
    }
    .map_err(Error::from)
}

/// Build and write the request, response, and `metadata` records for an exchange, returning the
/// CDXJ index entry framing the response.
///
/// The capture event returns its records in the order they cross-reference each other, so that a
/// reader working forwards has already seen every record a `WARC-Concurrent-To` names: the request
/// first, then the response naming it, then the `metadata` record naming the response. A referring
/// URI, when given, is written into the `metadata` record as its `via` field.
fn write_exchange<W: Write>(
    writer: &mut WarcWriter<W>,
    exchange: Exchange,
    warcinfo_id: &Uri<String>,
    warc_name: &str,
    gzip: bool,
    via: Option<&str>,
) -> Result<cdxj::Item<'static>, Error> {
    let mut event = CaptureEvent::new(exchange.captured.target_uri.clone(), exchange.date)
        .warcinfo_id(warcinfo_id.clone())
        .ip_address(exchange.captured.ip_address)
        .identify_payload_type()
        // Setting the fetch time adds the `metadata` record carrying `fetchTimeMs`, tied to the
        // response by `WARC-Concurrent-To` as in Annex B.5 of the standard.
        .fetch_time(exchange.captured.fetch_time);

    if let Some(digest) = &exchange.payload_digest {
        event = event.payload_digest(LabelledDigest::from_digest(
            DigestAlgorithm::Sha256,
            &digest.0,
        ));
    }

    if let Some(reason) = exchange.captured.truncated.clone() {
        event = event.truncated(reason);
    }

    let mut records = event.exchange(exchange.captured.request, exchange.captured.response)?;

    // The capture event builds the `metadata` record (present whenever a fetch time is set, as it
    // always is here); the referrer is added to its `warc-fields` body before the record is
    // rendered, so the rendered block, its digest, and the index framing all cover it. The
    // `Content-Length` declared when the record was built described the body without the new field,
    // so it is re-declared from the extended body.
    if let (
        Some(via),
        Some(Record::Metadata {
            header,
            body: FieldsBlock::Fields(fields),
        }),
    ) = (via, records.metadata.as_mut())
    {
        fields.push(MetadataField::Via, via)?;
        header.core.content_length = Some(fields.rendered_len() as u64);
    }

    let mime = records
        .response
        .payload()
        .and_then(|payload| payload.identified_payload_type.as_ref())
        .map(ToString::to_string);

    write_record(writer, records.request, gzip)?;
    let response = write_record(writer, records.response, gzip)?;

    if let Some(metadata) = records.metadata {
        write_record(writer, metadata, gzip)?;
    }

    Ok(cdxj::Item {
        key: Cow::Owned(exchange.key),
        // CDXJ permits millisecond precision. Preserve it by default so captures within one second
        // remain chronologically distinguishable while the WARC records retain microseconds.
        timestamp: cdxj::Timestamp::with_milliseconds(exchange.date.date_time()),
        fields: cdxj::Fields {
            url: Cow::Owned(exchange.captured.target_uri.into_string()),
            digest: exchange
                .payload_digest
                .map(|digest| Cow::Owned(digest.to_string())),
            mime: mime.map(Cow::Owned),
            status: Some(exchange.status),
            offset: Some(response.offset),
            length: Some(response.length),
            filename: Some(Cow::Owned(warc_name.to_owned())),
            record_digest: Some(stored_digest(&response)),
            extra: ExtraProperties::default(),
        },
    })
}

/// The `warcinfo` record at the start of the WARC file.
///
/// The builder derives `Content-Type`, `Content-Length`, `format`, and `conformsTo` from the WARC
/// version and body. The body identifies the software and HTTP `User-Agent`; crawl sessions also
/// identify the operator and session.
fn warcinfo_record(warc_name: &str, options: &WarcinfoOptions<'_>) -> Result<Record, Error> {
    // The WARC file name is either a compile-time constant or derived from a session identifier
    // restricted to URL-safe characters. The `User-Agent` is already a valid HTTP header value, but
    // the warc-fields grammar is stricter because it also rejects tabs. Software and operator
    // values are validated here for the first time.
    let (software_name, software_version) = options
        .software
        .unwrap_or((env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")));

    let mut builder = Record::warcinfo(record_date())
        .filename(warc_name)
        .expect("well-formed WARC file name")
        .software(software_name, software_version)?;

    if let Some((name, email)) = options.operator {
        builder = builder.operator(name, email)?;
    }

    builder = builder
        .http_header_user_agent(options.user_agent)
        .map_err(|_| Error::InvalidUserAgent(options.user_agent.to_owned()))?;

    if let Some(session_id) = options.session_id {
        builder = builder
            .is_part_of(session_id)
            .expect("well-formed session identifier");
    }

    Ok(builder.build())
}
