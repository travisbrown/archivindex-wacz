//! Property-testing strategies for indexed values.

use archivindex_warc::value::{Algorithm, LabelledDigest, WarcDate};
use archivindex_warc::version::WarcVersion;
use fluent_uri::Uri;
use proptest::prelude::*;
use proptest::sample::select;

use crate::payload::RevisitTarget;
use crate::resource::{ResourceKey, ResourceStateUpdate, Variance};

/// A WARC date at second or sub-second precision, in the WARC 1.1 grammar.
fn warc_date() -> impl Strategy<Value = WarcDate> {
    (
        1970..=2100_i32,
        1..=12_u32,
        1..=28_u32,
        0..=23_u32,
        0..=59_u32,
        0..=59_u32,
        proptest::option::of("[0-9]{1,9}"),
    )
        .prop_map(|(year, month, day, hour, minute, second, fraction)| {
            let fraction = fraction.map_or_else(String::new, |digits| format!(".{digits}"));
            let value = format!(
                "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{fraction}Z"
            );

            WarcDate::parse(&value, WarcVersion::V1_1)
                .expect("invariant violation: a generated date parses")
        })
}

/// An absolute URI, in the forms a record identifier and a target URI take.
fn uri() -> impl Strategy<Value = Uri<String>> {
    prop_oneof![
        (
            select(vec!["https", "http"]),
            select(vec!["example.com", "example.org:8080"]),
            "(/[a-z0-9._~-]{0,4}){0,3}",
            proptest::option::of("[a-z]{1,3}=[a-z0-9]{1,3}"),
        )
            .prop_map(|(scheme, authority, path, query)| {
                let query = query.map_or_else(String::new, |query| format!("?{query}"));
                format!("{scheme}://{authority}{path}{query}")
            }),
        "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
            .prop_map(|uuid| format!("urn:uuid:{uuid}")),
    ]
    .prop_map(|value| Uri::parse(value).expect("invariant violation: a generated URI parses"))
}

/// A digest of the length its algorithm requires.
fn labelled_digest(
    bytes: impl Strategy<Value = u8> + Clone,
) -> impl Strategy<Value = LabelledDigest> {
    select(Algorithm::ALL.as_slice()).prop_flat_map(move |algorithm| {
        proptest::collection::vec(bytes.clone(), algorithm.digest_length())
            .prop_map(move |bytes: Vec<u8>| LabelledDigest::from_digest(algorithm, &bytes))
    })
}

/// A digest drawn from a small pool, so that generated digests collide often.
fn pooled_digest() -> impl Strategy<Value = LabelledDigest> {
    labelled_digest(0..=2_u8)
}

/// A resource key drawn from a small pool, so that generated keys collide often.
pub fn resource_key() -> impl Strategy<Value = ResourceKey> {
    select(vec![
        "https://example.com/",
        "https://example.com/a",
        "https://example.org/b?c=d",
    ])
    .prop_map(|value| {
        ResourceKey::new(
            Uri::parse(value.to_owned()).expect("invariant violation: a pooled URI parses"),
        )
    })
}

/// A canonical payload-bearing record, digested from the pool.
pub fn revisit_target() -> impl Strategy<Value = RevisitTarget> {
    (
        pooled_digest(),
        // Lengths are stored in a signed column, so `i64::MAX` is the largest one that fits.
        proptest::option::of(0..=u64::MAX >> 1),
        uri(),
        uri(),
        warc_date(),
    )
        .prop_map(
            |(payload_digest, payload_length, record_id, target_uri, warc_date)| RevisitTarget {
                payload_digest,
                payload_length,
                record_id,
                target_uri,
                warc_date,
            },
        )
}

/// A variance drawn from a small pool, so that generated variances collide often.
fn variance() -> impl Strategy<Value = Variance> {
    select(vec![
        None,
        Some("Accept-Encoding"),
        Some("User-Agent"),
        Some("*"),
    ])
    .prop_map(|vary| Variance::declared(vary, |name| (name == "user-agent").then_some("Archivist")))
}

/// A resource-state transition of either kind.
pub fn resource_state_update() -> impl Strategy<Value = ResourceStateUpdate> {
    let validator = || proptest::option::of("[\\x20-\\x7e]{0,8}");

    prop_oneof![
        (
            validator(),
            validator(),
            proptest::option::of(labelled_digest(any::<u8>())),
            proptest::option::of(uri()),
            proptest::option::of(warc_date()),
            warc_date(),
            variance(),
        )
            .prop_map(
                |(
                    etag,
                    last_modified,
                    payload_digest,
                    record_id,
                    warc_date,
                    observed_at,
                    variance,
                )| {
                    ResourceStateUpdate::Representation {
                        etag,
                        last_modified,
                        payload_digest,
                        record_id,
                        warc_date,
                        observed_at,
                        variance,
                    }
                }
            ),
        (validator(), validator(), warc_date()).prop_map(|(etag, last_modified, observed_at)| {
            ResourceStateUpdate::NotModified {
                etag,
                last_modified,
                observed_at,
            }
        }),
    ]
}
