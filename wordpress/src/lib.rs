//! Capturing and reading `WordPress` REST API v2 resources.
//!
//! The [`archive`] module archives a site's collections endpoint by endpoint through an
//! `archivindex-archiver` session. [`CommentDriver`] captures a bounded window of a site's
//! comments, and the [`read`] module reads comments from the resulting WARC file and checks its
//! page coverage.
//!
//! # Modules
//!
//! * [`archive`]: archiving the supported collections a site exposes
//! * [`complete`]: capturing pages missing from an archived comment collection
//! * [`endpoint`]: names of REST API v2 collection endpoints
//! * [`lint`]: validating a collection archive's capture and pagination structure
//! * [`read`]: reading archived comments and checking page coverage
//! * [`resume`]: recovering an archive checkpoint from a WARC file
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod archive;
pub mod complete;
pub mod endpoint;
pub mod lint;
pub mod read;
pub mod resume;

#[cfg(test)]
mod strategies;

use std::collections::HashSet;

use archivindex_archiver::Error;
use archivindex_archiver::session::{Capture, Driver, Inspection, Request};
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde::Deserialize;
use url::Url;

/// The maximum number of items `WordPress` permits one collection request to return.
const PER_PAGE: usize = 100;

/// The REST API v2 comments collection, relative to the `WordPress` installation root.
const COMMENTS_ENDPOINT: &str = "wp-json/wp/v2/comments";

/// Error code returned by `WordPress` when a requested collection page no longer exists.
const INVALID_PAGE_ERROR_CODE: &str = "rest_post_invalid_page_number";

/// The explanation given when Cloudflare's managed challenge answers a request.
const CLOUDFLARE_CHALLENGE: &str = "Cloudflare requires an interactive browser challenge; \
     browser-derived clearance cookies are required";

/// Drive a session through the `WordPress` REST API v2 comments endpoint.
///
/// The driver takes a window of comment creation times when it is constructed. Its first request
/// is a seed for page one of the comments in ascending ID order within that window, also given by
/// [`first_comment_url`](Self::first_comment_url). It walks every page advertised by
/// `X-WP-TotalPages` and normally finishes after one sweep when the pagination headers are stable
/// and the number of visible comments does not exceed `X-WP-Total`. `WordPress` applies
/// per-comment read checks after its pagination query, so the reported total can legitimately
/// exceed the number of comments returned. A second sweep runs when the consistency checks fail,
/// or when explicitly requested with [`second_sweep`](Self::second_sweep). The fixed cutoff
/// prevents ordinary additions after construction from moving the snapshot. Already captured IDs
/// remain retained even if they are deleted during collection.
///
/// Every page after the first is requested via the preceding page, and a validation sweep repeats
/// pages already read, its page one requested via the page that ended the first sweep. A repeated
/// page the server answers with `304 Not Modified` adds no IDs, and the sweep continues by the
/// page count last advertised. Malformed JSON or an unexpected HTTP response makes the session
/// incomplete instead of silently ending pagination, and a failed capture ends the driver's
/// requests.
///
/// # Examples
///
/// ```no_run
/// use archivindex_archiver::config::Operator;
/// use archivindex_archiver::{Archiver, Config};
/// use archivindex_archiver::session::Session;
/// use archivindex_wordpress::CommentDriver;
/// use chrono::{TimeDelta, Utc};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let before = Utc::now();
/// let after = before - TimeDelta::days(1);
/// let driver = CommentDriver::for_window("https://example.com/", after, before)?;
/// let config = Config {
///     operator: Some(Operator {
///         name: "A. Archivist".to_owned(),
///         email: None,
///     }),
///     ..Config::default()
/// };
///
/// let summary = Session::new(
///     Archiver::new(config)?,
///     "wordpress-comments",
///     driver,
///     "wordpress-comments.warc",
/// )?
/// .run()?;
///
/// assert!(summary.is_complete());
/// # Ok(())
/// # }
/// ```
pub struct CommentDriver {
    endpoint: Url,
    site_name: String,
    after: Option<DateTime<Utc>>,
    before: DateTime<Utc>,
    seen_ids: HashSet<u64>,
    first_date: Option<NaiveDate>,
    last_date: Option<NaiveDate>,
    traversal: Traversal,
    force_second_sweep: bool,
}

