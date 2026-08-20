//! SQLite connection, schema, and queries.

use std::path::Path;

use archivindex_warc::value::{DigestAlgorithm, LabelledDigest, WarcDate};
use fluent_uri::Uri;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{Error, ResourceKey, ResourceState, ResourceStateUpdate, RevisitTarget};

const SCHEMA_VERSION: u32 = 1;

const SCHEMA: &str = include_str!("schema.sql");

/// A persistent payload and conditional-request state index.
pub struct Index {
    pub(crate) connection: Connection,
}

impl Index {
    /// Open a database at `path`, initialize a new schema, and reject incompatible versions.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let connection = Connection::open(path).map_err(Error::database("open database"))?;
        Self::initialize(connection)
    }

    /// Open a fresh in-memory database.
    pub fn open_in_memory() -> Result<Self, Error> {
        let connection =
            Connection::open_in_memory().map_err(Error::database("open in-memory database"))?;
        Self::initialize(connection)
    }

    fn initialize(connection: Connection) -> Result<Self, Error> {
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;",
            )
            .map_err(Error::database("configure database"))?;
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Error::database("read schema version"))?;

        if version == 0 {
            connection
                .execute_batch(SCHEMA)
                .map_err(Error::database("initialize schema"))?;
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(Error::database("write schema version"))?;
        } else if version != SCHEMA_VERSION {
            return Err(Error::SchemaVersion {
                expected: SCHEMA_VERSION,
                found: version,
            });
        }

        Ok(Self { connection })
    }

    /// Find the canonical payload-bearing WARC record for `digest`.
    pub fn lookup_payload(&self, digest: &LabelledDigest) -> Result<Option<RevisitTarget>, Error> {
        lookup_payload(&self.connection, digest)
    }

    /// Insert a canonical payload source without replacing an existing source for the digest.
    ///
    /// Returns `true` when a row was inserted and `false` when the digest was already indexed.
    pub fn insert_payload(&self, target: &RevisitTarget) -> Result<bool, Error> {
        insert_payload(&self.connection, target)
    }

    /// Find conditional-request state for `key`.
    pub fn lookup_resource(&self, key: &ResourceKey) -> Result<Option<ResourceState>, Error> {
        lookup_resource(&self.connection, key)
    }

    /// Apply a representation or not-modified update and return the resulting state.
    ///
    /// A not-modified update against an unknown key is a no-op and returns `None`.
    pub fn update_resource(
        &self,
        key: &ResourceKey,
        update: ResourceStateUpdate,
    ) -> Result<Option<ResourceState>, Error> {
        update_resource(&self.connection, key, update)
    }

    /// Begin a transaction for bulk WARC ingestion.
    pub fn begin(&mut self) -> Result<Transaction<'_>, Error> {
        let transaction = self
            .connection
            .transaction()
            .map_err(Error::database("begin transaction"))?;
        Ok(Transaction { transaction })
    }
}

/// A bulk indexing transaction.
pub struct Transaction<'connection> {
    pub(crate) transaction: rusqlite::Transaction<'connection>,
}

impl Transaction<'_> {
    /// Find a canonical payload within the transaction.
    pub fn lookup_payload(&self, digest: &LabelledDigest) -> Result<Option<RevisitTarget>, Error> {
        lookup_payload(&self.transaction, digest)
    }

    /// Insert a canonical payload source without replacing an existing source.
    pub fn insert_payload(&self, target: &RevisitTarget) -> Result<bool, Error> {
        insert_payload(&self.transaction, target)
    }

    /// Find resource state within the transaction.
    pub fn lookup_resource(&self, key: &ResourceKey) -> Result<Option<ResourceState>, Error> {
        lookup_resource(&self.transaction, key)
    }

    /// Apply a resource-state update within the transaction.
    pub fn update_resource(
        &self,
        key: &ResourceKey,
        update: ResourceStateUpdate,
    ) -> Result<Option<ResourceState>, Error> {
        update_resource(&self.transaction, key, update)
    }

    /// Commit all changes atomically.
    pub fn commit(self) -> Result<(), Error> {
        self.transaction
            .commit()
            .map_err(Error::database("commit transaction"))
    }
}

