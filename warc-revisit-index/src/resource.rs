//! Conditional-request resource state.

use archivindex_warc::value::{LabelledDigest, WarcDate};
use fluent_uri::Uri;

use crate::Error;

/// The request identity used for conditional HTTP state.
///
/// A key represents the crawler's canonical GET representation of a target URI. One URI may still
/// have several representations selected by request header fields; [`Variance`] records which
/// fields a stored response declared as selecting, so that state is not reused across variants.
///
/// Requests carrying credentials or cookies are outside this model. A server may return different
/// content to two such requests without declaring anything in `Vary`, so callers that send them
/// must not share one index between identities.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ResourceKey {
    target_uri: Uri<String>,
}

impl ResourceKey {
    /// Construct a key for the canonical GET representation of `target_uri`.
    #[must_use]
    pub const fn new(target_uri: Uri<String>) -> Self {
        Self { target_uri }
    }

    /// Return the key's target URI.
    #[must_use]
    pub const fn target_uri(&self) -> &Uri<String> {
        &self.target_uri
    }
}

impl From<Uri<String>> for ResourceKey {
    fn from(target_uri: Uri<String>) -> Self {
        Self::new(target_uri)
    }
}

/// What a stored response declared about the request fields that select its representation.
///
/// HTTP allows one URI to have several representations chosen by request header fields, which a
/// response announces in `Vary`. Stored validators belong to the representation that was actually
/// captured, so a later request may reuse them only when it selects that same representation.
/// Without the check, a crawl configured with a different `User-Agent` could revalidate against
/// another variant's `ETag` and, on a `304 Not Modified`, record a revisit pointing at bytes it
/// never received.
///
/// A response that declares no `Vary` is treated as invariant, as an HTTP cache treats it: a server
/// that varies its representations without saying so is indistinguishable from one that does not
/// vary at all.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Variance {
    /// The response declared no `Vary` field, so every request for the URI selects it.
    #[default]
    Invariant,
    /// The stored representation cannot be selected by request header fields.
    ///
    /// Either the response declared `Vary: *`, or it named selecting fields whose values in the
    /// originating request are unknown. Neither can be matched, so state stored under this
    /// variance is never reused for revalidation.
    Unselectable,
    /// The response named selecting fields, recorded with the originating request's values.
    Selected(SelectingHeaders),
}

impl Variance {
    /// The stored marker for [`Variance::Unselectable`].
    ///
    /// An encoded selection always contains a line feed, so no selection can collide with it.
    const UNSELECTABLE: &'static str = "*";

