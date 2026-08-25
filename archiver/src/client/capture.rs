//! HTTP capture, conditional revalidation, and redirect handling.

use std::borrow::Cow;
use std::fmt::Write as _;

use archivindex_warc::recorder::CapturedExchange;
use archivindex_warc::value::marker::Sha256;
use archivindex_warc::value::{LabelledDigest, Supported as _, WarcDate, WarcDatePrecision};
use archivindex_warc_revisit_index::payload::RevisitTarget;
use archivindex_warc_revisit_index::resource::{ResourceKey, ResourceState};
use fluent_uri::Uri;
use http::StatusCode;
use http::header::{HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use url::{Position, Url};

use super::collection::Collection;
use super::{Archiver, Error};

/// Captured exchanges, with any terminal fetch failure represented explicitly.
pub enum CaptureOutcome {
    /// The redirect chain completed.
    Captured(Vec<Exchange>),
    /// Fetching stopped after zero or more recorded exchanges.
    Failed {
        /// Exchanges completed before the failure.
        exchanges: Vec<Exchange>,
        /// The terminal failure.
        error: Error,
    },
}

impl CaptureOutcome {
    pub(crate) fn fail(self, error: Error) -> Self {
        match self {
            Self::Captured(exchanges) | Self::Failed { exchanges, .. } => {
                Self::Failed { exchanges, error }
            }
        }
    }
}

/// The precision at which `WARC-Date` fields are recorded.
pub(super) const DATE_PRECISION: WarcDatePrecision = WarcDatePrecision::Fraction(6);

/// A single captured exchange not yet written.
pub struct Exchange {
    /// The capture date at the recorded precision, shared by the WARC records.
    pub(super) date: WarcDate,
    pub(crate) status: u16,
    /// The decoded entity body when it differs from the stored body.
    decoded: Option<Vec<u8>>,
    /// The SHA-256 digest of the entity body, absent when transfer decoding fails.
    pub(super) payload_digest: Option<LabelledDigest>,
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
                    .as_ref()
                    .filter(|_| !self.payload().is_empty() && self.captured.truncated.is_none())
                    .cloned()
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

    /// The entity body, or the stored body when transfer decoding fails.
    pub(crate) fn payload(&self) -> &[u8] {
        self.decoded
            .as_deref()
            .unwrap_or_else(|| self.captured.stored_body())
    }

    /// The length of [`payload`](Self::payload).
    pub(super) fn payload_length(&self) -> u64 {
        self.payload().len() as u64
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
    pub(crate) fn capture(&self, url: &str, revalidate: Option<&Collection>) -> CaptureOutcome {
        let mut exchanges = Vec::new();
        let mut current = match Url::parse(url) {
            Ok(url) => url,
            Err(error) => {
                return CaptureOutcome::Failed {
                    exchanges,
                    error: error.into(),
                };
            }
        };

        loop {
            match self.fetch(&current, revalidate) {
                Ok((exchange, location)) => {
                    exchanges.push(exchange);

                    match location {
                        Some(next) if exchanges.len() <= self.config.max_redirects => {
                            current = next;
                        }
                        _ => return CaptureOutcome::Captured(exchanges),
                    }
                }
                Err(error) => return CaptureOutcome::Failed { exchanges, error },
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

        let request_target = request_target(url);
        let target = request_target
            .parse::<http::Uri>()
            .map_err(|source| Error::InvalidUri {
                url: url.to_string(),
                source,
            })?;
        // The collection keys captures by the recorded target URI, which carries no fragment.
        let original = revalidate
            .map(|collection| {
                let target_uri = Uri::parse(request_target.as_ref())
                    .map_err(archivindex_warc::recorder::Error::TargetUri)?
                    .to_owned();

                collection.original(target_uri)
            })
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
        // Decode and digest the entity body once for revisit detection and processing.
        let (decoded, payload_digest) = captured.entity_body().map_or((None, None), |payload| {
            let mut hasher = Sha256::hasher();
            hasher.update(&payload);
            let decoded = match payload {
                Cow::Owned(decoded) => Some(decoded),
                // Keep a borrowed body only when it differs from the stored body.
                Cow::Borrowed(body) => {
                    (body.len() != captured.stored_body().len()).then(|| body.to_vec())
                }
            };
            (decoded, Some(hasher.finalize_labelled()))
        });

        Ok((
            Exchange {
                date: WarcDate::new(captured.date, DATE_PRECISION),
                status,
                decoded,
                payload_digest,
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

/// Render an RFC 3986 request target without a fragment.
///
/// Percent-encode the characters the WHATWG serializer leaves bare but RFC 3986 forbids:
/// `|`, `^`, `[`, `]`, `{`, `}`, and `` ` ``.
fn request_target(url: &Url) -> Cow<'_, str> {
    let text = &url[..Position::AfterQuery];
    let path_start = url[..Position::BeforePath].len();
    let needs_encoding =
        |character: char| matches!(character, '|' | '^' | '[' | ']' | '{' | '}' | '`');

    if !text[path_start..].contains(needs_encoding) {
        return Cow::Borrowed(text);
    }

    let mut encoded = String::with_capacity(text.len() + 8);
    encoded.push_str(&text[..path_start]);

    for character in text[path_start..].chars() {
        if needs_encoding(character) {
            // Writing to a `String` cannot fail.
            let _ = write!(encoded, "%{:02X}", u32::from(character));
        } else {
            encoded.push(character);
        }
    }

    Cow::Owned(encoded)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_targets_are_valid_uris() {
        let url = Url::parse("http://example.com/a|b^c[d]?x={y}`z#frag").expect("valid URL");
        let target = request_target(&url);
        assert_eq!(target, "http://example.com/a%7Cb%5Ec%5Bd%5D?x=%7By%7D%60z");
        assert!(Uri::parse(target.as_ref()).is_ok());
    }

    #[test]
    fn plain_request_targets_are_borrowed() {
        let url = Url::parse("http://example.com/a?b=c#frag").expect("valid URL");
        assert!(matches!(
            request_target(&url),
            Cow::Borrowed("http://example.com/a?b=c")
        ));
    }
}
