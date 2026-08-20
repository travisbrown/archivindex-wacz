//! Persistent crawl state derived from WARC records.
//!
//! This crate maintains two deliberately separate indexes in one SQLite database:
//!
//! - The payload index maps a digest to a canonical, payload-bearing WARC record. It primarily
//!   supports WARC `identical-payload-digest` revisits.
//! - The resource-state index maps a resource/request identity to HTTP validators and the prior
//!   representation state. It primarily supports conditional requests and
//!   `server-not-modified` revisits.
//!
//! The database is derived, rebuildable state; WARC files remain the source of truth. This crate
//! is intentionally unaware of WACZ. A caller may ingest records from standalone WARC files,
//! WARC streams extracted from WACZ packages, or any other source.
//!
//! Resource identity is currently one canonical GET representation per target URI. The
//! [`ResourceKey`] wrapper leaves room for explicitly representing method, authorization context,
//! cookies, and `Vary`-selected request headers in a future schema. The same payload may be linked
//! to any number of independent resource keys.
//!
//! # Example
//!
//! ```
//! use archivindex_warc::value::{DigestAlgorithm, LabelledDigest};
//! use archivindex_warc_revisit_index::{Index, ResourceKey};
//! use fluent_uri::Uri;
//!
//! let index = Index::open_in_memory()?;
//! let key = ResourceKey::new(Uri::parse("https://example.com/")?.to_owned());
//! let digest = LabelledDigest::from_digest(DigestAlgorithm::Sha256, &[0; 32]);
//!
//! assert!(index.lookup_payload(&digest)?.is_none());
//! assert!(index.lookup_resource(&key)?.is_none());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod db;
mod error;
mod ingest;
mod payload;
mod resource;

pub use db::{Index, Transaction};
pub use error::Error;
pub use ingest::IndexRecordOutcome;
pub use payload::RevisitTarget;
pub use resource::{ResourceKey, ResourceState, ResourceStateUpdate};
