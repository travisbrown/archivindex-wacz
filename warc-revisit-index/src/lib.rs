//! Persistent crawl state derived from WARC records.
//!
//! This crate maintains two deliberately separate indexes in one SQLite database:
//!
//! - The payload index maps a digest to the canonical payload-bearing WARC record: the
//!   `WARC-Refers-To` target of any revisit, whether an `identical-payload-digest` revisit found
//!   the digest again or a `server-not-modified` revisit confirmed it unchanged.
//! - The resource-state index maps a resource/request identity to its HTTP validators and the
//!   digest of its prior representation. The validators drive conditional requests; the digest
//!   leads, through the payload index, to the record a `server-not-modified` revisit refers to.
//!
//! The database is derived, rebuildable state; WARC files remain the source of truth. This crate
//! is intentionally unaware of WACZ. A caller may ingest records from standalone WARC files,
//! WARC streams extracted from WACZ packages, or any other source.
//!
//! Resource identity is currently one canonical GET representation per target URI. The
//! [`resource::ResourceKey`] wrapper leaves room for explicitly representing method, authorization context,
//! cookies, and `Vary`-selected request headers in a future schema. The same payload may be linked
//! to any number of independent resource keys.
//!
//! # Example
//!
//! ```
//! use archivindex_warc::value::{Algorithm, LabelledDigest};
//! use archivindex_warc_revisit_index::db::Index;
//! use archivindex_warc_revisit_index::resource::ResourceKey;
//! use fluent_uri::Uri;
//!
//! let index = Index::open_in_memory()?;
//! let key = ResourceKey::new(Uri::parse("https://example.com/")?.to_owned());
//! let digest = LabelledDigest::from_digest(Algorithm::Sha256, &[0; 32]);
//!
//! assert!(index.lookup_payload(&digest)?.is_none());
//! assert!(index.lookup_resource(&key)?.is_none());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod db;
pub mod error;
pub mod ingest;
pub mod payload;
pub mod resource;
