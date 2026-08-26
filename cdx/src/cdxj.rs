//! The CDXJ line model.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

use crate::capture::Capture;
use crate::properties::{self, ExtraProperties};
use crate::timestamp::{self, Timestamp};

/// A CDXJ line cannot be parsed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The line does not contain a key, timestamp, and JSON object.
    #[error("truncated CDXJ line: {0}")]
    Truncated(String),
    /// The timestamp is invalid.
    #[error(transparent)]
    InvalidTimestamp(#[from] timestamp::Error),
    /// The JSON field object is invalid.
    #[error("invalid CDXJ field block")]
    InvalidFields(#[source] serde_json::Error),
}

/// A single CDXJ line.
///
/// [`Item`] uses lenient [`Fields`] by default; [`ConformingItem`] requires every standard field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Item<'a, F = Fields<'a>> {
    /// The searchable URL key.
    pub key: Cow<'a, str>,
    /// The capture timestamp.
    pub timestamp: Timestamp,
    /// The JSON field object.
    pub fields: F,
}

impl<'a> Item<'a> {
    /// Parse a CDXJ line without its trailing newline.
    pub fn parse(line: &'a str) -> Result<Self, Error> {
        let (key, rest) = line
            .split_once(' ')
            .ok_or_else(|| Error::Truncated(line.to_owned()))?;
        let (timestamp, fields) = rest
            .split_once(' ')
            .ok_or_else(|| Error::Truncated(line.to_owned()))?;
        if key.is_empty() || timestamp.is_empty() || fields.is_empty() {
            return Err(Error::Truncated(line.to_owned()));
        }

        Ok(Self {
            key: Cow::Borrowed(key),
            timestamp: timestamp.parse()?,
            fields: serde_json::from_str(fields).map_err(Error::InvalidFields)?,
        })
    }

    /// Detach this item from its input.
    #[must_use]
    pub fn into_owned(self) -> Item<'static> {
        Item {
            key: Cow::Owned(self.key.into_owned()),
            timestamp: self.timestamp,
            fields: self.fields.into_owned(),
        }
    }
}

/// A CDXJ item with every field required by CDXJ 0.1.0.
pub type ConformingItem<'a> = Item<'a, ConformingFields<'a>>;

impl FromStr for Item<'static> {
    type Err = Error;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let item: Item<'_> = Item::parse(line)?;
        Ok(item.into_owned())
    }
}

impl<F: serde::Serialize> fmt::Display for Item<'_, F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fields = serde_json::to_string(&self.fields).map_err(|_| fmt::Error)?;
        write!(formatter, "{} {} {fields}", self.key, self.timestamp)
    }
}

impl<'a> TryFrom<&Item<'a>> for ConformingItem<'a> {
    type Error = ConformanceError;

    fn try_from(item: &Item<'a>) -> Result<Self, Self::Error> {
        Ok(Self {
            key: item.key.clone(),
            timestamp: item.timestamp,
            fields: ConformingFields::try_from(&item.fields)?,
        })
    }
}

/// A lenient CDXJ JSON field object.
///
/// Numeric fields accept JSON numbers and decimal strings. They are emitted as strings to match
/// established CDXJ indexers. Unrecognized properties are preserved.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Fields<'a> {
    /// The original URL.
    #[serde(borrow)]
    pub url: Cow<'a, str>,
    /// The response payload digest.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub digest: Option<Cow<'a, str>>,
    /// The response media type.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub mime: Option<Cow<'a, str>>,
    /// The HTTP response status.
    #[serde(
        default,
        deserialize_with = "crate::attributes::optional_integer",
        serialize_with = "crate::attributes::optional_integer_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub status: Option<u16>,
    /// The WARC record offset.
    #[serde(
        default,
        deserialize_with = "crate::attributes::optional_integer",
        serialize_with = "crate::attributes::optional_integer_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub offset: Option<u64>,
    /// The WARC record length.
    #[serde(
        default,
        deserialize_with = "crate::attributes::optional_integer",
        serialize_with = "crate::attributes::optional_integer_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub length: Option<u64>,
    /// The WARC filename.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub filename: Option<Cow<'a, str>>,
    /// The digest of the complete stored WARC record.
    #[serde(
        rename = "recordDigest",
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub record_digest: Option<Cow<'a, str>>,
    /// Additional JSON properties.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl Fields<'_> {
    /// Detach this field object from its input.
    #[must_use]
    pub fn into_owned(self) -> Fields<'static> {
        Fields {
            url: owned(self.url),
            digest: self.digest.map(owned),
            mime: self.mime.map(owned),
            status: self.status,
            offset: self.offset,
            length: self.length,
            filename: self.filename.map(owned),
            record_digest: self.record_digest.map(owned),
            extra: self.extra,
        }
    }
}

