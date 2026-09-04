//! Serde helpers shared across the WACZ wire formats.

use std::fmt;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde::de::{Deserializer, Unexpected, Visitor};

/// The zone-less date-time layouts accepted by the compatibility parser, interpreted as UTC.
const NAIVE_DATETIME_LAYOUTS: [&str; 2] = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"];

/// The layout of a bare date accepted by the compatibility parser, interpreted as midnight UTC.
const DATE_LAYOUT: &str = "%Y-%m-%d";

/// Parse an RFC 3339 date-time, the only form the specification permits, normalizing to UTC.
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
