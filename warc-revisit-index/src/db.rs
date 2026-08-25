//! SQLite connection, schema, and queries.

use std::path::Path;

use archivindex_warc::value::{Algorithm, LabelledDigest, WarcDate};
use fluent_uri::Uri;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{DatabaseError, Error, OpenError};
use crate::payload::RevisitTarget;
use crate::resource::{ResourceKey, ResourceState, ResourceStateUpdate};

const SCHEMA_VERSION: u32 = 2;

const SCHEMA: &str = include_str!("schema.sql");

const INSERT_PAYLOAD: &str = "INSERT INTO payloads (
     digest_algorithm, digest, payload_length, record_id, target_uri, warc_date
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
 ON CONFLICT (digest_algorithm, digest) DO NOTHING";

const UPSERT_RESOURCE: &str = "INSERT INTO resource_state (
     target_uri, etag, last_modified, digest_algorithm, digest, record_id, warc_date
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
 ON CONFLICT (target_uri) DO UPDATE SET
     etag = excluded.etag,
     last_modified = excluded.last_modified,
     digest_algorithm = excluded.digest_algorithm,
     digest = excluded.digest,
     record_id = excluded.record_id,
     warc_date = excluded.warc_date";

/// A database connection view shared by persistent indexes and bulk transactions.
pub struct Store<C> {
    connection: C,
    connection_ref: fn(&C) -> &Connection,
}

/// A persistent payload and conditional-request state index.
pub type Index = Store<Connection>;

/// A bulk indexing transaction.
pub type Transaction<'connection> = Store<rusqlite::Transaction<'connection>>;

impl Store<Connection> {
    /// Open a database at `path`, initialize a new schema, and reject incompatible versions.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot open or configure the database, schema initialization
    /// fails, or the database declares an unsupported schema version.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, OpenError> {
        let connection = Connection::open(path).map_err(DatabaseError::during("open database"))?;
        Self::initialize(connection)
    }

    /// Open a fresh in-memory database.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot create, configure, or initialize the database.
    pub fn open_in_memory() -> Result<Self, OpenError> {
        let connection = Connection::open_in_memory()
            .map_err(DatabaseError::during("open in-memory database"))?;
        Self::initialize(connection)
    }

    fn initialize(connection: Connection) -> Result<Self, OpenError> {
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;",
            )
            .map_err(DatabaseError::during("configure database"))?;
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(DatabaseError::during("read schema version"))?;

        if version == 0 {
            connection
                .execute_batch(SCHEMA)
                .map_err(DatabaseError::during("initialize schema"))?;
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(DatabaseError::during("write schema version"))?;
        } else if version != SCHEMA_VERSION {
            return Err(OpenError::SchemaVersion {
                expected: SCHEMA_VERSION,
                found: version,
            });
        }

        Ok(Self {
            connection,
            connection_ref: |connection| connection,
        })
    }

    /// Begin a transaction for bulk WARC ingestion.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot begin the transaction.
    pub fn begin(&mut self) -> Result<Transaction<'_>, DatabaseError> {
        let transaction = self
            .connection
            .transaction()
            .map_err(DatabaseError::during("begin transaction"))?;
        Ok(Store {
            connection: transaction,
            connection_ref: |transaction| transaction,
        })
    }
}

impl<C> Store<C> {
    pub(super) fn connection(&self) -> &Connection {
        (self.connection_ref)(&self.connection)
    }

    /// Find the canonical payload-bearing WARC record for `digest`.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported or malformed digests, query failures, or malformed
    /// persisted values.
    pub fn lookup_payload(&self, digest: &LabelledDigest) -> Result<Option<RevisitTarget>, Error> {
        lookup_payload(self.connection(), digest)
    }

    /// Insert a canonical payload source without replacing an existing source.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or when SQLite cannot write the row.
    pub fn insert_payload(&self, target: &RevisitTarget) -> Result<bool, Error> {
        insert_payload(self.connection(), target)
    }

    /// Find resource state within the transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails or persisted state is malformed.
    pub fn lookup_resource(&self, key: &ResourceKey) -> Result<Option<ResourceState>, Error> {
        lookup_resource(self.connection(), key)
    }

    /// Apply a resource-state update within the transaction.
    ///
    /// # Returns
    ///
    /// Whether a row was inserted or updated.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid digest data or a SQLite query failure.
    pub fn update_resource(
        &self,
        key: &ResourceKey,
        update: ResourceStateUpdate,
    ) -> Result<bool, Error> {
        update_resource(self.connection(), key, update)
    }
}

impl Transaction<'_> {
    /// Copy every row of `source` into this transaction.
    ///
    /// Existing canonical payload records are preserved; resource-state rows are replaced.
    ///
    /// # Errors
    ///
    /// Returns an error when either database fails to read or write a row.
    pub fn merge_from(&self, source: &Index) -> Result<(), Error> {
        copy_rows(
            source.connection(),
            "SELECT digest_algorithm, digest, payload_length, record_id, target_uri, warc_date
             FROM payloads",
            self.connection(),
            INSERT_PAYLOAD,
            "merge payloads",
        )?;
        copy_rows(
            source.connection(),
            "SELECT target_uri, etag, last_modified, digest_algorithm, digest, record_id, warc_date
             FROM resource_state",
            self.connection(),
            UPSERT_RESOURCE,
            "merge resource state",
        )
    }

    /// Commit all changes atomically.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot commit the transaction.
    pub fn commit(self) -> Result<(), DatabaseError> {
        self.connection
            .commit()
            .map_err(DatabaseError::during("commit transaction"))
    }
}

