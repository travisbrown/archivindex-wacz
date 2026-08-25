//! Capturing and reading `WordPress` REST API v2 resources.

pub mod read;

use std::collections::HashSet;

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde::Deserialize;
use url::Url;

use crate::session::{Capture, CaptureProcessor, Inspection};

/// The maximum number of comments `WordPress` permits one REST API request to return.
const COMMENTS_PER_PAGE: usize = 100;

/// Error code returned by `WordPress` when a requested collection page no longer exists.
const INVALID_PAGE_ERROR_CODE: &str = "rest_post_invalid_page_number";

/// Inspect batches from the `WordPress` REST API v2 comments endpoint.
///
/// The processor takes a snapshot cutoff when it is constructed. Start a crawl with
/// [`first_comment_url`](Self::first_comment_url), which requests comments in ascending ID order up
/// to that cutoff. It walks every page advertised by `X-WP-TotalPages` and normally finishes after
/// one sweep when the pagination headers are stable and the number of visible comments does not
/// exceed `X-WP-Total`. `WordPress` applies per-comment read checks after its pagination query, so
/// the reported total can legitimately exceed the number of comments returned. A second sweep
/// runs when the consistency checks fail, or when explicitly requested with
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
///     "wordpress-comments.warc",
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
    first_date: Option<NaiveDate>,
    last_date: Option<NaiveDate>,
    sweep: Sweep,
    force_second_sweep: bool,
    complete: bool,
}

struct Sweep {
    number: usize,
    page: usize,
    total: Option<usize>,
    previous_total: Option<usize>,
    headers_consistent: bool,
    total_pages: Option<usize>,
}

impl Sweep {
    const fn first() -> Self {
        Self {
            number: 1,
            page: 1,
            total: None,
            previous_total: None,
            headers_consistent: true,
            total_pages: None,
        }
    }

    fn next(&self) -> Self {
        Self {
            number: self.number + 1,
            page: 1,
            total: None,
            previous_total: self.total.or(self.previous_total),
            headers_consistent: true,
            total_pages: self.total_pages,
        }
    }

    fn effective_total(&self) -> Option<usize> {
        self.total.or(self.previous_total)
    }
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
            first_date: None,
            last_date: None,
            sweep: Sweep::first(),
            force_second_sweep: false,
            complete: false,
        })
    }

    /// Request a validation sweep even when the first sweep's total is consistent.
    #[must_use]
    pub const fn second_sweep(mut self, enabled: bool) -> Self {
        self.force_second_sweep = enabled;
        self
    }

    /// Return progress through the current snapshot once `WordPress` has reported its total.
    #[must_use]
    pub fn progress(&self) -> Option<CommentProgress> {
        Some(CommentProgress {
            downloaded: self.seen_ids.len(),
            total: self.sweep.effective_total()?,
            first_date: self.first_date,
            last_date: self.last_date,
            complete: self.complete,
        })
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
        let total = self.sweep.effective_total();
        let count_is_plausible = total.is_some_and(|total| self.seen_ids.len() <= total);
        let snapshot_is_consistent = self.sweep.headers_consistent && count_is_plausible;
        if self.sweep.number == 1 && (self.force_second_sweep || !snapshot_is_consistent) {
            self.sweep = self.sweep.next();
            return Inspection::recapture(self.comment_url(1));
        }

        if !snapshot_is_consistent {
            return Inspection::error(format!(
                "WordPress reported {} comments after sweep {}, but {} distinct IDs were captured{}",
                total.map_or_else(|| "no total".to_owned(), |value| value.to_string()),
                self.sweep.number,
                self.seen_ids.len(),
                if self.sweep.headers_consistent {
                    ""
                } else {
                    " and pagination headers changed during validation"
                }
            ));
        }

        self.complete = true;
        Inspection::default()
    }

    /// Title a parsed comment batch by its ID and GMT date ranges.
    fn title(&self, comments: &[Comment]) -> Option<String> {
        let (first_id, last_id) = bounds(comments.iter().map(|comment| comment.id))?;
        let (first_date, last_date) = bounds(comments.iter().filter_map(Comment::date))?;

        Some(format!(
            "{} comments {first_id}-{last_id} ({} to {})",
            self.site_name,
            first_date.date_naive(),
            last_date.date_naive()
        ))
    }
}

