//! The canonicalization behind [`Canonicalizer`].
//!
//! Every rule set repairs its input the way the Python `surt` library's `handyurl.parse` does
//! (trimming whitespace, removing tabs and newlines, defaulting to `http://`, and unwrapping
//! `http://https://...`), lowercases the scheme, host, path, and query, drops the user
//! information and fragment, drops a default port and an empty query, resolves path dot segments,
//! strips trailing dots from the host, rewrites numeric IPv4 hosts as dotted quads, and encodes
//! non-ASCII hosts as IDNA. The flags on [`Canonicalizer`] select the remaining rules.

use std::borrow::Cow;
use std::fmt::Write;

use crate::url::{self, Canonicalizer, Url};
use crate::{Surt, escape, session};

impl Canonicalizer {
    /// The Wayback Machine's rules, shared by pywb, `cdxj-indexer`, and `py-wacz`.
    pub const WAYBACK: Self = Self {
        strip_www: true,
        strip_session_ids: true,
        strip_trailing_slash: true,
        normalize_escapes: true,
        collapse_slashes: true,
        sort_query: true,
        strip_ipv6_brackets: true,
    };

    /// The rules of `warcio.js`, used by Browsertrix and `wabac.js`.
    ///
    /// These keep a trailing slash and existing escapes, which are lowercased with the rest of
    /// the URL. Characters that a WHATWG URL parser would escape are left as they are.
    pub const WARCIO: Self = Self {
        strip_www: true,
        strip_session_ids: false,
        strip_trailing_slash: false,
        normalize_escapes: false,
        collapse_slashes: false,
        sort_query: true,
        strip_ipv6_brackets: false,
    };

    /// Only the transformation the Heritrix glossary describes: reordering and lowercasing.
    pub const HERITRIX: Self = Self {
        strip_www: false,
        strip_session_ids: false,
        strip_trailing_slash: false,
        normalize_escapes: false,
        collapse_slashes: false,
        sort_query: false,
        strip_ipv6_brackets: false,
    };

    /// Canonicalize a URL and transform it into a SURT key.
    ///
    /// # Errors
    ///
    /// Fails when the URL cannot be split into components; see [`url::Error`].
    pub fn surt(&self, url: &str) -> Result<Surt<'static>, url::Error> {
        let key = self.canonicalize(url)?.surt();

