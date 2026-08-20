//! Payload-digest index types.

use archivindex_warc::value::{LabelledDigest, WarcDate};
use fluent_uri::Uri;

/// A canonical payload-bearing WARC record suitable as an identical-payload revisit target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisitTarget {
    /// The digest of the archived payload bytes.
    pub payload_digest: LabelledDigest,
    /// The payload byte length, when known.
    pub payload_length: Option<u64>,
    /// The canonical record's `WARC-Record-ID`.
    pub record_id: Uri<String>,
    /// The canonical record's `WARC-Target-URI`.
    pub target_uri: Uri<String>,
    /// The canonical record's `WARC-Date`.
    pub warc_date: WarcDate,
}
