use std::error::Error as StdError;

use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::header::RevisitProfile;
use archivindex_warc::record::header::truncated_type::TruncatedType;
use archivindex_warc::value::{Algorithm, LabelledDigest, WarcDate};
use archivindex_warc_revisit_index::db::Index;
use archivindex_warc_revisit_index::error::Error;
use archivindex_warc_revisit_index::payload::RevisitTarget;
use archivindex_warc_revisit_index::resource::{ResourceKey, ResourceStateUpdate};
use fluent_uri::Uri;
use sha2::Digest as _;

const URI_A: &str = "https://example.com/a";
const URI_B: &str = "https://example.com/b";
const RECORD_A: &str = "urn:uuid:00000000-0000-4000-8000-00000000000a";
const RECORD_B: &str = "urn:uuid:00000000-0000-4000-8000-00000000000b";

fn uri(value: &str) -> Uri<String> {
    Uri::parse(value).expect("test URI").to_owned()
}

fn date(value: &str) -> WarcDate {
    WarcDate::parse(value, archivindex_warc::version::WarcVersion::V1_1).expect("test WARC date")
}

fn sha256(bytes: &[u8]) -> LabelledDigest {
    LabelledDigest::from_digest(Algorithm::Sha256, &sha2::Sha256::digest(bytes))
}

fn target(
    digest: LabelledDigest,
    record_id: &str,
    target_uri: &str,
    warc_date: &str,
    payload_length: Option<u64>,
) -> RevisitTarget {
    RevisitTarget {
        payload_digest: digest,
        payload_length,
        record_id: uri(record_id),
        target_uri: uri(target_uri),
        warc_date: date(warc_date),
    }
}

fn key(value: &str) -> ResourceKey {
    ResourceKey::new(uri(value))
}

fn response(
    target_uri: &str,
    record_id: &str,
    warc_date: &str,
    headers: &str,
    payload: &[u8],
) -> Result<Record, Box<dyn StdError>> {
    let mut message = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{headers}\r\n",
        payload.len()
    )
    .into_bytes();
    message.extend_from_slice(payload);

    Ok(
        Record::<NoExtension>::response(target_uri, date(warc_date))?
            .record_id(uri(record_id))
            .payload_digest(sha256(payload))
            .body(message)?,
    )
}

#[test]
fn payload_round_trips_every_field_and_missing_is_none() -> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let digest = LabelledDigest::from_digest(Algorithm::Sha256, &[0xa5; 32]);
    let expected = target(
        digest.clone(),
        RECORD_A,
        URI_A,
        "2025-02-03T04:05:06.123Z",
        Some(4_294_967_300),
    );

    assert!(index.lookup_payload(&sha256(b"missing"))?.is_none());
    assert!(index.insert_payload(&expected)?);
    assert_eq!(index.lookup_payload(&digest)?, Some(expected));
    Ok(())
}

#[test]
fn payload_persists_across_reopening() -> Result<(), Box<dyn StdError>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crawl-state.sqlite3");
    let expected = target(
        sha256(b"persistent"),
        RECORD_A,
        URI_A,
        "2025-01-01T00:00:00Z",
        Some(10),
    );
    Index::open(&path)?.insert_payload(&expected)?;

    assert_eq!(
        Index::open(&path)?.lookup_payload(&expected.payload_digest)?,
        Some(expected)
    );
    Ok(())
}

#[test]
fn duplicate_payload_insert_is_idempotent_and_preserves_canonical_source()
-> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let digest = sha256(b"same");
    let canonical = target(
        digest.clone(),
        RECORD_A,
        URI_A,
        "2025-01-01T00:00:00Z",
        Some(4),
    );
    let later = target(
        digest.clone(),
        RECORD_B,
        URI_B,
        "2026-01-01T00:00:00Z",
        Some(4),
    );

    assert!(index.insert_payload(&canonical)?);
    assert!(!index.insert_payload(&later)?);
    assert!(!index.insert_payload(&later)?);
    assert_eq!(index.lookup_payload(&digest)?, Some(canonical));
    Ok(())
}

#[test]
fn digest_algorithm_is_part_of_the_payload_key() -> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let md5 = LabelledDigest::from_digest(Algorithm::Md5, &[7; 16]);
    let sha1 = LabelledDigest::from_digest(Algorithm::Sha1, &[7; 20]);
    let md5_target = target(md5.clone(), RECORD_A, URI_A, "2025-01-01", None);
    let sha1_target = target(sha1.clone(), RECORD_B, URI_B, "2025-01-02", None);

    assert!(index.insert_payload(&md5_target)?);
    assert!(index.insert_payload(&sha1_target)?);
    assert_eq!(index.lookup_payload(&md5)?, Some(md5_target));
    assert_eq!(index.lookup_payload(&sha1)?, Some(sha1_target));
    Ok(())
}

