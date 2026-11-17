//! Serde helpers shared across the WACZ wire formats.

use std::fmt;

use jiff::Timestamp;
use jiff::civil::{Date, DateTime};
use jiff::tz::Offset;
use serde::de::{Deserializer, Unexpected, Visitor};

/// The zone-less date-time layouts accepted by the compatibility parser, interpreted as UTC.
const NAIVE_DATETIME_LAYOUTS: [&str; 2] = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"];

/// The layout of a bare date accepted by the compatibility parser, interpreted as midnight UTC.
const DATE_LAYOUT: &str = "%Y-%m-%d";

/// Parse an RFC 3339 date-time, the only form the specification permits, normalizing to UTC.
///
/// Leap seconds and fractions longer than nine digits are rejected to avoid losing precision.
pub fn parse_rfc_3339(value: &str) -> Option<Timestamp> {
    // Jiff also accepts Temporal syntax, including annotations and abbreviated offsets.
    // Check the RFC 3339 layout before delegating calendar and offset validation to Jiff.
    let bytes = value.as_bytes();
    let prefix = bytes.get(..19)?;
    if !prefix.iter().enumerate().all(|(index, byte)| match index {
        4 | 7 => *byte == b'-',
        10 => matches!(byte, b'T' | b't' | b' '),
        13 | 16 => *byte == b':',
        _ => byte.is_ascii_digit(),
    }) {
        return None;
    }
    // Leap seconds cannot be represented losslessly by Jiff.
    if &prefix[17..19] == b"60" {
        return None;
    }
    let suffix = &bytes[19..];
    let zone = if let Some(fraction) = suffix.strip_prefix(b".") {
        let digits = fraction
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if !(1..=9).contains(&digits) {
            return None;
        }
        &fraction[digits..]
    } else {
        suffix
    };
    let valid_zone = matches!(zone, b"Z" | b"z")
        || (zone.len() == 6
            && matches!(zone[0], b'+' | b'-')
            && zone[3] == b':'
            && zone[1..3].iter().chain(&zone[4..]).all(u8::is_ascii_digit)
            && &zone[1..3] < b"24".as_slice());

    valid_zone.then(|| value.parse().ok()).flatten()
}

/// Parse RFC 3339 or a nonstandard date-time form produced by WACZ tools.
///
/// Zone-less date-times are interpreted as UTC and bare dates as midnight UTC.
pub fn parse_compatible_datetime(value: &str) -> Option<Timestamp> {
    if value.as_bytes().get(17..19) == Some(b"60") {
        None
    } else {
        parse_rfc_3339(value)
            .or_else(|| {
                NAIVE_DATETIME_LAYOUTS
                    .iter()
                    .find_map(|layout| DateTime::strptime(layout, value).ok())
                    .and_then(|datetime| Offset::UTC.to_timestamp(datetime).ok())
            })
            .or_else(|| {
                Date::strptime(DATE_LAYOUT, value)
                    .ok()
                    .and_then(|date| Offset::UTC.to_timestamp(date.at(0, 0, 0, 0)).ok())
            })
    }
}

/// A visitor that accepts only the RFC 3339 form required by the specification.
struct DateTimeVisitor;

impl Visitor<'_> for DateTimeVisitor {
    type Value = Timestamp;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an RFC 3339 date-time")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        parse_rfc_3339(v).ok_or_else(|| E::invalid_value(Unexpected::Str(v), &self))
    }
}

/// Deserialize a date-time, requiring the RFC 3339 form.
///
/// # Errors
///
/// Returns the deserializer's error if the value is not an RFC 3339 date-time string.
pub fn rfc_3339_datetime<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Timestamp, D::Error> {
    deserializer.deserialize_str(DateTimeVisitor)
}

