//! Queue execution, processor dispatch, and retry policy.

use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::thread;

use archivindex_warc::record::payload;
use archivindex_warc_revisit_index::Index as RevisitIndex;

use super::{Capture, Inspection, Session, SessionSummary};
use crate::client::{Collection, Error, Exchange};
use crate::response;

impl Session<'_> {
    /// Run the crawl to the end of its queue and write the WACZ file.
    pub fn run(mut self) -> Result<SessionSummary, Error> {
        let persistent_index = self
            .revisit_index
            .as_ref()
            .map(RevisitIndex::open)
            .transpose()?;
        let mut collection = self.archiver.session_collection(
            &self.id,
            (&self.software.0, &self.software.1),
            (&self.operator.name, self.operator.email.as_deref()),
            persistent_index,
        )?;
        let wacz = self.archiver.wacz_to_path(&self.output)?;
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
            let (exchanges, error) = self.capture_with_retry(&url, &collection);
            let captured = error.is_none();
            let title = captured
                .then(|| self.process_capture(&url, &exchanges, &mut seen, &mut queue))
                .flatten();
            let extra = !seeds.contains(&url);

            if let Err(error) =
                collection.record(url, exchanges, error, title, extra, via.as_deref())
            {
                fatal_error = Some(error);
                break;
            }
            capture_count += usize::from(captured);
        }

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

    /// Show a successful capture to the processor and enqueue its discoveries and recaptures.
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
            response: &last.captured.response,
        };
        let Inspection {
            links,
            recaptures,
            title,
        } = processor.inspect(&capture);

        for discovered in links {
            if seen.insert(discovered.clone()) {
                queue.push_back((discovered, Some(capture.final_url.to_owned())));
            }
        }
        for recapture in recaptures {
            queue.push_back((recapture, Some(capture.final_url.to_owned())));
        }

        title
    }

    /// Capture a URL, revalidating the collection's earlier captures and retrying transient
    /// failures with exponential backoff.
    fn capture_with_retry(
        &self,
        url: &str,
        collection: &Collection,
    ) -> (Vec<Exchange>, Option<Error>) {
        let attempts = self.retry.attempts.max(1);
        let mut backoff = self.retry.initial_backoff;

        for _ in 1..attempts {
            let (exchanges, error) = self.archiver.capture(url, Some(collection));
            match error {
                Some(error) if is_transient(&error) => {
                    drop((exchanges, error));
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(self.retry.max_backoff);
                }
                error => return (exchanges, error),
            }
        }

        self.archiver.capture(url, Some(collection))
    }
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
