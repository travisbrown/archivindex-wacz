//! SQLite connection, schema, and queries.

use std::path::Path;
use std::time::Duration;

use archivindex_warc::value::{Algorithm, LabelledDigest, WarcDate};
use fluent_uri::Uri;
use rusqlite::{Connection, OptionalExtension, params};

use crate::payload::RevisitTarget;
use crate::resource::{ResourceKey, ResourceState, ResourceStateUpdate, Variance};
use crate::{DatabaseError, Error, Index, OpenError, Store, Transaction};

/// A SQLite handle a [`Store`] can run statements through.
///
/// The trait lives in this private module, so [`Connection`] and [`rusqlite::Transaction`] are
/// its only implementors and no caller can name it to add another.
pub trait Handle {
    /// Borrow the underlying connection.
    fn as_connection(&self) -> &Connection;
}

impl Handle for Connection {
    fn as_connection(&self) -> &Connection {
        self
    }
}

impl Handle for rusqlite::Transaction<'_> {
    // `Transaction` dereferences to the connection it borrows.
    fn as_connection(&self) -> &Connection {
        self
    }
}

/// How long a statement waits for a competing writer before giving up.
///
/// SQLite defaults to zero, which turns any overlap between two processes indexing into the same
/// database into an immediate `SQLITE_BUSY`. Writes here are single-row upserts, so a competing
/// writer clears quickly and waiting is almost always better than failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA_VERSION: u32 = 4;

const SCHEMA: &str = include_str!("schema.sql");

const INSERT_PAYLOAD: &str = "INSERT INTO payloads (
     digest_algorithm, digest, payload_length, record_id, target_uri, warc_date
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
 ON CONFLICT (digest_algorithm, digest) DO NOTHING";

const UPSERT_RESOURCE: &str = "INSERT INTO resource_state (
     target_uri, etag, last_modified, digest_algorithm, digest, record_id, warc_date,
     observed_at, observed_seconds, observed_nanos, variance
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
 ON CONFLICT (target_uri) DO UPDATE SET
     etag = excluded.etag,
     last_modified = excluded.last_modified,
     digest_algorithm = excluded.digest_algorithm,
     digest = excluded.digest,
     record_id = excluded.record_id,
     warc_date = excluded.warc_date,
     observed_at = excluded.observed_at,
     observed_seconds = excluded.observed_seconds,
     observed_nanos = excluded.observed_nanos,
     variance = excluded.variance
 WHERE excluded.observed_seconds > resource_state.observed_seconds
    OR (excluded.observed_seconds = resource_state.observed_seconds
        AND excluded.observed_nanos >= resource_state.observed_nanos)";

impl Store<Connection> {
    /// Open a database at `path`, initialize a new schema, and reject incompatible versions.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError::Database`] when SQLite cannot open, configure, or initialize the
    /// database, and [`OpenError::SchemaVersion`] when the database was written by an
    /// incompatible version of this crate.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, OpenError> {
        let connection = Connection::open(path).map_err(DatabaseError::during("open database"))?;
        Self::initialize(connection)
    }

    /// Open a fresh in-memory database.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError::Database`] when SQLite cannot create, configure, or initialize the
    /// database. A fresh database never reports [`OpenError::SchemaVersion`].
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
        connection
            .busy_timeout(BUSY_TIMEOUT)
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

        Ok(Self { connection })
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
        })
    }
}

impl<C: Handle> Store<C> {
    pub(crate) fn connection(&self) -> &Connection {
        self.connection.as_connection()
    }

    /// Find the canonical payload-bearing WARC record for `digest`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedDigestAlgorithm`], [`Error::UndecodableDigest`], or
    /// [`Error::InvalidDigestLength`] for a digest this index cannot store, [`Error::Database`]
    /// for a query failure, and a `Malformed*` variant for a row this crate did not write.
    pub fn lookup_payload(&self, digest: &LabelledDigest) -> Result<Option<RevisitTarget>, Error> {
        lookup_payload(self.connection(), digest)
    }