    /// Record the variance a response declared, resolved against the request that produced it.
    ///
    /// `vary` is the response's `Vary` field value, absent when the response declares none.
    /// `field` returns the request's value for a lowercase field name, or `None` when the request
    /// sent no such field; a field that was not sent is distinct from one sent empty.
    ///
    /// A field value containing a line break cannot have come from a real HTTP message, so it
    /// yields [`Variance::Unselectable`] rather than state that could never be matched back.
    #[must_use]
    pub fn declared<'a>(
        vary: Option<&str>,
        mut field: impl FnMut(&str) -> Option<&'a str>,
    ) -> Self {
        let Some(vary) = vary else {
            return Self::Invariant;
        };
        let mut entries = Vec::new();

        for name in vary.split(',') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if name == Self::UNSELECTABLE {
                return Self::Unselectable;
            }
            let name = name.to_ascii_lowercase();
            let value = field(&name);
            if name.contains(['\r', '\n'])
                || value.is_some_and(|value| value.contains(['\r', '\n']))
            {
                return Self::Unselectable;
            }
            entries.push((name.into_boxed_str(), value.map(Box::from)));
        }

        if entries.is_empty() {
            return Self::Invariant;
        }
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        entries.dedup_by(|(left, _), (right, _)| left == right);

        Self::Selected(SelectingHeaders { entries })
    }

    /// Record the variance a response declared when the originating request is unavailable.
    ///
    /// A response naming selecting fields becomes [`Variance::Unselectable`]: with no record of the
    /// request that produced it, there is nothing for a later request to match. State recovered
    /// this way therefore supports revalidation only when its response declared no `Vary`.
    #[must_use]
    pub fn declared_without_request(vary: Option<&str>) -> Self {
        match vary {
            Some(vary) if vary.split(',').any(|name| !name.trim().is_empty()) => Self::Unselectable,
            _ => Self::Invariant,
        }
    }

    /// Whether a request selects the representation this state was stored for.
    ///
    /// Validators must not be sent for a request this returns `false` for: the server would answer
    /// about a representation other than the one the request selects.
    ///
    /// `field` follows the same contract as in [`Variance::declared`].
    #[must_use]
    pub fn matches<'a>(&self, mut field: impl FnMut(&str) -> Option<&'a str>) -> bool {
        match self {
            Self::Invariant => true,
            Self::Unselectable => false,
            Self::Selected(headers) => headers
                .entries
                .iter()
                .all(|(name, value)| field(name) == value.as_deref()),
        }
    }

    /// Encode for storage, as `None` for the invariant case that most responses fall in.
    pub(crate) fn encode(&self) -> Option<String> {
        match self {
            Self::Invariant => None,
            Self::Unselectable => Some(Self::UNSELECTABLE.to_owned()),
            Self::Selected(headers) => {
                let mut encoded = String::new();
                for (name, value) in &headers.entries {
                    encoded.push_str(name);
                    encoded.push('\n');
                    match value {
                        // A field value cannot contain a line feed, so the two are unambiguous.
                        Some(value) => {
                            encoded.push('=');
                            encoded.push_str(value);
                        }
                        None => encoded.push('!'),
                    }
                    encoded.push('\n');
                }
                Some(encoded)
            }
        }
    }

    /// Decode a stored encoding, which only [`Variance::encode`] writes.
    pub(crate) fn decode(stored: Option<String>) -> Result<Self, Error> {
        let Some(stored) = stored else {
            return Ok(Self::Invariant);
        };
        if stored == Self::UNSELECTABLE {
            return Ok(Self::Unselectable);
        }

        let malformed = || Error::MalformedVariance {
            value: stored.clone(),
        };
        let mut entries = Vec::new();
        let mut fields = stored.strip_suffix('\n').unwrap_or(&stored).split('\n');

        while let Some(name) = fields.next() {
            let value = fields.next().ok_or_else(malformed)?;
            let value = if let Some(value) = value.strip_prefix('=') {
                Some(Box::from(value))
            } else if value == "!" {
                None
            } else {
                return Err(malformed());
            };
            if name.is_empty() {
                return Err(malformed());
            }
            entries.push((Box::from(name), value));
        }

        Ok(Self::Selected(SelectingHeaders { entries }))
    }
}

/// The values a request carried for the fields a response named in `Vary`.
///
/// Names are lowercased and sorted, so one selection compares equal to another regardless of the
/// order or case the server wrote its `Vary` field in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectingHeaders {
    entries: Vec<(Box<str>, Option<Box<str>>)>,
}

/// HTTP validators and prior representation identity for one resource key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceState {
    /// The resource/request identity.
    pub key: ResourceKey,
    /// The exact `ETag` field value to use in `If-None-Match`.
    pub etag: Option<String>,
    /// The exact `Last-Modified` field value to use in `If-Modified-Since`.
    pub last_modified: Option<String>,
    /// The prior representation's payload digest, when known.
    pub payload_digest: Option<LabelledDigest>,
    /// The prior representation's WARC record identity, when known.
    pub record_id: Option<Uri<String>>,
    /// The prior representation's WARC capture date, when known.
    pub warc_date: Option<WarcDate>,
    /// When this state was most recently observed.
    ///
    /// This is the date of the response or revisit that established or confirmed the state, not
    /// necessarily the date of the canonical payload-bearing record.
    pub observed_at: WarcDate,
    /// Which requests the stored representation was selected by.
    ///
    /// The validators above describe this representation alone, so they may be sent only for a
    /// request [`Variance::matches`] accepts.
    pub variance: Variance,
}

