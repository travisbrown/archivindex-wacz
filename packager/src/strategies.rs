//! Property-testing strategies for conversion state.

use proptest::prelude::*;

use crate::spool::{Annotation, PageDraft};

/// Page properties contributed by one `metadata` record.
pub fn annotation() -> impl Strategy<Value = Annotation> {
    (
        proptest::option::of("[a-z ]{0,8}"),
        any::<bool>(),
        proptest::option::of("https://example.com/[a-z]{0,4}"),
    )
        .prop_map(|(title, via, page_url)| Annotation::new(title, via, page_url))
}

/// The page a capture may become.
pub fn page_draft() -> impl Strategy<Value = PageDraft> {
    (
        "urn:uuid:[0-9a-f]{8}",
        "https://example.com/[a-z]{0,4}",
        0..=4_102_444_799_i64,
        proptest::option::of("[a-z ]{0,8}"),
    )
        .prop_map(|(record_id, url, seconds, title)| {
            let date = chrono::DateTime::from_timestamp(seconds, 0)
                .expect("invariant violation: a generated instant is in range");

            PageDraft::new(record_id, url, date, title)
        })
}
