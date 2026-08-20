//! HTTP capture, conditional revalidation, and redirect handling.

use std::borrow::Cow;

use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_warc::record::payload;
use archivindex_warc::recorder::CapturedExchange;
use archivindex_warc::value::{WarcDate, WarcDatePrecision};
use http::StatusCode;
use http::header::{HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use url::{Position, Url};

use super::{Archiver, Collection, Error};
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
    /// The digest of the earlier payload that this `304 Not Modified` response, answering a
    /// conditional request, confirms unchanged.
    pub(super) revalidated: Option<Sha256Digest>,
    pub(crate) captured: CapturedExchange,
}

impl Exchange {
    /// The digest of the stored payload this exchange revisits, making its response a `revisit`
    /// record when that payload was captured earlier: the payload a `304 Not Modified` confirmed
    /// unchanged, or this exchange's own payload, which may duplicate an earlier capture's.
    ///
    /// Exchanges without a decodable payload, with an empty payload, or with a truncated response
    /// never revisit by their own payload: the first two save nothing, and a truncated capture's
    /// digest does not describe the complete payload.
    pub(super) fn revisit_key(&self) -> Option<Sha256Digest> {
        self.revalidated.or_else(|| {
            self.payload_digest
                .filter(|_| self.payload_length > 0 && self.captured.truncated.is_none())
        })
    }

    /// The validators a later capture of this exchange's URL sends to ask whether the stored
    /// payload is still current, if the response carries any.
    ///
    /// A redirect is never revalidated: a `304` in place of its `Location` would end the chain.
    pub(super) fn original(&self) -> Option<Original> {
        if is_redirect(self.status) {
            return None;
        }
        let payload_digest = self.revisit_key()?;
        let validator = |name| {
            response::header(&self.captured.response, name)
                .and_then(|value| HeaderValue::from_bytes(value).ok())
        };
        let etag = validator("etag");
        let last_modified = validator("last-modified");

        (etag.is_some() || last_modified.is_some()).then_some(Original {
            payload_digest,
            etag,
            last_modified,
        })
    }
}

/// An earlier complete capture of a URL: the digest identifying its stored payload, and the
/// validators a later request sends to ask the server whether that payload is still current.
#[derive(Clone, Debug)]
pub(super) struct Original {
    payload_digest: Sha256Digest,
    etag: Option<HeaderValue>,
    last_modified: Option<HeaderValue>,
}

impl Original {
    /// The request headers extended with the preconditions under which the server may answer
    /// `304 Not Modified` instead of repeating the payload.
    fn conditional_headers(&self, headers: &HeaderMap) -> HeaderMap {
        let mut headers = headers.clone();
        if let Some(etag) = &self.etag {
            headers.insert(IF_NONE_MATCH, etag.clone());
        }
        if let Some(last_modified) = &self.last_modified {
            headers.insert(IF_MODIFIED_SINCE, last_modified.clone());
        }
        headers
    }
}

impl Archiver {
    /// Fetch a URL and every hop of its redirect chain, in order.
    ///
    /// Given a collection, a hop whose URL it already holds a complete capture of is requested
    /// conditionally on that capture's validators, so that the server may answer `304 Not
    /// Modified`, which the collection then stores as a revisit of the earlier capture.
    pub(crate) fn capture(
        &self,
        url: &str,
        revalidate: Option<&Collection>,
    ) -> (Vec<Exchange>, Option<Error>) {
        let mut exchanges = Vec::new();
        let mut current = match Url::parse(url) {
            Ok(url) => url,
            Err(error) => return (exchanges, Some(error.into())),
        };

        loop {
            match self.fetch(&current, revalidate) {
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
    fn fetch(
        &self,
        url: &Url,
        revalidate: Option<&Collection>,
    ) -> Result<(Exchange, Option<Url>), Error> {
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
        // The collection keys captures by the recorded target URI, which carries no fragment.
        let original =
            revalidate.and_then(|collection| collection.original(&url[..Position::AfterQuery]));
        let headers = original.map_or(Cow::Borrowed(&self.headers), |original| {
            Cow::Owned(original.conditional_headers(&self.headers))
        });
        let captured = self
            .recorder
            .fetch(&http::Method::GET, &target, &headers, None)?;
        let head = response::head(&captured.response)
            .expect("invariant violation: the recorder stores a well-formed response head");
        let location = next_location(url, head.status, head.location.as_deref());
        let revalidated = original
            .filter(|_| head.status == StatusCode::NOT_MODIFIED.as_u16())
            .map(|original| original.payload_digest);
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
                revalidated,
                captured,
            },
            location,
        ))
    }
}

/// Whether a status redirects to the response's `Location`.
const fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The redirect target of a response, when present and followable over HTTP.
fn next_location(current: &Url, status: u16, location: Option<&str>) -> Option<Url> {
    if !is_redirect(status) {
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
