//! A format-neutral capture record.

use std::borrow::Cow;

use crate::field::Field;
use crate::properties::ExtraProperties;
use crate::timestamp::Timestamp;

/// A standard field in a CDX record is absent or malformed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    /// A field required for a capture model is absent.
    #[error("missing CDX field `{0}`")]
    Missing(&'static str),
    /// A standard field cannot be parsed.
    #[error("invalid CDX field `{field}`: `{value}`")]
    Invalid {
        /// The field name.
        field: &'static str,
        /// The unparseable value.
        value: String,
    },
}

/// The common capture semantics represented by classic CDX, CDXJ, and CDX Server JSON.
///
/// The searchable key, timestamp, and original URL are required. Other standard fields are
/// optional because classic layouts and CDX Server field selections vary. A single hyphen in a
/// text representation becomes `None`; fields that are not modeled are retained in
/// [`extra`](Self::extra).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture<'a> {
    /// Searchable URL key, usually a SURT but retained as text for non-URL records and legacy data.
    pub key: Cow<'a, str>,
    /// Capture timestamp.
    pub timestamp: Timestamp,
    /// Original captured URL.
    pub url: Cow<'a, str>,
    /// Response media type.
    pub mime: Option<Cow<'a, str>>,
    /// HTTP response status.
    pub status: Option<u16>,
    /// Payload digest in the encoding used by the index producer.
    pub digest: Option<Cow<'a, str>>,
    /// Redirect target.
    pub redirect: Option<Cow<'a, str>>,
    /// Robots or AIF meta flags.
    pub robot_flags: Option<Cow<'a, str>>,
    /// Stored record length. This is signed because negative values occur in public CDX results.
    pub length: Option<i64>,
    /// Stored record offset.
    pub offset: Option<u64>,
    /// Archive filename.
    pub filename: Option<Cow<'a, str>>,
    /// Digest of the complete stored record.
    pub record_digest: Option<Cow<'a, str>>,
    /// Resolved original location for revisit records.
    pub original: Option<Location<'a>>,
    /// Fields that are not modeled, keyed by the names or legend markers they appeared with.
    pub extra: ExtraProperties,
}

/// The original payload-bearing record referenced by a revisit entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location<'a> {
    /// Stored record length.
    pub length: Option<i64>,
    /// Stored record offset.
    pub offset: Option<u64>,
    /// Archive filename.
    pub filename: Option<Cow<'a, str>>,
}

impl Capture<'_> {
    /// Detach this capture from borrowed input.
    #[must_use]
    pub fn into_owned(self) -> Capture<'static> {
        Capture {
            key: Cow::Owned(self.key.into_owned()),
            timestamp: self.timestamp,
            url: Cow::Owned(self.url.into_owned()),
            mime: self.mime.map(|value| Cow::Owned(value.into_owned())),
            status: self.status,
            digest: self.digest.map(|value| Cow::Owned(value.into_owned())),
            redirect: self.redirect.map(|value| Cow::Owned(value.into_owned())),
            robot_flags: self.robot_flags.map(|value| Cow::Owned(value.into_owned())),
            length: self.length,
            offset: self.offset,
            filename: self.filename.map(|value| Cow::Owned(value.into_owned())),
            record_digest: self
                .record_digest
                .map(|value| Cow::Owned(value.into_owned())),
            original: self.original.map(Location::into_owned),
            extra: self.extra,
        }
    }
}

impl Location<'_> {
    fn into_owned(self) -> Location<'static> {
        Location {
            length: self.length,
            offset: self.offset,
            filename: self.filename.map(|value| Cow::Owned(value.into_owned())),
        }
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::ToBoundedStatic for Capture<'_> {
    type Static = Capture<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::IntoBoundedStatic for Capture<'_> {
    type Static = Capture<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::ToBoundedStatic for Location<'_> {
    type Static = Location<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::IntoBoundedStatic for Location<'_> {
    type Static = Location<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

pub(crate) fn present(value: &str) -> Option<&str> {
    (value != "-").then_some(value)
}

pub(crate) fn from_fields<'a>(fields: &[(Field<'_>, Cow<'a, str>)]) -> Result<Capture<'a>, Error> {
    let key = required_value(fields, &Field::UrlKey, "urlkey")?;
    let timestamp = required_value(fields, &Field::Timestamp, "timestamp")?;
    let url = required_value(fields, &Field::Url, "url")?;

    let timestamp = timestamp
        .parse()
        .map_err(|_| invalid("timestamp", timestamp.as_ref()))?;
    let original = location(fields)?;
    let mut extra = ExtraProperties::default();

    for (field, value) in fields {
        if let (Field::Other(name), Some(value)) = (field, present(value)) {
            extra.insert(
                name.to_string(),
                serde_json::Value::String(value.to_owned()),
            );
        }
    }

    Ok(Capture {
        key,
        timestamp,
        url,
        mime: optional_value(fields, &Field::Mime),
        status: parse_optional(fields, &Field::Status, "status")?,
        digest: optional_value(fields, &Field::Digest),
        redirect: optional_value(fields, &Field::Redirect),
        robot_flags: optional_value(fields, &Field::RobotFlags),
        length: parse_optional(fields, &Field::Length, "length")?,
        offset: parse_optional(fields, &Field::Offset, "offset")?,
        filename: optional_value(fields, &Field::Filename),
        record_digest: optional_value(fields, &Field::RecordDigest),
        original,
        extra,
    })
}

fn required_value<'a>(
    fields: &[(Field<'_>, Cow<'a, str>)],
    target: &Field<'_>,
    name: &'static str,
) -> Result<Cow<'a, str>, Error> {
    fields
        .iter()
        .find(|(field, _)| field == target)
        .and_then(|(_, value)| present(value).map(|_| value.clone()))
        .ok_or(Error::Missing(name))
}

fn optional_value<'a>(
    fields: &[(Field<'_>, Cow<'a, str>)],
    target: &Field<'_>,
) -> Option<Cow<'a, str>> {
    fields
        .iter()
        .find(|(field, _)| field == target)
        .and_then(|(_, value)| present(value).map(|_| value.clone()))
}

fn parse_optional<T: std::str::FromStr>(
    fields: &[(Field<'_>, Cow<'_, str>)],
    target: &Field<'_>,
    name: &'static str,
) -> Result<Option<T>, Error> {
    optional_value(fields, target)
        .map(|value| value.parse().map_err(|_| invalid(name, value.as_ref())))
        .transpose()
}

fn location<'a>(fields: &[(Field<'_>, Cow<'a, str>)]) -> Result<Option<Location<'a>>, Error> {
    let length = parse_optional(fields, &Field::OriginalLength, "orig.length")?;
    let offset = parse_optional(fields, &Field::OriginalOffset, "orig.offset")?;
    let filename = optional_value(fields, &Field::OriginalFilename);
    Ok(
        (length.is_some() || offset.is_some() || filename.is_some()).then_some(Location {
            length,
            offset,
            filename,
        }),
    )
}

fn invalid(field: &'static str, value: &str) -> Error {
    Error::Invalid {
        field,
        value: value.to_owned(),
    }
}
