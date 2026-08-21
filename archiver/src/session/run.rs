//! Queue execution, processor dispatch, and retry policy.

use std::collections::{HashSet, VecDeque};
use std::thread;
use std::time::Duration;

use archivindex_warc_revisit_index::db::Index as RevisitIndex;

use super::{Capture, Inspection, Session, SessionSummary};
use crate::client::{
    ArchiveSummary, CaptureControl, CaptureEvent, CaptureOutcome, Collection, Error, Exchange,
    notify_outcome,
};

enum AttemptOutcome {
    Finished(CaptureOutcome),
    Cancelled,
}

enum CrawlOutcome {
    Complete,
    Cancelled,
    Fatal(Error),
}

impl CrawlOutcome {
    fn finish(
        self,
        archive: Result<ArchiveSummary, Error>,
        seeds: &HashSet<String>,
    ) -> Result<SessionSummary, Error> {
        match (self, archive) {
            (Self::Fatal(error), Err(_)) | (_, Err(error)) => Err(error),
            (outcome, Ok(summary)) => Ok(outcome.into_summary(summary, seeds)),
        }
    }

    fn into_summary(self, summary: ArchiveSummary, seeds: &HashSet<String>) -> SessionSummary {
        let (seed_captures, extra_captures) = summary
            .captures
            .into_iter()
            .partition(|capture| seeds.contains(&capture.url));

        match self {
            Self::Complete => SessionSummary {
                seed_captures,
                extra_captures,
                failures: summary.failures,
                fatal_error: None,
                cancelled: false,
            },
            Self::Cancelled => SessionSummary {
                seed_captures,
                extra_captures,
                failures: summary.failures,
                fatal_error: None,
                cancelled: true,
            },
            Self::Fatal(error) => SessionSummary {
                seed_captures,
                extra_captures,
                failures: summary.failures,
                fatal_error: Some(error),
                cancelled: false,
            },
        }
    }
}

impl Session<'_> {
    /// Run the crawl to the end of its queue and atomically publish its WARC file.
    pub fn run(mut self) -> Result<SessionSummary, Error> {
        let persistent_index = self
            .revisit_index
            .as_ref()
            .map(RevisitIndex::open)
            .transpose()?;
        let mut collection = self.archiver.session_collection(
            &self.id,
            &self.software,
            &self.operator,
            persistent_index,
        )?;
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();

        for seed in std::mem::take(&mut self.seeds) {
            if seen.insert(seed.clone()) {
                queue.push_back((seed, None));
            }
        }

        let seeds = seen.clone();
        let mut capture_count = 0;

        let crawl_outcome = loop {
            if self.limit.is_some_and(|limit| capture_count >= limit) {
                break CrawlOutcome::Complete;
            }
            let Some((url, via)) = queue.pop_front() else {
                break CrawlOutcome::Complete;
            };
            if self
                .events
                .as_mut()
                .is_some_and(|events| events.started(&url, 1))
            {
                break CrawlOutcome::Cancelled;
            }
            let mut outcome = match self.capture_with_retry(&url, &collection) {
                AttemptOutcome::Finished(outcome) => outcome,
                AttemptOutcome::Cancelled => break CrawlOutcome::Cancelled,
            };
            let cancel_after_write = self
                .events
                .as_mut()
                .is_some_and(|events| notify_outcome(events.as_mut(), &url, &outcome));
            let (title, processor_error) = match &outcome {
                CaptureOutcome::Captured(exchanges) => {
                    let inspection = self.process_capture(&url, exchanges, &mut seen, &mut queue);
                    if inspection.1.is_none() {
                        capture_count += 1;
                    }
                    inspection
                }
                CaptureOutcome::Failed { .. } => (None, None),
            };
            let stop_after_write = processor_error.is_some();
            if let Some(error) = processor_error {
                outcome = outcome.fail(error);
            }
            if let Err(error) =
                collection.record(url.clone(), outcome, title.as_deref(), via.as_deref())
            {
                break CrawlOutcome::Fatal(error);
            }
            if cancel_after_write
                || self.event(CaptureEvent::Written { url: &url }) == CaptureControl::Cancel
            {
                break CrawlOutcome::Cancelled;
            }
            if stop_after_write {
                break CrawlOutcome::Complete;
            }
        };

        crawl_outcome.finish(collection.finish_to_path(&self.output), &seeds)
    }

    /// Show a successful capture to the processor and enqueue its discoveries and recaptures.
    fn process_capture(
        &mut self,
        url: &str,
        exchanges: &[Exchange],
        seen: &mut HashSet<String>,
        queue: &mut VecDeque<(String, Option<String>)>,
    ) -> (Option<String>, Option<Error>) {
        let Some(processor) = self.processor.as_mut() else {
            return (None, None);
        };
        let last = exchanges
            .last()
            .expect("a capture without an error has at least one exchange");
        let payload = last.payload();
        let capture = Capture {
            url,
            final_url: last.captured.target_uri.as_str(),
            status: last.status,
            payload: &payload,
            response: &last.captured.response,
            response_metadata: last.captured.response_metadata.clone(),
        };
        let Inspection {
            links,
            recaptures,
            title,
            error,
        } = processor.inspect(&capture);

        if let Some(message) = error {
            return (
                title,
                Some(Error::Processor {
                    url: url.to_owned(),
                    message,
                }),
            );
        }

        for discovered in links {
            if seen.insert(discovered.clone()) {
                queue.push_back((discovered, Some(capture.final_url.to_owned())));
            }
        }
        for recapture in recaptures {
            queue.push_back((recapture, Some(capture.final_url.to_owned())));
        }

        (title, None)
    }

    /// Capture a URL, revalidating the collection's earlier captures and retrying transient
    /// failures with exponential backoff.
    fn capture_with_retry(&mut self, url: &str, collection: &Collection) -> AttemptOutcome {
        let attempts = self.retry.attempts.max(1);
        let mut delays = RetryDelays::new(&self.retry);

        for attempt in 0..attempts {
            if attempt > 0
                && self
                    .events
                    .as_mut()
                    .is_some_and(|events| events.started(url, attempt + 1))
            {
                return AttemptOutcome::Cancelled;
            }
            match self.archiver.capture(url, Some(collection)) {
                CaptureOutcome::Failed { exchanges, error }
                    if is_transient(&error) && attempt + 1 < attempts =>
                {
                    drop((exchanges, error));
                    if self.event(CaptureEvent::Retrying {
                        url,
                        attempt: attempt + 2,
                        delay: delays.backoff,
                    }) == CaptureControl::Cancel
                    {
                        return AttemptOutcome::Cancelled;
                    }
                    thread::sleep(delays.backoff);
                    delays.advance();
                }
                CaptureOutcome::Failed { exchanges, error } => {
                    return AttemptOutcome::Finished(CaptureOutcome::Failed { exchanges, error });
                }
                CaptureOutcome::Captured(exchanges) => {
                    let status = exchanges
                        .last()
                        .map(|exchange| exchange.status)
                        .filter(|status| is_retryable_status(*status));
                    if let Some(status) = status {
                        if attempt + 1 == attempts {
                            return AttemptOutcome::Finished(CaptureOutcome::Failed {
                                exchanges,
                                error: Error::HttpStatus {
                                    url: url.to_owned(),
                                    status,
                                },
                            });
                        }
                        let delay = exchanges.last().map_or_else(
                            || delays.backoff,
                            |exchange| delays.for_exchange(exchange),
                        );
                        if self.event(CaptureEvent::Retrying {
                            url,
                            attempt: attempt + 2,
                            delay,
                        }) == CaptureControl::Cancel
                        {
                            return AttemptOutcome::Cancelled;
                        }
                        thread::sleep(delay);
                        delays.advance();
                    } else {
                        return AttemptOutcome::Finished(CaptureOutcome::Captured(exchanges));
                    }
                }
            }
        }

        unreachable!("at least one capture attempt is made")
    }
}