/// Deserialize an optional date-time, requiring the RFC 3339 form.
///
/// # Errors
///
/// Returns the deserializer's error if a present value is not an RFC 3339 date-time string.
pub fn optional_rfc_3339_datetime<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Timestamp>, D::Error> {
    struct OptionVisitor;

    impl<'de> Visitor<'de> for OptionVisitor {
        type Value = Option<Timestamp>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("optional RFC 3339 date-time")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            rfc_3339_datetime(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(OptionVisitor)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    #[proptest::property_test]
    fn rfc_3339_dates_round_trip(#[strategy = strategies::datetime()] datetime: Timestamp) {
        prop_assert_eq!(
            parse_compatible_datetime(&datetime.to_string()),
            Some(datetime)
        );
    }

    #[proptest::property_test]
    fn zone_offsets_name_the_same_instant(
        #[strategy = strategies::datetime()] datetime: Timestamp,
        #[strategy = strategies::time_zone_offset()] offset: Offset,
    ) {
        let elsewhere = jiff::fmt::temporal::DateTimePrinter::new()
            .timestamp_with_offset_to_string(&datetime, offset);

        prop_assert_eq!(parse_compatible_datetime(&elsewhere), Some(datetime));
    }

    #[proptest::property_test]
    fn zoneless_date_times_are_read_as_utc(
        #[strategy = strategies::datetime()] datetime: Timestamp,
    ) {
        // `%.f` prints nothing at all for a whole second, so both layouts are exercised.
        let written = datetime.strftime("%Y-%m-%dT%H:%M:%S%.f").to_string();

        prop_assert_eq!(parse_compatible_datetime(&written), Some(datetime));
    }

    #[proptest::property_test]
    fn bare_dates_are_read_as_midnight_utc(
        #[strategy = strategies::datetime()] datetime: Timestamp,
    ) {
        let written = datetime.strftime("%Y-%m-%d").to_string();
        let parsed = parse_compatible_datetime(&written);

        prop_assert_eq!(
            parsed.map(|parsed| Offset::UTC.to_datetime(parsed).date()),
            Some(Offset::UTC.to_datetime(datetime).date())
        );
        prop_assert_eq!(
            parsed.map(|parsed| Offset::UTC.to_datetime(parsed).time()),
            Some(jiff::civil::Time::midnight())
        );
    }

    #[test]
    fn dates_parse_in_every_accepted_form() {
        let expected = "2020-10-07T21:22:36Z".parse::<Timestamp>().ok();

        assert_eq!(parse_compatible_datetime("2020-10-07T21:22:36Z"), expected);
        assert_eq!(
            parse_compatible_datetime("2020-10-07T23:22:36+02:00"),
            expected
        );
        assert_eq!(parse_compatible_datetime("2020-10-07T21:22:36"), expected);
        assert_eq!(
            parse_compatible_datetime("2020-10-07T21:22:36.000"),
            expected
        );
        assert_eq!(
            parse_compatible_datetime("2020-10-07T21:22"),
            expected.map(|datetime| datetime - jiff::SignedDuration::from_secs(36))
        );
        assert_eq!(
            parse_compatible_datetime("2020-10-07").map(|datetime| datetime.to_string()),
            Some("2020-10-07T00:00:00Z".to_owned())
        );
        assert_eq!(parse_compatible_datetime("October 7, 2020"), None);
        assert_eq!(parse_compatible_datetime(""), None);
    }

    #[test]
    fn compatible_dates_are_not_conforming() {
        for value in [
            "2020-10-07T21:22:36",
            "2020-10-07T21:22:36.000",
            "2020-10-07T21:22",
            "2020-10-07",
        ] {
            assert!(parse_compatible_datetime(value).is_some());
            assert_eq!(parse_rfc_3339(value), None);
        }
    }

    #[test]
    fn strict_dates_reject_temporal_extensions_and_precision_loss() {
        for value in [
            "20201007T212236Z",
            "2020-10-07T21:22Z",
            "2020-10-07T21:22:36+02",
            "2020-10-07T21:22:36+0200",
            "2020-10-07T21:22:36+02:00:01",
            "2020-10-07T21:22:36+24:00",
            "2020-10-07T21:22:36Z[UTC]",
            "2020-10-07T21:22:36Z[u-ca=iso8601]",
            "2020-10-07T21:22:36,123Z",
            "2020-10-07T21:22:36.Z",
            "2020-10-07T21:22:36.1234567891Z",
            "2016-12-31T23:59:60Z",
            "2016-12-31T23:59:60",
            "2020-02-30T21:22:36Z",
            "2020-10-07T21:22:36Z trailing",
            "日2020-10-07T21:22:36Z",
        ] {
            assert_eq!(parse_rfc_3339(value), None, "accepted {value}");
            assert_eq!(parse_compatible_datetime(value), None, "accepted {value}");
        }
    }

    #[test]
    fn strict_dates_preserve_nanoseconds_and_normalize_offsets() {
        let expected = "1969-12-31T23:59:59.123456789Z".parse().unwrap();
        for value in [
            "1969-12-31T23:59:59.123456789Z",
            "1969-12-31t23:59:59.123456789z",
            "1969-12-31 23:59:59.123456789Z",
            "1970-01-01T01:59:59.123456789+02:00",
            "1969-12-31T18:59:59.123456789-05:00",
            "1969-12-31T23:59:59.123456789-00:00",
        ] {
            assert_eq!(parse_rfc_3339(value), Some(expected), "rejected {value}");
        }
    }

    #[test]
    fn optional_dates_reject_non_strings() {
        let mut deserializer = serde_json::Deserializer::from_str("1602105756");
        let result = optional_rfc_3339_datetime(&mut deserializer);

        assert!(result.is_err());

        let mut deserializer = serde_json::Deserializer::from_str("null");
        let result = optional_rfc_3339_datetime(&mut deserializer);

        assert_eq!(result.ok(), Some(None));
    }
}
