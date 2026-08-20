//! Capturing and reading `WordPress` REST API v2 resources.

pub mod read;

use std::collections::HashSet;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use serde::Deserialize;
use url::Url;

use crate::session::{Capture, CaptureProcessor, Inspection};

/// The maximum number of comments `WordPress` permits one REST API request to return.
const COMMENTS_PER_PAGE: usize = 100;

/// Inspect batches from the `WordPress` REST API v2 comments endpoint.
///
/// The processor takes a snapshot cutoff when it is constructed. Start a crawl with
/// [`first_comment_url`](Self::first_comment_url), which requests comments in ascending ID order up
/// to that cutoff. It walks every page advertised by `X-WP-TotalPages` and normally finishes after
/// one sweep when `X-WP-Total` is stable and equals the number of distinct comment IDs seen. A
/// second sweep runs when those consistency checks fail, or when explicitly requested with
/// [`second_sweep`](Self::second_sweep). The fixed cutoff prevents ordinary additions after
/// construction from moving the snapshot. Already captured IDs remain retained even if they are
/// deleted during collection.
///
/// A recapture the server answers with `304 Not Modified` repeats a page already read: it adds no
/// IDs, and the sweep continues by the page count last advertised. Malformed JSON or an unexpected
/// HTTP response makes the session incomplete instead of silently ending pagination.
///
/// # Examples
///
/// ```no_run
/// use archivindex_archiver::client::Archiver;
/// use archivindex_archiver::config::Config;
/// use archivindex_archiver::session::{Operator, Session};
/// use archivindex_archiver::wordpress::CommentCaptureProcessor;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let processor = CommentCaptureProcessor::new("https://example.com/")?;
/// let first = processor.first_comment_url();
///
/// let summary = Session::new(
///     Archiver::new(Config::default())?,
///     "wordpress-comments",
///     Operator {
///         name: "A. Archivist".to_owned(),
///         email: None,
///     },
///     [first],
///     "wordpress-comments.warc.gz",
/// )?
/// .processor(processor)
/// .run()?;
///
/// assert!(summary.is_complete());
/// # Ok(())
/// # }
/// ```
pub struct CommentCaptureProcessor {
    endpoint: Url,
    site_name: String,
    before: DateTime<Utc>,
    seen_ids: HashSet<u64>,
    page: usize,
    sweeps_completed: usize,
    force_second_sweep: bool,
    total_comments: Option<usize>,
    previous_sweep_total: Option<usize>,
    totals_consistent: bool,
    /// The page count most recently advertised by `X-WP-TotalPages`.
    total_pages: Option<usize>,
}

impl CommentCaptureProcessor {
    /// Create a comment processor for a `WordPress` site's base URL.
    ///
    /// The current time is saved as the snapshot cutoff used by the first and every subsequent
    /// comments request. A base URL ending in a path is treated as the `WordPress` installation
    /// root, so `https://example.com/blog` targets `https://example.com/blog/wp-json/...`.
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, url::ParseError> {
        Self::with_before(base_url.as_ref(), Utc::now())
    }

    /// Produce the first comments URL for this processor's saved snapshot cutoff.
    ///
    /// The request asks for the first page in ascending comment-ID order.
    #[must_use]
    pub fn first_comment_url(&self) -> String {
        self.comment_url(1)
    }

    /// Construct a processor with an explicit snapshot cutoff.
    fn with_before(base_url: &str, before: DateTime<Utc>) -> Result<Self, url::ParseError> {
        let mut base = Url::parse(base_url)?;
        base.set_query(None);
        base.set_fragment(None);

        let path = format!("{}/", base.path().trim_end_matches('/'));
        base.set_path(&path);

        let endpoint = base.join("wp-json/wp/v2/comments")?;
        let site_name = base.host_str().unwrap_or(base.as_str()).to_owned();

        Ok(Self {
            endpoint,
            site_name,
            before,
            seen_ids: HashSet::new(),
            page: 1,
            sweeps_completed: 0,
            force_second_sweep: false,
            total_comments: None,
            previous_sweep_total: None,
            totals_consistent: true,
            total_pages: None,
        })
    }

    /// Request a validation sweep even when the first sweep's total is consistent.
    #[must_use]
    pub const fn second_sweep(mut self, enabled: bool) -> Self {
        self.force_second_sweep = enabled;
        self
    }

