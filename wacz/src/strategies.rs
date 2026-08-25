//! Property-testing strategies for WACZ values.

use std::borrow::Cow;
use std::ops::RangeInclusive;

use archivindex_cdx::properties::ExtraProperties;
use chrono::{DateTime, FixedOffset, Utc};
use proptest::prelude::*;
use proptest::sample::select;

use crate::digest::Sha256Digest;
use crate::pages::{FORMAT, Page, PageListHeader};
use crate::{ARCHIVE_PREFIX, INDEXES_PREFIX, PAGES_PREFIX};

/// The tokens free text is built from, including ones that JSON and line reading must escape.
const TEXT_TOKENS: &[&str] = &[
    "a",
    "Z",
    "0",
    " ",
    "\t",
    "\"",
    "{",
    "}",
    "\u{7f}",
    "é",
    "日",
    "\u{1f600}",
];

/// Strings of `range` tokens drawn from `tokens`.
fn tokens_of(
    tokens: &'static [&'static str],
    range: RangeInclusive<usize>,
) -> impl Strategy<Value = String> {
    proptest::collection::vec(select(tokens), range).prop_map(|tokens| tokens.concat())
}

/// A SHA-256 digest.
pub fn digest() -> impl Strategy<Value = Sha256Digest> {
    proptest::collection::vec(any::<u8>(), 32).prop_map(|bytes| {
        Sha256Digest(
            bytes
                .try_into()
                .expect("invariant violation: a generated digest is 32 bytes"),
        )
    })
}

/// The stem of a member file name, including stems a safe path may not contain.
fn stem() -> impl Strategy<Value = String> {
    const TOKENS: &[&str] = &["a", "Z", "0", "-", "_", ".", "..", "data", "é"];

    tokens_of(TOKENS, 1..=3)
}

/// A file name extension, including the ones the member classifiers recognize.
fn extension() -> impl Strategy<Value = String> {
    select(vec![
        "", ".warc", ".warc.gz", ".cdx", ".cdx.gz", ".idx", ".gz", ".jsonl", ".Warc", ".CDX",
    ])
    .prop_map(str::to_owned)
}

/// A member path: a recognized WACZ directory prefix or none, a name, and an extension.
pub fn member_path() -> impl Strategy<Value = String> {
    (
        select(vec!["", ARCHIVE_PREFIX, INDEXES_PREFIX, PAGES_PREFIX, "/"]),
        proptest::collection::vec(stem(), 1..=2),
        extension(),
    )
        .prop_map(|(prefix, segments, extension)| {
            format!("{prefix}{}{extension}", segments.join("/"))
        })
}

/// A `ZipNum` member path: a summary or a block file directly under the index directory.
pub fn zipnum_path() -> impl Strategy<Value = String> {
    (stem(), select(vec![".cdx.gz", ".idx"]))
        .prop_map(|(stem, extension)| format!("{INDEXES_PREFIX}{stem}{extension}"))
}

/// The text of one line: never a line break, and long enough to need an excerpt.
fn line_text() -> impl Strategy<Value = String> {
    tokens_of(TEXT_TOKENS, 0..=200)
}

/// Short free text, such as a title or an identifier.
fn text() -> impl Strategy<Value = String> {
    tokens_of(TEXT_TOKENS, 0..=8)
}

/// Lines with their line endings, and whether the last line ends with one.
pub fn lines() -> impl Strategy<Value = (Vec<(String, &'static str)>, bool)> {
    (
        proptest::collection::vec((line_text(), select(vec!["\n", "\r\n"])), 0..=8),
        any::<bool>(),
    )
}

/// An instant every accepted date layout can represent, with sub-second precision.
pub fn datetime() -> impl Strategy<Value = DateTime<Utc>> {
    (0..=4_102_444_799_i64, 0..1_000_000_000_u32).prop_map(|(seconds, nanoseconds)| {
        DateTime::from_timestamp(seconds, nanoseconds)
            .expect("invariant violation: a generated instant is in range")
    })
}

/// A time zone offset on a quarter-hour boundary, as time zones in use are.
pub fn time_zone_offset() -> impl Strategy<Value = FixedOffset> {
    (-48..=56_i32).prop_map(|quarter_hours| {
        FixedOffset::east_opt(quarter_hours * 900)
            .expect("invariant violation: a generated offset is in range")
    })
}

/// Extension properties, named so that they cannot collide with a modeled property.
fn extra_properties() -> impl Strategy<Value = ExtraProperties> {
    proptest::collection::vec((text(), text()), 0..=3).prop_map(|entries| {
        let mut extra = ExtraProperties::default();

        for (name, value) in entries {
            extra.insert(format!("x-{name}"), serde_json::Value::String(value));
        }

        extra
    })
}

/// A page list header declaring the supported format.
pub fn page_list_header() -> impl Strategy<Value = PageListHeader<'static>> {
    (
        proptest::option::of(text()),
        proptest::option::of(text()),
        extra_properties(),
    )
        .prop_map(|(id, title, extra)| PageListHeader {
            format: Cow::Borrowed(FORMAT),
            id: id.map(Cow::Owned),
            title: title.map(Cow::Owned),
            extra,
        })
}

/// A page entry.
pub fn page() -> impl Strategy<Value = Page<'static>> {
    (
        text(),
        datetime(),
        proptest::option::of(text()),
        proptest::option::of(text()),
        proptest::option::of(line_text()),
        proptest::option::of(any::<u64>()),
        extra_properties(),
    )
        .prop_map(|(url, ts, id, title, page_text, size, extra)| Page {
            url: Cow::Owned(url),
            ts,
            id: id.map(Cow::Owned),
            title: title.map(Cow::Owned),
            text: page_text.map(Cow::Owned),
            size,
            extra,
        })
}
