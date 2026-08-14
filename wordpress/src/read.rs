//! Reading comments captured from the `WordPress` REST API v2 from WARC files.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read};
use std::path::Path;

use archivindex_warc::io::read::{self as warc_read, WarcReader};
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::{Record, payload};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use serde_json::Value;

/// Comments read from an archive and any conflicting duplicate captures.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CommentReadResult {
    /// One complete JSON object per comment, sorted by numeric comment ID.
    ///
    /// When more than one object has the same ID, the first object encountered in the archive is
    /// retained here. Unequal later objects are reported in [`warnings`](Self::warnings).
    pub comments: Vec<Value>,
    /// Pairs of objects with the same comment ID but different content.
    pub warnings: Vec<CommentConflict>,
}

/// Two archived JSON objects that disagree about one comment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommentConflict {
    /// The shared `WordPress` comment ID.
    pub id: u64,
    /// The object encountered earlier in the archive.
    pub first: Value,
    /// The object encountered later in the archive.
    pub second: Value,
}

/// The site and instant from which a subsequent comments capture should overlap.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommentUpdateAnchor {
    /// Base URL of the archived `WordPress` installation.
    pub base_url: String,
    /// Latest valid comment datetime, or the archived request cutoff when no comment has one.
    pub datetime: DateTime<Utc>,
    /// Whether [`datetime`](Self::datetime) came from a comment rather than a request cutoff.
    pub from_comment: bool,
}

/// Coverage of the page range advertised by archived `WordPress` comments records.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CommentCompleteness {
    /// Greatest valid `X-WP-TotalPages` value on a qualifying record, when any was found.
    pub total_pages: Option<usize>,
    /// Valid advertised totals in encounter order, retaining each transition but not repetitions.
    pub advertised_page_totals: Vec<usize>,
    /// Distinct page numbers with a qualifying response, in ascending order.
    pub captured_pages: Vec<usize>,
}

/// Page coverage for one archived `WordPress` comments endpoint.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommentCollectionCompleteness {
    /// Comments endpoint shared by the records in [`coverage`](Self::coverage), without a query.
    pub endpoint: String,
    /// Coverage computed only from records targeting [`endpoint`](Self::endpoint).
    pub coverage: CommentCompleteness,
}

impl CommentCompleteness {
    /// Whether every page from one through [`total_pages`](Self::total_pages) was captured.
    ///
    /// An archive with no valid `X-WP-TotalPages` value is incomplete. A reported total of zero
    /// describes an empty range and is complete because the response carrying it was captured.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.total_pages.is_some_and(|total_pages| {
            let required = self
                .captured_pages
                .partition_point(|page| *page <= total_pages);

            required == total_pages
                && self.captured_pages[..required]
                    .iter()
                    .copied()
                    .eq(1..=total_pages)
        })
    }

    /// Whether `X-WP-TotalPages` changed between qualifying responses.
    #[must_use]
    pub fn advertised_total_changed(&self) -> bool {
        self.advertised_page_totals
            .windows(2)
            .any(|pair| pair[0] != pair[1])
    }

    /// Page numbers in the advertised range for which no qualifying response was found.
    pub fn missing_pages(&self) -> impl Iterator<Item = usize> + '_ {
        (1..=self.total_pages.unwrap_or(0))
            .filter(|page| self.captured_pages.binary_search(page).is_err())
    }

    /// Number of pages in the advertised range without a qualifying response.
    #[must_use]
    pub fn missing_page_count(&self) -> Option<usize> {
        self.total_pages.map(|total_pages| {
            let captured = self
                .captured_pages
                .partition_point(|page| *page <= total_pages);
            total_pages - captured
        })
    }
}

/// An error produced while reading archived `WordPress` comments.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The WARC file could not be opened.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A WARC record could not be parsed.
    #[error("invalid WARC file {path}")]
    Warc {
        /// The source WARC file's path.
        path: String,
        /// The parsing failure.
        #[source]
        source: warc_read::Error,
    },
    /// A successful comments response does not contain a valid HTTP message.
    #[error("invalid HTTP response for {url}")]
    InvalidResponse {
        /// The captured comments URL.
        url: String,
    },
    /// A successful comments response's HTTP entity body could not be extracted.
    #[error("invalid HTTP response payload for {url}")]
    Payload {
        /// The captured comments URL.
        url: String,
        /// The payload extraction failure.
        #[source]
        source: archivindex_warc::record::payload::Error,
    },
    /// A successful comments response is not a JSON array.
    #[error("invalid WordPress comments JSON for {url}")]
    Json {
        /// The captured comments URL.
        url: String,
        /// The JSON parsing failure.
        #[source]
        source: serde_json::Error,
    },
    /// A value in a comments response is not an object with an unsigned integer `id`.
    #[error("WordPress comment in {url} has no unsigned integer id")]
    MissingId {
        /// The captured comments URL.
        url: String,
    },
    /// No archived capture identifies the `WordPress` installation to update.
    #[error("comments WARC has no WordPress comments capture URL")]
    MissingUpdateUrl,
    /// No comment datetime or request `before` cutoff can anchor an update.
    #[error("comments WARC has no valid comment datetime or before cutoff")]
    MissingUpdateDatetime,
    /// A caller requiring one collection was given an archive containing several.
    #[error("comments WARC contains {0} WordPress comments endpoints; exactly one is required")]
    MultipleCommentCollections(usize),
}