        Ok(if self.strip_ipv6_brackets && key.host().starts_with('[') {
            // The host's brackets are the first `[` and `]` in the key.
            Surt::from_canonical_key(key.as_str().replacen(['[', ']'], "", 2))
        } else {
            key
        })
    }

    /// Canonicalize a URL.
    ///
    /// # Errors
    ///
    /// Fails when the URL cannot be split into components; see [`url::Error`].
    pub fn canonicalize(&self, url: &str) -> Result<Url<'static>, url::Error> {
        let repaired = self.repair(url);
        let parsed = Url::parse(&repaired)?;
        let mut text = String::with_capacity(repaired.len() + 8);
        let scheme = escape::lowercase(parsed.scheme());
        text.push_str(&scheme);
        text.push_str("://");
        self.push_host(&mut text, parsed.host());

        if let Some(port) = parsed
            .port()
            .filter(|&port| default_port(&scheme) != Some(port))
        {
            // Writing to a `String` cannot fail, so the `fmt::Result` carries no information.
            let _ = write!(text, ":{port}");
        }

        self.push_path(&mut text, parsed.path());

        if let Some(query) = parsed.query() {
            self.push_query(&mut text, query);
        }

        Url::from_string(text)
    }

    /// Repair the input as the Python `surt` library's `handyurl.parse` does.
    fn repair(self, url: &str) -> Cow<'_, str> {
        let mut text = Cow::Borrowed(url.trim_matches(|c: char| c.is_ascii_whitespace()));

        if text.contains(['\t', '\r', '\n']) {
            text = Cow::Owned(
                text.chars()
                    .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
                    .collect(),
            );
        }

        if url::scheme_end(&text).is_none() {
            text = Cow::Owned(format!("http://{text}"));
        }

        // A URL wrapped in another's scheme, like `http://https://example.com/`.
        let mut start = 0;

        while let Some(rest) = strip_http_prefix(&text[start..])
            && strip_http_prefix(rest).is_some()
        {
            start = text.len() - rest.len();
        }

        if start > 0 {
            text = match text {
                Cow::Borrowed(borrowed) => Cow::Borrowed(&borrowed[start..]),
                Cow::Owned(owned) => Cow::Owned(owned[start..].to_owned()),
            };
        }

        // Extra slashes before the host, like `http:////example.com/`.
        if let Some(rest) = strip_http_prefix(&text)
            && rest.starts_with('/')
        {
            let prefix = &text[..text.len() - rest.len()];
            text = Cow::Owned(format!("{prefix}{}", rest.trim_start_matches('/')));
        }

        if self.normalize_escapes && text.contains(' ') {
            text = Cow::Owned(text.replace(' ', "%20"));
        }

        text
    }

    fn push_host(self, output: &mut String, host: &str) {
        if host.starts_with('[') {
            output.push_str(&escape::lowercase(host));
            return;
        }

        let unescaped = if self.normalize_escapes {
            match String::from_utf8(escape::unescape_repeatedly(host.as_bytes())) {
                Ok(text) => Cow::Owned(text),
                // Not text, so not a domain name: keep the bytes, escaped.
                Err(error) => {
                    let mut bytes = error.into_bytes();
                    bytes.make_ascii_lowercase();
                    escape::escape_once_into(output, &bytes);
                    return;
                }
            }
        } else {
            Cow::Borrowed(host)
        };
        let dedotted = if self.normalize_escapes && unescaped.contains("..") {
            Cow::Owned(unescaped.replace("..", "."))
        } else {
            unescaped
        };
        let trimmed = dedotted.trim_matches('.');
        let lowered = escape::lowercase(trimmed);
        let host = if self.strip_www {
            strip_www(&lowered)
        } else {
            &lowered
        };

        // Unlike the Python library, `www` is stripped before the numeric check, so that
        // `www.0` and `0.0.0.0` reach the same key on every pass.
        if let Some(address) = escape::numeric_ipv4(host) {
            let _ = write!(output, "{address}");
            return;
        }

        for (index, label) in host.split('.').enumerate() {
            if index > 0 {
                output.push('.');
            }

            if let Some(encoded) = (!label.is_ascii())
                .then(|| escape::idna_label(label))
                .flatten()
            {
                output.push_str(&encoded);
            } else if self.normalize_escapes {
                escape::escape_once_into(output, label.as_bytes());
            } else {
                output.push_str(label);
            }
        }
    }

    fn push_path(self, output: &mut String, path: &str) {
        let escaped = escape::normalize_escapes(path, self.normalize_escapes);
        let normalized = escape::normalize_path(&escaped, self.collapse_slashes);
        let lowered = escape::lowercase(&normalized);
        let stripped = if self.strip_session_ids {
            session::strip_path(&lowered)
        } else {
            Cow::Borrowed(&*lowered)
        };
        let trimmed = if self.strip_trailing_slash && stripped.len() > 1 {
            stripped.strip_suffix('/').unwrap_or(&stripped)
        } else {
            &stripped
        };

        output.push_str(trimmed);
    }

    fn push_query(self, output: &mut String, query: &str) {
        let escaped = escape::normalize_escapes(query, self.normalize_escapes);
        let stripped = if self.strip_session_ids {
            session::strip_query(&escaped)
        } else {
            Cow::Borrowed(&*escaped)
        };

        if stripped.is_empty() {
            return;
        }

        let lowered = escape::lowercase(&stripped);
        output.push('?');

        if self.sort_query && lowered.contains('&') {
            let mut parameters: Vec<&str> = lowered.split('&').collect();
            // Python compares `(name, value)` tuples, so a bare name sorts before `name=`.
            parameters.sort_by_key(|parameter| {
                parameter
                    .split_once('=')
                    .map_or((*parameter, None), |(name, value)| (name, Some(value)))
            });

            for (index, parameter) in parameters.into_iter().enumerate() {
                if index > 0 {
                    output.push('&');
                }

                output.push_str(parameter);
            }
        } else {
            output.push_str(&lowered);
        }
    }
}