    /// Insert a canonical payload source without replacing an existing source.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedDigestAlgorithm`], [`Error::UndecodableDigest`], or
    /// [`Error::InvalidDigestLength`] for a digest this index cannot store,
    /// [`Error::IntegerOutOfRange`] for a payload length SQLite cannot hold, and
    /// [`Error::Database`] for a write failure.
    pub fn insert_payload(&self, target: &RevisitTarget) -> Result<bool, Error> {
        insert_payload(self.connection(), target)
    }

    /// Find the stored conditional-request state for `key`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Database`] for a query failure, [`Error::IncompleteDigest`] when only one
    /// of the two digest columns is populated, and a digest or `Malformed*` variant for a row
    /// this crate did not write.
    pub fn lookup_resource(&self, key: &ResourceKey) -> Result<Option<ResourceState>, Error> {
        lookup_resource(self.connection(), key)
    }

    /// Apply a resource-state update.
    ///
    /// # Returns
    ///
    /// Whether a row was inserted or updated. An update older than the stored observation is
    /// ignored and returns `false`; an equal observation replaces the stored state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedDigestAlgorithm`], [`Error::UndecodableDigest`], or
    /// [`Error::InvalidDigestLength`] for a digest this index cannot store, and
    /// [`Error::Database`] for a write failure.
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
    /// Existing canonical payload records are preserved. A resource-state row is replaced only
    /// when the incoming state was observed at least as recently as the stored state.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot read a row from `source` or write it here.
    pub fn merge_from(&self, source: &Index) -> Result<(), DatabaseError> {
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
            "SELECT target_uri, etag, last_modified, digest_algorithm, digest, record_id, warc_date,
                    observed_at, observed_seconds, observed_nanos, variance
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

pub fn lookup_payload(
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

pub fn insert_payload(connection: &Connection, target: &RevisitTarget) -> Result<bool, Error> {
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

pub fn lookup_resource(
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
        String,
        Option<String>,
    );

    let stored: Option<Stored> = cached(
        connection,
        "SELECT etag, last_modified, digest_algorithm, digest, record_id, warc_date, observed_at,
                variance
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
            row.get(6)?,
            row.get(7)?,
        ))
    })
    .optional()
    .map_err(DatabaseError::during("look up resource state"))?;

    stored
        .map(
            |(
                etag,
                last_modified,
                algorithm,
                digest,
                record_id,
                warc_date,
                observed_at,
                variance,
            )| {
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
                    observed_at: parse_date("observed_at", observed_at)?,
                    variance: Variance::decode(variance)?,
                })
            },
        )
        .transpose()
}

pub fn update_resource(
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
            observed_at,
            variance,
        } => {
            let (algorithm, digest) = payload_digest
                .as_ref()
                .map(digest_parts)
                .transpose()?
                .map_or((None, None), |(algorithm, digest)| {
                    (Some(algorithm.label()), Some(digest))
                });
            let (observed_at, observed_seconds, observed_nanos) = observation_parts(observed_at);
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
                observed_at,
                observed_seconds,
                observed_nanos,
                variance.encode(),
            ])
            .map_err(DatabaseError::during("update resource representation"))?
        }
        ResourceStateUpdate::NotModified {
            etag,
            last_modified,
            observed_at,
        } => {
            let (observed_at, observed_seconds, observed_nanos) = observation_parts(observed_at);
            cached(
                connection,
                "UPDATE resource_state SET
                 etag = COALESCE(?2, etag),
                 last_modified = COALESCE(?3, last_modified),
                 observed_at = ?4,
                 observed_seconds = ?5,
                 observed_nanos = ?6
             WHERE target_uri = ?1
               AND (?5 > observed_seconds OR (?5 = observed_seconds AND ?6 >= observed_nanos))",
                "update not-modified resource state",
            )?
            .execute(params![
                key.target_uri().as_str(),
                etag,
                last_modified,
                observed_at,
                observed_seconds,
                observed_nanos,
            ])
            .map_err(DatabaseError::during("update not-modified resource state"))?
        }
    };
    Ok(changed > 0)
}