#[test]
fn resource_validators_digest_and_warc_identity_round_trip() -> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let resource_key = key(URI_A);
    let digest = sha256(b"representation");
    index.update_resource(
        &resource_key,
        ResourceStateUpdate::Representation {
            etag: Some("W/\"opaque, value\"".to_owned()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned()),
            payload_digest: Some(digest.clone()),
            record_id: Some(uri(RECORD_A)),
            warc_date: Some(date("2025-01-01T01:02:03.456789Z")),
        },
    )?;

    let state = index.lookup_resource(&resource_key)?.expect("stored state");
    assert_eq!(state.etag.as_deref(), Some("W/\"opaque, value\""));
    assert_eq!(
        state.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
    assert_eq!(state.payload_digest, Some(digest));
    assert_eq!(state.record_id, Some(uri(RECORD_A)));
    assert_eq!(state.warc_date, Some(date("2025-01-01T01:02:03.456789Z")));
    Ok(())
}

#[test]
fn new_representation_replaces_state_and_clears_omitted_validators() -> Result<(), Box<dyn StdError>>
{
    let index = Index::open_in_memory()?;
    let resource_key = key(URI_A);
    index.update_resource(
        &resource_key,
        ResourceStateUpdate::Representation {
            etag: Some("\"old\"".to_owned()),
            last_modified: Some("old date".to_owned()),
            payload_digest: Some(sha256(b"old")),
            record_id: Some(uri(RECORD_A)),
            warc_date: Some(date("2025-01-01T00:00:00Z")),
        },
    )?;
    index.update_resource(
        &resource_key,
        ResourceStateUpdate::Representation {
            etag: None,
            last_modified: None,
            payload_digest: Some(sha256(b"new")),
            record_id: Some(uri(RECORD_B)),
            warc_date: Some(date("2025-01-02T00:00:00Z")),
        },
    )?;

    let state = index.lookup_resource(&resource_key)?.expect("stored state");
    assert_eq!(state.etag, None);
    assert_eq!(state.last_modified, None);
    assert_eq!(state.payload_digest, Some(sha256(b"new")));
    assert_eq!(state.record_id, Some(uri(RECORD_B)));
    Ok(())
}

#[test]
fn not_modified_merges_validators_and_preserves_representation_identity()
-> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let resource_key = key(URI_A);
    let digest = sha256(b"body");
    let original_date = date("2025-01-01T00:00:00Z");
    index.update_resource(
        &resource_key,
        ResourceStateUpdate::Representation {
            etag: Some("\"old\"".to_owned()),
            last_modified: Some("old date".to_owned()),
            payload_digest: Some(digest.clone()),
            record_id: Some(uri(RECORD_A)),
            warc_date: Some(original_date),
        },
    )?;
    index.update_resource(
        &resource_key,
        ResourceStateUpdate::NotModified {
            etag: Some("\"new\"".to_owned()),
            last_modified: None,
        },
    )?;

    let state = index.lookup_resource(&resource_key)?.expect("stored state");
    assert_eq!(state.etag.as_deref(), Some("\"new\""));
    assert_eq!(state.last_modified.as_deref(), Some("old date"));
    assert_eq!(state.payload_digest, Some(digest));
    assert_eq!(state.record_id, Some(uri(RECORD_A)));
    assert_eq!(state.warc_date, Some(original_date));
    assert!(!index.update_resource(
        &key(URI_B),
        ResourceStateUpdate::NotModified {
            etag: None,
            last_modified: None,
        },
    )?);
    Ok(())
}

#[test]
fn two_resources_share_a_payload_but_keep_independent_state() -> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let digest = sha256(b"shared");
    index.insert_payload(&target(
        digest.clone(),
        RECORD_A,
        URI_A,
        "2025-01-01T00:00:00Z",
        Some(6),
    ))?;
    for (resource, etag, record) in [(URI_A, "\"a\"", RECORD_A), (URI_B, "\"b\"", RECORD_B)] {
        index.update_resource(
            &key(resource),
            ResourceStateUpdate::Representation {
                etag: Some(etag.to_owned()),
                last_modified: None,
                payload_digest: Some(digest.clone()),
                record_id: Some(uri(record)),
                warc_date: Some(date("2025-01-01T00:00:00Z")),
            },
        )?;
    }

    assert_eq!(
        index.lookup_resource(&key(URI_A))?.unwrap().etag.as_deref(),
        Some("\"a\"")
    );
    assert_eq!(
        index.lookup_resource(&key(URI_B))?.unwrap().etag.as_deref(),
        Some("\"b\"")
    );
    assert_eq!(
        index.lookup_payload(&digest)?.unwrap().target_uri,
        uri(URI_A)
    );
    Ok(())
}