/// Read all comments captured in a plain or gzip-compressed WARC file.
///
/// Successful HTTP responses whose target path ends in `/wp-json/wp/v2/comments` are parsed as
/// comment batches. Redirect responses and captures of other endpoints are ignored. The returned
/// comments are sorted by numeric ID and deduplicated, retaining the first archived object for each
/// ID. Every pair of unequal objects sharing an ID is included in the warnings.
pub fn read_comments(path: impl AsRef<Path>) -> Result<CommentReadResult, Error> {
    let path = path.as_ref();
    let display_path = path.display().to_string();
    let mut comments = CommentCollector::default();

    if is_gzip_file(path)? {
        collect_records(
            WarcReader::from_path_gzip(path)?,
            &display_path,
            &mut comments,
        )?;
    } else {
        collect_records(WarcReader::from_path(path)?, &display_path, &mut comments)?;
    }

    Ok(comments.finish())
}

/// Find the one site and latest datetime that anchor an incremental comments update.
///
/// This single-collection convenience function rejects a WARC containing several sites; use
/// [`find_comment_update_anchors`] for a multi-site archive.
pub fn find_comment_update_anchor(path: impl AsRef<Path>) -> Result<CommentUpdateAnchor, Error> {
    let mut anchors = find_comment_update_anchors(path)?;
    let count = anchors.len();
    match anchors.as_mut_slice() {
        [] => Err(Error::MissingUpdateUrl),
        [anchor] => Ok(anchor.clone()),
        _ => Err(Error::MultipleCommentCollections(count)),
    }
}

