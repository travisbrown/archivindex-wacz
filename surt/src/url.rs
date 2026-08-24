//! Canonicalizing hierarchical URLs and splitting them into their components.
//!
//! [`Url`] is a lightweight split of `scheme://[userinfo@]host[:port][/path][?query][#fragment]`
//! for the URIs that appear in web archives, not a validating parser: it locates the components
//! and leaves their contents alone. The transforms that reorder a URL's host are rendered from it:
//! [`Url::surt`], [`Url::heritrix`], and [`Url::ssurt`]. A [`Canonicalizer`] applies a
//! convention's normalization rules to URL text first, so that equivalent URLs render the same.

use std::borrow::Cow;
use std::fmt::{self, Write};
use std::net::Ipv4Addr;

use crate::Surt;

/// A URL could not be split into components.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    /// The text does not start with a scheme.
    #[error("not a URL: `{url}` has no scheme")]
    MissingScheme {
        /// The offending text.
        url: String,
    },
    /// The URL has no `//` authority, like `dns:example.com` or `urn:uuid:...`.
    #[error("not a hierarchical URL: `{url}` has no `//` authority")]
    Opaque {
        /// The offending URL.
        url: String,
    },
    /// The authority has no host.
    #[error("`{url}` has no host")]
    MissingHost {
        /// The offending URL.
        url: String,
    },
    /// The host is not a domain name, IPv4 address, or bracketed IPv6 literal.
    #[error("`{url}` has a malformed host")]
    MalformedHost {
        /// The offending URL.
        url: String,
    },
    /// The port is not a number between 0 and 65535.
    #[error("`{url}` has an invalid port")]
    InvalidPort {
        /// The offending URL.
        url: String,
    },
    /// The text contains whitespace or a control character.
    #[error("`{url}` contains whitespace or a control character")]
    InvalidCharacter {
        /// The offending text.
        url: String,
    },
}

/// Byte offsets of a URL's components within its text.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Layout {
    /// Index of the `:` that ends the scheme.
    scheme_end: usize,
    /// Index of the first byte of the host.
    host_start: usize,
    /// Index just past the host.
    host_end: usize,
    /// Index of the first byte of the path (the end of the authority).
    path_start: usize,
    /// Index of the `?` that begins the query.
    query_start: Option<usize>,
    /// Index of the `#` that begins the fragment.
    fragment_start: Option<usize>,
    /// The port, if one was given.
    port: Option<u16>,
}

/// The canonicalization rules applied to a URL before it is transformed into a [`crate::Surt`].
///
/// Presets cover the conventions in use: [`Canonicalizer::WAYBACK`] reproduces the Wayback
/// Machine's `urlkey` (the Python `surt` library, used by pywb, `cdxj-indexer`, and `py-wacz`),
/// [`Canonicalizer::WARCIO`] follows `warcio.js` (Browsertrix and `wabac.js`), and
/// [`Canonicalizer::HERITRIX`] performs only the reordering and lowercasing the Heritrix
/// glossary describes. Rules can also be combined freely from a preset:
/// `Canonicalizer { strip_www: false, ..Canonicalizer::WAYBACK }`.
///
/// Whichever flags are set, the input is repaired the way the Python library does (whitespace
/// trimmed, tabs and newlines removed, `http://` defaulted, `http://https://...` unwrapped),
/// the scheme, host, path, and query are lowercased, the user information and fragment are
/// dropped along with a default port or an empty query, `.` and `..` path segments are
/// resolved (keeping a `..` with nothing above it, as the Python library does), trailing dots are stripped from the host, numeric IPv4 hosts become dotted quads,
/// and non-ASCII hosts are encoded as IDNA.
// The flags are independent, so they are not a state machine in disguise.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Canonicalizer {
    /// Drop a leading `www`, `www2`, ... host label.
    pub strip_www: bool,
    /// Remove known session identifiers (`jsessionid`, `phpsessid`, `sid`, `ASPSESSIONID`,
    /// `cfid`/`cftoken`, and ASP.NET cookieless path segments) from the path and query.
    pub strip_session_ids: bool,
    /// Drop a trailing `/` from a path other than `/`.
    pub strip_trailing_slash: bool,
    /// Decode percent-escapes repeatedly, then re-encode only controls, space, `#`, `%`, and
    /// non-ASCII bytes, so that `%7B`, `%7b`, and `{` are all `{` and `%2520` is `%20`.
    pub normalize_escapes: bool,
    /// Collapse runs of `/` in the path, so that `/a//b` is `/a/b`.
    pub collapse_slashes: bool,
    /// Sort query parameters by name, then value.
    pub sort_query: bool,
    /// Drop the brackets around an IPv6 host, as the Python library does: `2001:db8::1:8080)/`
    /// rather than `[2001:db8::1]:8080)/`.
    pub strip_ipv6_brackets: bool,
}

