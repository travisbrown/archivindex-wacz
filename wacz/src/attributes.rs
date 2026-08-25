//! Serde helpers shared across the WACZ wire formats.

use std::borrow::Cow;
use std::fmt;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde::de::{Deserializer, SeqAccess, Unexpected, Visitor};

/// The zone-less date-time layouts accepted by the compatibility parser, interpreted as UTC.
const NAIVE_DATETIME_LAYOUTS: [&str; 2] = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"];

/// The layout of a bare date accepted by the compatibility parser, interpreted as midnight UTC.
const DATE_LAYOUT: &str = "%Y-%m-%d";

/// A visitor that borrows string data from the input when possible.
///
/// Serde's derived `Cow<str>` deserialization always allocates; this is the zero-copy alternative
/// behind the `borrowed_*` helpers in this module.
struct StrVisitor;

impl<'de> Visitor<'de> for StrVisitor {
    type Value = Cow<'de, str>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("string")
    }

    fn visit_borrowed_str<E: serde::de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(v))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(v.to_owned()))
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(Cow::Owned(v))
    }
}

/// Deserialize an optional string field, borrowing from the input when possible.
///
/// Serde's `#[serde(borrow)]` does not reach inside `Option`, so optional `Cow` fields use this
/// helper instead.
pub fn borrowed_option_str<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Cow<'de, str>>, D::Error> {
    struct OptionVisitor;

    impl<'de> Visitor<'de> for OptionVisitor {
        type Value = Option<Cow<'de, str>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("optional string")
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
            deserializer.deserialize_str(StrVisitor).map(Some)
        }
    }

    deserializer.deserialize_option(OptionVisitor)
}

/// Deserialize a sequence of strings, borrowing from the input when possible.
///
/// Serde's `#[serde(borrow)]` does not reach inside `Vec`, so string-array fields use this helper
/// instead.
pub fn borrowed_str_seq<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Cow<'de, str>>, D::Error> {
    /// A newtype giving `Cow<str>` a borrowing `Deserialize` impl for use as a sequence element.
    struct BorrowedStr<'a>(Cow<'a, str>);

    impl<'de> serde::de::Deserialize<'de> for BorrowedStr<'de> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_str(StrVisitor).map(BorrowedStr)
        }
    }

    struct SeqVisitor;

    impl<'de> Visitor<'de> for SeqVisitor {
        type Value = Vec<Cow<'de, str>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("sequence of strings")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(BorrowedStr(value)) = seq.next_element()? {
                values.push(value);
            }

            Ok(values)
        }
    }

    deserializer.deserialize_seq(SeqVisitor)
}

/// Parse an RFC 3339 date-time, the only form the WACZ specification permits.
pub fn parse_rfc_3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .ok()
}

/// Parse RFC 3339 or a nonstandard date-time form produced by WACZ tools.
///
/// Zone-less date-times are interpreted as UTC and bare dates as midnight UTC.
pub fn parse_compatible_datetime(value: &str) -> Option<DateTime<Utc>> {
    parse_rfc_3339(value)
        .or_else(|| {
            NAIVE_DATETIME_LAYOUTS
                .iter()
                .find_map(|layout| NaiveDateTime::parse_from_str(value, layout).ok())
                .map(|datetime| datetime.and_utc())
        })
        .or_else(|| {
            NaiveDate::parse_from_str(value, DATE_LAYOUT)
                .ok()
                .map(|date| date.and_time(NaiveTime::MIN).and_utc())
        })
}

/// A visitor that accepts only the RFC 3339 form required by the specification.
struct DateTimeVisitor;

impl Visitor<'_> for DateTimeVisitor {
    type Value = DateTime<Utc>;

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
) -> Result<DateTime<Utc>, D::Error> {
    deserializer.deserialize_str(DateTimeVisitor)
}

/// Deserialize an optional date-time, requiring the RFC 3339 form.
///
/// # Errors
///
/// Returns the deserializer's error if a present value is not an RFC 3339 date-time string.
pub fn optional_rfc_3339_datetime<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error> {
    struct OptionVisitor;

    impl<'de> Visitor<'de> for OptionVisitor {
        type Value = Option<DateTime<Utc>>;

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

    #[test_strategy::proptest]
    fn rfc_3339_dates_round_trip(#[strategy(strategies::datetime())] datetime: DateTime<Utc>) {
        prop_assert_eq!(
            parse_compatible_datetime(&datetime.to_rfc3339()),
            Some(datetime)
        );
    }

    #[test_strategy::proptest]
    fn zone_offsets_name_the_same_instant(
        #[strategy(strategies::datetime())] datetime: DateTime<Utc>,
        #[strategy(strategies::time_zone_offset())] offset: chrono::FixedOffset,
    ) {
        let elsewhere = datetime.with_timezone(&offset).to_rfc3339();

        prop_assert_eq!(parse_compatible_datetime(&elsewhere), Some(datetime));
    }

    #[test_strategy::proptest]
    fn zoneless_date_times_are_read_as_utc(
        #[strategy(strategies::datetime())] datetime: DateTime<Utc>,
    ) {
        // `%.f` prints nothing at all for a whole second, so both layouts are exercised.
        let written = datetime.format("%Y-%m-%dT%H:%M:%S%.f").to_string();

        prop_assert_eq!(parse_compatible_datetime(&written), Some(datetime));
    }

    #[test_strategy::proptest]
    fn bare_dates_are_read_as_midnight_utc(
        #[strategy(strategies::datetime())] datetime: DateTime<Utc>,
    ) {
        let written = datetime.format("%Y-%m-%d").to_string();
        let parsed = parse_compatible_datetime(&written);

        prop_assert_eq!(
            parsed.map(|parsed| parsed.date_naive()),
            Some(datetime.date_naive())
        );
        prop_assert_eq!(parsed.map(|parsed| parsed.time()), Some(NaiveTime::MIN));
    }

    #[test]
    fn dates_parse_in_every_accepted_form() {
        let expected = DateTime::parse_from_rfc3339("2020-10-07T21:22:36Z")
            .map(|datetime| datetime.with_timezone(&Utc))
            .ok();

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
            expected.map(|datetime| datetime - chrono::Duration::seconds(36))
        );
        assert_eq!(
            parse_compatible_datetime("2020-10-07").map(|datetime| datetime.to_rfc3339()),
            Some("2020-10-07T00:00:00+00:00".to_owned())
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
    fn optional_dates_reject_non_strings() {
        let mut deserializer = serde_json::Deserializer::from_str("1602105756");
        let result = optional_rfc_3339_datetime(&mut deserializer);

        assert!(result.is_err());

        let mut deserializer = serde_json::Deserializer::from_str("null");
        let result = optional_rfc_3339_datetime(&mut deserializer);

        assert_eq!(result.ok(), Some(None));
    }
}
