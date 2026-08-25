//! Convenience ingestion from semantic WARC records.

use archivindex_warc::record::Record;
use archivindex_warc::record::extension::Extension;
use archivindex_warc::record::header::{RevisitHeader, RevisitProfile};
use archivindex_warc::record::http::ResponseMetadata;
use rusqlite::Connection;

use crate::db::Store;
use crate::db::{insert_payload, lookup_payload, lookup_resource, update_resource};
use crate::error::Error;
use crate::payload::RevisitTarget;
use crate::resource::{ResourceKey, ResourceStateUpdate};

/// The changes made while indexing one WARC record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexRecordOutcome {
    /// A new canonical payload source was inserted.
    pub payload_inserted: bool,
    /// Resource state was inserted or updated.
    pub resource_updated: bool,
}

impl<C> Store<C> {
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
        index_record(self.connection(), record)
    }
}

fn index_record<E: Extension>(
    connection: &Connection,
    record: &Record<E>,
) -> Result<IndexRecordOutcome, Error> {
    match record {
        Record::Response { body, .. } if body.starts_with(b"HTTP/") => {
            index_response(connection, record)
        }
        Record::Revisit { header, body } => index_revisit(connection, header, body),
        _ => Ok(IndexRecordOutcome::default()),
    }
}

fn index_response<E: Extension>(
    connection: &Connection,
    record: &Record<E>,
) -> Result<IndexRecordOutcome, Error> {
    let Record::Response { header, body } = record else {
        unreachable!("index_response is only called for response records");
    };

    // A truncated body is neither a representation nor a revisit target.
    if header.core.truncated.is_some() {
        return Ok(IndexRecordOutcome::default());
    }
    let metadata = http_metadata(body)?;
    let payload_digest = header.payload.payload_digest.clone();
    // Decode only transfer-coded bodies; otherwise the stored body gives the payload length.
    let payload_length = if metadata.transfer_encoded {
        record
            .payload_bytes()
            .map_err(Error::MalformedWarcPayload)?
            .map(|payload| payload.len() as u64)
    } else {
        Some((body.len() - metadata.body_offset) as u64)
    };

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
            ResourceStateUpdate::Representation {
                etag: metadata.etag,
                last_modified: metadata.last_modified,
                payload_digest,
                record_id: Some(header.core.record_id.clone()),
                warc_date: Some(header.core.date),
            },
        )?
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
            // A revisit without a canonical payload record cannot itself become the original.
            let (record_id, warc_date) = canonical.as_ref().map_or_else(
                || (header.refers_to.clone(), header.refers_to_date),
                |target| (Some(target.record_id.clone()), Some(target.warc_date)),
            );

            update_resource(
                connection,
                &key,
                ResourceStateUpdate::Representation {
                    etag: metadata.etag,
                    last_modified: metadata.last_modified,
                    payload_digest: Some(digest.clone()),
                    record_id,
                    warc_date,
                },
            )?
        }
        RevisitProfile::ServerNotModified(_) => {
            if lookup_resource(connection, &key)?.is_some() {
                update_resource(
                    connection,
                    &key,
                    ResourceStateUpdate::NotModified {
                        etag: metadata.etag,
                        last_modified: metadata.last_modified,
                    },
                )?
            } else if header.refers_to.is_some() && header.refers_to_date.is_some() {
                update_resource(
                    connection,
                    &key,
                    ResourceStateUpdate::Representation {
                        etag: metadata.etag,
                        last_modified: metadata.last_modified,
                        payload_digest: header.payload.payload_digest.clone(),
                        record_id: header.refers_to.clone(),
                        warc_date: header.refers_to_date,
                    },
                )?
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
    /// Where the stored body begins.
    body_offset: usize,
    /// Whether the head declares a `Transfer-Encoding`.
    transfer_encoded: bool,
}

fn http_metadata(message: &[u8]) -> Result<HttpMetadata, Error> {
    let metadata = ResponseMetadata::parse(message)
        .ok_or(Error::MalformedHttpResponse("invalid HTTP response head"))?;
    Ok(HttpMetadata {
        status: metadata.status,
        etag: metadata
            .header("etag")
            .and_then(|value| std::str::from_utf8(value).ok())
            .map(str::to_owned),
        last_modified: metadata
            .header("last-modified")
            .and_then(|value| std::str::from_utf8(value).ok())
            .map(str::to_owned),
        body_offset: metadata.body_offset,
        transfer_encoded: metadata.header("transfer-encoding").is_some(),
    })
}
