//! HTTP capture, conditional revalidation, and redirect handling.

use std::borrow::Cow;

use archivindex_warc::recorder::CapturedExchange;
use archivindex_warc::value::{DigestAlgorithm, LabelledDigest, WarcDate, WarcDatePrecision};
use archivindex_warc_revisit_index::payload::RevisitTarget;
use archivindex_warc_revisit_index::resource::{ResourceKey, ResourceState};
use http::StatusCode;
use http::header::{HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use sha2::{Digest, Sha256};
use url::{Position, Url};

use super::{Archiver, Collection, Error};

/// The precision at which `WARC-Date` fields are recorded.
const DATE_PRECISION: WarcDatePrecision = WarcDatePrecision::Fraction(6);

/// A single captured exchange not yet written.
pub struct Exchange {
    /// The capture date at the recorded precision, shared by the WARC records.
    pub(super) date: WarcDate,
    pub(crate) status: u16,
    /// The response entity-body digest, absent when transfer decoding fails.
    pub(super) payload_digest: Option<[u8; 32]>,
    pub(super) payload_length: u64,
    /// The earlier capture that this `304 Not Modified` response, answering a conditional request,
    /// confirms unchanged.
    pub(super) revalidated: Option<RevisitTarget>,
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
    pub(super) fn revisit_key(&self) -> Option<LabelledDigest> {
        self.revalidated
            .as_ref()
            .map(|target| target.payload_digest.clone())
            .or_else(|| {
                self.payload_digest
                    .filter(|_| self.payload_length > 0 && self.captured.truncated.is_none())
                    .map(labelled_digest)
            })
    }

    /// The resource key for the recorded target URI.
    pub(super) fn resource_key(&self) -> ResourceKey {
        ResourceKey::new(self.captured.target_uri.clone())
    }

    /// Return a readable response validator exactly as received.
    pub(crate) fn validator(&self, name: &str) -> Option<String> {
        self.captured
            .response_metadata
            .header(name)
            .and_then(|value| std::str::from_utf8(value).ok())
            .map(str::to_owned)
    }

    pub(crate) fn payload(&self) -> Cow<'_, [u8]> {
        self.captured
            .entity_body()
            .unwrap_or_else(|_| Cow::Borrowed(self.captured.stored_body()))
    }
}

/// An earlier complete capture of a URL: the digest identifying its stored payload, and the
/// validators a later request sends to ask the server whether that payload is still current.
#[derive(Clone, Debug)]
pub(super) struct Original {
    target: RevisitTarget,
    etag: Option<HeaderValue>,
    last_modified: Option<HeaderValue>,
}

impl Original {
    /// Build a conditionally usable original from complete persisted representation state.
    pub(super) fn from_state(
        state: ResourceState,
        canonical: Option<RevisitTarget>,
    ) -> Option<Self> {
        let payload_digest = state.payload_digest?;
        let target = match canonical {
            Some(target) => target,
            None => RevisitTarget {
                payload_digest,
                payload_length: None,
                record_id: state.record_id?,
                target_uri: state.key.target_uri().clone(),
                warc_date: state.warc_date?,
            },
        };
        let etag = state
            .etag
            .and_then(|value| HeaderValue::from_str(&value).ok());
        let last_modified = state
            .last_modified
            .and_then(|value| HeaderValue::from_str(&value).ok());

        (etag.is_some() || last_modified.is_some()).then_some(Self {
            target,
            etag,
            last_modified,
        })
    }

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

        let target = url
            .as_str()
            .parse::<http::Uri>()
            .map_err(|source| Error::InvalidUri {
                url: url.to_string(),
                source,
            })?;
        // The collection keys captures by the recorded target URI, which carries no fragment.
        let original = revalidate
            .map(|collection| collection.original(&url[..Position::AfterQuery]))
            .transpose()?
            .flatten();
        let headers = original
            .as_ref()
            .map_or(Cow::Borrowed(&self.headers), |original| {
                Cow::Owned(original.conditional_headers(&self.headers))
            });
        let captured = self
            .recorder
            .fetch(&http::Method::GET, &target, &headers, None)?;
        let status = captured.response_metadata.status;
        let location = captured
            .response_metadata
            .header("location")
            .and_then(|value| std::str::from_utf8(value).ok());
        let location = next_location(url, status, location);
        let revalidated = original
            .filter(|_| status == StatusCode::NOT_MODIFIED.as_u16())
            .map(|original| original.target);
        let (payload_digest, payload_length) = captured.entity_body().map_or_else(
            |_| (None, captured.stored_body().len() as u64),
            |payload| (Some(Sha256::digest(&payload).into()), payload.len() as u64),
        );

        Ok((
            Exchange {
                date: WarcDate::new(captured.date, DATE_PRECISION),
                status,
                payload_digest,
                payload_length,
                revalidated,
                captured,
            },
            location,
        ))
    }
}

/// Express the archiver's fixed SHA-256 payload digest in WARC's labelled representation.
pub(super) fn labelled_digest(digest: [u8; 32]) -> LabelledDigest {
    LabelledDigest::from_digest(DigestAlgorithm::Sha256, &digest)
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
