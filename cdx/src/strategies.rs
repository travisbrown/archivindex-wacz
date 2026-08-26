//! Property-testing strategies for CDX values and records.

use std::borrow::Cow;
use std::fmt::Display;
use std::ops::RangeInclusive;

use chrono::{TimeZone as _, Utc};
use proptest::prelude::*;
use proptest::sample::select;

use crate::cdxj::{ConformingFields, Fields, Item};
use crate::classic::Record;
use crate::properties::ExtraProperties;
use crate::timestamp::Timestamp;

/// The value a text representation uses for an absent field.
const ABSENT: &str = "-";

/// Strings of `range` tokens drawn from `tokens`.
fn tokens_of(
    tokens: &'static [&'static str],
    range: RangeInclusive<usize>,
) -> impl Strategy<Value = String> {
    proptest::collection::vec(select(tokens), range).prop_map(|tokens| tokens.concat())
}

/// Text for a value that appears outside a JSON string: a search key, or a field of a
/// delimiter-separated record.
pub fn bare_text() -> impl Strategy<Value = String> {
    const TOKENS: &[&str] = &[
        "a", "Z", "0", "9", "-", "_", "~", ".", ",", "(", ")", "/", "?", "=", "&", ":", "+", "#",
        "%20", "sha1:", "é",
    ];

    tokens_of(TOKENS, 1..=8)
}

/// Text that is not the absent marker, for a field that must be present.
pub fn present_text() -> impl Strategy<Value = String> {
    bare_text().prop_filter("value is present", |value| value != ABSENT)
}

/// Text for a value inside a JSON string, including the characters JSON must escape.
pub fn json_text() -> impl Strategy<Value = String> {
    const TOKENS: &[&str] = &[
        "a",
        "Z",
        "0",
        " ",
        "\"",
        "\\",
        "{",
        "}",
        "[",
        "]",
        ":",
        ",",
        "\n",
        "\t",
        "\u{7f}",
        "é",
        "日",
        "\u{1f600}",
    ];

    tokens_of(TOKENS, 1..=8)
}

/// A timestamp in either supported precision.
pub fn timestamp() -> impl Strategy<Value = Timestamp> {
    (
        946_684_800_i64..1_893_456_000_i64,
        any::<bool>(),
        0_u32..1_000,
    )
        .prop_map(|(seconds, milliseconds, fraction)| {
            let instant = Utc
                .timestamp_opt(seconds, fraction * 1_000_000)
                .single()
                .expect("generated timestamp is in range");

            if milliseconds {
                Timestamp::with_milliseconds(instant)
            } else {
                Timestamp::new(instant)
            }
        })
}

/// Extension properties under names that cannot collide with a modeled field.
pub fn extra_properties() -> impl Strategy<Value = ExtraProperties> {
    proptest::collection::vec((bare_text(), json_text()), 0..=3).prop_map(|entries| {
        let mut extra = ExtraProperties::default();
        for (name, value) in entries {
            extra.insert(format!("x-{name}"), serde_json::Value::String(value));
        }
        extra
    })
}

/// A lenient CDXJ field object.
pub fn fields() -> impl Strategy<Value = Fields<'static>> {
    (
        json_text(),
        proptest::option::of(bare_text()),
        proptest::option::of(json_text()),
        proptest::option::of(any::<u16>()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(json_text()),
        proptest::option::of(bare_text()),
        extra_properties(),
    )
        .prop_map(
            |(url, digest, mime, status, offset, length, filename, record_digest, extra)| Fields {
                url: Cow::Owned(url),
                digest: digest.map(Cow::Owned),
                mime: mime.map(Cow::Owned),
                status,
                offset,
                length,
                filename: filename.map(Cow::Owned),
                record_digest: record_digest.map(Cow::Owned),
                extra,
            },
        )
}

