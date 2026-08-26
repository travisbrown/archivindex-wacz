//! Sort-friendly URI Reordering Transform (SURT) keys and the URL canonicalization that
//! precedes them.
//!
//! A SURT reorders a URL's host so that keys for related pages sort together: `www.example.com`
//! becomes `com,example,www`, and everything under `example.com` shares the prefix `com,example`.
//! The form was introduced by [Heritrix][heritrix] as `http://(com,example,www,)/path`; CDX
//! indexes, the Wayback Machine, pywb, and WACZ files use the shorter key form
//! `com,example)/path?query`, which this crate represents as [`Surt`]. The reversible
//! [SSURT][ssurt] variant is available from [`url::Url::ssurt`].
//!
//! Because two URLs only share a key if they were canonicalized the same way, a
//! [`url::Canonicalizer`] defines which rules apply. Its default, [`url::Canonicalizer::WAYBACK`],
//! reproduces the Wayback Machine's `urlkey` (the Python `surt` library, as used by pywb,
//! `cdxj-indexer`, and `py-wacz`), which [`Surt::from_url`] applies:
//!
//! ```
//! use archivindex_surt::Surt;
//!
//! let key = Surt::from_url("https://www.Example.com:443/Movies/?b=2&a=1#top")?;
//!
//! assert_eq!(key.as_str(), "com,example)/movies?a=1&b=2");
//! assert_eq!(key.labels().collect::<Vec<_>>(), ["com", "example"]);
//! assert_eq!(key.url("https").to_string(), "https://example.com/movies?a=1&b=2");
//! # Ok::<_, archivindex_surt::url::Error>(())
//! ```
//!
//! Keys and URLs convert to and from the URL types other crates hand around: `fluent_uri::Uri`
//! under the default `fluent-uri` feature, and `url::Url` under the `url` feature.
//!
//! [heritrix]: http://crawler.archive.org/articles/user_manual/glossary.html#surt
//! [ssurt]: https://github.com/iipc/urlcanon/blob/master/ssurt.rst

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod canonicalize;
mod escape;
mod session;
#[cfg(test)]
mod strategies;
pub mod url;

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// Text is not a SURT key.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    /// The key has no `)` ending its host.
    #[error("not a SURT: `{key}` has no `)`")]
    MissingHostTerminator {
        /// The offending text.
        key: String,
    },
    /// The host has no labels, or an IPv6 literal is unclosed or followed by something other
    /// than a port.
    #[error("not a SURT: `{key}` has a malformed host")]
    MalformedHost {
        /// The offending text.
        key: String,
    },
    /// The port is not a number between 0 and 65535.
    #[error("not a SURT: `{key}` has an invalid port")]
    InvalidPort {
        /// The offending text.
        key: String,
    },
    /// The key contains whitespace or a control character.
    #[error("not a SURT: `{key}` contains whitespace or a control character")]
    InvalidCharacter {
        /// The offending text.
        key: String,
    },
}

/// Byte offsets of a key's components within its text.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Shape {
    /// Index just past the comma-separated labels.
    labels_end: usize,
    /// Index of the `)` that ends the host.
    host_end: usize,
    /// Index of the `?` that begins the query.
    query_start: Option<usize>,
}

/// A SURT key: `com,example[:port])[path][?query]`.
///
/// The host's labels are in reverse DNS order, separated by commas, and followed by `)`; an IPv6
/// literal is a single bracketed label, or a bare label in Wayback-style keys, where whatever
/// follows the address is part of the host rather than a port. Keys sort as text, so a key
/// prefix selects everything beneath it.
///
/// Parsing borrows the text; [`Surt::into_owned`] detaches it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Surt<'a> {
    key: Cow<'a, str>,
    shape: Shape,
}

impl<'a> Surt<'a> {
    /// Parse a key, borrowing the text.
    ///
    /// # Errors
    ///
    /// Fails when the text does not have the shape of a key: a comma-separated host, an optional
    /// numeric port, and `)`, with no whitespace or control characters.
    pub fn parse(key: &'a str) -> Result<Self, Error> {
        Ok(Self {
            key: Cow::Borrowed(key),
            shape: shape(key)?,
        })
    }

