//! The archiving client's implementation.

use std::io::Write;
use std::path::Path;

use archivindex_warc::recorder::Recorder;
use archivindex_warc_revisit_index::db::Index as RevisitIndex;
use http::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};

use crate::capture::{ArchiveSummary, CaptureControl, CaptureEvent, CaptureEventSink};
use crate::{Archiver, Config, Error, InvalidUserAgent};

pub mod collection;
pub mod outcome;
mod pool;
mod warc_fields;
mod warc_mapping;

use collection::Collection;
use outcome::CaptureOutcome;
use warc_fields::WarcinfoOptions;

const WARC_NAME: &str = "data.warc";
const GZIP_WARC_NAME: &str = "data.warc.gz";

struct IgnoreEvents;

impl CaptureEventSink for IgnoreEvents {
    fn event(&mut self, _event: CaptureEvent<'_>) -> CaptureControl {
        CaptureControl::Continue
    }
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

pub fn notify_outcome(
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