/// A CDXJ field object with every field CDXJ 0.1.0 requires.
pub fn conforming_fields() -> impl Strategy<Value = ConformingFields<'static>> {
    (
        json_text(),
        bare_text(),
        json_text(),
        any::<u16>(),
        any::<u64>(),
        any::<u64>(),
        json_text(),
        proptest::option::of(bare_text()),
        extra_properties(),
    )
        .prop_map(
            |(url, digest, mime, status, offset, length, filename, record_digest, extra)| {
                let fields =
                    ConformingFields::new(url, digest, mime, status, offset, length, filename);
                let fields = match record_digest {
                    Some(record_digest) => fields.with_record_digest(record_digest),
                    None => fields,
                };
                fields
                    .with_extra(extra)
                    .expect("generated extensions do not collide")
            },
        )
}

/// A CDXJ line model.
pub fn item() -> impl Strategy<Value = Item<'static>> {
    (present_text(), timestamp(), fields()).prop_map(|(key, timestamp, fields)| Item {
        key: Cow::Owned(key),
        timestamp,
        fields,
    })
}

/// A delimiter used by classic CDX files.
pub fn delimiter() -> impl Strategy<Value = char> {
    select(vec![' ', '|', '\t'])
}

/// A legend marker: a standard classic marker, or a name that is not modeled.
fn marker() -> impl Strategy<Value = String> {
    prop_oneof![
        select(vec![
            "N",
            "b",
            "a",
            "m",
            "s",
            "k",
            "r",
            "M",
            "S",
            "V",
            "g",
            "urlkey",
            "timestamp",
            "original",
            "mimetype",
            "statuscode",
        ])
        .prop_map(str::to_owned),
        bare_text(),
    ]
}

/// A classic header and a record with a matching number of values, which may be empty.
pub fn legend_and_values() -> impl Strategy<Value = (char, bool, Vec<String>, Vec<String>)> {
    (delimiter(), any::<bool>(), 1_usize..=6).prop_flat_map(|(delimiter, leading, count)| {
        (
            Just(delimiter),
            Just(leading),
            proptest::collection::vec(marker(), count),
            proptest::collection::vec(
                proptest::option::of(bare_text()).prop_map(Option::unwrap_or_default),
                count,
            ),
        )
    })
}

/// The values of a capture as they appear in a delimiter-separated or JSON-array record.
#[derive(Clone, Debug)]
pub struct CaptureParts {
    pub key: String,
    pub timestamp: Timestamp,
    pub url: String,
    pub mime: Option<String>,
    pub status: Option<u16>,
    pub digest: Option<String>,
    pub redirect: Option<String>,
    pub robot_flags: Option<String>,
    pub length: Option<u64>,
    pub offset: Option<u64>,
    pub filename: Option<String>,
}

impl CaptureParts {
    /// These values in the order of the standard 11-field legend (`N b a m s k r M S V g`),
    /// with `-` for every absent field.
    pub fn values(&self) -> Vec<Cow<'static, str>> {
        [
            self.key.clone(),
            self.timestamp.to_string(),
            self.url.clone(),
            or_absent(self.mime.as_deref()),
            or_absent(self.status),
            or_absent(self.digest.as_deref()),
            or_absent(self.redirect.as_deref()),
            or_absent(self.robot_flags.as_deref()),
            or_absent(self.length),
            or_absent(self.offset),
            or_absent(self.filename.as_deref()),
        ]
        .into_iter()
        .map(Cow::Owned)
        .collect()
    }

    /// These values as a classic CDX record.
    pub fn record(&self) -> Record<'static> {
        Record::new(self.values())
    }
}

/// The values of a capture, each optional field present or absent.
pub fn capture_parts() -> impl Strategy<Value = CaptureParts> {
    (
        present_text(),
        timestamp(),
        present_text(),
        proptest::option::of(present_text()),
        proptest::option::of(any::<u16>()),
        proptest::option::of(present_text()),
        proptest::option::of(present_text()),
        proptest::option::of(present_text()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(present_text()),
    )
        .prop_map(
            |(
                key,
                timestamp,
                url,
                mime,
                status,
                digest,
                redirect,
                robot_flags,
                length,
                offset,
                filename,
            )| CaptureParts {
                key,
                timestamp,
                url,
                mime,
                status,
                digest,
                redirect,
                robot_flags,
                length,
                offset,
                filename,
            },
        )
}

/// Render a value, or the absent marker when it has none.
fn or_absent<T: Display>(value: Option<T>) -> String {
    value.map_or_else(|| ABSENT.to_owned(), |value| value.to_string())
}