    /// Canonicalize a URL with the Wayback Machine's rules and transform it into a key.
    ///
    /// # Errors
    ///
    /// Fails when the URL cannot be split into components; see [`url::Error`].
    pub fn from_url(url: &str) -> Result<Surt<'static>, url::Error> {
        url::Canonicalizer::WAYBACK.surt(url)
    }

    /// Wrap a key rendered from a [`url::Url`], which always has a key's shape.
    pub(crate) fn from_canonical_key(key: String) -> Surt<'static> {
        let shape = shape(&key).expect("keys rendered from URLs are well-formed");

        Surt {
            key: Cow::Owned(key),
            shape,
        }
    }

    /// The key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.key
    }

    /// The comma-separated labels, in reverse DNS order: `com,example`.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.key[..self.shape.labels_end]
    }

    /// The host's labels, in reverse DNS order; `.rev()` yields them in DNS order.
    #[must_use]
    pub fn labels(&self) -> Labels<'_> {
        Labels(self.host().split(','))
    }

    /// The port, if the key has one.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        // The shape check has already confirmed the digits parse.
        (self.shape.labels_end < self.shape.host_end)
            .then(|| {
                self.key[self.shape.labels_end + 1..self.shape.host_end]
                    .parse()
                    .ok()
            })
            .flatten()
    }

    /// The path, which is either empty or starts with `/`.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.key[self.shape.host_end + 1..self.shape.query_start.unwrap_or(self.key.len())]
    }

    /// The query, without the leading `?`, if the key has one.
    #[must_use]
    pub fn query(&self) -> Option<&str> {
        self.shape.query_start.map(|start| &self.key[start + 1..])
    }

    /// The URL the key stands for, given a scheme: `scheme://example.com[:port]/path[?query]`.
    ///
    /// The key does not record its scheme, user information, or fragment, so the result is the
    /// canonical URL rather than the original one. It is rendered on demand, so `to_string` or
    /// `write!` produces the text.
    #[must_use]
    pub fn url<'s>(&'s self, scheme: &'s str) -> impl fmt::Display + 's {
        fmt::from_fn(move |f| {
            write!(f, "{scheme}://")?;

            // The Wayback Machine writes IPv6 addresses bare, but a URL needs them bracketed.
            let host = self.host();
            let bare_ipv6 = host.contains(':') && !host.starts_with('[');

            if bare_ipv6 {
                f.write_str("[")?;
            }

            for (index, label) in self.labels().rev().enumerate() {
                if index > 0 {
                    f.write_str(".")?;
                }

                f.write_str(label)?;
            }

            if bare_ipv6 {
                f.write_str("]")?;
            }

            if let Some(port) = self.port() {
                write!(f, ":{port}")?;
            }

            let path = self.path();
            f.write_str(if path.is_empty() { "/" } else { path })?;

            if let Some(query) = self.query() {
                write!(f, "?{query}")?;
            }

            Ok(())
        })
    }

    /// The URL the key stands for as a [`fluent_uri::Uri`], given a scheme.
    ///
    /// [`url`](Self::url) renders the same text; this parses it, so that the result can be passed
    /// to an interface that takes URIs, such as the WARC record headers of `archivindex-warc`.
    ///
    /// # Errors
    ///
    /// Fails when the canonical URL is not a URI. Escape normalization re-encodes only controls,
    /// space, `#`, `%`, and non-ASCII bytes, so a path can keep characters like `{` or `|` that
    /// RFC 3986 does not allow.
    ///
    /// # Examples
    ///
    /// ```
    /// use archivindex_surt::Surt;
    ///
    /// let key = Surt::parse("com,example)/movies?a=1")?;
    ///
    /// assert_eq!(key.to_uri("https")?.as_str(), "https://example.com/movies?a=1");
    /// # Ok::<_, Box<dyn std::error::Error>>(())
    /// ```
    #[cfg(feature = "fluent-uri")]
    #[cfg_attr(docsrs, doc(cfg(feature = "fluent-uri")))]
    pub fn to_uri(&self, scheme: &str) -> Result<fluent_uri::Uri<String>, fluent_uri::ParseError> {
        // The owned parse returns the input alongside the error, which the caller still holds.
        fluent_uri::Uri::parse(self.url(scheme).to_string()).map_err(|(error, _)| error)
    }

    /// The URL the key stands for as a [`url::Url`](::url::Url), given a scheme.
    ///
    /// [`url`](Self::url) renders the same text; this parses it. The WHATWG rules that parser
    /// follows are more forgiving than RFC 3986, so it accepts keys [`to_uri`](Self::to_uri)
    /// rejects, and it percent-encodes whatever a URI would not allow.
    ///
    /// # Errors
    ///
    /// Fails when the canonical URL is not a URL, which for a well-formed key means a host the
    /// WHATWG parser rejects.
    #[cfg(feature = "url")]
    #[cfg_attr(docsrs, doc(cfg(feature = "url")))]
    pub fn to_url(&self, scheme: &str) -> Result<::url::Url, ::url::ParseError> {
        ::url::Url::parse(&self.url(scheme).to_string())
    }

    /// Detach the key from the text it was parsed from.
    #[must_use]
    pub fn into_owned(self) -> Surt<'static> {
        Surt {
            key: Cow::Owned(self.key.into_owned()),
            shape: self.shape,
        }
    }
}