#[test]
fn malformed_persisted_state_returns_an_error() -> Result<(), Box<dyn StdError>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("corrupt.sqlite3");
    let index = Index::open(&path)?;
    index.update_resource(
        &key(URI_A),
        ResourceStateUpdate::Representation {
            etag: None,
            last_modified: None,
            payload_digest: Some(sha256(b"body")),
            record_id: Some(uri(RECORD_A)),
            warc_date: Some(date("2025-01-01T00:00:00Z")),
        },
    )?;
    drop(index);
    let connection = rusqlite::Connection::open(&path)?;
    connection.execute(
        "UPDATE resource_state SET digest_algorithm = 'bogus' WHERE target_uri = ?1",
        [URI_A],
    )?;
    drop(connection);

    assert!(matches!(
        Index::open(&path)?.lookup_resource(&key(URI_A)),
        Err(Error::UnsupportedDigestAlgorithm(_))
    ));
    Ok(())
}

#[test]
fn incompatible_schema_version_is_rejected_clearly() -> Result<(), Box<dyn StdError>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("future.sqlite3");
    let connection = rusqlite::Connection::open(&path)?;
    connection.pragma_update(None, "user_version", 99)?;
    drop(connection);

    assert!(matches!(
        Index::open(path),
        Err(Error::SchemaVersion {
            expected: 2,
            found: 99
        })
    ));
    Ok(())
}

#[test]
fn bulk_transaction_commits_records_together() -> Result<(), Box<dyn StdError>> {
    let mut index = Index::open_in_memory()?;
    let one = target(sha256(b"one"), RECORD_A, URI_A, "2025-01-01", Some(3));
    let two = target(sha256(b"two"), RECORD_B, URI_B, "2025-01-02", Some(3));
    let transaction = index.begin()?;
    transaction.insert_payload(&one)?;
    transaction.insert_payload(&two)?;
    transaction.commit()?;

    assert_eq!(index.lookup_payload(&one.payload_digest)?, Some(one));
    assert_eq!(index.lookup_payload(&two.payload_digest)?, Some(two));
    Ok(())
}

#[test]
fn response_ingestion_creates_payload_and_resource_state_but_ignores_http_date()
-> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let record = response(
        URI_A,
        RECORD_A,
        "2025-01-01T00:00:00Z",
        "ETag: \"v1\"\r\nDate: Wed, 21 Oct 2015 07:28:00 GMT\r\n",
        b"hello",
    )?;

    let outcome = index.index_record(&record)?;
    assert!(outcome.payload_inserted);
    assert!(outcome.resource_updated);
    let payload = index.lookup_payload(&sha256(b"hello"))?.unwrap();
    assert_eq!(payload.payload_length, Some(5));
    assert_eq!(payload.record_id, uri(RECORD_A));
    let state = index.lookup_resource(&key(URI_A))?.unwrap();
    assert_eq!(state.etag.as_deref(), Some("\"v1\""));
    assert_eq!(state.last_modified, None);
    Ok(())
}

#[test]
fn identical_revisit_does_not_replace_payload_bearing_canonical_source()
-> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let original = response(
        URI_A,
        RECORD_A,
        "2025-01-01T00:00:00Z",
        "ETag: \"a\"\r\n",
        b"shared",
    )?;
    index.index_record(&original)?;
    let digest = sha256(b"shared");
    let revisit = Record::<NoExtension>::revisit(
        URI_B,
        date("2025-01-02T00:00:00Z"),
        RevisitProfile::IDENTICAL_PAYLOAD_DIGEST,
    )?
    .record_id(uri(RECORD_B))
    .payload_digest(digest.clone())
    .refers_to(uri(RECORD_A))
    .refers_to_target_uri(uri(URI_A))
    .refers_to_date(date("2025-01-01T00:00:00Z"))
    .body(Vec::new())?;

    let outcome = index.index_record(&revisit)?;
    assert!(!outcome.payload_inserted);
    assert_eq!(
        index.lookup_payload(&digest)?.unwrap().record_id,
        uri(RECORD_A)
    );
    assert_eq!(
        index.lookup_resource(&key(URI_B))?.unwrap().record_id,
        Some(uri(RECORD_A))
    );
    Ok(())
}

