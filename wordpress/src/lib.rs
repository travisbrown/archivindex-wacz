//! Capturing and reading `WordPress` REST API v2 resources.
//!
//! [`CommentCaptureProcessor`] archives a site's paginated comments collection through an
//! `archivindex-archiver` crawl session. The [`read`] module reads comments from the resulting
//! WARC file and checks its page coverage.
//!
//! # Modules
//!
//! * [`complete`]: capturing pages missing from an archived comment collection
//! * [`read`]: reading archived comments and checking page coverage
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod complete;
pub mod read;

#[cfg(test)]
mod strategies;

use std::collections::HashSet;

use archivindex_archiver::session::{Capture, CaptureProcessor, Inspection};
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde::Deserialize;
use url::Url;

/// The maximum number of comments `WordPress` permits one REST API request to return.
const COMMENTS_PER_PAGE: usize = 100;

/// The REST API v2 comments collection, relative to the `WordPress` installation root.
const COMMENTS_ENDPOINT: &str = "wp-json/wp/v2/comments";

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
/// Each page is requested as a link from the preceding page, and a validation sweep repeats pages
/// already read, so the session must not skip repeated discoveries. A repeated page the server
/// answers with `304 Not Modified` adds no IDs, and the sweep continues by the page count last
/// advertised. Malformed JSON or an unexpected HTTP response makes the session incomplete instead
/// of silently ending pagination.
///
/// A session that stops early reports the page it had yet to request as unrequested;
/// [`resume`](Self::resume) continues the same snapshot from that page, which
/// [`first_comment_via`](Self::first_comment_via) links to the preceding page as a session extra.
///
/// # Examples
///
/// ```no_run
/// use archivindex_archiver::config::Operator;
/// use archivindex_archiver::{Archiver, Config};
/// use archivindex_archiver::session::Session;
/// use archivindex_wordpress::CommentCaptureProcessor;
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let processor = CommentCaptureProcessor::new("https://example.com/")?;
/// let first = processor.first_comment_url();
/// // A resumed run instead passes `(first, via)` to `Session::extras` and no seeds.
/// assert_eq!(processor.first_comment_via(), None);
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
///     [first],
///     "wordpress-comments.warc",
/// )?
/// // Validation sweeps repeat page URLs, which a deduplicating session would skip.
/// .dedupe_discoveries(false)
/// .processor(processor)
/// .run()?;
///
/// assert!(summary.is_complete());
/// // Another session continues from `summary.unrequested` when this one stopped early.
/// # Ok(())
/// # }
/// ```
pub struct CommentCaptureProcessor {
    endpoint: Url,
    site_name: String,
    after: Option<DateTime<Utc>>,
    before: DateTime<Utc>,
    seen_ids: HashSet<u64>,
    first_date: Option<NaiveDate>,
    last_date: Option<NaiveDate>,
    traversal: Traversal,
    /// Page this run begins with: one, or the page an earlier session left unrequested.
    first_page: usize,
    force_second_sweep: bool,
}

#[derive(Clone)]
enum Traversal {
    Active(Sweep),
    Complete(Sweep),
}

impl Traversal {
    const fn sweep(&self) -> &Sweep {
        match self {
            Self::Active(sweep) | Self::Complete(sweep) => sweep,
        }
    }

    const fn active_sweep_mut(&mut self) -> Option<&mut Sweep> {
        match self {
            Self::Active(sweep) => Some(sweep),
            Self::Complete(_) => None,
        }
    }