/// A hierarchical URL split into its components.
///
/// Parsing borrows the text; [`Url::into_owned`] detaches it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Url<'a> {
    text: Cow<'a, str>,
    layout: Layout,
}

impl<'a> Url<'a> {
    /// Split a URL into its components.
    ///
    /// # Errors
    ///
    /// Fails when the text has no scheme or authority, when the host is missing or malformed,
    /// when the port is not a number, or when the text contains whitespace or control characters.
    pub fn parse(url: &'a str) -> Result<Self, Error> {
        Ok(Self {
            text: Cow::Borrowed(url),
            layout: locate(url)?,
        })
    }

    /// Wrap text that is already known to be a URL, re-locating its components.
    pub(crate) fn from_string(text: String) -> Result<Url<'static>, Error> {
        Ok(Url {
            layout: locate(&text)?,
            text: Cow::Owned(text),
        })
    }

    /// The URL text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The scheme, without the trailing `:`.
    #[must_use]
    pub fn scheme(&self) -> &str {
        &self.text[..self.layout.scheme_end]
    }

    /// The user information, without the trailing `@`, if present.
    #[must_use]
    pub fn userinfo(&self) -> Option<&str> {
        let start = self.layout.scheme_end + 3;

        (self.layout.host_start > start).then(|| &self.text[start..self.layout.host_start - 1])
    }

    /// The host: a domain name, an IPv4 address, or a bracketed IPv6 literal.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.text[self.layout.host_start..self.layout.host_end]
    }

    /// The port, if one was given.
    #[must_use]
    pub const fn port(&self) -> Option<u16> {
        self.layout.port
    }

    /// The path, which is either empty or starts with `/`.
    #[must_use]
    pub fn path(&self) -> &str {
        let end = self
            .layout
            .query_start
            .or(self.layout.fragment_start)
            .unwrap_or(self.text.len());

        &self.text[self.layout.path_start..end]
    }

    /// The query, without the leading `?`, if present.
    #[must_use]
    pub fn query(&self) -> Option<&str> {
        let end = self.layout.fragment_start.unwrap_or(self.text.len());

        self.layout
            .query_start
            .map(|start| &self.text[start + 1..end])
    }

    /// The fragment, without the leading `#`, if present.
    #[must_use]
    pub fn fragment(&self) -> Option<&str> {
        self.layout
            .fragment_start
            .map(|start| &self.text[start + 1..])
    }

    /// Detach the URL from the text it was parsed from.
    #[must_use]
    pub fn into_owned(self) -> Url<'static> {
        Url {
            text: Cow::Owned(self.text.into_owned()),
            layout: self.layout,
        }
    }

    /// The SURT key: `com,example[:port])[path][?query]`.
    ///
    /// The host's labels are reversed and joined with commas (IPv4 addresses included, as the
    /// Wayback Machine does; IPv6 literals are kept whole). The user information and fragment are
    /// dropped, and a path is supplied as `/` when there is none but there is a query. No other
    /// canonicalization is applied; see [`Canonicalizer`] for that.
    #[must_use]
    pub fn surt(&self) -> Surt<'static> {
        let mut key = String::with_capacity(self.text.len());
        push_reversed_host(&mut key, self.host(), false);

        if let Some(port) = self.layout.port {
            // Writing to a `String` cannot fail, so the `fmt::Result` carries no information.
            let _ = write!(key, ":{port}");
        }

        key.push(')');
        let path = self.path();

        if path.is_empty() && self.layout.query_start.is_some() {
            key.push('/');
        }

        key.push_str(path);

        if let Some(query) = self.query() {
            key.push('?');
            key.push_str(query);
        }

        Surt::from_canonical_key(key)
    }

    /// The SURT in Heritrix's form: `http://(com,example,[:port])[path][?query]`.
    ///
    /// As the Heritrix glossary specifies, `https` becomes `http`, the reordered host is
    /// parenthesized with a trailing comma, and the `/` after the host appears only if the URL
    /// had one. The user information and fragment are dropped and the scheme is lowercased;
    /// lowercasing the rest is left to a canonicalizer, such as
    /// [`Canonicalizer::HERITRIX`].
    #[must_use]
    pub fn heritrix(&self) -> String {
        let mut output = String::with_capacity(self.text.len() + 3);

        if self.scheme().eq_ignore_ascii_case("https") {
            output.push_str("http");
        } else {
            // Schemes are ASCII by construction, so lowercasing byte by byte is sound.
            output.extend(
                self.scheme()
                    .bytes()
                    .map(|byte| byte.to_ascii_lowercase() as char),
            );
        }

        output.push_str("://(");
        push_reversed_host(&mut output, self.host(), true);

        if let Some(port) = self.layout.port {
            let _ = write!(output, ":{port}");
        }

        output.push(')');
        output.push_str(self.path());

        if let Some(query) = self.query() {
            output.push('?');
            output.push_str(query);
        }

        output
    }

    /// The [SSURT](https://github.com/iipc/urlcanon/blob/master/ssurt.rst):
    /// `com,example,//[port:]scheme[@userinfo]:[path][?query][#fragment]`.
    ///
    /// SSURT is reversible, so every component is kept and nothing is canonicalized. Domain names
    /// are reversed with a trailing comma; IPv4 addresses and IPv6 literals are kept whole. As in
    /// the `urlcanon` implementation, the `:` before the scheme appears only when a port does.
    #[must_use]
    pub fn ssurt(&self) -> String {
        let mut output = String::with_capacity(self.text.len() + 2);
        let host = self.host();

        if host.starts_with('[') || host.parse::<Ipv4Addr>().is_ok() {
            output.push_str(host);
        } else {
            push_reversed_host(&mut output, host, true);
        }

        output.push_str("//");

        if let Some(port) = self.layout.port {
            let _ = write!(output, "{port}:");
        }

        output.push_str(self.scheme());

        if let Some(userinfo) = self.userinfo() {
            output.push('@');
            output.push_str(userinfo);
        }

        output.push(':');
        output.push_str(&self.text[self.layout.path_start..]);
        output
    }
}