    /// Build one page URL, retaining the snapshot cutoff on every request.
    fn comment_url(&self, page: usize) -> String {
        let before = format_timestamp(self.before);
        let query = format!(
            "before={before}&orderby=id&order=asc&page={page}&per_page={COMMENTS_PER_PAGE}"
        );

        let mut url = self.endpoint.clone();
        url.set_query(Some(&query));

        url.into()
    }

    /// Finish a sweep, optionally scheduling one validation sweep.
    fn finish_sweep(&mut self) -> Inspection {
        self.sweeps_completed += 1;
        self.page = 1;

        let total = self.total_comments.or(self.previous_sweep_total);
        let counts_match = self.totals_consistent && total == Some(self.seen_ids.len());
        if self.sweeps_completed == 1 && (self.force_second_sweep || !counts_match) {
            self.previous_sweep_total = self.total_comments;
            self.total_comments = None;
            self.totals_consistent = true;
            return Inspection {
                recaptures: vec![self.comment_url(1)],
                ..Inspection::default()
            };
        }

        if !counts_match {
            return Inspection {
                error: Some(format!(
                    "WordPress reported {} comments after sweep {}, but {} distinct IDs were captured{}",
                    total.map_or_else(|| "no total".to_owned(), |value| value.to_string()),
                    self.sweeps_completed,
                    self.seen_ids.len(),
                    if self.totals_consistent {
                        ""
                    } else {
                        " and totals changed during the sweep"
                    }
                )),
                ..Inspection::default()
            };
        }

        Inspection::default()
    }

    /// Title a parsed comment batch by its ID and GMT date ranges.
    fn title(&self, comments: &[Comment]) -> Option<String> {
        let first_id = comments.iter().map(|comment| comment.id).min()?;
        let last_id = comments.iter().map(|comment| comment.id).max()?;
        let first_date = comments
            .iter()
            .filter_map(Comment::date)
            .min()?
            .date_naive();
        let last_date = comments
            .iter()
            .filter_map(Comment::date)
            .max()?
            .date_naive();

        Some(format!(
            "{} comments {first_id}-{last_id} ({first_date} to {last_date})",
            self.site_name
        ))
    }
}

impl CaptureProcessor for CommentCaptureProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        // A page can disappear between requests when deletions reduce the page count. WordPress
        // reports that as a 400 `rest_post_invalid_page_number`; treat it as the end of this sweep
        // and validate again from page one when the sweep found anything new.
        if capture.status == 400 && self.page > 1 {
            return self.finish_sweep();
        }

        if !matches!(capture.status, 200 | 304) {
            return Inspection {
                error: Some(format!(
                    "unexpected WordPress comments response status {} on page {}",
                    capture.status, self.page
                )),
                ..Inspection::default()
            };
        }

        // A revalidated recapture carries no batch and no fresh page count: the page is unchanged
        // since it was last read, so the sweep continues by the count last advertised.
        let revalidated = capture.status == 304;
        let comments = if revalidated {
            Vec::new()
        } else {
            let Ok(comments) = serde_json::from_slice::<Vec<Comment>>(capture.payload) else {
                return Inspection {
                    error: Some(format!(
                        "invalid WordPress comments response on page {}",
                        self.page
                    )),
                    ..Inspection::default()
                };
            };
            let Some(total_comments) = capture
                .header("x-wp-total")
                .and_then(|value| value.parse::<usize>().ok())
            else {
                return Inspection {
                    error: Some(format!(
                        "missing or invalid X-WP-Total on WordPress comments page {}",
                        self.page
                    )),
                    ..Inspection::default()
                };
            };
            if self
                .total_comments
                .is_some_and(|total| total != total_comments)
            {
                self.totals_consistent = false;
            }
            self.total_comments = Some(total_comments);
            self.total_pages = capture
                .header("x-wp-totalpages")
                .and_then(|value| value.parse::<usize>().ok());
            comments
        };

        let title = self.title(&comments);
        self.seen_ids
            .extend(comments.iter().map(|comment| comment.id));

        let has_next = self
            .total_pages
            .map_or(comments.len() == COMMENTS_PER_PAGE, |total| {
                self.page < total
            });
        let mut inspection = if has_next {
            self.page += 1;
            Inspection {
                recaptures: vec![self.comment_url(self.page)],
                ..Inspection::default()
            }
        } else {
            self.finish_sweep()
        };
        inspection.title = title;
        inspection
    }
}

