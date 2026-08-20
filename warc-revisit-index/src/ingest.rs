//! Convenience ingestion from semantic WARC records.

use archivindex_warc::record::Record;
use archivindex_warc::record::extension::Extension;
use archivindex_warc::record::header::{ResponseHeader, RevisitHeader, RevisitProfile};
use rusqlite::Connection;

use crate::db::{insert_payload, lookup_payload, lookup_resource, update_resource};
use crate::{Error, Index, ResourceKey, ResourceStateUpdate, RevisitTarget, Transaction};

/// The changes made while indexing one WARC record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexRecordOutcome {
    /// A new canonical payload source was inserted.
    pub payload_inserted: bool,
    /// Resource state was inserted or updated.
    pub resource_updated: bool,
}

impl Index {
    /// Index one semantic WARC record.
    ///
    /// Payload-bearing HTTP `response` records establish canonical payloads. HTTP 200 responses
    /// update resource state. Revisit records never enter the canonical payload table:
    /// `identical-payload-digest` revisits resolve their resource state to an existing canonical
    /// source or their explicit `WARC-Refers-To` fields, while `server-not-modified` revisits
    /// preserve prior representation identity and merge only validators.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed WARC/HTTP metadata, unsupported or malformed digests,
    /// malformed persisted state, or a SQLite query failure.
    pub fn index_record<E: Extension>(
        &self,
        record: &Record<E>,
    ) -> Result<IndexRecordOutcome, Error> {
        index_record(&self.connection, record)
    }
}

impl Transaction<'_> {
    /// Index one semantic WARC record within this bulk transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed WARC/HTTP metadata, unsupported or malformed digests,
    /// malformed persisted state, or a SQLite query failure.
    pub fn index_record<E: Extension>(
        &self,
        record: &Record<E>,
    ) -> Result<IndexRecordOutcome, Error> {
        index_record(&self.transaction, record)
    }
}

fn index_record<E: Extension>(
    connection: &Connection,
    record: &Record<E>,
) -> Result<IndexRecordOutcome, Error> {
    match record {
        Record::Response { header, body } if body.starts_with(b"HTTP/") => {
            index_response(connection, record, header, body)
        }
        Record::Revisit { header, body } => index_revisit(connection, header, body),
        _ => Ok(IndexRecordOutcome::default()),
    }
}

fn index_response<E: Extension>(
    connection: &Connection,
    record: &Record<E>,
    header: &ResponseHeader<E>,
    body: &[u8],
) -> Result<IndexRecordOutcome, Error> {
    let metadata = http_metadata(body)?;
    let payload_digest = header.payload.payload_digest.clone();
    let payload_length = record
        .payload_bytes()
        .map_err(|error| Error::MalformedWarcPayload(error.to_string()))?
        .map(|payload| payload.len() as u64);

    let payload_inserted = if let (Some(payload_digest), Some(payload_length)) =
        (payload_digest.as_ref(), payload_length)
    {
        insert_payload(
            connection,
            &RevisitTarget {
                payload_digest: payload_digest.clone(),
                payload_length: Some(payload_length),
                record_id: header.core.record_id.clone(),
                target_uri: header.target_uri.clone(),
                warc_date: header.core.date,
            },
        )?
    } else {
        false
    };

    let resource_updated = if metadata.status == 200 {
        let key = ResourceKey::new(header.target_uri.clone());
        update_resource(
            connection,
            &key,
            ResourceStateUpdate::representation(
                metadata.etag,
                metadata.last_modified,
                payload_digest,
                Some(header.core.record_id.clone()),
                Some(header.core.date),
            ),
        )?
        .is_some()
    } else {
        false
    };

    Ok(IndexRecordOutcome {
        payload_inserted,
        resource_updated,
    })
}