fn observation_parts(date: WarcDate) -> (String, i64, i64) {
    let date_time = date.date_time();
    (
        date.to_string(),
        date_time.timestamp(),
        i64::from(date_time.timestamp_subsec_nanos()),
    )
}

/// Copy rows selected from one connection into another.
fn copy_rows(
    source: &Connection,
    select: &str,
    target: &Connection,
    insert: &str,
    operation: &'static str,
) -> Result<(), DatabaseError> {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    #[test_strategy::proptest]
    fn integer_columns_round_trip(#[strategy(0..=u64::MAX >> 1)] value: u64) {
        let round_tripped =
            signed("payload_length", value).and_then(|value| unsigned("payload_length", value));

        prop_assert_eq!(round_tripped.ok(), Some(value));
    }

    #[test_strategy::proptest]
    fn out_of_range_integers_are_rejected(
        #[strategy((u64::MAX >> 1) + 1..=u64::MAX)] too_large: u64,
        #[strategy(i64::MIN..0)] negative: i64,
    ) {
        prop_assert!(signed("payload_length", too_large).is_err());
        prop_assert!(unsigned("payload_length", negative).is_err());
    }

    #[test_strategy::proptest]
    fn payload_sources_are_inserted_once_and_read_back(
        #[strategy(proptest::collection::vec(strategies::revisit_target(), 0..=6))] targets: Vec<
            RevisitTarget,
        >,
    ) {
        let index = Index::open_in_memory().unwrap();
        let mut expected: HashMap<String, RevisitTarget> = HashMap::new();

        for target in targets {
            let inserted = index.insert_payload(&target).unwrap();
            let digest = target.payload_digest.to_string();

            // The first record seen for a digest stays canonical.
            prop_assert_eq!(inserted, !expected.contains_key(&digest));
            expected.entry(digest).or_insert(target);
        }

        for target in expected.values() {
            let found = index.lookup_payload(&target.payload_digest).unwrap();

            prop_assert_eq!(found.as_ref(), Some(target));
        }
    }

    #[test_strategy::proptest]
    fn resource_state_follows_the_update_model(
        #[strategy(proptest::collection::vec(
            (strategies::resource_key(), strategies::resource_state_update()),
            0..=8,
        ))]
        updates: Vec<(ResourceKey, ResourceStateUpdate)>,
    ) {
        let index = Index::open_in_memory().unwrap();
        let mut expected: HashMap<String, ResourceState> = HashMap::new();

        for (key, update) in updates {
            let changed = index.update_resource(&key, update.clone()).unwrap();
            let uri = key.target_uri().as_str().to_owned();

            match update {
                ResourceStateUpdate::Representation {
                    etag,
                    last_modified,
                    payload_digest,
                    record_id,
                    warc_date,
                    observed_at,
                    variance,
                } => {
                    let accepted = expected.get(&uri).is_none_or(|state| {
                        observed_at.date_time() >= state.observed_at.date_time()
                    });
                    prop_assert_eq!(changed, accepted);
                    if accepted {
                        expected.insert(
                            uri,
                            ResourceState {
                                key,
                                etag,
                                last_modified,
                                payload_digest,
                                record_id,
                                warc_date,
                                observed_at,
                                variance,
                            },
                        );
                    }
                }
                ResourceStateUpdate::NotModified {
                    etag,
                    last_modified,
                    observed_at,
                } => {
                    let accepted = expected.get(&uri).is_some_and(|state| {
                        observed_at.date_time() >= state.observed_at.date_time()
                    });
                    prop_assert_eq!(changed, accepted);

                    if accepted && let Some(state) = expected.get_mut(&uri) {
                        state.etag = etag.or_else(|| state.etag.take());
                        state.last_modified = last_modified.or_else(|| state.last_modified.take());
                        state.observed_at = observed_at;
                    }
                }
            }
        }

        for state in expected.values() {
            let found = index.lookup_resource(&state.key).unwrap();

            prop_assert_eq!(found.as_ref(), Some(state));
        }
    }
}