impl CaptureProcessor for CommentCaptureProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        // A page can disappear between requests when deletions reduce the page count. Some
        // WordPress endpoints report that condition with this posts-controller error code; only
        // that specific 400 ends the sweep, while unrelated client errors fail the traversal.
        let invalid_page = capture.status == 400
            && self.sweep.page > 1
            && serde_json::from_slice::<WordPressError>(capture.payload)
                .is_ok_and(|error| error.code == INVALID_PAGE_ERROR_CODE);
        if invalid_page {
            self.sweep.headers_consistent = false;
            return self.finish_sweep();
        }

        if !matches!(capture.status, 200 | 304) {
            return Inspection::error(format!(
                "unexpected WordPress comments response status {} on page {}",
                capture.status, self.sweep.page
            ));
        }

        // A revalidated recapture carries no batch and no fresh page count: the page is unchanged
        // since it was last read, so the sweep continues by the count last advertised.
        let revalidated = capture.status == 304;
        let comments = if revalidated {
            Vec::new()
        } else {
            let Ok(comments) = serde_json::from_slice::<Vec<Comment>>(capture.payload) else {
                return Inspection::error(format!(
                    "invalid WordPress comments response on page {}",
                    self.sweep.page
                ));
            };
            let Some(total_comments) = capture
                .header("x-wp-total")
                .and_then(|value| value.parse::<usize>().ok())
            else {
                return Inspection::error(format!(
                    "missing or invalid X-WP-Total on WordPress comments page {}",
                    self.sweep.page
                ));
            };
            if self
                .sweep
                .effective_total()
                .is_some_and(|total| total != total_comments)
            {
                self.sweep.headers_consistent = false;
            }
            self.sweep.total = Some(total_comments);
            let total_pages = capture
                .header("x-wp-totalpages")
                .and_then(|value| value.parse::<usize>().ok());
            if self
                .sweep
                .total_pages
                .zip(total_pages)
                .is_some_and(|(previous, current)| previous != current)
            {
                self.sweep.headers_consistent = false;
            }
            self.sweep.total_pages = total_pages;
            comments
        };

        let title = self.title(&comments);
        for comment in &comments {
            if self.seen_ids.insert(comment.id)
                && let Some(date) = comment.date().map(|date| date.date_naive())
            {
                self.first_date = Some(self.first_date.map_or(date, |first| first.min(date)));
                self.last_date = Some(self.last_date.map_or(date, |last| last.max(date)));
            }
        }

        let has_next = self
            .sweep
            .total_pages
            .map_or(comments.len() == COMMENTS_PER_PAGE, |total| {
                self.sweep.page < total
            });
        let mut inspection = if has_next {
            self.sweep.page += 1;
            Inspection::recapture(self.comment_url(self.sweep.page))
        } else {
            self.finish_sweep()
        };
        inspection.title = title;
        inspection
    }
}

/// Aggregate progress through a `WordPress` comment snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommentProgress {
    /// Number of distinct comment IDs downloaded so far.
    pub downloaded: usize,
    /// Total comments reported by `WordPress` for the snapshot.
    pub total: usize,
    /// Earliest valid GMT date among downloaded comments.
    pub first_date: Option<NaiveDate>,
    /// Latest valid GMT date among downloaded comments.
    pub last_date: Option<NaiveDate>,
    /// Whether the processor completed a stable traversal of the snapshot.
    pub complete: bool,
}

impl CommentProgress {
    /// Number included in `X-WP-Total` but omitted from the completed public response pages.
    ///
    /// `WordPress` performs per-comment visibility checks after querying and paginating, so this
    /// difference ordinarily represents comments attached to posts the requester cannot read.
    #[must_use]
    pub const fn visibility_shortfall(self) -> Option<usize> {
        if self.complete && self.downloaded < self.total {
            Some(self.total - self.downloaded)
        } else {
            None
        }
    }
}

impl std::fmt::Display for CommentProgress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.visibility_shortfall().is_some() {
            write!(formatter, "Downloaded {} visible comments", self.downloaded)?;
            write!(
                formatter,
                " (WordPress reported {} before visibility filtering",
                self.total
            )?;
            if let (Some(first), Some(last)) = (self.first_date, self.last_date) {
                write!(formatter, "; {first} to {last}")?;
            }
            formatter.write_str(")")?;
        } else {
            write!(
                formatter,
                "Downloaded {} of {} comments",
                self.downloaded, self.total
            )?;
            if let (Some(first), Some(last)) = (self.first_date, self.last_date) {
                write!(formatter, " ({first} to {last})")?;
            }
        }
        Ok(())
    }
}

/// The fields used from one `WordPress` REST API v2 comment.
#[derive(Deserialize)]
struct Comment {
    id: u64,
    date_gmt: String,
}