pub(crate) fn lookup_payload(
    connection: &Connection,
    digest: &LabelledDigest,
) -> Result<Option<RevisitTarget>, Error> {
    let (algorithm, bytes) = digest_parts(digest)?;
    let stored = connection
        .query_row(
            "SELECT payload_length, record_id, target_uri, warc_date
             FROM payloads WHERE digest_algorithm = ?1 AND digest = ?2",
            params![algorithm, bytes],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(Error::database("look up payload"))?;

    stored
        .map(|(length, record_id, target_uri, warc_date)| {
            Ok(RevisitTarget {
                payload_digest: digest_from_parts(algorithm, bytes)?,
                payload_length: length
                    .map(|value| unsigned("payload_length", value))
                    .transpose()?,
                record_id: parse_uri("record_id", record_id)?,
                target_uri: parse_uri("target_uri", target_uri)?,
                warc_date: parse_date("warc_date", warc_date)?,
            })
        })
        .transpose()
}

pub(crate) fn insert_payload(
    connection: &Connection,
    target: &RevisitTarget,
) -> Result<bool, Error> {
    let (algorithm, digest) = digest_parts(&target.payload_digest)?;
    let payload_length = target
        .payload_length
        .map(|value| signed("payload_length", value))
        .transpose()?;
    let changed = connection
        .execute(
            "INSERT INTO payloads (
                 digest_algorithm, digest, payload_length, record_id, target_uri, warc_date
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (digest_algorithm, digest) DO NOTHING",
            params![
                algorithm,
                digest,
                payload_length,
                target.record_id.as_str(),
                target.target_uri.as_str(),
                target.warc_date.to_string(),
            ],
        )
        .map_err(Error::database("insert payload"))?;
    Ok(changed != 0)
}

pub(crate) fn lookup_resource(
    connection: &Connection,
    key: &ResourceKey,
) -> Result<Option<ResourceState>, Error> {
    type Stored = (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
    );

    let stored: Option<Stored> = connection
        .query_row(
            "SELECT etag, last_modified, digest_algorithm, digest, record_id, warc_date
             FROM resource_state WHERE target_uri = ?1",
            [key.target_uri().as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(Error::database("look up resource state"))?;

    stored
        .map(
            |(etag, last_modified, algorithm, digest, record_id, warc_date)| {
                let payload_digest = match (algorithm, digest) {
                    (Some(algorithm), Some(digest)) => Some(digest_from_parts(algorithm, digest)?),
                    (None, None) => None,
                    _ => return Err(Error::IncompleteDigest),
                };

                Ok(ResourceState {
                    key: key.clone(),
                    etag,
                    last_modified,
                    payload_digest,
                    record_id: record_id
                        .map(|value| parse_uri("record_id", value))
                        .transpose()?,
                    warc_date: warc_date
                        .map(|value| parse_date("warc_date", value))
                        .transpose()?,
                })
            },
        )
        .transpose()
}

pub(crate) fn update_resource(
    connection: &Connection,
    key: &ResourceKey,
    update: ResourceStateUpdate,
) -> Result<Option<ResourceState>, Error> {
    match update {
        ResourceStateUpdate::Representation {
            etag,
            last_modified,
            payload_digest,
            record_id,
            warc_date,
        } => {
            let (algorithm, digest) = payload_digest
                .as_ref()
                .map(digest_parts)
                .transpose()?
                .map_or((None, None), |(algorithm, digest)| {
                    (Some(algorithm), Some(digest))
                });
            connection
                .execute(
                    "INSERT INTO resource_state (
                         target_uri, etag, last_modified, digest_algorithm, digest,
                         record_id, warc_date
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (target_uri) DO UPDATE SET
                         etag = excluded.etag,
                         last_modified = excluded.last_modified,
                         digest_algorithm = excluded.digest_algorithm,
                         digest = excluded.digest,
                         record_id = excluded.record_id,
                         warc_date = excluded.warc_date",
                    params![
                        key.target_uri().as_str(),
                        etag,
                        last_modified,
                        algorithm,
                        digest,
                        record_id.as_ref().map(Uri::as_str),
                        warc_date.map(|date| date.to_string()),
                    ],
                )
                .map_err(Error::database("update resource representation"))?;
        }
        ResourceStateUpdate::NotModified {
            etag,
            last_modified,
        } => {
            connection
                .execute(
                    "UPDATE resource_state SET
                         etag = COALESCE(?2, etag),
                         last_modified = COALESCE(?3, last_modified)
                     WHERE target_uri = ?1",
                    params![key.target_uri().as_str(), etag, last_modified],
                )
                .map_err(Error::database("update not-modified resource state"))?;
        }
    }

    lookup_resource(connection, key)
}

fn digest_parts(digest: &LabelledDigest) -> Result<(i64, Vec<u8>), Error> {
    let algorithm = algorithm_code(digest.algorithm())?;
    let bytes = digest
        .decoded()
        .ok_or_else(|| Error::UndecodableDigest(digest.to_string()))?;
    validate_digest_length(digest.algorithm(), bytes.len())?;
    Ok((algorithm, bytes))
}

fn algorithm_code(algorithm: &DigestAlgorithm) -> Result<i64, Error> {
    match algorithm {
        DigestAlgorithm::Md5 => Ok(1),
        DigestAlgorithm::Sha1 => Ok(2),
        DigestAlgorithm::Sha256 => Ok(3),
        DigestAlgorithm::Other(label) => Err(Error::UnsupportedDigestAlgorithm(label.to_string())),
    }
}

fn algorithm_from_code(code: i64) -> Result<DigestAlgorithm, Error> {
    match code {
        1 => Ok(DigestAlgorithm::Md5),
        2 => Ok(DigestAlgorithm::Sha1),
        3 => Ok(DigestAlgorithm::Sha256),
        _ => Err(Error::UnsupportedDigestAlgorithm(format!(
            "database code {code}"
        ))),
    }
}

fn digest_from_parts(code: i64, bytes: Vec<u8>) -> Result<LabelledDigest, Error> {
    let algorithm = algorithm_from_code(code)?;
    validate_digest_length(&algorithm, bytes.len())?;
    Ok(LabelledDigest::from_digest(algorithm, &bytes))
}

fn validate_digest_length(algorithm: &DigestAlgorithm, actual: usize) -> Result<(), Error> {
    let expected = algorithm
        .digest_length()
        .ok_or_else(|| Error::UnsupportedDigestAlgorithm(algorithm.label().to_owned()))?;
    if actual == expected {
        Ok(())
    } else {
        Err(Error::InvalidDigestLength {
            algorithm: algorithm.label().to_owned(),
            expected,
            actual,
        })
    }
}

fn signed(field: &'static str, value: u64) -> Result<i64, Error> {
    i64::try_from(value).map_err(|_| Error::IntegerOutOfRange { field, value })
}

fn unsigned(field: &'static str, value: i64) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| Error::MalformedInteger { field, value })
}

fn parse_uri(field: &'static str, value: String) -> Result<Uri<String>, Error> {
    Uri::parse(value).map_err(|(source, value)| Error::MalformedUri {
        field,
        value,
        source,
    })
}

fn parse_date(field: &'static str, value: String) -> Result<WarcDate, Error> {
    WarcDate::parse(&value, archivindex_warc::version::WarcVersion::V1_1)
        .ok_or(Error::MalformedDate { field, value })
}