impl fmt::Display for Surt<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key)
    }
}

impl FromStr for Surt<'static> {
    type Err = Error;

    fn from_str(key: &str) -> Result<Self, Self::Err> {
        Surt::parse(key).map(Surt::into_owned)
    }
}

impl<'a> From<Surt<'a>> for Cow<'a, str> {
    fn from(surt: Surt<'a>) -> Self {
        surt.key
    }
}

impl AsRef<str> for Surt<'_> {
    fn as_ref(&self) -> &str {
        &self.key
    }
}

/// Canonicalizes with the Wayback Machine's rules, as [`Surt::from_url`] does; another convention
/// is applied by passing [`Uri::as_str`](fluent_uri::Uri::as_str) to [`url::Canonicalizer::surt`].
#[cfg(feature = "fluent-uri")]
#[cfg_attr(docsrs, doc(cfg(feature = "fluent-uri")))]
impl TryFrom<&fluent_uri::Uri<String>> for Surt<'static> {
    type Error = url::Error;

    fn try_from(uri: &fluent_uri::Uri<String>) -> Result<Self, Self::Error> {
        Self::from_url(uri.as_str())
    }
}

/// See the implementation for [`fluent_uri::Uri<String>`].
#[cfg(feature = "fluent-uri")]
#[cfg_attr(docsrs, doc(cfg(feature = "fluent-uri")))]
impl TryFrom<fluent_uri::Uri<&str>> for Surt<'static> {
    type Error = url::Error;

    fn try_from(uri: fluent_uri::Uri<&str>) -> Result<Self, Self::Error> {
        Self::from_url(uri.as_str())
    }
}

/// Canonicalizes with the Wayback Machine's rules, as [`Surt::from_url`] does. The URL has already
/// been through the WHATWG parser, which normalizes some of the same things in its own way.
#[cfg(feature = "url")]
#[cfg_attr(docsrs, doc(cfg(feature = "url")))]
impl TryFrom<&::url::Url> for Surt<'static> {
    type Error = url::Error;

    fn try_from(url: &::url::Url) -> Result<Self, Self::Error> {
        Self::from_url(url.as_str())
    }
}

#[cfg(feature = "serde")]
#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]
impl serde::Serialize for Surt<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.key)
    }
}