/// Find the latest incremental-update anchor independently for every site in a WARC.
///
/// For each site, the greatest valid `date_gmt` among its archived comments is preferred. Only
/// when that site has no comment datetime is the greatest valid `before` value on one of its
/// archived comments response or revisit URLs used. Anchors are returned in domain-name order.
pub fn find_comment_update_anchors(
    path: impl AsRef<Path>,
) -> Result<Vec<CommentUpdateAnchor>, Error> {
    let path = path.as_ref();
    let mut contexts = BTreeMap::new();
    let display_path = path.display().to_string();
    if is_gzip_file(path)? {
        collect_update_context(
            WarcReader::from_path_gzip(path)?,
            &display_path,
            &mut contexts,
        )?;
    } else {
        collect_update_context(WarcReader::from_path(path)?, &display_path, &mut contexts)?;
    }
    if contexts.is_empty() {
        return Err(Error::MissingUpdateUrl);
    }

    let mut anchors = contexts
        .into_iter()
        .map(|(base_url, context)| {
            let (datetime, from_comment) = context
                .latest_comment
                .map(|datetime| (datetime, true))
                .or_else(|| context.before.map(|datetime| (datetime, false)))
                .ok_or(Error::MissingUpdateDatetime)?;

            Ok(CommentUpdateAnchor {
                base_url,
                datetime,
                from_comment,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    anchors.sort_by(|left, right| {
        update_anchor_domain(left)
            .cmp(&update_anchor_domain(right))
            .then_with(|| left.base_url.cmp(&right.base_url))
    });

    Ok(anchors)
}

/// Check coverage of the page range advertised by a plain or gzip-compressed comments WARC.
///
/// A page is captured when a record targets the `WordPress` REST API v2 comments endpoint, holds an
/// HTTP 200 response, and has that page number in its target URI (or omits `page`, which means page
/// one). Both `response` and `revisit` records must additionally have
/// `WARC-Identified-Payload-Type` set to `application/json`. The greatest valid
/// `X-WP-TotalPages` value on a qualifying record determines the required range. Every transition
/// in that value is retained so callers can warn about pagination changes. Duplicate captures
/// satisfy the page only once. An archive without a valid advertised total is incomplete. This
/// single-collection convenience function rejects a WARC containing several endpoints; use
/// [`check_comment_collections`] for a multi-site archive.
pub fn check_comment_completeness(path: impl AsRef<Path>) -> Result<CommentCompleteness, Error> {
    let mut collections = check_comment_collections(path)?;
    let count = collections.len();
    match collections.as_mut_slice() {
        [] => Ok(CommentCompleteness::default()),
        [collection] => Ok(std::mem::take(&mut collection.coverage)),
        _ => Err(Error::MultipleCommentCollections(count)),
    }
}

/// Check page coverage independently for every comments endpoint in a WARC.
///
/// Collections are returned in domain-name order. Records for one site can never supply page
/// coverage or advertised totals for another, which makes this suitable for archives written by a
/// directory-based `update-comments` run. An archive without a qualifying capture returns an empty
/// vector.
pub fn check_comment_collections(
    path: impl AsRef<Path>,
) -> Result<Vec<CommentCollectionCompleteness>, Error> {
    let path = path.as_ref();
    let display_path = path.display().to_string();
    let mut coverage = BTreeMap::new();

    if is_gzip_file(path)? {
        collect_coverage(
            WarcReader::from_path_gzip(path)?,
            &display_path,
            &mut coverage,
        )?;
    } else {
        collect_coverage(WarcReader::from_path(path)?, &display_path, &mut coverage)?;
    }

    let mut collections = coverage
        .into_iter()
        .map(
            |(endpoint, coverage): (String, CoverageCollector)| CommentCollectionCompleteness {
                endpoint,
                coverage: coverage.finish(),
            },
        )
        .collect::<Vec<_>>();
    collections.sort_by(|left, right| {
        endpoint_domain(&left.endpoint)
            .cmp(&endpoint_domain(&right.endpoint))
            .then_with(|| left.endpoint.cmp(&right.endpoint))
    });

    Ok(collections)
}

pub(crate) fn is_gzip_file(path: &Path) -> Result<bool, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut magic = [0; 2];
    Ok(file.read(&mut magic)? == magic.len() && magic == [0x1f, 0x8b])
}

fn collect_records<R: BufRead>(
    reader: WarcReader<R>,
    path: &str,
    comments: &mut CommentCollector,
) -> Result<(), Error> {
    for record in reader.iter_records::<NoExtension>().records() {
        let record = record.map_err(|source| Error::Warc {
            path: path.to_owned(),
            source,
        })?;

        let Record::Response { header, body } = record else {
            continue;
        };
        let url = header.target_uri.into_string();

        if !is_comment_endpoint(&url) {
            continue;
        }

        let response = archivindex_warc::record::http::ResponseMetadata::parse(&body)
            .ok_or_else(|| Error::InvalidResponse { url: url.clone() })?;

        if !(200..300).contains(&response.status) {
            continue;
        }

        let entity = payload::entity_body(&body).map_err(|source| Error::Payload {
            url: url.clone(),
            source,
        })?;
        let batch =
            serde_json::from_slice::<Vec<Value>>(&entity).map_err(|source| Error::Json {
                url: url.clone(),
                source,
            })?;

        comments.extend(batch, &url)?;
    }
    Ok(())
}

fn collect_coverage<R: BufRead>(
    reader: WarcReader<R>,
    path: &str,
    collections: &mut BTreeMap<String, CoverageCollector>,
) -> Result<(), Error> {
    for record in reader.iter_records::<NoExtension>().records() {
        let record = record.map_err(|source| Error::Warc {
            path: path.to_owned(),
            source,
        })?;

        let Some((url, response)) = qualifying_comment_capture(&record) else {
            continue;
        };
        let Some(endpoint) = comment_endpoint(url) else {
            continue;
        };
        let coverage = collections.entry(endpoint).or_default();

        if let Some(page) = comment_page(url) {
            coverage.pages.insert(page);
        }
        if let Some(total_pages) = response
            .header("x-wp-totalpages")
            .and_then(|value| std::str::from_utf8(value).ok())
            .and_then(|value| value.trim().parse::<usize>().ok())
        {
            if coverage.advertised_page_totals.last() != Some(&total_pages) {
                coverage.advertised_page_totals.push(total_pages);
            }
            coverage.total_pages = Some(
                coverage
                    .total_pages
                    .map_or(total_pages, |previous| previous.max(total_pages)),
            );
        }
    }

    Ok(())
}

#[derive(Default)]
struct UpdateContext {
    latest_comment: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
}

fn collect_update_context<R: BufRead>(
    reader: WarcReader<R>,
    path: &str,
    contexts: &mut BTreeMap<String, UpdateContext>,
) -> Result<(), Error> {
    for record in reader.iter_records::<NoExtension>().records() {
        let record = record.map_err(|source| Error::Warc {
            path: path.to_owned(),
            source,
        })?;
        let (url, response_body) = match record {
            Record::Response { header, body } => (header.target_uri.into_string(), Some(body)),
            Record::Revisit { header, .. } => (header.target_uri.into_string(), None),
            _ => continue,
        };
        if !is_comment_endpoint(&url) {
            continue;
        }
        let Some(parsed) = url::Url::parse(&url).ok() else {
            continue;
        };
        let Some(base_url) = comment_base_url(parsed.clone()) else {
            continue;
        };
        let context = contexts.entry(base_url).or_default();
        if let Some(before) = query_datetime(&parsed, "before") {
            context.before = Some(context.before.map_or(before, |current| current.max(before)));
        }

        let Some(body) = response_body else {
            continue;
        };
        let response = archivindex_warc::record::http::ResponseMetadata::parse(&body)
            .ok_or_else(|| Error::InvalidResponse { url: url.clone() })?;
        if !(200..300).contains(&response.status) {
            continue;
        }
        let entity = payload::entity_body(&body).map_err(|source| Error::Payload {
            url: url.clone(),
            source,
        })?;
        let batch =
            serde_json::from_slice::<Vec<Value>>(&entity).map_err(|source| Error::Json {
                url: url.clone(),
                source,
            })?;
        for comment in batch {
            if comment.get("id").and_then(Value::as_u64).is_none() {
                return Err(Error::MissingId { url: url.clone() });
            }
            let Some(datetime) = comment
                .get("date_gmt")
                .and_then(Value::as_str)
                .and_then(parse_comment_datetime)
            else {
                continue;
            };
            context.latest_comment = Some(
                context
                    .latest_comment
                    .map_or(datetime, |current| current.max(datetime)),
            );
        }
    }

    Ok(())
}

fn update_anchor_domain(anchor: &CommentUpdateAnchor) -> String {
    url::Url::parse(&anchor.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_else(|| anchor.base_url.to_ascii_lowercase())
}

fn comment_base_url(mut url: url::Url) -> Option<String> {
    let path = url.path().trim_end_matches('/');
    let root = path.strip_suffix("/wp-json/wp/v2/comments")?;
    let root = if root.is_empty() {
        "/".to_owned()
    } else {
        format!("{root}/")
    };
    url.set_path(&root);
    url.set_query(None);
    url.set_fragment(None);

    Some(url.into())
}

fn query_datetime(url: &url::Url, name: &str) -> Option<DateTime<Utc>> {
    let mut values = url
        .query_pairs()
        .filter_map(|(key, value)| (key == name).then_some(value));
    let datetime = parse_comment_datetime(values.next()?.as_ref())?;

    values.next().is_none().then_some(datetime)
}

fn parse_comment_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
                .map(|date| date.and_utc())
                .ok()
        })
}

/// The target and HTTP metadata of a record that satisfies comments page coverage.
///
/// Completion planning uses this same predicate, so it cannot choose a paging template from a
/// record the coverage check would reject or overlook an acceptable revisit.
pub(crate) fn qualifying_comment_capture(
    record: &Record,
) -> Option<(&str, archivindex_warc::record::http::ResponseMetadata)> {
    let (url, payload, body) = match record {
        Record::Response { header, body } => {
            (header.target_uri.as_str(), &header.payload, body.as_slice())
        }
        Record::Revisit { header, body } => {
            (header.target_uri.as_str(), &header.payload, body.as_slice())
        }
        _ => return None,
    };
    if !is_comment_endpoint(url)
        || !payload
            .identified_payload_type
            .as_ref()
            .is_some_and(|media_type| media_type.is("application", "json"))
    {
        return None;
    }
    let response = archivindex_warc::record::http::ResponseMetadata::parse(body)?;

    (response.status == 200).then_some((url, response))
}

/// Whether a captured URL targets the comments collection endpoint (with any query string).
pub(crate) fn is_comment_endpoint(url: &str) -> bool {
    url.split_once('?')
        .map_or(url, |(path, _)| path)
        .trim_end_matches('/')
        .ends_with("/wp-json/wp/v2/comments")
}

/// The positive `page` query value, defaulting to the first page when it is absent.
pub(crate) fn comment_page(url: &str) -> Option<usize> {
    let url = url::Url::parse(url).ok()?;
    let mut values = url
        .query_pairs()
        .filter_map(|(name, value)| (name == "page").then_some(value));
    let page = match values.next() {
        Some(value) => value.parse::<usize>().ok().filter(|page| *page > 0)?,
        None => 1,
    };

    values.next().is_none().then_some(page)
}

fn comment_endpoint(url: &str) -> Option<String> {
    let mut url = url::Url::parse(url).ok()?;
    let path = url.path().trim_end_matches('/').to_owned();
    if !path.ends_with("/wp-json/wp/v2/comments") {
        return None;
    }
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);

    Some(url.into())
}