/// A resource-state transition.
///
/// Transitions older than the stored observation are ignored. At equal observation times, the
/// incoming transition is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceStateUpdate {
    /// A successful response carried a representation.
    ///
    /// Validator fields replace the previous representation's validators. In particular, an
    /// omitted validator is cleared rather than incorrectly retained for different bytes.
    Representation {
        /// `ETag`, if present.
        etag: Option<String>,
        /// `Last-Modified`, if present. The HTTP `Date` field is never substituted.
        last_modified: Option<String>,
        /// The representation payload digest, if known.
        payload_digest: Option<LabelledDigest>,
        /// The WARC record representing this capture, if known.
        record_id: Option<Uri<String>>,
        /// The WARC capture date, if known.
        warc_date: Option<WarcDate>,
        /// When this representation was observed.
        observed_at: WarcDate,
        /// Which requests this representation was selected by.
        variance: Variance,
    },
    /// A `304 Not Modified` or `server-not-modified` revisit confirmed the prior representation.
    ///
    /// Present validators replace their stored counterparts; omitted validators and all payload
    /// and WARC identity fields are retained. The stored variance is retained too: a `304` answers
    /// for the representation the request already selected.
    NotModified {
        /// A replacement `ETag`, if the 304 supplies one.
        etag: Option<String>,
        /// A replacement `Last-Modified`, if the 304 supplies one.
        last_modified: Option<String>,
        /// When the unchanged representation was confirmed.
        observed_at: WarcDate,
    },
}

#[cfg(test)]
mod tests {
    use super::Variance;

    /// Resolve field names against a fixed request.
    fn request<'a>(fields: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> {
        move |name| {
            fields
                .iter()
                .find(|(field, _)| *field == name)
                .map(|(_, value)| *value)
        }
    }

    #[test]
    fn a_response_without_vary_is_invariant() {
        let variance = Variance::declared(None, request(&[("user-agent", "Desktop")]));

        assert_eq!(variance, Variance::Invariant);
        assert!(variance.matches(request(&[("user-agent", "Mobile")])));
    }

    #[test]
    fn a_differing_selecting_field_does_not_match() {
        let variance = Variance::declared(
            Some("User-Agent"),
            request(&[("user-agent", "Desktop"), ("accept", "*/*")]),
        );

        assert!(variance.matches(request(&[("user-agent", "Desktop")])));
        assert!(!variance.matches(request(&[("user-agent", "Mobile")])));
        assert!(!variance.matches(request(&[])));
    }

    #[test]
    fn vary_star_never_matches_even_an_identical_request() {
        let fields = [("user-agent", "Desktop")];
        let variance = Variance::declared(Some("User-Agent, *"), request(&fields));

        assert_eq!(variance, Variance::Unselectable);
        assert!(!variance.matches(request(&fields)));
    }

    #[test]
    fn selecting_fields_are_recorded_independently_of_their_written_form() {
        let fields = [("user-agent", "Desktop"), ("accept-encoding", "gzip")];

        assert_eq!(
            Variance::declared(Some("User-Agent, Accept-Encoding"), request(&fields)),
            Variance::declared(Some("accept-encoding ,USER-AGENT"), request(&fields))
        );
    }

    #[test]
    fn a_response_named_vary_is_unselectable_without_its_request() {
        assert_eq!(
            Variance::declared_without_request(Some("Accept-Encoding")),
            Variance::Unselectable
        );
        assert_eq!(
            Variance::declared_without_request(None),
            Variance::Invariant
        );
    }

    #[test]
    fn variances_round_trip_through_their_encoding() {
        let selected = Variance::declared(
            Some("User-Agent, Accept-Encoding"),
            request(&[("user-agent", "Desktop=!")]),
        );

        for variance in [Variance::Invariant, Variance::Unselectable, selected] {
            assert_eq!(Variance::decode(variance.encode()).ok(), Some(variance));
        }
    }
}
