//! Reading comments captured from the `WordPress` REST API v2 from WARC files.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use archivindex_warc::io::read::{self as warc_read, WarcReader};
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::{Record, payload};
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

    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gz"))
    {
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

fn collect_records<R: BufRead>(
    reader: WarcReader<R>,
    path: &str,
    comments: &mut CommentCollector,
) -> Result<(), Error> {
    for record in reader.iter_records::<NoExtension>() {
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

/// Whether a captured URL targets the comments collection endpoint (with any query string).
fn is_comment_endpoint(url: &str) -> bool {
    url.split_once('?')
        .map_or(url, |(path, _)| path)
        .trim_end_matches('/')
        .ends_with("/wp-json/wp/v2/comments")
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
    use chrono::Utc;
    use serde_json::json;

    use super::{CommentConflict, read_comments};

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
