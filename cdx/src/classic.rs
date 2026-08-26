//! Header-described, delimiter-separated CDX records.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use crate::capture::{self, Capture};
use crate::field::Field;

/// A classic CDX header or record is malformed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    /// The header does not begin with `CDX` in either accepted form.
    #[error("invalid CDX header: {0}")]
    InvalidHeader(String),
    /// The legend has no fields or contains an empty field marker.
    #[error("invalid CDX legend: {0}")]
    InvalidLegend(String),
    /// A record has a different number of values from its legend.
    #[error("CDX record has {actual} fields, expected {expected}")]
    FieldCount {
        /// Number of fields named by the header.
        expected: usize,
        /// Number of values in the record.
        actual: usize,
    },
    /// A record cannot be converted to the common capture model.
    #[error(transparent)]
    Capture(#[from] capture::Error),
}

/// A classic CDX legend and its delimiter.
///
/// Classic headers may start with the delimiter (for example, ` CDX N b a`) or directly with
/// `CDX`. Both forms are retained when the header is displayed again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header<'a> {
    delimiter: char,
    leading_delimiter: bool,
    markers: Vec<Cow<'a, str>>,
}

impl<'a> Header<'a> {
    /// Construct a header from a delimiter and legend markers.
    pub fn new(
        delimiter: char,
        leading_delimiter: bool,
        markers: Vec<Cow<'a, str>>,
    ) -> Result<Self, Error> {
        if matches!(delimiter, '\r' | '\n')
            || markers.is_empty()
            || markers
                .iter()
                .any(|marker| marker.is_empty() || marker.contains(delimiter))
        {
            return Err(Error::InvalidLegend(join(delimiter, &markers)));
        }
        Ok(Self {
            delimiter,
            leading_delimiter,
            markers,
        })
    }

    /// Parse a classic CDX header without its trailing newline.
    pub fn parse(line: &'a str) -> Result<Self, Error> {
        if line.contains(['\r', '\n']) {
            return Err(Error::InvalidHeader(line.to_owned()));
        }

        let (leading_delimiter, delimiter, rest) = if let Some(rest) = line.strip_prefix("CDX") {
            let delimiter = rest
                .chars()
                .next()
                .ok_or_else(|| Error::InvalidLegend(line.to_owned()))?;
            (false, delimiter, &rest[delimiter.len_utf8()..])
        } else {
            let delimiter = line
                .chars()
                .next()
                .ok_or_else(|| Error::InvalidHeader(line.to_owned()))?;
            let rest = &line[delimiter.len_utf8()..];
            let rest = rest
                .strip_prefix("CDX")
                .ok_or_else(|| Error::InvalidHeader(line.to_owned()))?;
            let rest = rest
                .strip_prefix(delimiter)
                .ok_or_else(|| Error::InvalidHeader(line.to_owned()))?;
            (true, delimiter, rest)
        };

        // `new` can only reject the legend itself, since the delimiter is a character of this
        // line, which has no line break. Report the whole header rather than that fragment.
        let markers = rest.split(delimiter).map(Cow::Borrowed).collect::<Vec<_>>();
        Self::new(delimiter, leading_delimiter, markers)
            .map_err(|_| Error::InvalidLegend(line.to_owned()))
    }