pub(crate) fn lookup_payload(
    connection: &Connection,
    digest: &LabelledDigest,
) -> Result<Option<RevisitTarget>, Error> {
    let (algorithm, bytes) = digest_parts(digest)?;
    let stored = cached(
        connection,
        "SELECT payload_length, record_id, target_uri, warc_date
         FROM payloads WHERE digest_algorithm = ?1 AND digest = ?2",
        "look up payload",
    )?
    .query_row(params![algorithm.label(), bytes.as_slice()], |row| {
        Ok((
            row.get::<_, Option<i64>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })
    .optional()
    .map_err(DatabaseError::during("look up payload"))?;

    stored
        .map(|(length, record_id, target_uri, warc_date)| {
            Ok(RevisitTarget {
                payload_digest: LabelledDigest::from_digest(algorithm, &bytes),
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
    let changed = cached(connection, INSERT_PAYLOAD, "insert payload")?
        .execute(params![
            algorithm.label(),
            digest.as_slice(),
            payload_length,
            target.record_id.as_str(),
            target.target_uri.as_str(),
            target.warc_date.to_string(),
        ])
        .map_err(DatabaseError::during("insert payload"))?;
    Ok(changed != 0)
}

pub(crate) fn lookup_resource(
    connection: &Connection,
    key: &ResourceKey,
) -> Result<Option<ResourceState>, Error> {
    type Stored = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
    );

    let stored: Option<Stored> = cached(
        connection,
        "SELECT etag, last_modified, digest_algorithm, digest, record_id, warc_date
         FROM resource_state WHERE target_uri = ?1",
        "look up resource state",
    )?
    .query_row([key.target_uri().as_str()], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })
    .optional()
    .map_err(DatabaseError::during("look up resource state"))?;

    stored
        .map(
            |(etag, last_modified, algorithm, digest, record_id, warc_date)| {
                let payload_digest = match (algorithm, digest) {
                    (Some(algorithm), Some(digest)) => {
                        Some(digest_from_parts(&algorithm, &digest)?)
                    }
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
) -> Result<bool, Error> {
    let changed = match update {
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
                    (Some(algorithm.label()), Some(digest))
                });
            cached(
                connection,
                UPSERT_RESOURCE,
                "update resource representation",
            )?
            .execute(params![
                key.target_uri().as_str(),
                etag,
                last_modified,
                algorithm,
                digest.as_deref(),
                record_id.as_ref().map(Uri::as_str),
                warc_date.map(|date| date.to_string()),
            ])
            .map_err(DatabaseError::during("update resource representation"))?
        }
        ResourceStateUpdate::NotModified {
            etag,
            last_modified,
        } => cached(
            connection,
            "UPDATE resource_state SET
                 etag = COALESCE(?2, etag),
                 last_modified = COALESCE(?3, last_modified)
             WHERE target_uri = ?1",
            "update not-modified resource state",
        )?
        .execute(params![key.target_uri().as_str(), etag, last_modified])
        .map_err(DatabaseError::during("update not-modified resource state"))?,
    };
    Ok(changed > 0)
}

/// Copy rows selected from one connection into another.
fn copy_rows(
    source: &Connection,
    select: &str,
    target: &Connection,
    insert: &str,
    operation: &'static str,
) -> Result<(), Error> {
    let mut select = source
        .prepare(select)
        .map_err(DatabaseError::during(operation))?;
    let mut insert = cached(target, insert, operation)?;
    let columns = select.column_count();
    let mut rows = select.query([]).map_err(DatabaseError::during(operation))?;
    while let Some(row) = rows.next().map_err(DatabaseError::during(operation))? {
        let values = (0..columns)
            .map(|column| row.get::<_, rusqlite::types::Value>(column))
            .collect::<Result<Vec<_>, _>>()
            .map_err(DatabaseError::during(operation))?;
        insert
            .execute(rusqlite::params_from_iter(values))
            .map_err(DatabaseError::during(operation))?;
    }
    Ok(())
}

/// Fetch `sql` from the connection's statement cache, preparing it on first use.
fn cached<'connection>(
    connection: &'connection Connection,
    sql: &str,
    operation: &'static str,
) -> Result<rusqlite::CachedStatement<'connection>, DatabaseError> {
    connection
        .prepare_cached(sql)
        .map_err(DatabaseError::during(operation))
}

fn digest_parts(digest: &LabelledDigest) -> Result<(Algorithm, Vec<u8>), Error> {
    let algorithm = digest.algorithm().ok_or_else(|| {
        Error::UnsupportedDigestAlgorithm(digest.algorithm_as_read().into_owned())
    })?;
    let bytes = digest
        .decoded()
        .ok_or_else(|| Error::UndecodableDigest(digest.to_string()))?;
    validate_digest_length(algorithm, bytes.len())?;
    Ok((algorithm, bytes))
}

fn digest_from_parts(label: &str, bytes: &[u8]) -> Result<LabelledDigest, Error> {
    let algorithm = Algorithm::ALL
        .into_iter()
        .find(|algorithm| algorithm.label().eq_ignore_ascii_case(label))
        .ok_or_else(|| Error::UnsupportedDigestAlgorithm(label.to_owned()))?;
    validate_digest_length(algorithm, bytes.len())?;
    Ok(LabelledDigest::from_digest(algorithm, bytes))
}

fn validate_digest_length(algorithm: Algorithm, actual: usize) -> Result<(), Error> {
    let expected = algorithm.digest_length();
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