fn index_revisit<E: Extension>(
    connection: &Connection,
    header: &RevisitHeader<E>,
    body: &[u8],
) -> Result<IndexRecordOutcome, Error> {
    let metadata = if body.is_empty() {
        HttpMetadata::default()
    } else if body.starts_with(b"HTTP/") {
        http_metadata(body)?
    } else {
        return Err(Error::MalformedHttpResponse(
            "revisit block is not an HTTP response head",
        ));
    };
    let key = ResourceKey::new(header.target_uri.clone());

    let resource_updated = match &header.profile {
        RevisitProfile::IdenticalPayloadDigest(_) => {
            let digest =
                header
                    .payload
                    .payload_digest
                    .as_ref()
                    .ok_or(Error::MalformedHttpResponse(
                        "identical-payload-digest revisit has no payload digest",
                    ))?;
            let canonical = lookup_payload(connection, digest)?;
            let (record_id, warc_date) = canonical.as_ref().map_or_else(
                || {
                    (
                        header
                            .refers_to
                            .clone()
                            .or_else(|| Some(header.core.record_id.clone())),
                        header.refers_to_date.or(Some(header.core.date)),
                    )
                },
                |target| (Some(target.record_id.clone()), Some(target.warc_date)),
            );

            update_resource(
                connection,
                &key,
                ResourceStateUpdate::representation(
                    metadata.etag,
                    metadata.last_modified,
                    Some(digest.clone()),
                    record_id,
                    warc_date,
                ),
            )?
            .is_some()
        }
        RevisitProfile::ServerNotModified(_) => {
            if lookup_resource(connection, &key)?.is_some() {
                update_resource(
                    connection,
                    &key,
                    ResourceStateUpdate::not_modified(metadata.etag, metadata.last_modified),
                )?
                .is_some()
            } else if header.refers_to.is_some() && header.refers_to_date.is_some() {
                update_resource(
                    connection,
                    &key,
                    ResourceStateUpdate::representation(
                        metadata.etag,
                        metadata.last_modified,
                        header.payload.payload_digest.clone(),
                        header.refers_to.clone(),
                        header.refers_to_date,
                    ),
                )?
                .is_some()
            } else {
                false
            }
        }
        RevisitProfile::Other(_) => false,
    };

    Ok(IndexRecordOutcome {
        payload_inserted: false,
        resource_updated,
    })
}

#[derive(Default)]
struct HttpMetadata {
    status: u16,
    etag: Option<String>,
    last_modified: Option<String>,
}

fn http_metadata(message: &[u8]) -> Result<HttpMetadata, Error> {
    let mut lines = message.split_inclusive(|&byte| byte == b'\n');
    let status_line = lines
        .next()
        .ok_or(Error::MalformedHttpResponse("missing status line"))?;
    let status_line = status_line
        .strip_suffix(b"\n")
        .unwrap_or(status_line)
        .strip_suffix(b"\r")
        .unwrap_or_else(|| status_line.strip_suffix(b"\n").unwrap_or(status_line));
    let mut parts = status_line.split(u8::is_ascii_whitespace);
    let version = parts
        .next()
        .filter(|part| part.starts_with(b"HTTP/"))
        .ok_or(Error::MalformedHttpResponse("invalid status line"))?;
    if version.len() <= b"HTTP/".len() {
        return Err(Error::MalformedHttpResponse("invalid HTTP version"));
    }
    let status = parts
        .find(|part| !part.is_empty())
        .and_then(|part| std::str::from_utf8(part).ok())
        .filter(|part| part.len() == 3 && part.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|part| part.parse().ok())
        .ok_or(Error::MalformedHttpResponse("invalid status code"))?;

    let mut metadata = HttpMetadata {
        status,
        ..HttpMetadata::default()
    };
    let mut terminated = false;
    for line in lines {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            terminated = true;
            break;
        }
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            return Err(Error::MalformedHttpResponse("malformed header field"));
        };
        let name = &line[..colon];
        let value = line[colon + 1..].trim_ascii();
        let value = std::str::from_utf8(value).ok().map(str::to_owned);
        if metadata.etag.is_none() && name.eq_ignore_ascii_case(b"etag") {
            metadata.etag = value;
        } else if metadata.last_modified.is_none() && name.eq_ignore_ascii_case(b"last-modified") {
            metadata.last_modified = value;
        }
    }
    if !terminated {
        return Err(Error::MalformedHttpResponse(
            "unterminated HTTP header section",
        ));
    }

    Ok(metadata)
}