    /// Construct the common 11-field CDX legend (`N b a m s k r M S V g`).
    #[must_use]
    pub fn standard_11() -> Header<'static> {
        Self::standard(&["N", "b", "a", "m", "s", "k", "r", "M", "S", "V", "g"])
    }

    /// Construct the older 9-field CDX legend (`N b a m s k r V g`).
    #[must_use]
    pub fn standard_9() -> Header<'static> {
        Self::standard(&["N", "b", "a", "m", "s", "k", "r", "V", "g"])
    }

    /// The field delimiter.
    #[must_use]
    pub const fn delimiter(&self) -> char {
        self.delimiter
    }

    /// Whether the serialized header starts with the delimiter.
    #[must_use]
    pub const fn has_leading_delimiter(&self) -> bool {
        self.leading_delimiter
    }

    /// The legend markers in record order.
    #[must_use]
    pub fn markers(&self) -> &[Cow<'a, str>] {
        &self.markers
    }

    /// Detach this header from its input.
    #[must_use]
    pub fn into_owned(self) -> Header<'static> {
        Header {
            delimiter: self.delimiter,
            leading_delimiter: self.leading_delimiter,
            markers: self
                .markers
                .into_iter()
                .map(|marker| Cow::Owned(marker.into_owned()))
                .collect(),
        }
    }

    /// Interpret a legend marker as a semantic field.
    #[must_use]
    pub fn field(&self, index: usize) -> Option<Field<'_>> {
        self.markers.get(index).map(|marker| marker_field(marker))
    }

    /// Parse a record using this header.
    pub fn parse_record<'b>(&self, line: &'b str) -> Result<Record<'b>, Error> {
        let values = line
            .split(self.delimiter)
            .map(Cow::Borrowed)
            .collect::<Vec<_>>();
        self.check_count(values.len())?;
        Ok(Record { values })
    }

    /// Convert a record to the representation-neutral capture model.
    pub fn capture<'b>(&self, record: &Record<'b>) -> Result<Capture<'b>, Error> {
        self.check_count(record.values.len())?;
        let fields = self
            .markers
            .iter()
            .zip(&record.values)
            .map(|(marker, value)| (marker_field(marker), value.clone()))
            .collect::<Vec<_>>();
        Ok(crate::capture::from_fields(&fields)?)
    }

    /// Format a record with this header's delimiter.
    pub fn render(&self, record: &Record<'_>) -> Result<String, Error> {
        self.check_count(record.values.len())?;
        Ok(join(self.delimiter, &record.values))
    }

    fn standard(markers: &[&'static str]) -> Header<'static> {
        Header {
            delimiter: ' ',
            leading_delimiter: true,
            markers: markers
                .iter()
                .map(|marker| Cow::Borrowed(*marker))
                .collect(),
        }
    }

    const fn check_count(&self, actual: usize) -> Result<(), Error> {
        if actual == self.markers.len() {
            Ok(())
        } else {
            Err(Error::FieldCount {
                expected: self.markers.len(),
                actual,
            })
        }
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::ToBoundedStatic for Header<'_> {
    type Static = Header<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::IntoBoundedStatic for Header<'_> {
    type Static = Header<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::ToBoundedStatic for Record<'_> {
    type Static = Record<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::IntoBoundedStatic for Record<'_> {
    type Static = Record<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

impl fmt::Display for Header<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.leading_delimiter {
            write!(formatter, "{}", self.delimiter)?;
        }
        write!(formatter, "CDX{}", self.delimiter)?;
        formatter.write_str(&join(self.delimiter, &self.markers))
    }
}

impl FromStr for Header<'static> {
    type Err = Error;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let header: Header<'_> = Header::parse(line)?;
        Ok(header.into_owned())
    }
}

/// Interpret a legend marker: single characters are classic markers, anything longer is a name.
fn marker_field(marker: &str) -> Field<'_> {
    if marker.chars().count() == 1 {
        Field::classic(marker)
    } else {
        Field::named(marker)
    }
}

/// Join values with a delimiter character.
fn join(delimiter: char, values: &[Cow<'_, str>]) -> String {
    let mut joined = String::with_capacity(values.iter().map(|value| value.len() + 1).sum());
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            joined.push(delimiter);
        }
        joined.push_str(value);
    }
    joined
}

/// The values of a classic CDX record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record<'a> {
    values: Vec<Cow<'a, str>>,
}

impl<'a> Record<'a> {
    /// Construct a record from values in legend order.
    #[must_use]
    pub const fn new(values: Vec<Cow<'a, str>>) -> Self {
        Self { values }
    }

    /// The values in legend order.
    #[must_use]
    pub fn values(&self) -> &[Cow<'a, str>] {
        &self.values
    }

