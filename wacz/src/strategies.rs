//! Property-testing strategies for WACZ values.

use std::ops::RangeInclusive;

use proptest::prelude::*;
use proptest::sample::select;

use crate::digest::Sha256Digest;
use crate::{ARCHIVE_PREFIX, INDEXES_PREFIX, PAGES_PREFIX};

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

/// The text of one line: never a line break, and possibly empty.
fn line_text() -> impl Strategy<Value = String> {
    const TOKENS: &[&str] = &[
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

    tokens_of(TOKENS, 0..=200)
}

/// Lines with their line endings, and whether the last line ends with one.
pub fn lines() -> impl Strategy<Value = (Vec<(String, &'static str)>, bool)> {
    (
        proptest::collection::vec((line_text(), select(vec!["\n", "\r\n"])), 0..=8),
        any::<bool>(),
    )
}