    const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    fn complete(&mut self) {
        if let Self::Active(sweep) = self {
            *self = Self::Complete(sweep.clone());
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
    Validation { previous_total: Option<usize> },
}

impl Sweep {
    /// Begin a first sweep at `page`: one, or the page a resumed run continues from.
    const fn starting_at(page: usize) -> Self {
        Self {
            phase: SweepPhase::Primary,
            page,
            total: None,
            headers_consistent: true,
            total_pages: None,
        }
    }

    /// Begin a validation sweep, which re-traverses the same pages this sweep covered.
    const fn validation(&self, page: usize) -> Self {
        Self {
            phase: SweepPhase::Validation {
                previous_total: self.effective_total(),
            },
            page,
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

impl CommentCaptureProcessor {
    /// Create a comment processor for a `WordPress` site's base URL.
    ///
    /// The current time is saved as the snapshot cutoff used by the first and every subsequent
    /// comments request. A base URL ending in a path is treated as the `WordPress` installation
    /// root, so `https://example.com/blog` targets `https://example.com/blog/wp-json/...`.
    pub fn new(base_url: impl AsRef<str>) -> Result<Self, url::ParseError> {
        Self::with_before(base_url.as_ref(), Utc::now())
    }

    /// Create a comment processor restricted to a fixed update window.
    ///
    /// Every comments request carries `after` and `before` at whole-second UTC precision. The
    /// caller is responsible for choosing an `after` instant earlier than `before`.
    pub fn for_window(
        base_url: impl AsRef<str>,
        after: DateTime<Utc>,
        before: DateTime<Utc>,
    ) -> Result<Self, url::ParseError> {
        let mut processor = Self::with_before(base_url.as_ref(), before)?;
        processor.after = Some(after);

        Ok(processor)
    }

    /// Resume a traversal at a comments URL an earlier session left unrequested or failed.
    ///
    /// The URL must target a `WordPress` comments endpoint with the standard ascending-ID
    /// pagination parameters this processor sends. Its cutoff restores the earlier session's fixed
    /// snapshot, so the resumed run continues through the same pages, beginning with this one.
    pub fn resume(url: impl AsRef<str>) -> Result<Self, CommentResumeError> {
        let mut endpoint = Url::parse(url.as_ref())?;
        if endpoint.fragment().is_some() {
            return Err(CommentResumeError::InvalidParameter("fragment"));
        }

        let parameters = ResumeParameters::parse(&endpoint)?;
        endpoint.set_query(None);
        if !endpoint
            .path()
            .strip_suffix(COMMENTS_ENDPOINT)
            .is_some_and(|root| root.ends_with('/'))
        {
            return Err(CommentResumeError::Endpoint(endpoint.into()));
        }

        Ok(Self::at(
            endpoint,
            parameters.after,
            parameters.before,
            parameters.page,
        ))
    }

    /// Produce the first comments URL for this processor's saved snapshot cutoff.
    ///
    /// Normally the request asks for page one in ascending comment-ID order; a
    /// [resumed](Self::resume) processor asks for the page it was given.
    #[must_use]
    pub fn first_comment_url(&self) -> String {
        self.comment_url(self.first_page)
    }

    /// The URL the first page was discovered on, when this run resumes after page one.
    ///
    /// A session records it as the `via` of the resumed page, continuing the chain of pages from
    /// the earlier session's WARC.
    #[must_use]
    pub fn first_comment_via(&self) -> Option<String> {
        (self.first_page > 1).then(|| self.comment_url(self.first_page - 1))
    }

    /// The comments endpoint this processor requests, without a query.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    /// Construct a processor with an explicit snapshot cutoff.
    fn with_before(base_url: &str, before: DateTime<Utc>) -> Result<Self, url::ParseError> {
        let mut base = Url::parse(base_url)?;
        base.set_query(None);
        base.set_fragment(None);

        let path = format!("{}/", base.path().trim_end_matches('/'));
        base.set_path(&path);

        Ok(Self::at(base.join(COMMENTS_ENDPOINT)?, None, before, 1))
    }

    /// Construct a processor for a comments endpoint, beginning its first sweep at `first_page`.
    fn at(
        endpoint: Url,
        after: Option<DateTime<Utc>>,
        before: DateTime<Utc>,
        first_page: usize,
    ) -> Self {
        let site_name = endpoint.host_str().unwrap_or(endpoint.as_str()).to_owned();

        Self {
            endpoint,
            site_name,
            after,
            before,
            seen_ids: HashSet::new(),
            first_date: None,
            last_date: None,
            traversal: Traversal::Active(Sweep::starting_at(first_page)),
            first_page,
            force_second_sweep: false,
        }
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
            first_page: self.first_page,
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

    /// Build one page URL, retaining the snapshot cutoff on every request.
    fn comment_url(&self, page: usize) -> String {
        let before = format_timestamp(self.before);
        let after = self
            .after
            .map(format_timestamp)
            .map_or_else(String::new, |after| format!("after={after}&"));
        let query = format!(
            "{after}before={before}&orderby=id&order=asc&page={page}&per_page={COMMENTS_PER_PAGE}"
        );

        let mut url = self.endpoint.clone();
        url.set_query(Some(&query));

        url.into()
    }

    /// Finish a sweep, optionally scheduling one validation sweep.
    fn finish_sweep(&mut self) -> Inspection {
        let total = self.sweep().effective_total();
        let count_is_plausible = total.is_some_and(|total| self.seen_ids.len() <= total);
        let snapshot_is_consistent = self.sweep().headers_consistent && count_is_plausible;
        if self.sweep().is_primary() && (self.force_second_sweep || !snapshot_is_consistent) {
            // A resumed run validates the pages it actually covered, not the whole snapshot.
            let start = self.first_page;
            self.traversal = Traversal::Active(self.sweep().validation(start));
            return next_page(self.comment_url(start));
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

        self.traversal.complete();
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

/// An invalid URL supplied to [`CommentCaptureProcessor::resume`].
#[derive(Debug, thiserror::Error)]
pub enum CommentResumeError {
    /// The supplied value is not a URL.
    #[error("invalid resume URL: {0}")]
    Url(#[from] url::ParseError),
    /// The URL does not target a `WordPress` comments endpoint.
    #[error("resume URL {0} does not target a WordPress REST API comments endpoint")]
    Endpoint(String),
    /// A required query parameter is missing or invalid, or an unsupported parameter is present.
    #[error("resume URL has a missing, invalid, duplicate, or unsupported {0}")]
    InvalidParameter(&'static str),
}

/// The pagination state recovered from a resume URL.
struct ResumeParameters {
    after: Option<DateTime<Utc>>,
    before: DateTime<Utc>,
    page: usize,
}

impl ResumeParameters {
    /// Read the query, requiring exactly the parameters [`CommentCaptureProcessor`] itself sends.
    ///
    /// Anything else would describe a different traversal, whose page numbers this processor cannot
    /// continue from.
    fn parse(url: &Url) -> Result<Self, CommentResumeError> {
        let mut after = None;
        let mut before = None;
        let mut page = None;
        let mut orderby = None;
        let mut order = None;
        let mut per_page = None;

        for (name, value) in url.query_pairs() {
            let slot = match name.as_ref() {
                "after" => &mut after,
                "before" => &mut before,
                "page" => &mut page,
                "orderby" => &mut orderby,
                "order" => &mut order,
                "per_page" => &mut per_page,
                _ => return Err(CommentResumeError::InvalidParameter("query parameter")),
            };
            // A repeated parameter leaves which value the server used ambiguous.
            if slot.replace(value).is_some() {
                return Err(CommentResumeError::InvalidParameter("query parameter"));
            }
        }

        let after = after
            .as_deref()
            .map(DateTime::parse_from_rfc3339)
            .transpose()
            .map_err(|_| CommentResumeError::InvalidParameter("after parameter"))?
            .map(|after| after.with_timezone(&Utc));
        let before = before
            .as_deref()
            .ok_or(CommentResumeError::InvalidParameter("before parameter"))?;
        let before = DateTime::parse_from_rfc3339(before)
            .map_err(|_| CommentResumeError::InvalidParameter("before parameter"))?
            .with_timezone(&Utc);
        let page = page
            .as_deref()
            .ok_or(CommentResumeError::InvalidParameter("page parameter"))?
            .parse::<usize>()
            .ok()
            .filter(|page| *page > 0)
            .ok_or(CommentResumeError::InvalidParameter("page parameter"))?;

        if orderby.as_deref() != Some("id") {
            return Err(CommentResumeError::InvalidParameter("orderby parameter"));
        }
        if order.as_deref() != Some("asc") {
            return Err(CommentResumeError::InvalidParameter("order parameter"));
        }
        if per_page
            .as_deref()
            .and_then(|value| value.parse::<usize>().ok())
            != Some(COMMENTS_PER_PAGE)
        {
            return Err(CommentResumeError::InvalidParameter("per_page parameter"));
        }

        if after.is_some_and(|after| after >= before) {
            return Err(CommentResumeError::InvalidParameter("after parameter"));
        }

        Ok(Self {
            after,
            before,
            page,
        })
    }
}

impl CaptureProcessor for CommentCaptureProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        if self.traversal.is_complete() {
            return Inspection::default();
        }

        let page = self.sweep().page;

        // Cloudflare's managed challenge cannot be answered without a browser, and every further
        // request would meet it too, so the traversal ends rather than failing page by page.
        if capture.status == 403 && capture.header("cf-mitigated") == Some("challenge") {
            return Inspection::error(
                "Cloudflare requires an interactive browser challenge; browser-derived clearance \
                 cookies are required",
            );
        }

        // A page can disappear between requests when deletions reduce the page count. Some
        // WordPress endpoints report that condition with this posts-controller error code; only
        // that specific 400 ends the sweep, while unrelated client errors fail the traversal.
        let invalid_page = capture.status == 400
            && page > 1
            && serde_json::from_slice::<WordPressError>(capture.payload)
                .is_ok_and(|error| error.code == INVALID_PAGE_ERROR_CODE);
        if invalid_page {
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
            .map_or(comments.len() == COMMENTS_PER_PAGE, |total| page < total);
        let mut inspection = if has_next {
            let next = page + 1;
            self.active_sweep_mut().page = next;
            next_page(self.comment_url(next))
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
    /// Page this run began with: one, unless it resumed an earlier session.
    pub first_page: usize,
}

impl CommentProgress {
    /// Number included in `X-WP-Total` but omitted from the completed public response pages.
    ///
    /// `WordPress` performs per-comment visibility checks after querying and paginating, so this
    /// difference ordinarily represents comments attached to posts the requester cannot read.
    #[must_use]
    pub const fn visibility_shortfall(self) -> Option<usize> {
        // A resumed run downloads only the snapshot's tail, so the difference from the reported
        // total says nothing about visibility.
        if self.first_page == 1 && self.complete && self.downloaded < self.total {
            Some(self.total - self.downloaded)
        } else {
            None
        }
    }
}

impl std::fmt::Display for CommentProgress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.first_page > 1 {
            write!(
                formatter,
                "Downloaded {} comments from page {} (WordPress reported {} total",
                self.downloaded, self.first_page, self.total
            )?;
            if let (Some(first), Some(last)) = (self.first_date, self.last_date) {
                write!(formatter, "; {first} to {last}")?;
            }
            formatter.write_str(")")?;
        } else if self.visibility_shortfall().is_some() {
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

/// Request one further page next, ahead of anything else waiting in the session.
fn next_page(url: String) -> Inspection {
    Inspection {
        links: vec![url],
        ..Inspection::default()
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
    use archivindex_archiver::capture::Origin;
    use archivindex_archiver::session::{Capture, CaptureProcessor};
    use chrono::Utc;
    use proptest::prelude::*;
    use serde_json::json;

    use super::{CommentCaptureProcessor, CommentProgress, DateTime, bounds, format_timestamp};
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

    fn capture<'a>(payload: &'a [u8], response: &'a [u8]) -> Capture<'a> {
        Capture::new(
            "https://example.com/wp-json/wp/v2/comments",
            "https://example.com/wp-json/wp/v2/comments",
            Origin::Seed,
            payload,
            response,
        )
        .expect("a complete test response")
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
        assert_eq!(processor.first_comment_via(), None);
        assert_eq!(
            processor.endpoint(),
            "https://example.com/wp-json/wp/v2/comments"
        );
    }

    #[test]
    fn update_window_is_retained_on_every_page_and_resume() -> Result<(), Box<dyn std::error::Error>>
    {
        let processor = CommentCaptureProcessor::for_window(
            "https://example.com/",
            timestamp("2026-08-18T00:00:00Z"),
            timestamp(BEFORE),
        )
        .expect("a processor");
        let first = "https://example.com/wp-json/wp/v2/comments?\
            after=2026-08-18T00:00:00Z&before=2026-08-20T00:00:00Z&\
            orderby=id&order=asc&page=1&per_page=100";

        assert_eq!(processor.first_comment_url(), first);

        let second = first.replace("&page=1&", "&page=2&");
        let resumed = CommentCaptureProcessor::resume(&second)?;
        assert_eq!(resumed.first_comment_url(), second);
        assert_eq!(resumed.first_comment_via().as_deref(), Some(first));

        Ok(())
    }

    #[test]
    fn resume_url_restores_the_cutoff_and_starts_with_that_page() {
        let url = "https://example.com/wp-json/wp/v2/comments?\
            before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=8904&per_page=100";
        let mut processor = CommentCaptureProcessor::resume(url).expect("a valid resume URL");

        assert_eq!(processor.first_comment_url(), url);
        assert_eq!(
            processor.first_comment_via().as_deref(),
            Some(
                "https://example.com/wp-json/wp/v2/comments?\
                 before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=8903&per_page=100"
            )
        );

        let payload = br#"[
            {"id": 1, "date_gmt": "2026-08-19T13:00:00"},
            {"id": 2, "date_gmt": "2026-08-19T13:01:00"}
        ]"#;
        let response = b"HTTP/1.1 200 OK\r\nX-WP-Total: 890302\r\nX-WP-TotalPages: 8904\r\n\r\n";
        let inspection = processor.inspect(&capture(payload, response));
        let progress = processor.progress().expect("reported progress");

        assert_eq!(inspection.links, [] as [String; 0]);
        // The tail this run downloaded says nothing about the visibility of earlier pages.
        assert_eq!(progress.visibility_shortfall(), None);
        assert_eq!(
            progress.to_string(),
            "Downloaded 2 comments from page 8904 (WordPress reported 890302 total; \
             2026-08-19 to 2026-08-19)"
        );
    }

    #[test]
    fn a_resumed_validation_sweep_restarts_at_the_resume_point() {
        let mut processor = CommentCaptureProcessor::resume(
            "https://example.com/wp-json/wp/v2/comments?\
             before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=5&per_page=100",
        )
        .expect("a valid resume URL")
        .second_sweep(true);

        let inspection = processor.inspect(&capture(b"[]", ONE_PAGE));

        assert_eq!(
            inspection.links,
            ["https://example.com/wp-json/wp/v2/comments?\
                before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=5&per_page=100"]
        );
    }

    #[test]
    fn resume_url_must_target_a_comments_endpoint_with_standard_pagination() {
        let query = "before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100";
        assert!(
            CommentCaptureProcessor::resume(format!(
                "https://other.example/blog/wp-json/wp/v2/comments?{query}"
            ))
            .is_ok()
        );

        let rejected = [
            format!("https://example.com/wp-json/wp/v2/posts?{query}"),
            format!("https://example.com/wp-json/wp/v2/comments/?{query}"),
            query.replace("order=asc", "order=desc"),
            query.replace("orderby=id", "orderby=date"),
            query.replace("per_page=100", "per_page=50"),
            query.replace("page=2", "page=0"),
            query.replace("&page=2", ""),
            query.replace("before=2026-08-20T00:00:00Z", "before=yesterday"),
            format!("{query}&search=cats"),
            format!("{query}&page=3"),
        ]
        .map(|query| {
            if query.starts_with("https://") {
                query
            } else {
                format!("https://example.com/wp-json/wp/v2/comments?{query}")
            }
        });

        for url in rejected {
            assert!(
                CommentCaptureProcessor::resume(&url).is_err(),
                "{url} should be rejected"
            );
        }
    }

    #[test]
    fn a_cloudflare_challenge_ends_the_traversal_with_an_explanation() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com/", timestamp(BEFORE))
                .expect("a processor");
        let response = b"HTTP/1.1 403 Forbidden\r\ncf-mitigated: challenge\r\n\r\n";

        let inspection = processor.inspect(&capture(b"", response));

        assert!(
            inspection
                .error
                .expect("a challenge should end the traversal")
                .contains("interactive browser challenge")
        );
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

        let inspection = processor.inspect(&capture(payload, TWO_PAGES));

        assert_eq!(
            inspection.title.as_deref(),
            Some("example.com comments 211416-211420 (2020-11-28 to 2020-11-30)")
        );
        assert_eq!(
            inspection.links,
            ["https://example.com/wp-json/wp/v2/comments?\
                before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100"]
        );
        let progress = CommentProgress {
            downloaded: 2,
            total: 101,
            first_date: Some(timestamp("2020-11-28T00:00:00Z").date_naive()),
            last_date: Some(timestamp("2020-11-30T00:00:00Z").date_naive()),
            complete: false,
            first_page: 1,
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
            processor.inspect(&capture(&page_one, TWO_PAGES)).links,
            ["https://example.com/wp-json/wp/v2/comments?\
                before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=2&per_page=100"]
        );
        // Stable pagination headers make the first sweep sufficient.
        assert_eq!(
            processor.inspect(&capture(&page_two, TWO_PAGES)).links,
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
                .inspect(&capture(&original_page, TWO_PAGES))
                .links
                .len(),
            1
        );
        // ID 1 is deleted before page 2 is requested, reducing the collection to one page.
        assert_eq!(
            processor
                .inspect(&capture(INVALID_PAGE_ERROR, BAD_REQUEST))
                .links,
            [processor.first_comment_url()]
        );
        // The repeated first page now exposes ID 101, but the retained deleted ID means the
        // reported total still cannot account for every distinct ID observed.
        let validation = processor.inspect(&capture(&shifted_page, ONE_PAGE));
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

        let first = processor.inspect(&capture(b"[]", TWO_PAGES));
        assert_eq!(first.links.len(), 1);

        let inspection = processor.inspect(&capture(unrelated, BAD_REQUEST));

        assert_eq!(inspection.links, Vec::<String>::new());
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

        processor.inspect(&capture(&page_one, TWO_PAGES));
        processor.inspect(&capture(&page_two, TWO_PAGES));

        // The validation sweep finds both pages unchanged: the first revalidated page still leads
        // to the second, and the second ends the sweep with nothing new to validate.
        let first = processor.inspect(&capture(b"", NOT_MODIFIED));
        assert_eq!(first.title, None);
        assert_eq!(first.links, [page_two_url]);
        assert_eq!(
            processor.inspect(&capture(b"", NOT_MODIFIED)).links,
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

        let malformed = processor.inspect(&capture(b"not json", ONE_PAGE));
        assert!(malformed.error.is_some());
        assert_eq!(malformed.links, Vec::<String>::new());

        let empty = processor.inspect(&capture(b"[]", EMPTY_PAGE));
        assert_eq!(empty.error, None);
        assert_eq!(empty.title, None);
        assert_eq!(empty.links, Vec::<String>::new());
    }

    #[test]
    fn visibility_filtered_total_finishes_with_a_shortfall() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor");
        let payload = br#"[{"id": 1, "date_gmt": "2020-11-30T12:30:00"}]"#;
        let response = b"HTTP/1.1 200 OK\r\nX-WP-Total: 2\r\nX-WP-TotalPages: 1\r\n\r\n";

        let inspection = processor.inspect(&capture(payload, response));
        assert_eq!(inspection.links, Vec::<String>::new());
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

        let first = processor.inspect(&capture(payload, response));
        assert_eq!(first.links, [processor.first_comment_url()]);
        assert_eq!(first.error, None);

        let second = processor.inspect(&capture(payload, response));
        assert_eq!(second.links, Vec::<String>::new());
        assert!(second.error.is_some());
    }

    #[test]
    fn missing_total_fails_the_traversal() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor");
        let response = b"HTTP/1.1 200 OK\r\nX-WP-TotalPages: 1\r\n\r\n";

        let inspection = processor.inspect(&capture(b"[]", response));

        assert!(inspection.error.is_some());
        assert_eq!(inspection.links, Vec::<String>::new());
    }

    #[test]
    fn unexpected_status_fails_the_traversal() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://example.com", timestamp(BEFORE))
                .expect("a processor");

        let inspection = processor.inspect(&capture(b"{}", b"HTTP/1.1 403 Forbidden\r\n\r\n"));

        assert!(inspection.error.is_some());
        assert_eq!(inspection.links, Vec::<String>::new());
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