    /// Detach this record from its input.
    #[must_use]
    pub fn into_owned(self) -> Record<'static> {
        Record {
            values: self
                .values
                .into_iter()
                .map(|value| Cow::Owned(value.into_owned()))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    #[test]
    fn parses_standard_record() -> Result<(), Box<dyn std::error::Error>> {
        let header = Header::parse(" CDX N b a m s k r M S V g")?;
        let record = header.parse_record(concat!(
            "com,example)/ 20201007212236 https://example.com/ text/html 200 ",
            "sha1:TEST - - 1300 784 data.warc.gz"
        ))?;
        let capture = header.capture(&record)?;

        assert_eq!(capture.key, "com,example)/");
        assert_eq!(capture.status, Some(200));
        assert_eq!(capture.offset, Some(784));
        assert_eq!(capture.length, Some(1300));
        assert_eq!(capture.redirect, None);
        Ok(())
    }

    #[test]
    fn retains_header_form_and_unknown_fields() -> Result<(), Box<dyn std::error::Error>> {
        let header = Header::parse("CDX|urlkey|timestamp|url|custom")?;
        let record =
            header.parse_record("com,example)/|20201007212236|https://example.com/|value")?;
        assert_eq!(header.to_string(), "CDX|urlkey|timestamp|url|custom");
        assert_eq!(
            header.render(&record)?,
            "com,example)/|20201007212236|https://example.com/|value"
        );
        assert_eq!(header.capture(&record)?.extra["custom"], "value");
        Ok(())
    }

    #[test]
    fn keeps_unmodeled_fields_under_their_markers() -> Result<(), Box<dyn std::error::Error>> {
        let header = Header::parse(" CDX N b a e c")?;
        let record =
            header.parse_record("com,example)/ 20201007212236 https://example.com/ 10.0.0.1 -")?;

        assert_eq!(header.field(3), Some(Field::Other(Cow::Borrowed("e"))));
        let capture = header.capture(&record)?;
        assert_eq!(capture.extra["e"], "10.0.0.1");
        assert!(!capture.extra.contains_key("c"));
        Ok(())
    }

    #[test_strategy::proptest]
    fn header_and_record_round_trip(
        #[strategy(strategies::legend_and_values())] legend: (char, bool, Vec<String>, Vec<String>),
    ) {
        let (delimiter, leading_delimiter, markers, values) = legend;
        let header = Header::new(
            delimiter,
            leading_delimiter,
            markers.into_iter().map(Cow::Owned).collect(),
        )
        .unwrap();
        let record = Record::new(values.into_iter().map(Cow::Owned).collect());
        let header_text = header.to_string();
        let record_text = header.render(&record).unwrap();

        prop_assert_eq!(
            Header::parse(&header_text).map(Header::into_owned).ok(),
            Some(header.clone())
        );
        prop_assert_eq!(
            header
                .parse_record(&record_text)
                .map(Record::into_owned)
                .ok(),
            Some(record)
        );
    }

    #[test_strategy::proptest]
    fn the_standard_legend_recovers_every_value(
        #[strategy(strategies::capture_parts())] parts: strategies::CaptureParts,
    ) {
        let capture = Header::standard_11().capture(&parts.record()).unwrap();

        prop_assert_eq!(capture.key.as_ref(), parts.key.as_str());
        prop_assert_eq!(capture.timestamp, parts.timestamp);
        prop_assert_eq!(capture.url.as_ref(), parts.url.as_str());
        prop_assert_eq!(capture.mime.as_deref(), parts.mime.as_deref());
        prop_assert_eq!(capture.status, parts.status);
        prop_assert_eq!(capture.digest.as_deref(), parts.digest.as_deref());
        prop_assert_eq!(capture.redirect.as_deref(), parts.redirect.as_deref());
        prop_assert_eq!(capture.robot_flags.as_deref(), parts.robot_flags.as_deref());
        prop_assert_eq!(capture.length, parts.length);
        prop_assert_eq!(capture.offset, parts.offset);
        prop_assert_eq!(capture.filename.as_deref(), parts.filename.as_deref());
        prop_assert!(capture.original.is_none());
        prop_assert!(capture.extra.is_empty());
    }
}
