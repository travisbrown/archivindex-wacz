//! Parsing the HTTP date forms that recipients must accept.
//!
//! [RFC 9110 section 5.6.7](https://www.rfc-editor.org/rfc/rfc9110.html#section-5.6.7) defines
//! `HTTP-date` as IMF-fixdate, which senders must generate, plus two obsolete forms that recipients
//! must accept: RFC 850 and asctime. All three denote UTC.

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Utc};

/// The preferred form: `Sun, 06 Nov 1994 08:49:37 GMT`.
const IMF_FIXDATE: &str = "%a, %d %b %Y %H:%M:%S GMT";

/// The obsolete RFC 850 form after removing its weekday: `06-Nov-94 08:49:37 GMT`.
///
/// The day name is dropped rather than matched, because the century the two digits denote is not
/// known until the date is anchored, and the weekday depends on it.
const RFC_850: &str = "%d-%b-%y %H:%M:%S GMT";

/// The obsolete asctime form, with a space-padded day and no zone: `Sun Nov  6 08:49:37 1994`.
const ASCTIME: &str = "%a %b %e %H:%M:%S %Y";

/// Parse an HTTP-date, reading a two-digit year against `now`.
///
/// A date with a numeric zone offset is not an HTTP-date, but is accepted because it is
/// unambiguous and is what some servers send.
pub fn parse(value: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let value = value.trim();

    if let Ok(parsed) = NaiveDateTime::parse_from_str(value, IMF_FIXDATE) {
        return Some(Utc.from_utc_datetime(&parsed));
    }
    if let Some((day_name, rest)) = value.split_once(", ")
        && day_name
            .chars()
            .all(|character| character.is_ascii_alphabetic())
        && let Ok(parsed) = NaiveDateTime::parse_from_str(rest, RFC_850)
    {
        return anchored_year(parsed, now).map(|parsed| Utc.from_utc_datetime(&parsed));
    }
    if let Ok(parsed) = NaiveDateTime::parse_from_str(value, ASCTIME) {
        return Some(Utc.from_utc_datetime(&parsed));
    }

    DateTime::parse_from_rfc2822(value)
        .map(|value| value.to_utc())
        .ok()
}

/// Re-anchor a two-digit year against `now`.
///
/// A date that appears to be more than fifty years in the future represents the most recent past
/// year with the same last two digits, so the century a date belongs to depends on when it is
/// read. Only the last two digits of `parsed`'s year are significant here.
fn anchored_year(parsed: NaiveDateTime, now: DateTime<Utc>) -> Option<NaiveDateTime> {
    let earliest = now.year() - 49;

    // `rem_euclid` keeps the offset non-negative, so the result is the first year at or after
    // `earliest` with the same last two digits, and is at most fifty years ahead of `now`.
    parsed.with_year(earliest + (parsed.year() - earliest).rem_euclid(100))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The example RFC 9110 gives for each form, which all denote the same instant.
    const EXAMPLES: [&str; 3] = [
        "Sun, 06 Nov 1994 08:49:37 GMT",
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
    ];

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0)
            .single()
            .expect("a valid instant")
    }

    #[test]
    fn every_specified_form_is_accepted() {
        let expected = Utc
            .with_ymd_and_hms(1994, 11, 6, 8, 49, 37)
            .single()
            .expect("a valid instant");

        for example in EXAMPLES {
            assert_eq!(parse(example, now()), Some(expected), "{example}");
        }
    }

    #[test]
    fn a_two_digit_year_is_read_against_the_time_of_reading() {
        // Fifty years ahead of `now` is the last year the two digits can denote; the year after
        // that is more than fifty years ahead, and so belongs to the previous century.
        for (value, expected) in [("76", 2076), ("77", 1977), ("26", 2026), ("94", 1994)] {
            let parsed = parse(&format!("Friday, 21-Aug-{value} 12:00:00 GMT"), now())
                .unwrap_or_else(|| panic!("{value} is a year"));

            assert_eq!(parsed.year(), expected, "{value}");
        }
    }

    #[test]
    fn unparseable_values_are_rejected() {
        for value in [
            "",
            "not a date",
            "Sun, 06 Nov 1994 08:49:37",
            "1994-11-06T08:49:37Z",
        ] {
            assert_eq!(parse(value, now()), None, "{value}");
        }
    }

    /// Every instant written in the preferred form is read back unchanged, and the asctime form
    /// of the same instant reads back the same way.
    #[test_strategy::proptest]
    fn the_preferred_form_round_trips(
        #[strategy(0..=253_402_300_799i64)] timestamp: i64,
        #[strategy(proptest::sample::select(vec![-49, 0, 50]))] year_offset: i32,
    ) {
        let expected = DateTime::from_timestamp(timestamp, 0).expect("a valid instant");
        let now = expected
            .with_year(expected.year() - year_offset)
            .unwrap_or(expected);

        prop_assert_eq!(
            parse(&expected.format(IMF_FIXDATE).to_string(), now),
            Some(expected)
        );
        prop_assert_eq!(
            parse(&expected.format(ASCTIME).to_string(), now),
            Some(expected)
        );
    }
}
