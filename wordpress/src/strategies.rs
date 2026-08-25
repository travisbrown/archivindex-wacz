//! Property-testing strategies for `WordPress` capture values.

use chrono::{DateTime, Utc};
use proptest::prelude::*;
use proptest::sample::select;
use url::Url;

/// The path and query tokens a URL is built from, including characters RFC 3986 forbids bare.
const TOKENS: &[&str] = &[
    "a", "Z", "0", "-", "~", "|", "^", "[", "]", "{", "}", "`", "é", "日",
];

/// Strings of up to eight tokens, which a URL may need to percent-encode.
fn token_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(select(TOKENS), 0..=8).prop_map(|tokens| tokens.concat())
}

/// An instant, with sub-second precision.
pub fn datetime() -> impl Strategy<Value = DateTime<Utc>> {
    (0..=4_102_444_799_i64, 0..1_000_000_000_u32).prop_map(|(seconds, nanoseconds)| {
        DateTime::from_timestamp(seconds, nanoseconds)
            .expect("invariant violation: a generated instant is in range")
    })
}

/// An HTTP URL, optionally with credentials, a query, and a fragment.
pub fn url() -> impl Strategy<Value = Url> {
    (
        select(vec!["http", "https"]),
        proptest::option::of(select(vec!["user:s3cret-token", "user", ":s3cret-token"])),
        select(vec!["example.com", "example.org:8080"]),
        proptest::collection::vec(token_text(), 0..=3),
        proptest::option::of(token_text()),
        proptest::option::of(token_text()),
    )
        .prop_map(
            |(scheme, credentials, authority, segments, query, fragment)| {
                let credentials = credentials.map_or_else(String::new, |value| format!("{value}@"));
                let path = segments
                    .iter()
                    .fold(String::new(), |path, segment| path + "/" + segment);
                let query = query.map_or_else(String::new, |query| format!("?q={query}"));
                let fragment = fragment.map_or_else(String::new, |fragment| format!("#{fragment}"));

                Url::parse(&format!(
                    "{scheme}://{credentials}{authority}{path}{query}{fragment}"
                ))
                .expect("invariant violation: a generated URL parses")
            },
        )
}