impl Default for Canonicalizer {
    /// [`Canonicalizer::WAYBACK`], the convention of the WACZ ecosystem.
    fn default() -> Self {
        Self::WAYBACK
    }
}

const fn default_port(scheme: &str) -> Option<u16> {
    match scheme.as_bytes() {
        b"http" => Some(80),
        b"https" => Some(443),
        _ => None,
    }
}

fn strip_http_prefix(text: &str) -> Option<&str> {
    text.strip_prefix("http://")
        .or_else(|| text.strip_prefix("https://"))
}

/// Remove a leading `www`, `www2`, ... label, unless nothing would remain.
fn strip_www(host: &str) -> &str {
    host.strip_prefix("www")
        .and_then(|rest| {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();

            rest[digits..].strip_prefix('.')
        })
        .filter(|rest| !rest.is_empty())
        .unwrap_or(host)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    fn wayback(url: &str) -> String {
        Canonicalizer::WAYBACK.surt(url).unwrap().to_string()
    }

    #[test]
    fn matches_python_surt_examples() {
        assert_eq!(wayback("http://www.archive.org/"), "org,archive)/");
        assert_eq!(wayback("http://archive.org/goo/"), "org,archive)/goo");
        assert_eq!(wayback("http://archive.org/goo/?"), "org,archive)/goo");
        assert_eq!(
            wayback("http://archive.org/goo/?b&a"),
            "org,archive)/goo?a&b"
        );
        assert_eq!(
            wayback("http://archive.org/goo/?a=2&b&a=1"),
            "org,archive)/goo?a=1&a=2&b"
        );
        assert_eq!(
            wayback(
                "http://archive.org/index.php?PHPSESSID=0123456789abcdefghijklemopqrstuv&action=profile;u=4221"
            ),
            "org,archive)/index.php?action=profile;u=4221"
        );
        assert_eq!(
            wayback("whois://whois.isoc.org.il/shaveh.co.il"),
            "il,org,isoc,whois)/shaveh.co.il"
        );
        assert_eq!(
            wayback(
                "http://visit.webhosting.yahoo.com/visit.gif?&r=http%3A//web.archive.org/web/20090517140029/http%3A//anthonystewarthead.electric-chi.com/&b=Netscape%205.0%20%28Windows%3B%20en-US%29&s=1366x768&o=Win32&c=24&j=true&v=1.2"
            ),
            "com,yahoo,webhosting,visit)/visit.gif?&b=netscape%205.0%20(windows;%20en-us)&c=24&j=true&o=win32&r=http://web.archive.org/web/20090517140029/http://anthonystewarthead.electric-chi.com/&s=1366x768&v=1.2"
        );
        assert_eq!(
            wayback("http://example.com/app?item=Wroc%C5%82aw"),
            "com,example)/app?item=wroc%c5%82aw"
        );
        assert_eq!(wayback("http://192.168.1.254/info/"), "254,1,168,192)/info");
        assert_eq!(
            wayback("http://example.com/(S(4hqa0555fwsecu455xqckv45))/mileg.aspx"),
            "com,example)/mileg.aspx"
        );
    }

    #[test]
    fn matches_google_canonicalizer_examples() {
        assert_eq!(wayback("http://host/%25%32%35"), "host)/%25");
        assert_eq!(wayback("http://www.google.com/blah/.."), "com,google)/");
        assert_eq!(
            wayback("http://host.com//twoslashes?more//slashes"),
            "com,host)/twoslashes?more//slashes"
        );
        assert_eq!(
            wayback("http://www.google.com/foo\tbar\rbaz\n2"),
            "com,google)/foobarbaz2"
        );
        assert_eq!(wayback("http://3279880203/blah"), "11,0,127,195)/blah");
        assert_eq!(wayback("http://www.google.com.../"), "com,google)/");
        assert_eq!(wayback("http://evil.com/foo;"), "com,evil)/foo;");
        assert_eq!(wayback("http://www.google.com/q?r?"), "com,google)/q?r?");
        assert_eq!(wayback("  http://www.google.com/  "), "com,google)/");
        assert_eq!(
            wayback("http:// leadingspace.com/"),
            "com,%20leadingspace)/"
        );
        assert_eq!(wayback("http://%01%80.com/"), "com,%01%80)/");
        assert_eq!(wayback("http://%01%C3%BC.com/"), "com,%01%c3%bc)/");
        assert_eq!(wayback("http://bücher.ch/"), "ch,xn--bcher-kva)/");
        assert_eq!(wayback("http://B%C3%BCcher.ch/"), "ch,xn--bcher-kva)/");
        assert_eq!(wayback("http://☃.example/"), "example,xn--n3h)/");
    }

    #[test]
    fn repairs_input() {
        assert_eq!(wayback("www.google.com/"), "com,google)/");
        assert_eq!(
            wayback("http://https://order.1and1.com"),
            "com,1and1,order)/"
        );
        assert_eq!(wayback("http:////www.vikings.com/"), "com,vikings)/");
        assert_eq!(
            wayback("https://///EXAMPLE.com:80/foo/../bar"),
            "com,example:80)/bar"
        );
    }

    #[test]
    fn handles_ports_userinfo_and_fragments() {
        assert_eq!(wayback("http://www.archive.org:80/"), "org,archive)/");
        assert_eq!(wayback("https://www.archive.org:443/"), "org,archive)/");
        assert_eq!(wayback("https://www.archive.org:80/"), "org,archive:80)/");
        assert_eq!(
            wayback("http://user:pass@archive.org:8080/"),
            "org,archive:8080)/"
        );
        assert_eq!(wayback("http://example.com/#frag"), "com,example)/");
        assert_eq!(wayback("http://example.com:/"), "com,example)/");
        assert_eq!(
            wayback("HTTP://ARCHIVE.ORG/Path/To?Q=V"),
            "org,archive)/path/to?q=v"
        );
        assert_eq!(wayback("http://[2001:DB8::1]:8080/"), "2001:db8::1:8080)/");
    }

    #[test]
    fn rules_can_be_disabled() {
        let keep_www = Canonicalizer {
            strip_www: false,
            ..Canonicalizer::WAYBACK
        };

        assert_eq!(
            keep_www.surt("http://www.example.com/").unwrap().as_str(),
            "com,example,www)/"
        );
        assert_eq!(
            keep_www.surt("http://www.com/").unwrap().as_str(),
            "com,www)/"
        );
        assert_eq!(
            Canonicalizer::WAYBACK
                .surt("http://www2.example.com/")
                .unwrap()
                .as_str(),
            "com,example)/"
        );
        assert_eq!(
            Canonicalizer::WAYBACK
                .surt("http://www.com/")
                .unwrap()
                .as_str(),
            "com)/"
        );
        assert_eq!(
            Canonicalizer::WAYBACK
                .surt("http://www./")
                .unwrap()
                .as_str(),
            "www)/"
        );
    }

    #[test]
    fn follows_warcio_rules() {
        let warcio = |url: &str| Canonicalizer::WARCIO.surt(url).unwrap().to_string();

        assert_eq!(
            warcio("https://www.Example.com/Path/"),
            "com,example)/path/"
        );
        assert_eq!(
            warcio("http://example.com/a%7Bb%7D"),
            "com,example)/a%7bb%7d"
        );
        assert_eq!(
            warcio("http://example.com//a/./b/../c"),
            "com,example)//a/c"
        );
        assert_eq!(
            warcio("http://example.com:8080/?b=2&a=1"),
            "com,example:8080)/?a=1&b=2"
        );
        assert_eq!(warcio("http://example.com/?"), "com,example)/");
        assert_eq!(
            warcio("http://example.com/?jsessionid=0123456789abcdefghijklemopqrstuv"),
            "com,example)/?jsessionid=0123456789abcdefghijklemopqrstuv"
        );
    }

    #[test]
    fn follows_heritrix_rules() {
        let url = Canonicalizer::HERITRIX
            .canonicalize("HTTPS://www.Example.com:8080/Movies?B=2&A=1#x")
            .unwrap();

        assert_eq!(url.as_str(), "https://www.example.com:8080/movies?b=2&a=1");
        assert_eq!(
            url.heritrix(),
            "http://(com,example,www,:8080)/movies?b=2&a=1"
        );
    }

    #[test]
    fn rejects_opaque_urls() {
        assert!(matches!(
            Canonicalizer::WAYBACK.surt("dns:archive.org"),
            Err(url::Error::Opaque { .. })
        ));
        assert!(matches!(
            Canonicalizer::WAYBACK.surt("mailto:foo@example.com"),
            Err(url::Error::Opaque { .. })
        ));
        assert!(matches!(
            Canonicalizer::WAYBACK.surt(""),
            Err(url::Error::MissingHost { .. })
        ));
    }

    #[test]
    fn default_is_wayback() {
        assert_eq!(Canonicalizer::default(), Canonicalizer::WAYBACK);
    }

    fn is_sorted(query: &str) -> bool {
        let keys: Vec<_> = query
            .split('&')
            .map(|parameter| {
                parameter
                    .split_once('=')
                    .map_or((parameter, None), |(name, value)| (name, Some(value)))
            })
            .collect();

        keys.windows(2).all(|pair| pair[0] <= pair[1])
    }

    #[test_strategy::proptest]
    fn wayback_keys_are_canonical(#[strategy(strategies::url())] url: String) {
        let key = Canonicalizer::WAYBACK.surt(&url).unwrap();

        prop_assert!(!key.as_str().bytes().any(|byte| byte.is_ascii_uppercase()));
        prop_assert!(key.path() == "/" || !key.path().ends_with('/'));
        prop_assert!(!key.path().contains("//"));
        prop_assert!(key.query() != Some(""));
        prop_assert!(key.query().is_none_or(is_sorted));
        prop_assert!(!key.labels().next_back().unwrap().starts_with("www"));
    }

    #[test_strategy::proptest]
    fn wayback_is_idempotent(#[strategy(strategies::url())] url: String) {
        let first = Canonicalizer::WAYBACK.canonicalize(&url).unwrap();
        let second = Canonicalizer::WAYBACK.canonicalize(first.as_str()).unwrap();

        prop_assert_eq!(&first, &second);

        let key = first.surt();
        let via_key = Canonicalizer::WAYBACK
            .surt(&key.url(first.scheme()).to_string())
            .unwrap();

        prop_assert_eq!(via_key, key);
    }

    #[test_strategy::proptest]
    fn warcio_and_heritrix_are_idempotent(#[strategy(strategies::url())] url: String) {
        for rules in [Canonicalizer::WARCIO, Canonicalizer::HERITRIX] {
            let first = rules.canonicalize(&url).unwrap();
            let second = rules.canonicalize(first.as_str()).unwrap();

            prop_assert_eq!(&first, &second);
        }
    }

    #[test]
    fn strips_ipv6_brackets_only_for_wayback() {
        let url = "http://[2001:DB8::1]:8080/p";

        assert_eq!(
            Canonicalizer::WAYBACK.surt(url).unwrap().as_str(),
            "2001:db8::1:8080)/p"
        );
        assert_eq!(
            Canonicalizer::WARCIO.surt(url).unwrap().as_str(),
            "[2001:db8::1]:8080)/p"
        );
    }
}
