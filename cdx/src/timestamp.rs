//! Capture timestamps shared by CDX representations.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use chrono::{DateTime, NaiveDateTime, SubsecRound as _, Utc};

const SECONDS_FORMAT: &str = "%Y%m%d%H%M%S";
const SECONDS_LENGTH: usize = 14;
const MILLISECONDS_LENGTH: usize = 17;

/// A CDX timestamp is malformed or names an invalid UTC instant.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid CDX timestamp: {0}")]
pub struct TimestampError(pub String);

/// A 14- or 17-digit CDX timestamp (`YYYYmmddHHMMSS[sss]`, always UTC).
///
/// The shorter form has whole-second precision; the longer form appends milliseconds. Parsing and
/// display preserve the precision. Equality, hashing, and ordering follow the serialized form, so
/// whole seconds sort before the millisecond form of the same instant.
#[derive(Clone, Copy, Debug)]
pub struct Timestamp {
    instant: DateTime<Utc>,
    milliseconds: bool,
}

impl Timestamp {
    /// Create a timestamp, truncating the instant to whole seconds.
    #[must_use]
    pub fn new(instant: DateTime<Utc>) -> Self {
        Self {
            instant: instant.trunc_subsecs(0),
            milliseconds: false,
        }
    }

    /// Create a 17-digit timestamp, truncating the instant to milliseconds.
    #[must_use]
    pub fn with_milliseconds(instant: DateTime<Utc>) -> Self {
        Self {
            instant: instant.trunc_subsecs(3),
            milliseconds: true,
        }
    }

    /// The represented UTC instant.
    #[must_use]
    pub const fn datetime(self) -> DateTime<Utc> {
        self.instant
    }

    /// Whether the timestamp includes milliseconds.
    #[must_use]
    pub const fn has_milliseconds(self) -> bool {
        self.milliseconds
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.instant.format(SECONDS_FORMAT))?;
        if self.milliseconds {
            write!(formatter, "{:03}", self.instant.timestamp_subsec_millis())?;
        }
        Ok(())
    }
}

impl FromStr for Timestamp {
    type Err = TimestampError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !matches!(value.len(), SECONDS_LENGTH | MILLISECONDS_LENGTH)
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(TimestampError(value.to_owned()));
        }

        let seconds = NaiveDateTime::parse_from_str(&value[..SECONDS_LENGTH], SECONDS_FORMAT)
            .map_err(|_| TimestampError(value.to_owned()))?
            .and_utc();
        // Chrono represents a leap second as second 59 plus at least one billion nanoseconds.
        if seconds.timestamp_subsec_nanos() >= 1_000_000_000 {
            return Err(TimestampError(value.to_owned()));
        }

        if value.len() == MILLISECONDS_LENGTH {
            let milliseconds = value[SECONDS_LENGTH..]
                .parse::<i64>()
                .map_err(|_| TimestampError(value.to_owned()))?;
            Ok(Self::with_milliseconds(
                seconds + chrono::TimeDelta::milliseconds(milliseconds),
            ))
        } else {
            Ok(Self::new(seconds))
        }
    }
}

impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        self.instant == other.instant && self.milliseconds == other.milliseconds
    }
}

impl Eq for Timestamp {}

impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.instant
            .cmp(&other.instant)
            .then(self.milliseconds.cmp(&other.milliseconds))
    }
}

impl Hash for Timestamp {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.instant.hash(state);
        self.milliseconds.hash(state);
    }
}

impl From<DateTime<Utc>> for Timestamp {
    fn from(value: DateTime<Utc>) -> Self {
        Self::new(value)
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::ToBoundedStatic for Timestamp {
    type Static = Self;

    fn to_static(&self) -> Self::Static {
        *self
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::IntoBoundedStatic for Timestamp {
    type Static = Self;

    fn into_static(self) -> Self::Static {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_precision_and_text_order() -> Result<(), TimestampError> {
        let forms = [
            "20201007212235999",
            "20201007212236",
            "20201007212236000",
            "20201007212236001",
            "20201007212237",
        ];
        let values = forms
            .iter()
            .map(|value| value.parse::<Timestamp>())
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(values[1].to_string(), forms[1]);
        assert_eq!(values[2].to_string(), forms[2]);
        assert!(!values[1].has_milliseconds());
        assert!(values[2].has_milliseconds());
        assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
        Ok(())
    }

    #[test]
    fn rejects_invalid_forms() {
        for value in [
            "2020100721223",
            "2020100721223a",
            "2020100721223600",
            "20201007212236a00",
            "20201307212236",
            "20201007216000",
            "20201007212260",
        ] {
            assert!(value.parse::<Timestamp>().is_err(), "accepted {value}");
        }
    }

    #[test]
    fn serde_uses_the_cdx_text_form() -> Result<(), Box<dyn std::error::Error>> {
        let timestamp = "20201007212236123".parse::<Timestamp>()?;
        let json = serde_json::to_string(&timestamp)?;
        assert_eq!(json, "\"20201007212236123\"");
        assert_eq!(serde_json::from_str::<Timestamp>(&json)?, timestamp);
        Ok(())
    }
}