fn endpoint_domain(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_else(|| endpoint.to_ascii_lowercase())
}

#[derive(Default)]
struct CoverageCollector {
    total_pages: Option<usize>,
    advertised_page_totals: Vec<usize>,
    pages: BTreeSet<usize>,
}

impl CoverageCollector {
    fn finish(self) -> CommentCompleteness {
        CommentCompleteness {
            total_pages: self.total_pages,
            advertised_page_totals: self.advertised_page_totals,
            captured_pages: self.pages.into_iter().collect(),
        }
    }
}

/// Comments grouped by ID while archive records are being traversed.
#[derive(Default)]
struct CommentCollector {
    by_id: BTreeMap<u64, Vec<Value>>,
    warnings: Vec<CommentConflict>,
}

impl CommentCollector {
    /// Add a response batch, checking every new object against earlier objects with its ID.
    fn extend(&mut self, batch: Vec<Value>, url: &str) -> Result<(), Error> {
        for comment in batch {
            let id = comment
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| Error::MissingId {
                    url: url.to_owned(),
                })?;
            let versions = self.by_id.entry(id).or_default();

            if versions.contains(&comment) {
                continue;
            }

            for previous in versions.iter() {
                self.warnings.push(CommentConflict {
                    id,
                    first: previous.clone(),
                    second: comment.clone(),
                });
            }

            versions.push(comment);
        }