/// A CDXJ field object with all fields required by CDXJ 0.1.0.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ConformingFields<'a> {
    /// The original URL.
    url: Cow<'a, str>,
    /// The response payload digest.
    digest: Cow<'a, str>,
    /// The response media type.
    mime: Cow<'a, str>,
    /// The HTTP response status.
    #[serde(serialize_with = "crate::attributes::integer_str")]
    status: u16,
    /// The WARC record offset.
    #[serde(serialize_with = "crate::attributes::integer_str")]
    offset: u64,
    /// The WARC record length.
    #[serde(serialize_with = "crate::attributes::integer_str")]
    length: u64,
    /// The WARC filename.
    filename: Cow<'a, str>,
    /// The digest of the complete stored WARC record.
    #[serde(rename = "recordDigest", skip_serializing_if = "Option::is_none")]
    record_digest: Option<Cow<'a, str>>,
    /// Additional JSON properties.
    #[serde(flatten)]
    extra: ExtraProperties,
}

impl<'a> ConformingFields<'a> {
    /// Construct a conforming field object with no record digest or extension properties.
    #[must_use]
    pub fn new(
        url: impl Into<Cow<'a, str>>,
        digest: impl Into<Cow<'a, str>>,
        mime: impl Into<Cow<'a, str>>,
        status: u16,
        offset: u64,
        length: u64,
        filename: impl Into<Cow<'a, str>>,
    ) -> Self {
        Self {
            url: url.into(),
            digest: digest.into(),
            mime: mime.into(),
            status,
            offset,
            length,
            filename: filename.into(),
            record_digest: None,
            extra: ExtraProperties::default(),
        }
    }

    /// Set the digest of the complete stored WARC record.
    #[must_use]
    pub fn with_record_digest(mut self, value: impl Into<Cow<'a, str>>) -> Self {
        self.record_digest = Some(value.into());
        self
    }

    /// Set extension properties after checking that they do not collide with modeled properties.
    pub fn with_extra(mut self, value: ExtraProperties) -> Result<Self, properties::Error> {
        validate_extra(&value)?;
        self.extra = value;
        Ok(self)
    }

    /// The original URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The response payload digest.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The response media type.
    #[must_use]
    pub fn mime(&self) -> &str {
        &self.mime
    }

    /// The HTTP response status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// The WARC record offset.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The WARC record length.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }

    /// The WARC filename.
    #[must_use]
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// The digest of the complete stored WARC record.
    #[must_use]
    pub fn record_digest(&self) -> Option<&str> {
        self.record_digest.as_deref()
    }

    /// Additional JSON properties.
    #[must_use]
    pub const fn extra(&self) -> &ExtraProperties {
        &self.extra
    }

    /// Detach this field object from its input.
    #[must_use]
    pub fn into_owned(self) -> ConformingFields<'static> {
        ConformingFields {
            url: owned(self.url),
            digest: owned(self.digest),
            mime: owned(self.mime),
            status: self.status,
            offset: self.offset,
            length: self.length,
            filename: owned(self.filename),
            record_digest: self.record_digest.map(owned),
            extra: self.extra,
        }
    }
}

impl<'a> From<ConformingFields<'a>> for Fields<'a> {
    fn from(fields: ConformingFields<'a>) -> Self {
        Self {
            url: fields.url,
            digest: Some(fields.digest),
            mime: Some(fields.mime),
            status: Some(fields.status),
            offset: Some(fields.offset),
            length: Some(fields.length),
            filename: Some(fields.filename),
            record_digest: fields.record_digest,
            extra: fields.extra,
        }
    }
}