#[test]
fn conditional_304_flow_preserves_enough_state_for_server_not_modified_revisit()
-> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let first = response(
        URI_A,
        RECORD_A,
        "2025-01-01T00:00:00Z",
        "ETag: \"v1\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n",
        b"version one",
    )?;
    index.index_record(&first)?;

    let before = index.lookup_resource(&key(URI_A))?.unwrap();
    assert_eq!(before.etag.as_deref(), Some("\"v1\""));
    let revisit = Record::<NoExtension>::revisit(
        URI_A,
        date("2025-01-02T00:00:00Z"),
        RevisitProfile::SERVER_NOT_MODIFIED,
    )?
    .record_id(uri(RECORD_B))
    .refers_to(uri(RECORD_A))
    .refers_to_target_uri(uri(URI_A))
    .refers_to_date(date("2025-01-01T00:00:00Z"))
    .body(b"HTTP/1.1 304 Not Modified\r\nETag: \"v1-refreshed\"\r\n\r\n".to_vec())?;
    index.index_record(&revisit)?;

    let after = index.lookup_resource(&key(URI_A))?.unwrap();
    assert_eq!(after.etag.as_deref(), Some("\"v1-refreshed\""));
    assert_eq!(after.last_modified, before.last_modified);
    assert_eq!(after.payload_digest, before.payload_digest);
    assert_eq!(after.record_id, Some(uri(RECORD_A)));
    assert_eq!(after.warc_date, Some(date("2025-01-01T00:00:00Z")));
    Ok(())
}

#[test]
fn matching_payload_at_a_second_uri_reuses_first_target_but_keeps_resource_keys_separate()
-> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let first = response(
        URI_A,
        RECORD_A,
        "2025-01-01T00:00:00Z",
        "ETag: \"a\"\r\n",
        b"shared body",
    )?;
    let second = response(
        URI_B,
        RECORD_B,
        "2025-01-02T00:00:00Z",
        "ETag: \"b\"\r\n",
        b"shared body",
    )?;
    index.index_record(&first)?;
    let second_outcome = index.index_record(&second)?;

    assert!(!second_outcome.payload_inserted);
    assert_eq!(
        index
            .lookup_payload(&sha256(b"shared body"))?
            .unwrap()
            .record_id,
        uri(RECORD_A)
    );
    assert_eq!(
        index.lookup_resource(&key(URI_A))?.unwrap().etag.as_deref(),
        Some("\"a\"")
    );
    assert_eq!(
        index.lookup_resource(&key(URI_B))?.unwrap().etag.as_deref(),
        Some("\"b\"")
    );
    Ok(())
}

#[test]
fn truncated_response_is_not_indexed() -> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let payload = b"hel";
    let mut message = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"v1\"\r\n\r\n".to_vec();
    message.extend_from_slice(payload);
    let record = Record::<NoExtension>::response(URI_A, date("2025-01-01T00:00:00Z"))?
        .record_id(uri(RECORD_A))
        .payload_digest(sha256(payload))
        .truncated(TruncatedType::Length)
        .body(message)?;

    let outcome = index.index_record(&record)?;

    // A partial body is neither a revisit target nor the resource's representation.
    assert!(!outcome.payload_inserted);
    assert!(!outcome.resource_updated);
    assert!(index.lookup_payload(&sha256(payload))?.is_none());
    assert!(index.lookup_resource(&key(URI_A))?.is_none());
    Ok(())
}

#[test]
fn payload_less_revisit_never_becomes_the_resource_record() -> Result<(), Box<dyn StdError>> {
    let index = Index::open_in_memory()?;
    let digest = sha256(b"never indexed");
    let revisit = |record_id: &str, warc_date: &str| {
        Record::<NoExtension>::revisit(
            URI_A,
            date(warc_date),
            RevisitProfile::IDENTICAL_PAYLOAD_DIGEST,
        )
        .map(|builder| {
            builder
                .record_id(uri(record_id))
                .payload_digest(digest.clone())
        })
    };

    // WARC 1.1 permits an identical-payload-digest revisit without `WARC-Refers-To`. The
    // revisit's own identity must not be recorded as the resource's representation.
    index.index_record(&revisit(RECORD_A, "2025-01-01T00:00:00Z")?.body(Vec::new())?)?;
    let state = index.lookup_resource(&key(URI_A))?.expect("resource state");
    assert_eq!(state.payload_digest, Some(digest.clone()));
    assert_eq!(state.record_id, None);
    assert_eq!(state.warc_date, None);

    // When the revisit names its original, that identity is stored even without a payload row.
    index.index_record(
        &revisit(RECORD_B, "2025-01-02T00:00:00Z")?
            .refers_to(uri(RECORD_A))
            .refers_to_target_uri(uri(URI_A))
            .refers_to_date(date("2024-12-31T00:00:00Z"))
            .body(Vec::new())?,
    )?;
    let state = index.lookup_resource(&key(URI_A))?.expect("resource state");
    assert_eq!(state.record_id, Some(uri(RECORD_A)));
    assert_eq!(state.warc_date, Some(date("2024-12-31T00:00:00Z")));
    assert!(index.lookup_payload(&digest)?.is_none());
    Ok(())
}
