//! Capturing and reading `WordPress` REST API v2 resources.

pub mod read;

use std::collections::HashSet;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, TimeDelta, Utc};
use serde::Deserialize;
use url::Url;

use crate::session::{Capture, CaptureProcessor, Inspection};

/// The maximum number of comments `WordPress` permits one REST API request to return.
const COMMENTS_PER_PAGE: usize = 100;

/// Inspect batches from the `WordPress` REST API v2 comments endpoint.
///
/// The processor takes a snapshot cutoff when it is constructed. Start a crawl with
/// [`first_comment_url`](Self::first_comment_url), which requests comments in ascending GMT date
/// order up to that cutoff. Each successful batch is parsed as JSON and inspected for comment IDs
/// not seen in an earlier batch. When it finds any, the processor queues one more request whose
/// `after` cursor is one second before the latest new comment, preserving a small overlap at batch
/// boundaries. It stops when a batch contains no new IDs.
///
/// Malformed JSON or a response outside the expected `WordPress` comments shape yields no title or
/// next link, ending collection without panicking.
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
///     "wordpress-comments.wacz",
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
    latest: Option<DateTime<Utc>>,
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
    /// The request has no lower cursor and asks for the oldest comments first.
    #[must_use]
    pub fn first_comment_url(&self) -> String {
        self.comment_url(None)
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
            latest: None,
        })
    }

    /// Build a comments URL, retaining the snapshot cutoff on cursor requests.
    fn comment_url(&self, after: Option<DateTime<Utc>>) -> String {
        let before = format_timestamp(self.before);
        let query = after.map_or_else(
            || {
                format!(
                    "before={before}&orderby=date_gmt&order=asc&per_page={COMMENTS_PER_PAGE}"
                )
            },
            |after| {
                let after = format_timestamp(after);

                format!(
                    "after={after}&before={before}&orderby=date_gmt&order=asc&per_page={COMMENTS_PER_PAGE}"
                )
            },
        );

        let mut url = self.endpoint.clone();
        url.set_query(Some(&query));

        url.into()
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
        let Ok(comments) = serde_json::from_slice::<Vec<Comment>>(capture.payload) else {
            return Inspection::default();
        };

        let title = self.title(&comments);
        let latest_new = comments
            .iter()
            .filter(|comment| self.seen_ids.insert(comment.id))
            .filter_map(Comment::date)
            .max();

        let links = latest_new.map_or_else(Vec::new, |latest_new| {
            let latest = self
                .latest
                .map_or(latest_new, |latest| latest.max(latest_new));
            self.latest = Some(latest);

            let after = latest
                .checked_sub_signed(TimeDelta::seconds(1))
                .unwrap_or(latest);

            vec![self.comment_url(Some(after))]
        });

        Inspection { links, title }
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

    use super::CommentCaptureProcessor;
    use crate::session::{Capture, CaptureProcessor};

    const BEFORE: &str = "2026-08-20T00:00:00Z";

    fn timestamp(value: &str) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|date| date.with_timezone(&Utc))
            .expect("a test timestamp")
    }

    fn capture(payload: &[u8]) -> Capture<'_> {
        Capture {
            url: "https://jihadwatch.org/wp-json/wp/v2/comments",
            final_url: "https://jihadwatch.org/wp-json/wp/v2/comments",
            status: 200,
            payload,
        }
    }

    #[test]
    fn first_url_uses_the_saved_snapshot_cutoff() {
        let processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org/", timestamp(BEFORE))
                .expect("a processor");
        let expected = "https://jihadwatch.org/wp-json/wp/v2/comments?\
            before=2026-08-20T00:00:00Z&orderby=date_gmt&order=asc&per_page=100";

        assert_eq!(processor.first_comment_url(), expected);
        assert_eq!(processor.first_comment_url(), expected);
    }

    #[test]
    fn inspection_titles_a_batch_and_advances_with_an_overlapping_cursor() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");
        let payload = br#"[
            {"id": 211416, "date_gmt": "2020-11-28T08:15:00"},
            {"id": 211420, "date_gmt": "2020-11-30T12:30:00"}
        ]"#;

        let inspection = processor.inspect(&capture(payload));

        assert_eq!(
            inspection.title.as_deref(),
            Some("jihadwatch.org comments 211416-211420 (2020-11-28 to 2020-11-30)")
        );
        assert_eq!(
            inspection.links,
            ["https://jihadwatch.org/wp-json/wp/v2/comments?\
                after=2020-11-30T12:29:59Z&before=2026-08-20T00:00:00Z&\
                orderby=date_gmt&order=asc&per_page=100"]
        );
    }

    #[test]
    fn collection_stops_when_an_overlapping_batch_has_no_new_ids() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");
        let first = br#"[
            {"id": 1, "date_gmt": "2020-11-30T12:30:00"},
            {"id": 2, "date_gmt": "2020-11-30T12:30:01"}
        ]"#;
        let overlap_with_new = br#"[
            {"id": 2, "date_gmt": "2020-11-30T12:30:01"},
            {"id": 3, "date_gmt": "2020-11-30T12:31:00Z"}
        ]"#;
        let overlap_without_new = br#"[{"id": 3, "date_gmt": "2020-11-30T12:31:00Z"}]"#;

        assert_eq!(processor.inspect(&capture(first)).links.len(), 1);
        assert_eq!(
            processor.inspect(&capture(overlap_with_new)).links,
            ["https://jihadwatch.org/wp-json/wp/v2/comments?\
                after=2020-11-30T12:30:59Z&before=2026-08-20T00:00:00Z&\
                orderby=date_gmt&order=asc&per_page=100"]
        );
        assert_eq!(
            processor.inspect(&capture(overlap_without_new)).links,
            Vec::<String>::new()
        );
    }

    #[test]
    fn malformed_or_empty_batches_do_not_continue() {
        let mut processor =
            CommentCaptureProcessor::with_before("https://jihadwatch.org", timestamp(BEFORE))
                .expect("a processor");

        for payload in [&b"not json"[..], &b"[]"[..]] {
            let inspection = processor.inspect(&capture(payload));

            assert_eq!(inspection.title, None);
            assert_eq!(inspection.links, Vec::<String>::new());
        }
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