/// The discriminator in a `WordPress` REST API error response.
#[derive(Deserialize)]
struct WordPressError {
    code: String,
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

/// The minimum and maximum of `items` in one pass, or `None` when there are none.
fn bounds<T: Copy + Ord>(mut items: impl Iterator<Item = T>) -> Option<(T, T)> {
    let first = items.next()?;
    Some(items.fold((first, first), |(min, max), item| {
        (min.min(item), max.max(item))
    }))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use proptest::prelude::*;
    use serde_json::json;

    use super::{CommentCaptureProcessor, CommentProgress, DateTime, bounds, format_timestamp};
    use crate::session::{Capture, CaptureProcessor};
    use crate::strategies;

    #[test_strategy::proptest]
    fn bounds_agree_with_the_iterator_extremes(items: Vec<i64>) {
        let expected = items.iter().copied().min().zip(items.iter().copied().max());

        prop_assert_eq!(bounds(items.into_iter()), expected);
    }

    #[test_strategy::proptest]
    fn timestamps_are_rendered_at_whole_second_utc_precision(
        #[strategy(strategies::datetime())] timestamp: DateTime<Utc>,
    ) {
        let rendered = format_timestamp(timestamp);
        let parsed = DateTime::parse_from_rfc3339(&rendered).unwrap();

        prop_assert!(rendered.ends_with('Z'));
        prop_assert_eq!(parsed.timestamp(), timestamp.timestamp());
        prop_assert_eq!(parsed.timestamp_subsec_nanos(), 0);
    }

    #[test_strategy::proptest]
    fn comment_urls_query_the_endpoint_of_the_site(
        #[strategy(strategies::url())] base: url::Url,
        #[strategy(strategies::datetime())] before: DateTime<Utc>,
        #[strategy(1..=100_usize)] page: usize,
    ) {
        let processor = CommentCaptureProcessor::with_before(base.as_str(), before).unwrap();
        let url = url::Url::parse(&processor.comment_url(page)).unwrap();

        prop_assert!(url.path().ends_with("/wp-json/wp/v2/comments"));

        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        prop_assert_eq!(query["before"].as_ref(), format_timestamp(before));
        prop_assert_eq!(query["page"].as_ref(), page.to_string());
        prop_assert_eq!(query["order"].as_ref(), "asc");
    }

    const BEFORE: &str = "2026-08-20T00:00:00Z";

    fn timestamp(value: &str) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|date| date.with_timezone(&Utc))
            .expect("a test timestamp")
    }

    const EMPTY_PAGE: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 0\r\nX-WP-TotalPages: 1\r\n\r\n";
    const ONE_PAGE: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 100\r\nX-WP-TotalPages: 1\r\n\r\n";
    const TWO_PAGES: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 101\r\nX-WP-TotalPages: 2\r\n\r\n";
    const BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\r\n";
    const INVALID_PAGE_ERROR: &[u8] = br#"{
        "code": "rest_post_invalid_page_number",
        "message": "The page number requested is larger than the number of pages available.",
        "data": {"status": 400}
    }"#;
    const NOT_MODIFIED: &[u8] = b"HTTP/1.1 304 Not Modified\r\n\r\n";

    fn capture<'a>(payload: &'a [u8], status: u16, response: &'a [u8]) -> Capture<'a> {
        Capture {
            url: "https://example.com/wp-json/wp/v2/comments",
            final_url: "https://example.com/wp-json/wp/v2/comments",
            status,
            payload,
            response,
            response_metadata: std::borrow::Cow::Owned(
                archivindex_warc::record::http::ResponseMetadata::parse(response)
                    .expect("a complete test response"),
            ),
        }
    }

    #[test]
    fn first_url_uses_the_saved_snapshot_cutoff() {
        let processor =
            CommentCaptureProcessor::with_before("https://example.com/", timestamp(BEFORE))
                .expect("a processor");
        let expected = "https://example.com/wp-json/wp/v2/comments?\
            before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=1&per_page=100";

        assert_eq!(processor.first_comment_url(), expected);
        assert_eq!(processor.first_comment_url(), expected);
    }

    #[test]
    fn inspection_titles_a_batch_and_advances_by_page() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor");
        let payload = br#"[
            {"id": 211416, "date_gmt": "2020-11-28T08:15:00"},
            {"id": 211420, "date_gmt": "2020-11-30T12:30:00"}
        ]"#;

        let inspection = processor.inspect(&capture(payload, 200, TWO_PAGES));

        assert_eq!(
            inspection.title.as_deref(),
            Some("example.com comments 211416-211420 (2020-11-28 to 2020-11-30)")
        );
        assert_eq!(
            inspection.recaptures,
            ["https://example.com/wp-json/wp/v2/comments?\
                before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100"]
        );
        let progress = CommentProgress {
            downloaded: 2,
            total: 101,
            first_date: Some(timestamp("2020-11-28T00:00:00Z").date_naive()),
            last_date: Some(timestamp("2020-11-30T00:00:00Z").date_naive()),
            complete: false,
        };
        assert_eq!(processor.progress(), Some(progress));
        assert_eq!(
            progress.to_string(),
            "Downloaded 2 of 101 comments (2020-11-28 to 2020-11-30)"
        );
    }

    #[test]
    fn matching_total_finishes_after_one_complete_sweep() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
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
            ["https://example.com/wp-json/wp/v2/comments?\
                before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100"]
        );
        // Stable pagination headers make the first sweep sufficient.
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
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
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
                .inspect(&capture(INVALID_PAGE_ERROR, 400, BAD_REQUEST))
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
    fn unrelated_bad_request_on_a_later_page_fails_the_traversal() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor");
        let unrelated = br#"{
            "code": "rest_invalid_param",
            "message": "Invalid parameter(s): before",
            "data": {"status": 400}
        }"#;

        let first = processor.inspect(&capture(b"[]", 200, TWO_PAGES));
        assert_eq!(first.recaptures.len(), 1);

        let inspection = processor.inspect(&capture(unrelated, 400, BAD_REQUEST));

        assert_eq!(inspection.recaptures, Vec::<String>::new());
        assert_eq!(
            inspection.error.as_deref(),
            Some("unexpected WordPress comments response status 400 on page 2")
        );
    }

    #[test]
    fn revalidated_pages_continue_a_sweep_by_the_last_advertised_page_count()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor")
                .second_sweep(true);
        let page_one = serde_json::to_vec(
            &(1..=100)
                .map(|id| json!({"id": id, "date_gmt": "2020-11-30T12:30:00"}))
                .collect::<Vec<_>>(),
        )?;
        let page_two =
            serde_json::to_vec(&[json!({"id": 101, "date_gmt": "2020-11-30T12:30:00"})])?;
        let page_two_url = "https://example.com/wp-json/wp/v2/comments?\
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
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
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
    fn visibility_filtered_total_finishes_with_a_shortfall() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor");
        let payload = br#"[{"id": 1, "date_gmt": "2020-11-30T12:30:00"}]"#;
        let response = b"HTTP/1.1 200 OK\r\nX-WP-Total: 2\r\nX-WP-TotalPages: 1\r\n\r\n";

        let inspection = processor.inspect(&capture(payload, 200, response));
        assert_eq!(inspection.recaptures, Vec::<String>::new());
        assert_eq!(inspection.error, None);

        let progress = processor.progress().expect("reported progress");
        assert!(progress.complete);
        assert_eq!(progress.visibility_shortfall(), Some(1));
        assert_eq!(
            progress.to_string(),
            "Downloaded 1 visible comments (WordPress reported 2 before visibility filtering; \
             2020-11-30 to 2020-11-30)"
        );
    }

    #[test]
    fn more_visible_ids_than_reported_are_validated_then_rejected() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor");
        let payload = br#"[
            {"id": 1, "date_gmt": "2020-11-30T12:30:00"},
            {"id": 2, "date_gmt": "2020-11-30T12:30:00"}
        ]"#;
        let response = b"HTTP/1.1 200 OK\r\nX-WP-Total: 1\r\nX-WP-TotalPages: 1\r\n\r\n";

        let first = processor.inspect(&capture(payload, 200, response));
        assert_eq!(first.recaptures, [processor.first_comment_url()]);
        assert_eq!(first.error, None);

        let second = processor.inspect(&capture(payload, 200, response));
        assert_eq!(second.recaptures, Vec::<String>::new());
        assert!(second.error.is_some());
    }

    #[test]
    fn missing_total_fails_the_traversal() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor");
        let response = b"HTTP/1.1 200 OK\r\nX-WP-TotalPages: 1\r\n\r\n";

        let inspection = processor.inspect(&capture(b"[]", 200, response));

        assert!(inspection.error.is_some());
        assert_eq!(inspection.recaptures, Vec::<String>::new());
    }

    #[test]
    fn unexpected_status_fails_the_traversal() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
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