#[derive(Clone)]
enum Traversal {
    Active(Sweep),
    Complete(Sweep),
    /// A capture failed, so the sweep it was part of cannot be finished.
    Failed(Sweep),
}

impl Traversal {
    const fn sweep(&self) -> &Sweep {
        match self {
            Self::Active(sweep) | Self::Complete(sweep) | Self::Failed(sweep) => sweep,
        }
    }

    const fn active_sweep_mut(&mut self) -> Option<&mut Sweep> {
        match self {
            Self::Active(sweep) => Some(sweep),
            Self::Complete(_) | Self::Failed(_) => None,
        }
    }

    const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    /// End an active traversal, as complete or as failed.
    fn end(&mut self, complete: bool) {
        if let Self::Active(sweep) = self {
            *self = if complete {
                Self::Complete(sweep.clone())
            } else {
                Self::Failed(sweep.clone())
            };
        }
    }
}

#[derive(Clone)]
struct Sweep {
    phase: SweepPhase,
    page: usize,
    total: Option<usize>,
    headers_consistent: bool,
    total_pages: Option<usize>,
}

#[derive(Clone, Copy)]
enum SweepPhase {
    Primary,
    /// A repeat of the first sweep's pages, whose page one is requested via page `after`.
    Validation {
        previous_total: Option<usize>,
        after: usize,
    },
}

impl Sweep {
    /// Begin the first sweep at page one.
    const fn first() -> Self {
        Self {
            phase: SweepPhase::Primary,
            page: 1,
            total: None,
            headers_consistent: true,
            total_pages: None,
        }
    }

    /// Begin a validation sweep, which re-traverses the same pages this sweep covered.
    const fn validation(&self) -> Self {
        Self {
            phase: SweepPhase::Validation {
                previous_total: self.effective_total(),
                after: self.page,
            },
            page: 1,
            total: None,
            headers_consistent: true,
            total_pages: self.total_pages,
        }
    }

    const fn effective_total(&self) -> Option<usize> {
        match (self.total, self.phase) {
            (Some(total), _)
            | (
                None,
                SweepPhase::Validation {
                    previous_total: Some(total),
                    ..
                },
            ) => Some(total),
            (None, _) => None,
        }
    }

    const fn is_primary(&self) -> bool {
        matches!(self.phase, SweepPhase::Primary)
    }

    const fn number(&self) -> usize {
        match self.phase {
            SweepPhase::Primary => 1,
            SweepPhase::Validation { .. } => 2,
        }
    }
}

impl CommentDriver {
    /// Create a comment driver restricted to a fixed update window.
    ///
    /// Every comments request carries `after` and `before` at whole-second UTC precision. The
    /// caller is responsible for choosing an `after` instant earlier than `before`. A base URL
    /// ending in a path is treated as the `WordPress` installation root, so
    /// `https://example.com/blog` targets `https://example.com/blog/wp-json/...`.
    ///
    /// # Errors
    ///
    /// Returns [`url::ParseError`] when `base_url` is not a URL.
    pub fn for_window(
        base_url: impl AsRef<str>,
        after: DateTime<Utc>,
        before: DateTime<Utc>,
    ) -> Result<Self, url::ParseError> {
        let mut driver = Self::with_before(base_url.as_ref(), before)?;
        driver.after = Some(after);

        Ok(driver)
    }

    /// The first comments URL: page one in ascending comment-ID order within the window.
    #[must_use]
    pub fn first_comment_url(&self) -> String {
        self.comment_url(1)
    }

    /// Construct a driver with an explicit snapshot cutoff.
    fn with_before(base_url: &str, before: DateTime<Utc>) -> Result<Self, url::ParseError> {
        let mut base = Url::parse(base_url)?;
        base.set_query(None);
        base.set_fragment(None);

        let path = format!("{}/", base.path().trim_end_matches('/'));
        base.set_path(&path);
        let endpoint = base.join(COMMENTS_ENDPOINT)?;
        let site_name = endpoint.host_str().unwrap_or(endpoint.as_str()).to_owned();

        Ok(Self {
            endpoint,
            site_name,
            after: None,
            before,
            seen_ids: HashSet::new(),
            first_date: None,
            last_date: None,
            traversal: Traversal::Active(Sweep::first()),
            force_second_sweep: false,
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
            total: self.sweep().effective_total()?,
            first_date: self.first_date,
            last_date: self.last_date,
            complete: self.traversal.is_complete(),
        })
    }