/// A lenient CDXJ field object cannot be converted to the conforming model.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConformanceError {
    /// Required properties are absent.
    #[error("missing required CDXJ fields: {}", .0.join(", "))]
    Missing(Vec<&'static str>),
    /// An extension property duplicates a modeled property.
    #[error(transparent)]
    Extra(#[from] properties::Error),
}

impl Fields<'_> {
    fn check_conformance(&self) -> Result<(), ConformanceError> {
        let mut missing = Vec::new();
        if self.digest.is_none() {
            missing.push("digest");
        }
        if self.mime.is_none() {
            missing.push("mime");
        }
        if self.status.is_none() {
            missing.push("status");
        }
        if self.offset.is_none() {
            missing.push("offset");
        }
        if self.length.is_none() {
            missing.push("length");
        }
        if self.filename.is_none() {
            missing.push("filename");
        }
        if !missing.is_empty() {
            return Err(ConformanceError::Missing(missing));
        }

        Ok(validate_extra(&self.extra)?)
    }
}

impl<'a> TryFrom<Fields<'a>> for ConformingFields<'a> {
    type Error = ConformanceError;

    fn try_from(fields: Fields<'a>) -> Result<Self, Self::Error> {
        fields.check_conformance()?;

        Ok(Self {
            url: fields.url,
            digest: fields.digest.expect("checked above"),
            mime: fields.mime.expect("checked above"),
            status: fields.status.expect("checked above"),
            offset: fields.offset.expect("checked above"),
            length: fields.length.expect("checked above"),
            filename: fields.filename.expect("checked above"),
            record_digest: fields.record_digest,
            extra: fields.extra,
        })
    }
}

impl<'a> TryFrom<&Fields<'a>> for ConformingFields<'a> {
    type Error = ConformanceError;

    fn try_from(fields: &Fields<'a>) -> Result<Self, Self::Error> {
        fields.check_conformance()?;

        Ok(Self {
            url: fields.url.clone(),
            digest: fields.digest.clone().expect("checked above"),
            mime: fields.mime.clone().expect("checked above"),
            status: fields.status.expect("checked above"),
            offset: fields.offset.expect("checked above"),
            length: fields.length.expect("checked above"),
            filename: fields.filename.clone().expect("checked above"),
            record_digest: fields.record_digest.clone(),
            extra: fields.extra.clone(),
        })
    }
}

impl<'a> From<Item<'a>> for Capture<'a> {
    fn from(item: Item<'a>) -> Self {
        Self {
            key: item.key,
            timestamp: item.timestamp,
            url: item.fields.url,
            mime: item.fields.mime,
            status: item.fields.status,
            digest: item.fields.digest,
            redirect: None,
            robot_flags: None,
            length: item.fields.length,
            offset: item.fields.offset,
            filename: item.fields.filename,
            record_digest: item.fields.record_digest,
            original: None,
            extra: item.fields.extra,
        }
    }
}

fn validate_extra(extra: &ExtraProperties) -> Result<(), properties::Error> {
    extra.validate(
        "CDXJ fields",
        &[
            "url",
            "digest",
            "mime",
            "status",
            "offset",
            "length",
            "filename",
            "recordDigest",
        ],
    )
}