#[cfg(feature = "serde")]
#[cfg_attr(docsrs, doc(cfg(feature = "serde")))]
impl<'de: 'a, 'a> serde::Deserialize<'de> for Surt<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SurtVisitor;

        impl<'de> serde::de::Visitor<'de> for SurtVisitor {
            type Value = Surt<'de>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a SURT key")
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                key: &'de str,
            ) -> Result<Self::Value, E> {
                Surt::parse(key).map_err(E::custom)
            }

            fn visit_str<E: serde::de::Error>(self, key: &str) -> Result<Self::Value, E> {
                Surt::parse(key).map(Surt::into_owned).map_err(E::custom)
            }

            fn visit_string<E: serde::de::Error>(self, key: String) -> Result<Self::Value, E> {
                let shape = shape(&key).map_err(E::custom)?;

                Ok(Surt {
                    key: Cow::Owned(key),
                    shape,
                })
            }
        }

        // `Surt<'de>` coerces to `Surt<'a>` because `'de: 'a`.
        deserializer.deserialize_str(SurtVisitor)
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::ToBoundedStatic for Surt<'_> {
    type Static = Surt<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::IntoBoundedStatic for Surt<'_> {
    type Static = Surt<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

/// The labels of a key's host, in reverse DNS order; see [`Surt::labels`].
#[derive(Clone, Debug)]
pub struct Labels<'a>(std::str::Split<'a, char>);

impl<'a> Iterator for Labels<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl DoubleEndedIterator for Labels<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back()
    }
}

impl std::iter::FusedIterator for Labels<'_> {}

