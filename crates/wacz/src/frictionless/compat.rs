//! Compatibility parsing for manifests with nonstandard date-time values.
//!
//! Strict parsing requires RFC 3339. This module also accepts the zone-less date-times and bare
//! dates produced by some WACZ tools, while reporting every repaired property.

use std::borrow::Cow;

use bounded_static::IntoBoundedStatic;
use chrono::SecondsFormat;

use super::DataPackage;

/// The modeled date-time properties of a manifest, spelled as they appear on the wire.
const DATE_PROPERTIES: [&str; 3] = ["created", "modified", "mainPageDate"];

/// A manifest date-time property written in a form the strict parser does not accept.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NonConformingDate {
    /// The property name, as it appears in the manifest.
    pub property: &'static str,
    /// The value, as it was written.
    pub value: String,
}

/// Parse a manifest, repairing recognized date-time values that are not RFC 3339.
///
/// The returned list identifies every repaired property. Unrecognized date formats fail parsing.
///
/// # Errors
///
/// Returns the deserialization error if the bytes are not a valid manifest.
pub fn parse_data_package(
    bytes: &[u8],
) -> Result<(DataPackage<'static>, Vec<NonConformingDate>), serde_json::Error> {
    let mut document = serde_json::from_slice::<serde_json::Value>(bytes)?;
    let mut non_conforming = Vec::new();

    if let Some(properties) = document.as_object_mut() {
        for property in DATE_PROPERTIES {
            let Some(value) = properties.get_mut(property) else {
                continue;
            };
            let Some(written) = value.as_str() else {
                continue;
            };

            if crate::attributes::parse_rfc_3339(written).is_some() {
                continue;
            }

            if let Some(parsed) = crate::attributes::parse_compatible_datetime(written) {
                non_conforming.push(NonConformingDate {
                    property,
                    value: written.to_owned(),
                });
                *value =
                    serde_json::Value::String(parsed.to_rfc3339_opts(SecondsFormat::AutoSi, true));
            }
        }
    }

    // Re-serializing is only needed when something was repaired; otherwise the input can be read
    // directly, which also lets the manifest borrow from it.
    let source = if non_conforming.is_empty() {
        Cow::Borrowed(bytes)
    } else {
        Cow::Owned(serde_json::to_vec(&document)?)
    };

    serde_json::from_slice::<DataPackage<'_>>(&source)
        .map(|package| (package.into_static(), non_conforming))
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;

    /// A manifest with the structural properties every WACZ has, and `created` set to `value`.
    fn manifest(value: &str) -> String {
        format!(
            r#"{{"profile":"data-package","wacz_version":"1.1.1","resources":[],"created":"{value}"}}"#
        )
    }

    #[test]
    fn conforming_dates_are_not_reported() {
        let (package, non_conforming) =
            parse_data_package(manifest("2020-10-07T21:22:36Z").as_bytes())
                .expect("a parseable manifest");

        assert_eq!(
            package.created,
            DateTime::parse_from_rfc3339("2020-10-07T21:22:36Z")
                .ok()
                .map(|value| value.with_timezone(&Utc))
        );
        assert_eq!(non_conforming, Vec::new());
    }

    #[test]
    fn zoneless_dates_are_read_as_utc_and_reported() {
        let (package, non_conforming) =
            parse_data_package(manifest("2020-10-07T21:22:36").as_bytes())
                .expect("a repairable manifest");

        assert_eq!(
            package.created,
            DateTime::parse_from_rfc3339("2020-10-07T21:22:36Z")
                .ok()
                .map(|value| value.with_timezone(&Utc))
        );
        assert_eq!(
            non_conforming,
            vec![NonConformingDate {
                property: "created",
                value: "2020-10-07T21:22:36".to_owned()
            }]
        );
    }

    #[test]
    fn unrecognized_dates_fail() {
        assert!(parse_data_package(manifest("October 7, 2020").as_bytes()).is_err());
    }

    #[test]
    fn strict_parsing_rejects_what_the_compatibility_parser_repairs() {
        assert!(serde_json::from_str::<DataPackage<'_>>(&manifest("2020-10-07T21:22:36")).is_err());
    }
}