struct RetryDelays {
    backoff: Duration,
    maximum: Duration,
}

impl RetryDelays {
    fn new(config: &crate::session::RetryConfig) -> Self {
        Self {
            backoff: config.initial_backoff.min(config.max_backoff),
            maximum: config.max_backoff,
        }
    }

    fn advance(&mut self) {
        self.backoff = self
            .backoff
            .checked_mul(2)
            .unwrap_or(self.maximum)
            .min(self.maximum);
    }

    fn for_exchange(&self, exchange: &Exchange) -> Duration {
        exchange
            .validator("retry-after")
            .and_then(|value| parse_retry_after(&value, chrono::Utc::now()))
            .unwrap_or(self.backoff)
            .min(self.maximum)
    }
}

const fn is_retryable_status(status: u16) -> bool {
    status == 429 || matches!(status, 500 | 502 | 503 | 504)
}

fn parse_retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse() {
        return Some(Duration::from_secs(seconds));
    }

    let retry_at = chrono::DateTime::parse_from_rfc2822(value.trim())
        .ok()?
        .to_utc();
    (retry_at - now).to_std().ok()
}

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

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;
    use crate::session::RetryConfig;

    #[test]
    fn retry_delays_clamp_initial_values_and_overflowing_growth() {
        let delays = RetryDelays::new(&RetryConfig {
            attempts: 3,
            initial_backoff: Duration::MAX,
            max_backoff: Duration::from_secs(5),
        });
        assert_eq!(delays.backoff, Duration::from_secs(5));

        let mut delays = RetryDelays::new(&RetryConfig {
            attempts: 3,
            initial_backoff: Duration::MAX,
            max_backoff: Duration::MAX,
        });
        delays.advance();
        assert_eq!(delays.backoff, Duration::MAX);
    }

    #[test]
    fn retry_after_accepts_seconds_and_future_http_dates_only() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap();

        assert_eq!(
            parse_retry_after(" 42 ", now),
            Some(Duration::from_secs(42))
        );
        assert_eq!(
            parse_retry_after("Fri, 21 Aug 2026 12:01:00 +0000", now),
            Some(Duration::from_mins(1))
        );
        assert_eq!(
            parse_retry_after("Fri, 21 Aug 2026 11:59:00 +0000", now),
            None
        );
        assert_eq!(parse_retry_after("not a delay", now), None);
    }
}
