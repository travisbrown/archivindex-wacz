//! Property-testing strategies for keys and URLs.

use std::fmt;

use proptest::prelude::*;
use proptest::sample::select;

/// Strings of one to `max` characters drawn from `chars`.
fn string_of(chars: Vec<char>, max: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(select(chars), 1..=max).prop_map(|chars| chars.into_iter().collect())
}

/// Strings of `range` tokens drawn from `tokens`.
fn tokens_of(
    tokens: &'static [&'static str],
    range: std::ops::RangeInclusive<usize>,
) -> impl Strategy<Value = String> {
    proptest::collection::vec(select(tokens), range).prop_map(|tokens| tokens.concat())
}

/// A lowercase domain label that cannot start with `www`.
fn label() -> impl Strategy<Value = String> {
    string_of("abcdefghijklmnopqrstuvxyz0123456789-".chars().collect(), 6)
}

/// A domain name of one to four labels.
pub fn host() -> impl Strategy<Value = String> {
    proptest::collection::vec(label(), 1..=4).prop_map(|labels| labels.join("."))
}

/// A port, including the defaults for `http` and `https`.
fn port() -> impl Strategy<Value = Option<u16>> {
    proptest::option::of(select(vec![80, 443, 8080, 1, 65535]))
}

/// Text that can appear in a URL's path, query, or fragment, including escapes and dot segments.
fn path_text(range: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = String> {
    const TOKENS: &[&str] = &[
        "a", "B", "z", "0", "9", "-", "_", "~", ".", "..", "%20", "%2e", "%25", "%7B", "%C5%82",
        "%2F", "(", ")", "'", "*", "+", ",", ";", ":", "@", "=", "&", "%26",
    ];

    tokens_of(TOKENS, range)
}

/// Prefix each segment with `/`; no segments is the empty path.
fn join_path(segments: &[String]) -> String {
    segments.iter().fold(String::new(), |mut path, segment| {
        path.push('/');
        path.push_str(segment);
        path
    })
}

/// The components of a hierarchical URL.
#[derive(Clone, Debug)]
pub struct UrlParts {
    pub scheme: String,
    pub userinfo: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

impl fmt::Display for UrlParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://", self.scheme)?;

        if let Some(userinfo) = &self.userinfo {
            write!(f, "{userinfo}@")?;
        }

        f.write_str(&self.host)?;

        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }

        f.write_str(&self.path)?;

        if let Some(query) = &self.query {
            write!(f, "?{query}")?;
        }

        if let Some(fragment) = &self.fragment {
            write!(f, "#{fragment}")?;
        }

        Ok(())
    }
}

/// Components of URLs with mixed case, optional `www` prefixes, and awkward paths and queries.
pub fn url_parts() -> impl Strategy<Value = UrlParts> {
    let scheme = select(vec!["http", "https", "HTTP", "Https", "ftp"]).prop_map(str::to_owned);
    let host = (
        select(vec!["", "www.", "WWW2.", "Www."]),
        host(),
        any::<bool>(),
    )
        .prop_map(|(prefix, host, upper)| {
            let host = format!("{prefix}{host}");

            if upper { host.to_uppercase() } else { host }
        });
    let path = proptest::collection::vec(path_text(0..=3), 0..=4)
        .prop_map(|segments| join_path(&segments));
    let query = proptest::option::of(path_text(0..=6));
    let fragment = proptest::option::of(path_text(0..=3));
    let userinfo = proptest::option::of(select(vec!["user", "user:pass"]).prop_map(str::to_owned));

    (scheme, userinfo, host, port(), path, query, fragment).prop_map(
        |(scheme, userinfo, host, port, path, query, fragment)| UrlParts {
            scheme,
            userinfo,
            host,
            port,
            path,
            query,
            fragment,
        },
    )
}

/// URL text with mixed case, optional `www` prefixes, and awkward paths and queries.
pub fn url() -> impl Strategy<Value = String> {
    url_parts().prop_map(|parts| parts.to_string())
}

/// The components of a SURT key.
#[derive(Clone, Debug)]
pub struct KeyParts {
    pub labels: Vec<String>,
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
}

impl fmt::Display for KeyParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.labels.join(","))?;

        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }

        write!(f, "){}", self.path)?;

        if let Some(query) = &self.query {
            write!(f, "?{query}")?;
        }

        Ok(())
    }
}

/// Components of SURT keys in the form written by CDX indexes.
pub fn key_parts() -> impl Strategy<Value = KeyParts> {
    let labels = proptest::collection::vec(label(), 1..=4);
    let path = proptest::collection::vec(path_text(1..=3), 0..=3)
        .prop_map(|segments| join_path(&segments));
    let query = proptest::option::of(path_text(1..=6));

    (labels, port(), path, query).prop_map(|(labels, port, path, query)| KeyParts {
        labels,
        port,
        path,
        query,
    })
}
