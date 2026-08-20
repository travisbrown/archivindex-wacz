//! Archiving web pages over HTTP into WACZ files.
//!
//! This crate provides a small client that captures a list of URLs in a
//! [WACZ](https://specs.webrecorder.net/wacz/1.1.1/): a WARC file recording the exact wire bytes of
//! the HTTP request and response for every exchange (including each hop of a redirect chain) along
//! with the time each response took to collect, a CDXJ index over the responses, and a page list
//! entry for every archived URL. A response whose payload duplicates an earlier capture in the
//! same WACZ is stored as a `revisit` record referencing the original instead of repeating the
//! payload.
//!
//! # Examples
//!
//! ```no_run
//! use archivindex_archiver::client::Archiver;
//! use archivindex_archiver::config::Config;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let archiver = Archiver::new(Config::default())?;
//! let summary = archiver.archive_to_path(["https://www.example.com/"], "example.wacz")?;
//!
//! assert!(summary.is_complete());
//! # Ok(())
//! # }
//! ```
//!
//! Beyond one-shot lists, the [`session`] module offers crawl sessions: a queue of seed URLs grown
//! and titled by a user-supplied capture processor inspecting each response, captured (with retries
//! for transient network failures) into a single WARC file named after the session identifier. A
//! session recapturing a URL asks the server to revalidate the earlier response, storing a `304 Not
//! Modified` answer as a `revisit` record under the `server-not-modified` profile. A session may
//! use a persistent revisit index to deduplicate against earlier WARC captures and reuse their HTTP
//! validators across runs.
//!
//! # Modules
//!
//! * [`client`]: the archiving client and its outcome types
//! * [`config`]: client configuration
//! * [`conversion`]: converting existing WARC files into indexed WACZ packages
//! * [`session`]: queue-driven crawl sessions
//! * [`wordpress`]: capturing and reading resources from the `WordPress` REST API
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod conversion;
mod response;
pub mod session;
pub mod wordpress;