    const fn sweep(&self) -> &Sweep {
        self.traversal.sweep()
    }

    const fn active_sweep_mut(&mut self) -> &mut Sweep {
        self.traversal
            .active_sweep_mut()
            .expect("completed traversals are not inspected")
    }

    /// Build one page URL, retaining the window on every request.
    fn comment_url(&self, page: usize) -> String {
        let mut url = self.endpoint.clone();
        url.set_query(Some(&paging_query(self.after, self.before, page)));

        url.into()
    }

    /// Finish a sweep, optionally scheduling one validation sweep.
    fn finish_sweep(&mut self) -> Inspection {
        let total = self.sweep().effective_total();
        let count_is_plausible = total.is_some_and(|total| self.seen_ids.len() <= total);
        let snapshot_is_consistent = self.sweep().headers_consistent && count_is_plausible;
        if self.sweep().is_primary() && (self.force_second_sweep || !snapshot_is_consistent) {
            self.traversal = Traversal::Active(self.sweep().validation());
            return Inspection::default();
        }

        if !snapshot_is_consistent {
            return Inspection::error(format!(
                "WordPress reported {} comments after sweep {}, but {} distinct IDs were captured{}",
                total.map_or_else(|| "no total".to_owned(), |value| value.to_string()),
                self.sweep().number(),
                self.seen_ids.len(),
                if self.sweep().headers_consistent {
                    ""
                } else {
                    " and pagination headers changed during validation"
                }
            ));
        }

        self.traversal.end(true);
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

impl Driver for CommentDriver {
    fn next(&mut self) -> Option<Request> {
        let Traversal::Active(sweep) = &self.traversal else {
            return None;
        };
        let url = self.comment_url(sweep.page);

        Some(match (sweep.phase, sweep.page) {
            (SweepPhase::Primary, 1) => Request::seed(url),
            (SweepPhase::Validation { after, .. }, 1) => {
                Request::extra(url, self.comment_url(after))
            }
            (_, page) => Request::extra(url, self.comment_url(page - 1)),
        })
    }

    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        let Some(page) = self.traversal.active_sweep_mut().map(|sweep| sweep.page) else {
            return Inspection::default();
        };

        if is_cloudflare_challenge(capture) {
            return Inspection::error(CLOUDFLARE_CHALLENGE);
        }

        // A page can disappear between requests when deletions reduce the page count. Some
        // WordPress endpoints report that condition with this posts-controller error code; only
        // that specific 400 ends the sweep, while unrelated client errors fail the traversal.
        if capture.status == 400 && page > 1 && is_invalid_page_error(capture.payload) {
            self.active_sweep_mut().headers_consistent = false;
            return self.finish_sweep();
        }

        if !matches!(capture.status, 200 | 304) {
            return Inspection::error(format!(
                "unexpected WordPress comments response status {} on page {}",
                capture.status, page
            ));
        }

        // A revalidated repeated page carries no batch and no fresh page count: the page is
        // unchanged since it was last read, so the sweep continues by the count last advertised.
        let revalidated = capture.status == 304;
        let comments = if revalidated {
            Vec::new()
        } else {
            let Ok(comments) = serde_json::from_slice::<Vec<Comment>>(capture.payload) else {
                return Inspection::error(format!(
                    "invalid WordPress comments response on page {page}"
                ));
            };
            let Some(total_comments) = capture
                .header("x-wp-total")
                .and_then(|value| value.parse::<usize>().ok())
            else {
                return Inspection::error(format!(
                    "missing or invalid X-WP-Total on WordPress comments page {page}"
                ));
            };
            let total_pages = capture
                .header("x-wp-totalpages")
                .and_then(|value| value.parse::<usize>().ok());
            let sweep = self.active_sweep_mut();
            if sweep
                .effective_total()
                .is_some_and(|total| total != total_comments)
            {
                sweep.headers_consistent = false;
            }
            sweep.total = Some(total_comments);
            if sweep
                .total_pages
                .zip(total_pages)
                .is_some_and(|(previous, current)| previous != current)
            {
                sweep.headers_consistent = false;
            }
            sweep.total_pages = total_pages;
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
            .sweep()
            .total_pages
            .map_or(comments.len() == PER_PAGE, |total| page < total);
        let mut inspection = if has_next {
            self.active_sweep_mut().page = page + 1;
            Inspection::default()
        } else {
            self.finish_sweep()
        };
        inspection.title = title;
        inspection
    }

    fn failed(&mut self, _url: &str, _error: &Error) {
        self.traversal.end(false);
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
    /// Whether the driver completed a stable traversal of the snapshot.
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

/// Whether Cloudflare's managed challenge answered the request.
///
/// The challenge cannot be answered without a browser, and every further request would meet it
/// too, so a traversal ends rather than failing page by page.
fn is_cloudflare_challenge(capture: &Capture<'_>) -> bool {
    capture.status == 403 && capture.header("cf-mitigated") == Some("challenge")
}

/// Whether an error response says the requested collection page no longer exists.
fn is_invalid_page_error(payload: &[u8]) -> bool {
    serde_json::from_slice::<WordPressError>(payload)
        .is_ok_and(|error| error.code == INVALID_PAGE_ERROR_CODE)
}

/// The query requesting one page of a collection in ascending ID order within a time window.
fn paging_query(after: Option<DateTime<Utc>>, before: DateTime<Utc>, page: usize) -> String {
    let after = after
        .map(format_timestamp)
        .map_or_else(String::new, |after| format!("after={after}&"));

    format!(
        "{after}before={}&orderby=id&order=asc&page={page}&per_page={PER_PAGE}",
        format_timestamp(before)
    )
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
    use archivindex_archiver::Error;
    use archivindex_archiver::session::{Capture, Driver, Request};
    use chrono::Utc;
    use proptest::prelude::*;
    use serde_json::json;

    use super::{CommentDriver, CommentProgress, DateTime, bounds, format_timestamp};
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
        let driver = CommentDriver::with_before(base.as_str(), before).unwrap();
        let url = url::Url::parse(&driver.comment_url(page)).unwrap();

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

    fn capture<'a>(payload: &'a [u8], response: &'a [u8]) -> Capture<'a> {
        Capture::new(
            "https://example.com/wp-json/wp/v2/comments",
            "https://example.com/wp-json/wp/v2/comments",
            payload,
            response,
        )
        .expect("a complete test response")
    }

    /// A driver whose next request is `page`, via the page before it or `via` for page one.
    fn page_request(driver: &CommentDriver, page: usize, via: Option<&str>) -> Request {
        let via = via.map_or_else(|| driver.comment_url(page - 1), str::to_owned);

        Request::extra(driver.comment_url(page), via)
    }

    #[test]
    fn the_first_request_is_a_seed_for_the_saved_snapshot_cutoff() {
        let mut driver = CommentDriver::with_before("https://example.com/", timestamp(BEFORE))
            .expect("a driver");
        let expected = "https://example.com/wp-json/wp/v2/comments?\
            before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=1&per_page=100";

        assert_eq!(driver.first_comment_url(), expected);
        assert_eq!(driver.next(), Some(Request::seed(expected)));
    }

    #[test]
    fn a_failed_capture_ends_the_requests() {
        let mut driver = CommentDriver::with_before("https://example.com/", timestamp(BEFORE))
            .expect("a driver");
        let _ = driver.inspect(&capture(b"[]", TWO_PAGES));

        driver.failed(&driver.comment_url(2), &Error::MissingHost(String::new()));

        assert_eq!(driver.next(), None);
        assert!(driver.progress().is_some_and(|progress| !progress.complete));
    }

    #[test]
    fn update_window_is_sent_with_the_first_page() {
        let driver = CommentDriver::for_window(
            "https://example.com/",
            timestamp("2026-08-18T00:00:00Z"),
            timestamp(BEFORE),
        )
        .expect("a driver");
        let first = "https://example.com/wp-json/wp/v2/comments?\
            after=2026-08-18T00:00:00Z&before=2026-08-20T00:00:00Z&\
            orderby=id&order=asc&page=1&per_page=100";

        assert_eq!(driver.first_comment_url(), first);
    }

    #[test]
    fn a_cloudflare_challenge_ends_the_traversal_with_an_explanation() {
        let mut driver = CommentDriver::with_before("https://example.com/", timestamp(BEFORE))
            .expect("a driver");
        let response = b"HTTP/1.1 403 Forbidden\r\ncf-mitigated: challenge\r\n\r\n";

        let inspection = driver.inspect(&capture(b"", response));

        assert!(
            inspection
                .error
                .expect("a challenge should end the traversal")
                .contains("interactive browser challenge")
        );
    }

    #[test]
    fn inspection_titles_a_batch_and_advances_by_page() {
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");
        let payload = br#"[
            {"id": 211416, "date_gmt": "2020-11-28T08:15:00"},
            {"id": 211420, "date_gmt": "2020-11-30T12:30:00"}
        ]"#;

        let inspection = driver.inspect(&capture(payload, TWO_PAGES));

        assert_eq!(
            inspection.title.as_deref(),
            Some("example.com comments 211416-211420 (2020-11-28 to 2020-11-30)")
        );
        assert_eq!(
            driver.next(),
            Some(Request::extra(
                "https://example.com/wp-json/wp/v2/comments?\
                    before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100",
                driver.first_comment_url()
            ))
        );
        let progress = CommentProgress {
            downloaded: 2,
            total: 101,
            first_date: Some(timestamp("2020-11-28T00:00:00Z").date_naive()),
            last_date: Some(timestamp("2020-11-30T00:00:00Z").date_naive()),
            complete: false,
        };
        assert_eq!(driver.progress(), Some(progress));
        assert_eq!(
            progress.to_string(),
            "Downloaded 2 of 101 comments (2020-11-28 to 2020-11-30)"
        );
    }

    #[test]
    fn matching_total_finishes_after_one_complete_sweep() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");
        let page_one = serde_json::to_vec(
            &(1..=100)
                .map(|id| json!({"id": id, "date_gmt": "2020-11-30T12:30:00"}))
                .collect::<Vec<_>>(),
        )?;
        let page_two =
            serde_json::to_vec(&[json!({"id": 101, "date_gmt": "2020-11-30T12:30:00"})])?;

        let _ = driver.inspect(&capture(&page_one, TWO_PAGES));
        assert_eq!(driver.next(), Some(page_request(&driver, 2, None)));
        // Stable pagination headers make the first sweep sufficient.
        let _ = driver.inspect(&capture(&page_two, TWO_PAGES));
        assert_eq!(driver.next(), None);
        assert_eq!(driver.seen_ids.len(), 101);

        Ok(())
    }

    #[test]
    fn deletion_that_removes_a_page_cannot_hide_the_shifted_comment()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");
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

        let _ = driver.inspect(&capture(&original_page, TWO_PAGES));
        assert_eq!(driver.next(), Some(page_request(&driver, 2, None)));
        // ID 1 is deleted before page 2 is requested, reducing the collection to one page, so
        // the validation sweep begins via the page that vanished.
        let _ = driver.inspect(&capture(INVALID_PAGE_ERROR, BAD_REQUEST));
        assert_eq!(
            driver.next(),
            Some(page_request(&driver, 1, Some(&driver.comment_url(2))))
        );
        // The repeated first page now exposes ID 101, but the retained deleted ID means the
        // reported total still cannot account for every distinct ID observed.
        let validation = driver.inspect(&capture(&shifted_page, ONE_PAGE));
        assert_eq!(driver.seen_ids.len(), 101);
        assert!(validation.error.is_some());

        Ok(())
    }

    #[test]
    fn unrelated_bad_request_on_a_later_page_fails_the_traversal() {
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");
        let unrelated = br#"{
            "code": "rest_invalid_param",
            "message": "Invalid parameter(s): before",
            "data": {"status": 400}
        }"#;

        let _ = driver.inspect(&capture(b"[]", TWO_PAGES));
        assert_eq!(driver.next(), Some(page_request(&driver, 2, None)));

        let inspection = driver.inspect(&capture(unrelated, BAD_REQUEST));

        assert_eq!(
            inspection.error.as_deref(),
            Some("unexpected WordPress comments response status 400 on page 2")
        );
    }

