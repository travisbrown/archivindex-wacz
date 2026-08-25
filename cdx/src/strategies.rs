use std::borrow::Cow;

use chrono::{TimeZone as _, Utc};
use proptest::prelude::*;

use crate::cdxj::{ParsedFields, ParsedItem};
use crate::classic::{Header, Record};
use crate::json::Document;
use crate::properties::ExtraProperties;
use crate::timestamp::Timestamp;

fn text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9._~:/(),-]{1,32}"
}

fn timestamp() -> impl Strategy<Value = Timestamp> {
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

#[test_strategy::proptest]
fn timestamp_text_round_trips(#[strategy(timestamp())] value: Timestamp) {
    prop_assert_eq!(value.to_string().parse::<Timestamp>(), Ok(value));
}

#[test_strategy::proptest]
fn cdxj_text_round_trips(
    #[strategy(text())] key: String,
    #[strategy(timestamp())] captured_at: Timestamp,
    #[strategy(text())] url: String,
    #[strategy(proptest::option::of(any::<u16>()))] status: Option<u16>,
    #[strategy(proptest::option::of(any::<u64>()))] offset: Option<u64>,
    #[strategy(proptest::option::of(any::<u64>()))] length: Option<u64>,
) {
    let item = ParsedItem {
        key: Cow::Owned(key),
        timestamp: captured_at,
        fields: ParsedFields {
            url: Cow::Owned(url),
            digest: None,
            mime: None,
            status,
            offset,
            length,
            filename: None,
            record_digest: None,
            extra: ExtraProperties::default(),
        },
    };
    let line = item.to_string();
    let parsed = ParsedItem::parse(&line).unwrap().into_owned();
    prop_assert_eq!(parsed, item);
}

#[test_strategy::proptest]
fn classic_header_and_record_round_trip(
    #[strategy(prop_oneof![Just(' '), Just('|'), Just('\t')])] delimiter: char,
    leading: bool,
    #[strategy(prop::collection::vec(text(), 3))] values: Vec<String>,
) {
    let prefix = if leading {
        delimiter.to_string()
    } else {
        String::new()
    };
    let header_text = format!("{prefix}CDX{delimiter}N{delimiter}b{delimiter}a");
    let header = Header::parse(&header_text).unwrap();
    let record = Record::new(values.into_iter().map(Cow::Owned).collect());
    let rendered = header.render(&record).unwrap();
    let serialized_header = header.to_string();
    prop_assert_eq!(Header::parse(&serialized_header).unwrap(), header.clone());
    prop_assert_eq!(header.parse_record(&rendered).unwrap().into_owned(), record);
}

#[test_strategy::proptest]
fn json_document_round_trips(
    #[strategy(text())] key: String,
    #[strategy(timestamp())] captured_at: Timestamp,
    #[strategy(text())] url: String,
    #[strategy(proptest::option::of(text()))] resume_key: Option<String>,
) {
    let document = Document::new(
        vec![
            Cow::Borrowed("urlkey"),
            Cow::Borrowed("timestamp"),
            Cow::Borrowed("url"),
        ],
        vec![vec![
            Cow::Owned(key),
            Cow::Owned(captured_at.to_string()),
            Cow::Owned(url),
        ]],
        resume_key.map(Cow::Owned),
    )
    .unwrap();
    let serialized = serde_json::to_string(&document).unwrap();
    let parsed = serde_json::from_str::<Document<'_>>(&serialized)
        .unwrap()
        .into_owned();
    prop_assert_eq!(parsed, document.into_owned());
}
