//! HTTP capture and redirect handling.

use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_warc::record::payload;
use archivindex_warc::recorder::CapturedExchange;
use archivindex_warc::value::{WarcDate, WarcDatePrecision};
use url::Url;

use super::{Archiver, Error};
use crate::response;

/// The precision at which `WARC-Date` fields are recorded.
const DATE_PRECISION: WarcDatePrecision = WarcDatePrecision::Fraction(6);

/// A single captured exchange, indexed but not yet written.
pub struct Exchange {
    pub(super) key: String,
    /// The capture date at the recorded precision, shared by the WARC records and index entry.
    pub(super) date: WarcDate,
    pub(crate) status: u16,
    /// The response entity-body digest, absent when transfer decoding fails.
    pub(super) payload_digest: Option<Sha256Digest>,
    pub(super) payload_length: u64,
    pub(crate) captured: CapturedExchange,
}

impl Archiver {
    /// Fetch a URL and every hop of its redirect chain, in order.
    pub(crate) fn capture(&self, url: &str) -> (Vec<Exchange>, Option<Error>) {
        let mut exchanges = Vec::new();
        let mut current = match Url::parse(url) {
            Ok(url) => url,
            Err(error) => return (exchanges, Some(error.into())),
        };

        loop {
            match self.fetch(&current) {
                Ok((exchange, location)) => {
                    exchanges.push(exchange);

                    match location {
                        Some(next) if exchanges.len() <= self.config.max_redirects => {
                            current = next;
                        }
                        _ => return (exchanges, None),
                    }
                }
                Err(error) => return (exchanges, Some(error)),
            }
        }
    }

    /// Perform one `GET` request and return its recorded exchange and followable redirect target.
    fn fetch(&self, url: &Url) -> Result<(Exchange, Option<Url>), Error> {
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Error::CredentialedUrl(redact_credentials(url)));
        }
        if url.host_str().is_none() {
            return Err(Error::MissingHost(url.to_string()));
        }

        let key = cdxj::search_key(url.as_str())?;
        let target = url
            .as_str()
            .parse::<http::Uri>()
            .map_err(|source| Error::InvalidUri {
                url: url.to_string(),
                source,
            })?;
        let captured = self
            .recorder
            .fetch(&http::Method::GET, &target, &self.headers, None)?;
        let head = response::head(&captured.response)
            .expect("invariant violation: the recorder stores a well-formed response head");
        let location = next_location(url, head.status, head.location.as_deref());
        let (payload_digest, payload_length) = match payload::entity_body(&captured.response) {
            Ok(payload) => (Some(Sha256Digest::compute(&payload)), payload.len() as u64),
            Err(_) => (None, (captured.response.len() - head.body_offset) as u64),
        };

        Ok((
            Exchange {
                key,
                date: WarcDate::new(captured.date, DATE_PRECISION),
                status: head.status,
                payload_digest,
                payload_length,
                captured,
            },
            location,
        ))
    }
}

/// The redirect target of a response, when present and followable over HTTP.
fn next_location(current: &Url, status: u16, location: Option<&str>) -> Option<Url> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }

    let next = current.join(location?).ok()?;
    (matches!(next.scheme(), "http" | "https")
        && next.username().is_empty()
        && next.password().is_none())
    .then_some(next)
}

/// Render a URL with credentials removed so errors are safe to log.
fn redact_credentials(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    redacted.to_string()
}