    #[test]
    fn revalidated_pages_continue_a_sweep_by_the_last_advertised_page_count()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut driver = CommentDriver::with_before("https://example.com", timestamp(BEFORE))
            .expect("a driver")
            .second_sweep(true);
        let page_one = serde_json::to_vec(
            &(1..=100)
                .map(|id| json!({"id": id, "date_gmt": "2020-11-30T12:30:00"}))
                .collect::<Vec<_>>(),
        )?;
        let page_two =
            serde_json::to_vec(&[json!({"id": 101, "date_gmt": "2020-11-30T12:30:00"})])?;

        driver.inspect(&capture(&page_one, TWO_PAGES));
        driver.inspect(&capture(&page_two, TWO_PAGES));
        assert_eq!(
            driver.next(),
            Some(page_request(&driver, 1, Some(&driver.comment_url(2))))
        );

        // The validation sweep finds both pages unchanged: the first revalidated page still leads
        // to the second, and the second ends the sweep with nothing new to validate.
        let first = driver.inspect(&capture(b"", NOT_MODIFIED));
        assert_eq!(first.title, None);
        assert_eq!(driver.next(), Some(page_request(&driver, 2, None)));
        let _ = driver.inspect(&capture(b"", NOT_MODIFIED));
        assert_eq!(driver.next(), None);
        assert_eq!(driver.seen_ids.len(), 101);