fn shape(key: &str) -> Result<Shape, Error> {
    if key
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(Error::InvalidCharacter {
            key: key.to_owned(),
        });
    }

    let host_end = key.find(')').ok_or_else(|| Error::MissingHostTerminator {
        key: key.to_owned(),
    })?;
    let host = &key[..host_end];
    let labels_end = if host.starts_with('[') {
        host.find(']').ok_or_else(|| Error::MalformedHost {
            key: key.to_owned(),
        })? + 1
    } else if host.bytes().filter(|&byte| byte == b':').count() > 1 {
        // A bare IPv6 address: nothing after it can be told apart from the address.
        host_end
    } else {
        host.find(':').unwrap_or(host_end)
    };

    if labels_end == 0 {
        return Err(Error::MalformedHost {
            key: key.to_owned(),
        });
    }

    if let Some(rest) = host.get(labels_end..).filter(|rest| !rest.is_empty()) {
        let digits = rest.strip_prefix(':').ok_or_else(|| Error::MalformedHost {
            key: key.to_owned(),
        })?;

        if digits.is_empty()
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
            || digits.parse::<u16>().is_err()
        {
            return Err(Error::InvalidPort {
                key: key.to_owned(),
            });
        }
    }

    let query_start = key[host_end..].find('?').map(|index| host_end + index);

    Ok(Shape {
        labels_end,
        host_end,
        query_start,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parses_keys() {
        let key = Surt::parse("com,example,www:8080)/path/to?a=1&b=2").unwrap();

        assert_eq!(key.host(), "com,example,www");
        assert_eq!(key.labels().collect::<Vec<_>>(), ["com", "example", "www"]);
        assert_eq!(
            key.labels().rev().collect::<Vec<_>>(),
            ["www", "example", "com"]
        );
        assert_eq!(key.port(), Some(8080));
        assert_eq!(key.path(), "/path/to");
        assert_eq!(key.query(), Some("a=1&b=2"));
        assert_eq!(
            key.url("http").to_string(),
            "http://www.example.com:8080/path/to?a=1&b=2"
        );

        let key = Surt::parse("com,example)").unwrap();

        assert_eq!(key.port(), None);
        assert_eq!(key.path(), "");
        assert_eq!(key.query(), None);
        assert_eq!(key.url("https").to_string(), "https://example.com/");

        let key = Surt::parse("[2001:db8::1]:80)/?").unwrap();

        assert_eq!(key.labels().collect::<Vec<_>>(), ["[2001:db8::1]"]);
        assert_eq!(key.port(), Some(80));
        assert_eq!(key.query(), Some(""));
        assert_eq!(key.url("ws").to_string(), "ws://[2001:db8::1]:80/?");
    }

    #[test]
    fn rejects_malformed_keys() {
        let error = |key: &str| Surt::parse(key).unwrap_err();

        assert!(matches!(
            error("com,example/"),
            Error::MissingHostTerminator { .. }
        ));
        assert!(matches!(error(")/"), Error::MalformedHost { .. }));
        assert!(matches!(error("[::1)/"), Error::MalformedHost { .. }));
        assert!(matches!(error("[::1]x)/"), Error::MalformedHost { .. }));
        assert!(matches!(error("com,example:)/"), Error::InvalidPort { .. }));
        assert!(matches!(
            error("com,example:+5)/"),
            Error::InvalidPort { .. }
        ));
        assert!(matches!(
            error("com,example:65536)/"),
            Error::InvalidPort { .. }
        ));
        assert!(matches!(
            error("com,example)/a b"),
            Error::InvalidCharacter { .. }
        ));
    }

    #[test]
    fn converts_and_orders() {
        let key: Surt<'static> = "com,example)/".parse().unwrap();
        let cow: Cow<'_, str> = key.clone().into();

        assert_eq!(cow, "com,example)/");
        assert_eq!(key.to_string(), "com,example)/");
        assert!(key < Surt::parse("com,example)/a").unwrap());
        assert!(key < Surt::parse("com,example,www)/").unwrap());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn round_trips_through_serde() {
        let json = "\"com,example)/path\"";
        let key: Surt<'_> = serde_json::from_str(json).unwrap();

        assert!(matches!(key.key, Cow::Borrowed(_)));
        assert_eq!(serde_json::to_string(&key).unwrap(), json);
        assert!(serde_json::from_str::<Surt<'_>>("\"nope\"").is_err());
    }

    #[test_strategy::proptest]
    fn parsing_preserves_components(
        #[strategy(strategies::key_parts())] parts: strategies::KeyParts,
    ) {
        let text = parts.to_string();
        let key = Surt::parse(&text).unwrap();

        prop_assert_eq!(key.as_str(), text.as_str());
        prop_assert_eq!(key.labels().collect::<Vec<_>>(), parts.labels);
        prop_assert_eq!(key.port(), parts.port);
        prop_assert_eq!(key.path(), parts.path.as_str());
        prop_assert_eq!(key.query(), parts.query.as_deref());
        prop_assert_eq!(key.clone().into_owned(), key);
    }

    #[test]
    fn parses_bare_ipv6_keys() {
        let key = Surt::parse("2001:db8::1:8080)/p").unwrap();

        assert_eq!(key.host(), "2001:db8::1:8080");
        assert_eq!(key.labels().collect::<Vec<_>>(), ["2001:db8::1:8080"]);
        assert_eq!(key.port(), None);
        assert_eq!(key.path(), "/p");
        assert_eq!(key.url("http").to_string(), "http://[2001:db8::1:8080]/p");
        assert_eq!(
            Surt::parse("[2001:db8::1]:8080)/p")
                .unwrap()
                .url("http")
                .to_string(),
            "http://[2001:db8::1]:8080/p"
        );
    }

    #[cfg(feature = "fluent-uri")]
    #[test]
    fn converts_to_and_from_uris() {
        let uri =
            fluent_uri::Uri::parse("https://www.Example.com:443/Movies/?b=2&a=1#top".to_string())
                .unwrap();
        let key = Surt::try_from(&uri).unwrap();

        assert_eq!(key.as_str(), "com,example)/movies?a=1&b=2");
        assert_eq!(
            key.to_uri("https").unwrap().as_str(),
            "https://example.com/movies?a=1&b=2"
        );
    }

    #[cfg(feature = "fluent-uri")]
    #[test]
    fn converts_from_borrowing_uris() {
        let uri = fluent_uri::Uri::parse("http://EXAMPLE.com:80/A/B/").unwrap();

        assert_eq!(Surt::try_from(uri).unwrap().as_str(), "com,example)/a/b");
    }

    #[cfg(feature = "url")]
    #[test]
    fn converts_to_and_from_whatwg_urls() {
        let url = ::url::Url::parse("https://www.Example.com:443/Movies/?b=2&a=1#top").unwrap();
        let key = Surt::try_from(&url).unwrap();

        assert_eq!(key.as_str(), "com,example)/movies?a=1&b=2");
        assert_eq!(
            key.to_url("https").unwrap().as_str(),
            "https://example.com/movies?a=1&b=2"
        );
    }
}