/// The fields used from one `WordPress` REST API v2 comment.
#[derive(Deserialize)]
struct Comment {
    id: u64,
    date_gmt: String,
}

impl Comment {
    /// Parse the `WordPress` GMT timestamp, which is normally returned without a zone suffix.
    fn date(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.date_gmt)
            .map(|date| date.with_timezone(&Utc))
            .ok()
            .or_else(|| {
                NaiveDateTime::parse_from_str(&self.date_gmt, "%Y-%m-%dT%H:%M:%S")
                    .map(|date| date.and_utc())
                    .ok()
            })
    }
}

/// Render a `WordPress` REST API timestamp at whole-second UTC precision.
fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::CommentCaptureProcessor;
    use crate::session::{Capture, CaptureProcessor};

    const BEFORE: &str = "2026-08-20T00:00:00Z";

    fn timestamp(value: &str) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|date| date.with_timezone(&Utc))
            .expect("a test timestamp")
    }

    const EMPTY_PAGE: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 0\r\nX-WP-TotalPages: 1\r\n\r\n";
    const ONE_PAGE: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 100\r\nX-WP-TotalPages: 1\r\n\r\n";
    const TWO_PAGES: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 101\r\nX-WP-TotalPages: 2\r\n\r\n";
    const INVALID_PAGE: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\r\n";
    const NOT_MODIFIED: &[u8] = b"HTTP/1.1 304 Not Modified\r\n\r\n";

    fn capture<'a>(payload: &'a [u8], status: u16, response: &'a [u8]) -> Capture<'a> {
        Capture {
            url: "https://jihadwatch.org/wp-json/wp/v2/comments",
            final_url: "https://jihadwatch.org/wp-json/wp/v2/comments",
            status,
            payload,
            response,
            response_metadata: archivindex_warc::record::http::ResponseMetadata::parse(response)
                .expect("a complete test response"),
        }
    }

    #[test]
    fn first_url_uses_the_saved_snapshot_cutoff() {
        let processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org/", timestamp(BEFORE))
                .expect("a processor");
        let expected = "https://jihadwatch.org/wp-json/wp/v2/comments?\
            before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=1&per_page=100";

        assert_eq!(processor.first_comment_url(), expected);
        assert_eq!(processor.first_comment_url(), expected);
    }

    #[test]
    fn inspection_titles_a_batch_and_advances_by_page() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");
        let payload = br#"[
            {"id": 211416, "date_gmt": "2020-11-28T08:15:00"},
            {"id": 211420, "date_gmt": "2020-11-30T12:30:00"}
        ]"#;

        let inspection = processor.inspect(&capture(payload, 200, TWO_PAGES));

        assert_eq!(
            inspection.title.as_deref(),
            Some("jihadwatch.org comments 211416-211420 (2020-11-28 to 2020-11-30)")
        );
        assert_eq!(
            inspection.recaptures,
            ["https://jihadwatch.org/wp-json/wp/v2/comments?\
                before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100"]
        );
    }

    #[test]
    fn matching_total_finishes_after_one_complete_sweep() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");
        let page_one = serde_json::to_vec(
            &(1..=100)
                .map(|id| json!({"id": id, "date_gmt": "2020-11-30T12:30:00"}))
                .collect::<Vec<_>>(),
        )?;
        let page_two =
            serde_json::to_vec(&[json!({"id": 101, "date_gmt": "2020-11-30T12:30:00"})])?;

        assert_eq!(
            processor
                .inspect(&capture(&page_one, 200, TWO_PAGES))
                .recaptures,
            ["https://jihadwatch.org/wp-json/wp/v2/comments?\
                before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100"]
        );
        // A stable total matching the 101 distinct IDs makes the first sweep sufficient.
        assert_eq!(
            processor
                .inspect(&capture(&page_two, 200, TWO_PAGES))
                .recaptures,
            Vec::<String>::new()
        );
        assert_eq!(processor.seen_ids.len(), 101);

        Ok(())
    }

    #[test]
    fn deletion_that_removes_a_page_cannot_hide_the_shifted_comment()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");
        let original_page = serde_json::to_vec(
            &(1..=100)
                .map(|id| json!({"id": id, "date_gmt": "2020-11-30T12:30:00"}))
                .collect::<Vec<_>>(),
        )?;
        let shifted_page = serde_json::to_vec(
            &(2..=101)
                .map(|id| json!({"id": id, "date_gmt": "2020-11-30T12:30:00"}))
                .collect::<Vec<_>>(),
        )?;

        assert_eq!(
            processor
                .inspect(&capture(&original_page, 200, TWO_PAGES))
                .recaptures
                .len(),
            1
        );
        // ID 1 is deleted before page 2 is requested, reducing the collection to one page.
        assert_eq!(
            processor
                .inspect(&capture(b"{}", 400, INVALID_PAGE))
                .recaptures,
            [processor.first_comment_url()]
        );
        // The repeated first page now exposes ID 101, but the retained deleted ID means the
        // reported total still cannot account for every distinct ID observed.
        let validation = processor.inspect(&capture(&shifted_page, 200, ONE_PAGE));
        assert_eq!(processor.seen_ids.len(), 101);
        assert!(validation.error.is_some());

        Ok(())
    }

    #[test]
    fn revalidated_pages_continue_a_sweep_by_the_last_advertised_page_count()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor")
                .second_sweep(true);
        let page_one = serde_json::to_vec(
            &(1..=100)
                .map(|id| json!({"id": id, "date_gmt": "2020-11-30T12:30:00"}))
                .collect::<Vec<_>>(),
        )?;
        let page_two =
            serde_json::to_vec(&[json!({"id": 101, "date_gmt": "2020-11-30T12:30:00"})])?;
        let page_two_url = "https://jihadwatch.org/wp-json/wp/v2/comments?\
            before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100";

        processor.inspect(&capture(&page_one, 200, TWO_PAGES));
        processor.inspect(&capture(&page_two, 200, TWO_PAGES));

        // The validation sweep finds both pages unchanged: the first revalidated page still leads
        // to the second, and the second ends the sweep with nothing new to validate.
        let first = processor.inspect(&capture(b"", 304, NOT_MODIFIED));
        assert_eq!(first.title, None);
        assert_eq!(first.recaptures, [page_two_url]);
        assert_eq!(
            processor
                .inspect(&capture(b"", 304, NOT_MODIFIED))
                .recaptures,
            Vec::<String>::new()
        );
        assert_eq!(processor.seen_ids.len(), 101);

        Ok(())
    }

    #[test]
    fn malformed_batches_fail_but_empty_batches_finish() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");

        let malformed = processor.inspect(&capture(b"not json", 200, ONE_PAGE));
        assert!(malformed.error.is_some());
        assert_eq!(malformed.recaptures, Vec::<String>::new());

        let empty = processor.inspect(&capture(b"[]", 200, EMPTY_PAGE));
        assert_eq!(empty.error, None);
        assert_eq!(empty.title, None);
        assert_eq!(empty.links, Vec::<String>::new());
        assert_eq!(empty.recaptures, Vec::<String>::new());
    }

    #[test]
    fn total_mismatch_requests_one_second_sweep_then_fails_incomplete() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");
        let payload = br#"[{"id": 1, "date_gmt": "2020-11-30T12:30:00"}]"#;
        let response = b"HTTP/1.1 200 OK\r\nX-WP-Total: 2\r\nX-WP-TotalPages: 1\r\n\r\n";

        let first = processor.inspect(&capture(payload, 200, response));
        assert_eq!(first.recaptures, [processor.first_comment_url()]);
        assert_eq!(first.error, None);

        let second = processor.inspect(&capture(payload, 200, response));
        assert!(second.recaptures.is_empty());
        assert!(second.error.is_some());
    }

    #[test]
    fn missing_total_fails_the_traversal() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");
        let response = b"HTTP/1.1 200 OK\r\nX-WP-TotalPages: 1\r\n\r\n";

        let inspection = processor.inspect(&capture(b"[]", 200, response));

        assert!(inspection.error.is_some());
        assert!(inspection.recaptures.is_empty());
    }

    #[test]
    fn unexpected_status_fails_the_traversal() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");

        let inspection = processor.inspect(&capture(b"{}", 403, b"HTTP/1.1 403 Forbidden\r\n\r\n"));

        assert!(inspection.error.is_some());
        assert_eq!(inspection.recaptures, Vec::<String>::new());
    }

    #[test]
    fn a_path_base_is_the_wordpress_installation_root() {
        let processor = CommentCaptureProcessor::with_before(
            "https://example.com/blog?ignored=yes#fragment",
            timestamp(BEFORE),
        )
        .expect("a processor");

        assert!(
            processor
                .first_comment_url()
                .starts_with("https://example.com/blog/wp-json/wp/v2/comments?")
        );
    }
}