        Ok(())
    }

    #[test]
    fn malformed_batches_fail_but_empty_batches_finish() {
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");

        let malformed = driver.inspect(&capture(b"not json", ONE_PAGE));
        assert!(malformed.error.is_some());

        let empty = driver.inspect(&capture(b"[]", EMPTY_PAGE));
        assert_eq!(empty.error, None);
        assert_eq!(empty.title, None);
        assert_eq!(driver.next(), None);
    }

    #[test]
    fn visibility_filtered_total_finishes_with_a_shortfall() {
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");
        let payload = br#"[{"id": 1, "date_gmt": "2020-11-30T12:30:00"}]"#;
        let response = b"HTTP/1.1 200 OK\r\nX-WP-Total: 2\r\nX-WP-TotalPages: 1\r\n\r\n";

        let inspection = driver.inspect(&capture(payload, response));
        assert_eq!(inspection.error, None);
        assert_eq!(driver.next(), None);

        let progress = driver.progress().expect("reported progress");
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
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");
        let payload = br#"[
            {"id": 1, "date_gmt": "2020-11-30T12:30:00"},
            {"id": 2, "date_gmt": "2020-11-30T12:30:00"}
        ]"#;
        let response = b"HTTP/1.1 200 OK\r\nX-WP-Total: 1\r\nX-WP-TotalPages: 1\r\n\r\n";

        let first = driver.inspect(&capture(payload, response));
        assert_eq!(first.error, None);
        assert_eq!(
            driver.next(),
            Some(page_request(&driver, 1, Some(&driver.first_comment_url())))
        );

        let second = driver.inspect(&capture(payload, response));
        assert!(second.error.is_some());
    }

    #[test]
    fn missing_total_fails_the_traversal() {
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");
        let response = b"HTTP/1.1 200 OK\r\nX-WP-TotalPages: 1\r\n\r\n";

        let inspection = driver.inspect(&capture(b"[]", response));

        assert!(inspection.error.is_some());
    }

    #[test]
    fn unexpected_status_fails_the_traversal() {
        let mut driver =
            CommentDriver::with_before("https://example.com", timestamp(BEFORE)).expect("a driver");

        let inspection = driver.inspect(&capture(b"{}", b"HTTP/1.1 403 Forbidden\r\n\r\n"));

        assert!(inspection.error.is_some());
    }

    #[test]
    fn a_path_base_is_the_wordpress_installation_root() {
        let driver = CommentDriver::with_before(
            "https://example.com/blog?ignored=yes#fragment",
            timestamp(BEFORE),
        )
        .expect("a driver");

        assert!(
            driver
                .first_comment_url()
                .starts_with("https://example.com/blog/wp-json/wp/v2/comments?")
        );
    }
}