        Ok(())
    }

    /// Keep the first object for every ID; the map iteration supplies numeric ordering.
    fn finish(self) -> CommentReadResult {
        let comments = self
            .by_id
            .into_values()
            .filter_map(|versions| versions.into_iter().next())
            .collect();

        CommentReadResult {
            comments,
            warnings: self.warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use archivindex_warc::io::write::WarcWriter;
    use archivindex_warc::record::Record;
    use archivindex_warc::record::header::RevisitProfile;
    use archivindex_warc::record::header::truncated_type::TruncatedType;
    use archivindex_warc::value::{LabelledDigest, MediaType};
    use chrono::Utc;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use serde_json::json;

    use super::{
        CommentConflict, Error, check_comment_collections, check_comment_completeness,
        find_comment_update_anchor, find_comment_update_anchors, read_comments,
    };

    /// Write response records into a WARC fixture and return its temporary directory.
    fn fixture(
        responses: &[(&str, &str, &str)],
    ) -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("comments.warc");
        let mut warc_writer = WarcWriter::new(std::fs::File::create(&path)?);

        for (url, status, body) in responses {
            let message = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\n\r\n{body}",
                body.len()
            );
            let record: Record = Record::response(url, Utc::now())?.body(message.into_bytes())?;
            warc_writer.write(&record.into_raw()?)?;
        }
        warc_writer.flush()?;

        Ok((directory, path))
    }

    /// Write response records with configurable inferred types and advertised page counts.
    fn coverage_fixture(
        responses: &[(&str, &str, Option<&str>, Option<&str>)],
    ) -> Result<(tempfile::TempDir, std::path::PathBuf), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("comments.warc");
        let mut warc_writer = WarcWriter::new(std::fs::File::create(&path)?);

        for (url, status, inferred_type, total_pages) in responses {
            let total_pages = total_pages
                .map_or_else(String::new, |value| format!("x-wp-totalpages: {value}\r\n"));
            let message = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{total_pages}\
                 content-length: 2\r\n\r\n[]"
            );
            let mut builder = Record::response(url, Utc::now())?;
            if let Some(inferred_type) = inferred_type {
                builder =
                    builder.identified_payload_type(MediaType::parse(inferred_type.as_bytes())?);
            }
            let record: Record = builder.body(message.into_bytes())?;
            warc_writer.write(&record.into_raw()?)?;
        }
        warc_writer.flush()?;

        Ok((directory, path))
    }

    #[test]
    fn complete_archive_covers_every_advertised_page() -> Result<(), Box<dyn std::error::Error>> {
        let (directory, path) = coverage_fixture(&[
            (
                "https://example.com/wp-json/wp/v2/comments?before=x",
                "200 OK",
                Some("application/json"),
                Some("3"),
            ),
            (
                "https://example.com/wp-json/wp/v2/comments?page=3",
                "200 OK",
                Some("application/json"),
                Some("2"),
            ),
            (
                "https://example.com/wp-json/wp/v2/comments?page=2",
                "200 OK",
                Some("application/json"),
                Some("3"),
            ),
        ])?;

        let coverage = check_comment_completeness(&path)?;

        let gzip_path = directory.path().join("comments.warc.gz");
        let mut encoder =
            GzEncoder::new(std::fs::File::create(&gzip_path)?, Compression::default());
        std::io::copy(&mut std::fs::File::open(path)?, &mut encoder)?;
        encoder.finish()?;
        let gzip_coverage = check_comment_completeness(gzip_path)?;

        assert_eq!(coverage.total_pages, Some(3));
        assert_eq!(coverage.advertised_page_totals, [3, 2, 3]);
        assert_eq!(coverage.captured_pages, [1, 2, 3]);
        assert_eq!(
            coverage.missing_pages().collect::<Vec<_>>(),
            Vec::<usize>::new()
        );
        assert_eq!(coverage.missing_page_count(), Some(0));
        assert!(coverage.is_complete());
        assert!(coverage.advertised_total_changed());
        assert_eq!(gzip_coverage, coverage);

        Ok(())
    }

    #[test]
    fn multi_site_archive_checks_each_endpoint_independently()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = coverage_fixture(&[
            (
                "https://zeta.example/wp-json/wp/v2/comments?page=1",
                "200 OK",
                Some("application/json"),
                Some("3"),
            ),
            (
                "https://alpha.example/blog/wp-json/wp/v2/comments?page=1",
                "200 OK",
                Some("application/json"),
                Some("2"),
            ),
            (
                "https://zeta.example/wp-json/wp/v2/comments?page=3",
                "200 OK",
                Some("application/json"),
                Some("3"),
            ),
            (
                "https://alpha.example/blog/wp-json/wp/v2/comments?page=2",
                "200 OK",
                Some("application/json"),
                Some("2"),
            ),
        ])?;

        let collections = check_comment_collections(&path)?;

        assert_eq!(collections.len(), 2);
        assert_eq!(
            collections[0].endpoint,
            "https://alpha.example/blog/wp-json/wp/v2/comments"
        );
        assert_eq!(collections[0].coverage.total_pages, Some(2));
        assert_eq!(collections[0].coverage.captured_pages, [1, 2]);
        assert!(collections[0].coverage.is_complete());
        assert!(!collections[0].coverage.advertised_total_changed());
        assert_eq!(
            collections[1].endpoint,
            "https://zeta.example/wp-json/wp/v2/comments"
        );
        assert_eq!(collections[1].coverage.total_pages, Some(3));
        assert_eq!(collections[1].coverage.captured_pages, [1, 3]);
        assert_eq!(
            collections[1].coverage.missing_pages().collect::<Vec<_>>(),
            [2]
        );
        assert!(!collections[1].coverage.is_complete());
        assert!(!collections[1].coverage.advertised_total_changed());
        assert!(matches!(
            check_comment_completeness(path),
            Err(Error::MultipleCommentCollections(2))
        ));

        Ok(())
    }

    #[test]
    fn only_200_json_inferred_comment_responses_cover_pages()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = coverage_fixture(&[
            (
                "https://example.com/wp-json/wp/v2/comments?page=1",
                "200 OK",
                Some("application/json"),
                Some("3"),
            ),
            (
                "https://example.com/wp-json/wp/v2/comments?page=2",
                "200 OK",
                Some("text/plain"),
                Some("3"),
            ),
            (
                "https://example.com/wp-json/wp/v2/comments?page=3",
                "204 No Content",
                Some("application/json"),
                Some("3"),
            ),
            (
                "https://example.com/wp-json/wp/v2/posts?page=2",
                "200 OK",
                Some("application/json"),
                Some("3"),
            ),
        ])?;

        let coverage = check_comment_completeness(path)?;

        assert_eq!(coverage.total_pages, Some(3));
        assert_eq!(coverage.advertised_page_totals, [3]);
        assert_eq!(coverage.captured_pages, [1]);
        assert_eq!(coverage.missing_pages().collect::<Vec<_>>(), [2, 3]);
        assert_eq!(coverage.missing_page_count(), Some(2));
        assert!(!coverage.is_complete());
        assert!(!coverage.advertised_total_changed());

        Ok(())
    }

    #[test]
    fn only_an_inferred_json_200_revisit_covers_a_page() -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = coverage_fixture(&[(
            "https://example.com/wp-json/wp/v2/comments?page=1&per_page=100",
            "200 OK",
            Some("application/json"),
            Some("2"),
        )])?;
        let mut writer = WarcWriter::from_path(&path)?;
        let accepted: Record = Record::revisit(
            "https://example.com/wp-json/wp/v2/comments?page=2&per_page=100",
            Utc::now(),
            RevisitProfile::IDENTICAL_PAYLOAD_DIGEST,
        )?
        .content_type(MediaType::HTTP_RESPONSE)
        .identified_payload_type(MediaType::parse(b"application/json")?)
        .payload_digest(LabelledDigest::parse(b"sha1:AAAA")?)
        .truncated(TruncatedType::Length)
        .body(b"HTTP/1.1 200 OK\r\nx-wp-totalpages: 2\r\ncontent-length: 2\r\n\r\n".to_vec())?;
        writer.write(&accepted.into_raw()?)?;
        let missing_type: Record = Record::revisit(
            "https://example.com/wp-json/wp/v2/comments?page=3&per_page=100",
            Utc::now(),
            RevisitProfile::IDENTICAL_PAYLOAD_DIGEST,
        )?
        .content_type(MediaType::HTTP_RESPONSE)
        .payload_digest(LabelledDigest::parse(b"sha1:BBBB")?)
        .truncated(TruncatedType::Length)
        .body(b"HTTP/1.1 200 OK\r\nx-wp-totalpages: 3\r\ncontent-length: 2\r\n\r\n".to_vec())?;
        writer.write(&missing_type.into_raw()?)?;
        let wrong_status: Record = Record::revisit(
            "https://example.com/wp-json/wp/v2/comments?page=4&per_page=100",
            Utc::now(),
            RevisitProfile::SERVER_NOT_MODIFIED,
        )?
        .content_type(MediaType::HTTP_RESPONSE)
        .identified_payload_type(MediaType::parse(b"application/json")?)
        .body(
            b"HTTP/1.1 304 Not Modified\r\nx-wp-totalpages: 4\r\ncontent-length: 0\r\n\r\n"
                .to_vec(),
        )?;
        writer.write(&wrong_status.into_raw()?)?;
        writer.flush()?;

        let coverage = check_comment_completeness(path)?;

        assert_eq!(coverage.total_pages, Some(2));
        assert_eq!(coverage.captured_pages, [1, 2]);
        assert!(coverage.is_complete());

        Ok(())
    }

    #[test]
    fn archive_without_an_advertised_page_count_is_incomplete()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = coverage_fixture(&[(
            "https://example.com/wp-json/wp/v2/comments?page=1",
            "200 OK",
            Some("application/json"),
            None,
        )])?;

        let coverage = check_comment_completeness(path)?;

        assert_eq!(coverage.total_pages, None);
        assert!(coverage.advertised_page_totals.is_empty());
        assert_eq!(coverage.captured_pages, [1]);
        assert_eq!(coverage.missing_page_count(), None);
        assert!(!coverage.is_complete());
        assert!(!coverage.advertised_total_changed());

        Ok(())
    }

    #[test]
    fn comments_are_sorted_deduplicated_and_conflicts_are_reported()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = fixture(&[
            (
                "https://example.com/wp-json/wp/v2/comments?order=asc",
                "200 OK",
                r#"[{"id":2,"content":"two"},{"id":1,"content":"old"}]"#,
            ),
            (
                "https://example.com/wp-json/wp/v2/comments?after=x",
                "200 OK",
                r#"[{"id":1,"content":"old"},{"id":1,"content":"new"},{"id":1,"content":"newest"},{"id":3}]"#,
            ),
        ])?;

        let result = read_comments(path)?;

        assert_eq!(
            result.comments,
            [
                json!({"id": 1, "content": "old"}),
                json!({"id": 2, "content": "two"}),
                json!({"id": 3}),
            ]
        );
        assert_eq!(
            result.warnings,
            [
                CommentConflict {
                    id: 1,
                    first: json!({"id": 1, "content": "old"}),
                    second: json!({"id": 1, "content": "new"}),
                },
                CommentConflict {
                    id: 1,
                    first: json!({"id": 1, "content": "old"}),
                    second: json!({"id": 1, "content": "newest"}),
                },
                CommentConflict {
                    id: 1,
                    first: json!({"id": 1, "content": "new"}),
                    second: json!({"id": 1, "content": "newest"}),
                },
            ]
        );

        Ok(())
    }

    #[test]
    fn update_anchor_uses_the_latest_comment_datetime_and_installation_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = fixture(&[
            (
                "https://example.com/blog/wp-json/wp/v2/comments?before=2026-08-20T00:00:00Z&page=1",
                "200 OK",
                r#"[{"id":1,"date_gmt":"2026-08-18T12:00:00"}]"#,
            ),
            (
                "https://example.com/blog/wp-json/wp/v2/comments?before=2026-08-20T00:00:00Z&page=2",
                "200 OK",
                r#"[{"id":2,"date_gmt":"2026-08-19T13:14:15Z"}]"#,
            ),
        ])?;

        let anchor = find_comment_update_anchor(path)?;

        assert_eq!(anchor.base_url, "https://example.com/blog/");
        assert_eq!(anchor.datetime.to_rfc3339(), "2026-08-19T13:14:15+00:00");
        assert!(anchor.from_comment);

        Ok(())
    }

    #[test]
    fn empty_archive_uses_the_request_before_cutoff_as_its_update_anchor()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = fixture(&[
            (
                "https://example.com/wp-json/wp/v2/comments?before=2026-08-19T23:59:58Z&page=1",
                "200 OK",
                "[]",
            ),
            (
                "https://example.com/wp-json/wp/v2/comments?before=2026-08-20T00:00:00Z&page=2",
                "200 OK",
                "[]",
            ),
        ])?;

        let anchor = find_comment_update_anchor(path)?;

        assert_eq!(anchor.base_url, "https://example.com/");
        assert_eq!(anchor.datetime.to_rfc3339(), "2026-08-20T00:00:00+00:00");
        assert!(!anchor.from_comment);

        Ok(())
    }

    #[test]
    fn update_anchors_keep_comments_and_cutoffs_separate_by_site()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = fixture(&[
            (
                "https://zeta.example/wp-json/wp/v2/comments?before=2026-08-24T00:00:00Z&page=1",
                "200 OK",
                r#"[{"id":1,"date_gmt":"2026-08-21T00:00:00"}]"#,
            ),
            (
                "https://alpha.example/blog/wp-json/wp/v2/comments?before=2026-08-23T00:00:00Z&page=1",
                "200 OK",
                "[]",
            ),
            (
                "https://zeta.example/wp-json/wp/v2/comments?before=2026-08-25T00:00:00Z&page=2",
                "200 OK",
                r#"[{"id":2,"date_gmt":"2026-08-22T00:00:00"}]"#,
            ),
        ])?;

        let anchors = find_comment_update_anchors(&path)?;

        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].base_url, "https://alpha.example/blog/");
        assert_eq!(
            anchors[0].datetime.to_rfc3339(),
            "2026-08-23T00:00:00+00:00"
        );
        assert!(!anchors[0].from_comment);
        assert_eq!(anchors[1].base_url, "https://zeta.example/");
        assert_eq!(
            anchors[1].datetime.to_rfc3339(),
            "2026-08-22T00:00:00+00:00"
        );
        assert!(anchors[1].from_comment);
        assert!(matches!(
            find_comment_update_anchor(path),
            Err(Error::MultipleCommentCollections(2))
        ));

        Ok(())
    }

    #[test]
    fn compression_is_detected_from_content_not_the_extension()
    -> Result<(), Box<dyn std::error::Error>> {
        let (directory, path) = fixture(&[(
            "https://example.com/wp-json/wp/v2/comments",
            "200 OK",
            r#"[{"id":1}]"#,
        )])?;

        let gzip_path = directory.path().join("comments.data");
        let mut encoder =
            GzEncoder::new(std::fs::File::create(&gzip_path)?, Compression::default());
        std::io::copy(&mut std::fs::File::open(&path)?, &mut encoder)?;
        encoder.finish()?;

        let plain_gz_path = directory.path().join("comments.warc.gz");
        std::fs::copy(&path, &plain_gz_path)?;

        assert_eq!(read_comments(gzip_path)?.comments, [json!({"id": 1})]);
        assert_eq!(read_comments(plain_gz_path)?.comments, [json!({"id": 1})]);

        Ok(())
    }

    #[test]
    fn unrelated_redirect_and_failed_responses_are_ignored()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, path) = fixture(&[
            (
                "https://example.com/wp-json/wp/v2/comments",
                "301 Moved Permanently",
                "",
            ),
            (
                "https://example.com/wp-json/wp/v2/posts",
                "200 OK",
                r#"[{"id":10}]"#,
            ),
            (
                "https://example.com/wp-json/wp/v2/comments",
                "500 Server Error",
                r#"{"code":"error"}"#,
            ),
            (
                "https://example.com/wp-json/wp/v2/comments",
                "200 OK",
                r#"[{"id":11}]"#,
            ),
        ])?;

        let result = read_comments(path)?;

        assert_eq!(result.comments, [json!({"id": 11})]);
        assert_eq!(result.warnings, []);

        Ok(())
    }
}