fn owned(value: Cow<'_, str>) -> Cow<'static, str> {
    Cow::Owned(value.into_owned())
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::ToBoundedStatic for Fields<'_> {
    type Static = Fields<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::IntoBoundedStatic for Fields<'_> {
    type Static = Fields<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::ToBoundedStatic for ConformingFields<'_> {
    type Static = ConformingFields<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::IntoBoundedStatic for ConformingFields<'_> {
    type Static = ConformingFields<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::ToBoundedStatic for Item<'_> {
    type Static = Item<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::IntoBoundedStatic for Item<'_> {
    type Static = Item<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::ToBoundedStatic for ConformingItem<'_> {
    type Static = ConformingItem<'static>;

    fn to_static(&self) -> Self::Static {
        Item {
            key: Cow::Owned(self.key.to_string()),
            timestamp: self.timestamp,
            fields: self.fields.clone().into_owned(),
        }
    }
}

#[cfg(feature = "bounded-static")]
#[cfg_attr(docsrs, doc(cfg(feature = "bounded-static")))]
impl bounded_static::IntoBoundedStatic for ConformingItem<'_> {
    type Static = ConformingItem<'static>;

    fn into_static(self) -> Self::Static {
        Item {
            key: Cow::Owned(self.key.into_owned()),
            timestamp: self.timestamp,
            fields: self.fields.into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    const EXAMPLE: &str = concat!(
        "com,example)/ 20201007212236 {\"url\":\"https://example.com/\",",
        "\"digest\":\"sha256:test\",\"mime\":\"text/html\",\"status\":\"200\",",
        "\"offset\":\"784\",\"length\":\"1300\",\"filename\":\"data.warc.gz\"}",
    );

    #[test]
    fn parses_spec_shape() -> Result<(), Box<dyn std::error::Error>> {
        let item = Item::parse(EXAMPLE)?;
        assert_eq!(item.key, "com,example)/");
        assert_eq!(item.fields.status, Some(200));
        assert_eq!(item.fields.offset, Some(784));
        assert_eq!(item.fields.length, Some(1300));
        Ok(())
    }

    #[test]
    fn accepts_numeric_fields_and_extensions() -> Result<(), Box<dyn std::error::Error>> {
        let item = Item::parse(
            "com,example)/ 20201007212236 {\"url\":\"https://example.com/\",\"offset\":784,\"custom\":true}",
        )?;
        assert_eq!(item.fields.offset, Some(784));
        assert_eq!(item.fields.extra["custom"], true);
        assert!(matches!(item.fields.url, Cow::Borrowed(_)));
        Ok(())
    }

    #[test]
    fn display_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let item = Item::parse(EXAMPLE)?;
        assert_eq!(Item::parse(&item.to_string())?, item);
        Ok(())
    }

    #[test]
    fn required_fields_report_all_missing_properties() -> Result<(), Error> {
        let item = Item::parse("com,example)/ 20201007212236 {\"url\":\"https://example.com/\"}")?;
        assert_eq!(
            ConformingFields::try_from(&item.fields),
            Err(ConformanceError::Missing(vec![
                "digest", "mime", "status", "offset", "length", "filename"
            ]))
        );
        Ok(())
    }

    #[test]
    fn required_fields_reject_extension_collisions() {
        let mut fields = Fields {
            url: "https://example.com/".into(),
            digest: Some("sha1:test".into()),
            mime: Some("text/html".into()),
            status: Some(200),
            offset: Some(0),
            length: Some(1),
            filename: Some("data.warc.gz".into()),
            record_digest: None,
            extra: ExtraProperties::default(),
        };
        fields.extra.insert("offset".to_owned(), 1.into());

        assert!(matches!(
            ConformingFields::try_from(&fields),
            Err(ConformanceError::Extra(properties::Error { property, .. }))
                if property == "offset"
        ));
    }

    #[test_strategy::proptest]
    fn text_round_trips(#[strategy(strategies::item())] item: Item<'static>) {
        let line = item.to_string();

        prop_assert_eq!(Item::parse(&line).map(Item::into_owned).ok(), Some(item));
    }

    /// The lenient field object accepts a JSON number wherever it accepts a numeric string.
    #[test_strategy::proptest]
    fn numbers_and_numeric_strings_agree(
        status: u16,
        offset: u64,
        length: u64,
        #[strategy(strategies::json_text())] filename: String,
    ) {
        let filename = serde_json::Value::String(filename);
        let quoted = format!(
            "{{\"url\":\"u\",\"status\":\"{status}\",\"offset\":\"{offset}\",\
             \"length\":\"{length}\",\"filename\":{filename}}}"
        );
        let bare = format!(
            "{{\"url\":\"u\",\"status\":{status},\"offset\":{offset},\
             \"length\":{length},\"filename\":{filename}}}"
        );

        let parsed = serde_json::from_str::<Fields<'_>>(&quoted).ok();

        prop_assert!(parsed.is_some());
        prop_assert_eq!(parsed, serde_json::from_str::<Fields<'_>>(&bare).ok());
    }

    #[test_strategy::proptest]
    fn conforming_fields_survive_the_lenient_form(
        #[strategy(strategies::conforming_fields())] fields: ConformingFields<'static>,
    ) {
        let lenient = Fields::from(fields.clone());

        prop_assert_eq!(ConformingFields::try_from(&lenient).ok(), Some(fields));
    }
}