impl fmt::Display for Url<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Append a host with its labels reversed and comma-separated; IPv6 literals are kept whole.
fn push_reversed_host(output: &mut String, host: &str, trailing_comma: bool) {
    if host.starts_with('[') {
        output.push_str(host);
        return;
    }

    for label in host.rsplit('.') {
        output.push_str(label);
        output.push(',');
    }

    if !trailing_comma {
        output.pop();
    }
}

/// The index of the `:` ending a scheme (`[a-zA-Z][a-zA-Z0-9+.-]*`), if the text has one.
#[must_use]
pub(crate) fn scheme_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();

    if !bytes.first()?.is_ascii_alphabetic() {
        return None;
    }

    let end = bytes
        .iter()
        .position(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')))?;

    (bytes[end] == b':').then_some(end)
}

fn locate(url: &str) -> Result<Layout, Error> {
    let bytes = url.as_bytes();

    if bytes
        .iter()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(Error::InvalidCharacter {
            url: url.to_owned(),
        });
    }

    let scheme_end = scheme_end(url).ok_or_else(|| Error::MissingScheme {
        url: url.to_owned(),
    })?;

    if !url[scheme_end + 1..].starts_with("//") {
        return Err(Error::Opaque {
            url: url.to_owned(),
        });
    }

    let authority_start = scheme_end + 3;
    let path_start = url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |index| authority_start + index);
    let authority = &url[authority_start..path_start];
    // Like WHATWG and Python's `urlsplit`, the last `@` ends the user information.
    let host_start = authority
        .rfind('@')
        .map_or(authority_start, |index| authority_start + index + 1);
    let host_part = &url[host_start..path_start];
    let host_len = if host_part.starts_with('[') {
        host_part.find(']').ok_or_else(|| Error::MalformedHost {
            url: url.to_owned(),
        })? + 1
    } else {
        host_part.find(':').unwrap_or(host_part.len())
    };
    let host = &host_part[..host_len];

    if host.is_empty() {
        return Err(Error::MissingHost {
            url: url.to_owned(),
        });
    }

    // These would be misread as key structure by `Surt`.
    if host.contains([')', ',']) {
        return Err(Error::MalformedHost {
            url: url.to_owned(),
        });
    }

    let port = match &host_part[host_len..] {
        "" => None,
        rest => {
            let digits = rest.strip_prefix(':').ok_or_else(|| Error::MalformedHost {
                url: url.to_owned(),
            })?;

            if digits.is_empty() {
                None
            } else if digits.bytes().all(|byte| byte.is_ascii_digit()) {
                Some(digits.parse().map_err(|_| Error::InvalidPort {
                    url: url.to_owned(),
                })?)
            } else {
                return Err(Error::InvalidPort {
                    url: url.to_owned(),
                });
            }
        }
    };

    let query_start = url[path_start..]
        .find(['?', '#'])
        .map(|index| path_start + index)
        .filter(|&index| bytes[index] == b'?');
    let fragment_start = url[path_start..].find('#').map(|index| path_start + index);

    Ok(Layout {
        scheme_end,
        host_start,
        host_end: host_start + host_len,
        path_start,
        query_start,
        fragment_start,
        port,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    #[test]
    fn splits_components() {
        let url = Url::parse("HTTPS://user:pass@Example.com:8080/a/b?c=d&e#frag").unwrap();

        assert_eq!(url.scheme(), "HTTPS");
        assert_eq!(url.userinfo(), Some("user:pass"));
        assert_eq!(url.host(), "Example.com");
        assert_eq!(url.port(), Some(8080));
        assert_eq!(url.path(), "/a/b");
        assert_eq!(url.query(), Some("c=d&e"));
        assert_eq!(url.fragment(), Some("frag"));
    }

    #[test]
    fn handles_absent_components() {
        let url = Url::parse("http://example.com").unwrap();

        assert_eq!(url.userinfo(), None);
        assert_eq!(url.port(), None);
        assert_eq!(url.path(), "");
        assert_eq!(url.query(), None);
        assert_eq!(url.fragment(), None);

        let url = Url::parse("http://example.com:?#f").unwrap();

        assert_eq!(url.port(), None);
        assert_eq!(url.path(), "");
        assert_eq!(url.query(), Some(""));
        assert_eq!(url.fragment(), Some("f"));

        let url = Url::parse("http://example.com#f?notquery").unwrap();

        assert_eq!(url.query(), None);
        assert_eq!(url.fragment(), Some("f?notquery"));
    }

    #[test]
    fn handles_ip_literals() {
        let url = Url::parse("ws://[2001:db8::1]:80/chat").unwrap();

        assert_eq!(url.host(), "[2001:db8::1]");
        assert_eq!(url.port(), Some(80));
        assert_eq!(url.surt().as_str(), "[2001:db8::1]:80)/chat");
        assert_eq!(url.ssurt(), "[2001:db8::1]//80:ws:/chat");

        let url = Url::parse("http://10.0.0.1/").unwrap();

        assert_eq!(url.surt().as_str(), "1,0,0,10)/");
        assert_eq!(url.ssurt(), "10.0.0.1//http:/");
    }

    #[test]
    fn rejects_malformed_urls() {
        let error = |url: &str| Url::parse(url).unwrap_err();

        assert!(matches!(error("example.com/"), Error::MissingScheme { .. }));
        assert!(matches!(error("dns:example.com"), Error::Opaque { .. }));
        assert!(matches!(error("http:///path"), Error::MissingHost { .. }));
        assert!(matches!(
            error("http://user@/path"),
            Error::MissingHost { .. }
        ));
        assert!(matches!(error("http://[::1/"), Error::MalformedHost { .. }));
        assert!(matches!(
            error("http://[::1]x/"),
            Error::MalformedHost { .. }
        ));
        assert!(matches!(
            error("http://a,b.com/"),
            Error::MalformedHost { .. }
        ));
        assert!(matches!(
            error("http://example.com:x/"),
            Error::InvalidPort { .. }
        ));
        assert!(matches!(
            error("http://example.com:70000/"),
            Error::InvalidPort { .. }
        ));
        assert!(matches!(
            error("http://example.com/a b"),
            Error::InvalidCharacter { .. }
        ));
    }

    #[test]
    fn renders_surt_forms() {
        let url = Url::parse("https://user@www.example.com:8000/movies?a=1#top").unwrap();

        assert_eq!(url.surt().as_str(), "com,example,www:8000)/movies?a=1");
        assert_eq!(url.heritrix(), "http://(com,example,www,:8000)/movies?a=1");
        assert_eq!(
            url.ssurt(),
            "com,example,www,//8000:https@user:/movies?a=1#top"
        );

        let url = Url::parse("http://www.example.com").unwrap();

        assert_eq!(url.surt().as_str(), "com,example,www)");
        assert_eq!(url.heritrix(), "http://(com,example,www,)");
        assert_eq!(url.ssurt(), "com,example,www,//http:");

        assert_eq!(
            Url::parse("http://example.com?q").unwrap().surt().as_str(),
            "com,example)/?q"
        );
        assert_eq!(
            Url::parse("http://example.com/bar").unwrap().ssurt(),
            "com,example,//http:/bar"
        );
    }

    #[test_strategy::proptest]
    fn parsing_preserves_components(
        #[strategy(strategies::url_parts())] parts: strategies::UrlParts,
    ) {
        let text = parts.to_string();
        let url = Url::parse(&text).unwrap();

        prop_assert_eq!(url.as_str(), text.as_str());
        prop_assert_eq!(url.scheme(), parts.scheme.as_str());
        prop_assert_eq!(url.userinfo(), parts.userinfo.as_deref());
        prop_assert_eq!(url.host(), parts.host.as_str());
        prop_assert_eq!(url.port(), parts.port);
        prop_assert_eq!(url.path(), parts.path.as_str());
        prop_assert_eq!(url.query(), parts.query.as_deref());
        prop_assert_eq!(url.fragment(), parts.fragment.as_deref());
        prop_assert_eq!(url.clone().into_owned(), url);
    }

    #[test_strategy::proptest]
    fn surt_forms_agree(#[strategy(strategies::url_parts())] parts: strategies::UrlParts) {
        let text = parts.to_string();
        let url = Url::parse(&text).unwrap();
        let key = url.surt();
        let reversed: Vec<&str> = parts.host.rsplit('.').collect();
        let joined = reversed.join(",");
        let heritrix_host = format!("://({joined},");
        let ssurt_prefix = format!("{joined},//");

        prop_assert_eq!(key.labels().collect::<Vec<_>>(), reversed);
        prop_assert_eq!(key.port(), parts.port);
        prop_assert!(url.heritrix().contains(&heritrix_host));
        prop_assert!(url.ssurt().starts_with(&ssurt_prefix));
    }
}
