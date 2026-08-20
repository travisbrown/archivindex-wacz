//! CDXJ index lines mapping searchable URL keys to WARC records.
//!
//! A CDXJ index is a line-oriented text format. Each line pairs a searchable URL key (a
//! [SURT](http://crawler.archive.org/articles/user_manual/glossary.html#surt)) and a 14- or
//! 17-digit timestamp with a JSON block locating a capture within a WARC file. Lines are sorted
//! lexicographically for binary search.

use std::borrow::Cow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::BufRead;
use std::str::FromStr;

use bounded_static::{IntoBoundedStatic, ToStatic};
// Import the trait anonymously to make `trunc_subsecs` available without binding its name.
use chrono::{DateTime, NaiveDateTime, SubsecRound as _, Utc};

use crate::ExtraProperties;
use crate::digest::Sha256Digest;
use crate::lines::Lines;

/// The whole-second portion of a timestamp used in CDXJ lines.
const SECONDS_FORMAT: &str = "%Y%m%d%H%M%S";

/// The length of a whole-second CDXJ timestamp.
const SECONDS_LENGTH: usize = 14;

/// The length of a millisecond-precision CDXJ timestamp.
const MILLISECONDS_LENGTH: usize = 17;

/// An error type for CDXJ parsing and key generation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying stream could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The line does not contain the three space-separated parts of a CDXJ item.
    #[error("truncated CDXJ line: {0}")]
    Truncated(String),
    /// The timestamp is not a 14-digit `YYYYmmddHHMMSS` or 17-digit
    /// `YYYYmmddHHMMSSsss` value.
    #[error("invalid CDX timestamp: {0}")]
    InvalidTimestamp(String),
    /// The JSON block could not be parsed.
    #[error("invalid CDXJ field block")]
    InvalidFields(#[source] serde_json::Error),
    /// A URL to be transformed into a searchable key could not be parsed.
    #[error(transparent)]
    InvalidUrl(#[from] url::ParseError),
    /// A URL to be transformed into a searchable key has no host.
    #[error("URL has no host: {0}")]
    MissingHost(String),
}

/// A 14- or 17-digit CDX timestamp (`YYYYmmddHHMMSS[sss]`, always UTC).
///
/// The shorter form has whole-second precision; the longer form appends three millisecond digits.
/// Parsing and display preserve which form was used. Equality, hashing, and ordering compare the
/// represented instant, so the two encodings of an exact whole second are equal and timestamps of
/// either precision order chronologically.
#[derive(Clone, Copy, Debug, ToStatic)]
pub struct Timestamp {
    instant: DateTime<Utc>,
    milliseconds: bool,
}

impl Timestamp {
    /// Create a timestamp, truncating the instant to whole-second precision.
    #[must_use]
    pub fn new(instant: DateTime<Utc>) -> Self {
        Self {
            instant: instant.trunc_subsecs(0),
            milliseconds: false,
        }
    }

    /// Create a 17-digit timestamp, truncating the instant to millisecond precision.
    #[must_use]
    pub fn with_milliseconds(instant: DateTime<Utc>) -> Self {
        Self {
            instant: instant.trunc_subsecs(3),
            milliseconds: true,
        }
    }

    /// The underlying instant.
    #[must_use]
    pub const fn datetime(self) -> DateTime<Utc> {
        self.instant
    }

    /// Whether this timestamp is displayed with millisecond precision.
    #[must_use]
    pub const fn has_milliseconds(self) -> bool {
        self.milliseconds
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.instant.format(SECONDS_FORMAT))?;

        if self.milliseconds {
            write!(f, "{:03}", self.instant.timestamp_subsec_millis())?;
        }

        Ok(())
    }
}

impl FromStr for Timestamp {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // The length and digit checks reject values that chrono would accept through its flexible
        // handling of variable-width fields (such as five-digit years).
        if !matches!(s.len(), SECONDS_LENGTH | MILLISECONDS_LENGTH)
            || !s.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(Error::InvalidTimestamp(s.to_owned()));
        }

        let seconds = NaiveDateTime::parse_from_str(&s[..SECONDS_LENGTH], SECONDS_FORMAT)
            .map_err(|_| Error::InvalidTimestamp(s.to_owned()))?
            .and_utc();

        if s.len() == MILLISECONDS_LENGTH {
            let milliseconds = s[SECONDS_LENGTH..]
                .parse::<i64>()
                .map_err(|_| Error::InvalidTimestamp(s.to_owned()))?;

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
        self.instant == other.instant
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
        self.instant.cmp(&other.instant)
    }
}

impl Hash for Timestamp {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.instant.hash(state);
    }
}

impl From<DateTime<Utc>> for Timestamp {
    fn from(value: DateTime<Utc>) -> Self {
        Self::new(value)
    }
}

/// A single CDXJ index line.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic)]
pub struct Item<'a> {
    /// The searchable URL key the line is sorted by.
    pub key: Cow<'a, str>,
    /// The capture timestamp.
    pub timestamp: Timestamp,
    /// The JSON block locating the capture.
    pub fields: ParsedFields<'a>,
}

impl<'a> Item<'a> {
    /// Parse a CDXJ line (without its trailing newline).
    ///
    /// # Errors
    ///
    /// Fails if the line does not have three space-separated parts, if the timestamp is not a
    /// 14- or 17-digit value, or if the JSON block is invalid.
    pub fn parse(line: &'a str) -> Result<Self, Error> {
        let (key, rest) = line
            .split_once(' ')
            .ok_or_else(|| Error::Truncated(line.to_owned()))?;
        let (timestamp, fields) = rest
            .split_once(' ')
            .ok_or_else(|| Error::Truncated(line.to_owned()))?;

        Ok(Self {
            key: Cow::Borrowed(key),
            timestamp: timestamp.parse()?,
            fields: serde_json::from_str(fields).map_err(Error::InvalidFields)?,
        })
    }
}

impl fmt::Display for Item<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Serialization of the field block only fails on conditions that `Fields` values cannot
        // represent (such as non-string map keys), so the error is safely mapped to `fmt::Error`.
        let fields = serde_json::to_string(&self.fields).map_err(|_| fmt::Error)?;

        write!(f, "{} {} {}", self.key, self.timestamp, fields)
    }
}

/// The JSON block of a CDXJ line.
///
/// The numeric fields are written as decimal strings, following the convention of pywb-family
/// indexers, but are accepted as either strings or JSON numbers on parsing.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
pub struct ParsedFields<'a> {
    /// The original URL of the capture.
    #[serde(borrow)]
    pub url: Cow<'a, str>,
    /// A cryptographic digest of the HTTP response payload, in whatever encoding the indexer used.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub digest: Option<Cow<'a, str>>,
    /// The MIME type of the captured payload.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub mime: Option<Cow<'a, str>>,
    /// The HTTP status of the capture.
    #[serde(
        default,
        deserialize_with = "crate::attributes::optional_integer",
        serialize_with = "crate::attributes::optional_integer_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub status: Option<u16>,
    /// The byte offset of the record within its WARC file.
    #[serde(
        default,
        deserialize_with = "crate::attributes::optional_integer",
        serialize_with = "crate::attributes::optional_integer_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub offset: Option<u64>,
    /// The length in bytes of the record within its WARC file.
    #[serde(
        default,
        deserialize_with = "crate::attributes::optional_integer",
        serialize_with = "crate::attributes::optional_integer_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub length: Option<u64>,
    /// The name of the WARC file containing the record.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub filename: Option<Cow<'a, str>>,
    /// The SHA-256 digest of the stored bytes identified by [`offset`](Self::offset) and
    /// [`length`](Self::length). This covers a complete gzip member in a compressed WARC file or
    /// the serialized record in an uncompressed WARC file.
    #[serde(
        rename = "recordDigest",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub record_digest: Option<Sha256Digest>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

/// A conforming CDXJ field block used for index construction.
///
/// Unlike [`ParsedFields`], every property required by CDXJ 0.1.0 is present. Optional extension
/// properties, including the WACZ stored-record digest, remain optional.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Serialize)]
pub struct ConformingFields<'a> {
    /// The original URL of the capture.
    pub url: Cow<'a, str>,
    /// A cryptographic digest of the HTTP response payload.
    pub digest: Cow<'a, str>,
    /// The MIME type of the captured payload.
    pub mime: Cow<'a, str>,
    /// The HTTP status of the capture.
    #[serde(serialize_with = "crate::attributes::integer_str")]
    pub status: u16,
    /// The byte offset of the record within its WARC file.
    #[serde(serialize_with = "crate::attributes::integer_str")]
    pub offset: u64,
    /// The length in bytes of the record within its WARC file.
    #[serde(serialize_with = "crate::attributes::integer_str")]
    pub length: u64,
    /// The name of the WARC file containing the record.
    pub filename: Cow<'a, str>,
    /// The digest of the stored record bytes.
    #[serde(rename = "recordDigest", skip_serializing_if = "Option::is_none")]
    pub record_digest: Option<Sha256Digest>,
    /// Additional properties.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl<'a> From<ConformingFields<'a>> for ParsedFields<'a> {
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

/// The required properties missing from a leniently parsed CDXJ field block.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("missing required CDXJ fields: {}", .0.join(", "))]
pub struct MissingFields(pub Vec<&'static str>);

impl<'a> TryFrom<&ParsedFields<'a>> for ConformingFields<'a> {
    type Error = MissingFields;

    fn try_from(fields: &ParsedFields<'a>) -> Result<Self, Self::Error> {
        let mut missing = Vec::new();
        if fields.digest.is_none() {
            missing.push("digest");
        }
        if fields.mime.is_none() {
            missing.push("mime");
        }
        if fields.status.is_none() {
            missing.push("status");
        }
        if fields.offset.is_none() {
            missing.push("offset");
        }
        if fields.length.is_none() {
            missing.push("length");
        }
        if fields.filename.is_none() {
            missing.push("filename");
        }
        if !missing.is_empty() {
            return Err(MissingFields(missing));
        }
        Ok(Self {
            url: fields.url.clone(),
            digest: fields.digest.clone().expect("checked"),
            mime: fields.mime.clone().expect("checked"),
            status: fields.status.expect("checked"),
            offset: fields.offset.expect("checked"),
            length: fields.length.expect("checked"),
            filename: fields.filename.clone().expect("checked"),
            record_digest: fields.record_digest,
            extra: fields.extra.clone(),
        })
    }
}

/// Backwards-compatible name for leniently parsed fields.
pub type Fields<'a> = ParsedFields<'a>;

/// A reader that iteratively parses CDXJ items from a stream.
///
/// Blank lines (such as a trailing newline at the end of the file) are skipped rather than treated
/// as invalid items.
pub struct IndexReader<R> {
    lines: Lines<R>,
}

impl<R: BufRead> IndexReader<R> {
    /// Create a new reader.
    #[must_use]
    pub const fn new(reader: R) -> Self {
        Self {
            lines: Lines::new(reader),
        }
    }
}

impl<R: BufRead> Iterator for IndexReader<R> {
    type Item = Result<Item<'static>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.lines.next_content() {
            Ok(Some((_, content))) => {
                Some(Item::parse(content).map(IntoBoundedStatic::into_static))
            }
            Ok(None) => None,
            Err(error) => Some(Err(Error::Io(error))),
        }
    }
}

/// Transform a URL into a searchable key compatible with pywb's default canonicalization.
///
/// The host is lowercased, its labels are reversed and joined with commas (with any single trailing
/// dot dropped, so that `example.com.` and `example.com` share a key), and any non-default port is
/// kept. IP address hosts keep their usual order, following the SURT convention. The path and query
/// are lowercased, and query parameters are sorted so that lookups are insensitive to parameter
/// order. Userinfo and the fragment are dropped.
///
/// # Errors
///
/// Fails if the URL cannot be parsed or has no host.
pub fn search_key(url: &str) -> Result<String, Error> {
    let parsed = url::Url::parse(url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::MissingHost(url.to_owned()))?;

    let mut key = String::with_capacity(url.len());

    if let Some(url::Host::Domain(domain)) = parsed.host() {
        let domain = domain.strip_suffix('.').unwrap_or(domain);

        for (i, label) in domain.split('.').rev().enumerate() {
            if i > 0 {
                key.push(',');
            }

            key.push_str(label);
        }
    } else {
        // An IP address host (`host_str` keeps the brackets of an IPv6 address).
        key.push_str(host);
    }

    // `Url::port` is `None` when the port is the default for the scheme.
    if let Some(port) = parsed.port() {
        key.push(':');
        key.push_str(&port.to_string());
    }

    key.push(')');
    key.push_str(&parsed.path().to_lowercase());

    if let Some(query) = parsed.query()
        && !query.is_empty()
    {
        let lowered = query.to_lowercase();
        let mut parameters = lowered.split('&').collect::<Vec<_>>();
        parameters.sort_unstable();

        key.push('?');
        key.push_str(&parameters.join("&"));
    }

    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = concat!(
        "com,example)/ 20201007212236 {\"url\": \"https://example.com/\", ",
        "\"digest\": \"sha256:3ac6b4f7bda57f4bd0d9ce4ecb1e0ec6ee4b0ff3a7ae5b25e5ff89d1e46ec0cf\", ",
        "\"mime\": \"text/html\", \"status\": \"200\", \"offset\": \"784\", ",
        "\"length\": \"1300\", \"filename\": \"data.warc.gz\"}",
    );

    #[test]
    fn parse_example_line() -> Result<(), Box<dyn std::error::Error>> {
        let item = Item::parse(EXAMPLE)?;

        assert_eq!(item.key, "com,example)/");
        assert_eq!(item.timestamp.to_string(), "20201007212236");
        assert_eq!(item.fields.url, "https://example.com/");
        assert_eq!(item.fields.status, Some(200));
        assert_eq!(item.fields.offset, Some(784));
        assert_eq!(item.fields.length, Some(1300));
        assert_eq!(item.fields.filename.as_deref(), Some("data.warc.gz"));

        Ok(())
    }

    #[test]
    fn parse_accepts_numeric_json_fields() -> Result<(), Box<dyn std::error::Error>> {
        let item = Item::parse(
            "com,example)/ 20201007212236 {\"url\": \"https://example.com/\", \"offset\": 784}",
        )?;

        assert_eq!(item.fields.offset, Some(784));

        Ok(())
    }

    #[test]
    fn display_parse_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let item = Item::parse(EXAMPLE)?;
        let displayed = item.to_string();

        assert_eq!(Item::parse(&displayed)?, item);

        Ok(())
    }

    #[test]
    fn parse_rejects_truncated_lines() {
        assert!(matches!(
            Item::parse("com,example)/ 20201007212236"),
            Err(Error::Truncated(_))
        ));
    }

    #[test]
    fn parse_rejects_invalid_timestamps() {
        // Unsupported lengths, and supported lengths with a non-digit.
        for timestamp in [
            "2020100721223",
            "2020100721223a",
            "2020100721223600",
            "20201007212236a00",
            "202010072122360000",
        ] {
            assert!(matches!(
                Item::parse(&format!(
                    "com,example)/ {timestamp} {{\"url\": \"https://example.com/\"}}"
                )),
                Err(Error::InvalidTimestamp(_))
            ));
        }
    }

    #[test]
    fn parse_accepts_null_optional_fields() -> Result<(), Box<dyn std::error::Error>> {
        let item = Item::parse(
            "com,example)/ 20201007212236 {\"url\": \"https://example.com/\", \
             \"digest\": null, \"mime\": null, \"status\": null, \"offset\": null}",
        )?;

        assert_eq!(item.fields.digest, None);
        assert_eq!(item.fields.mime, None);
        assert_eq!(item.fields.status, None);
        assert_eq!(item.fields.offset, None);

        Ok(())
    }

    #[test]
    fn conforming_fields_report_every_missing_normative_property() -> Result<(), Error> {
        let item = Item::parse("com,example)/ 20201007212236 {\"url\":\"https://example.com/\"}")?;
        let error = ConformingFields::try_from(&item.fields).expect_err("fields are incomplete");

        assert_eq!(
            error.0,
            ["digest", "mime", "status", "offset", "length", "filename"]
        );
        Ok(())
    }

    #[test]
    fn record_digest_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        // Compact JSON, exactly as `Display` renders it, so the round trip is byte-identical.
        let line = concat!(
            "com,example)/ 20201007212236 {\"url\":\"https://example.com/\",\"recordDigest\":",
            "\"sha256:3ac6b4f7bda57f4bd0d9ce4ecb1e0ec6ee4b0ff3a7ae5b25e5ff89d1e46ec0cf\"}",
        );
        let item = Item::parse(line)?;

        assert!(item.fields.record_digest.is_some());
        assert_eq!(item.to_string(), line);

        Ok(())
    }

    #[test]
    fn timestamp_constructors_use_the_requested_precision() -> Result<(), Box<dyn std::error::Error>>
    {
        let instant = DateTime::parse_from_rfc3339("2020-10-07T21:22:36.750Z")?.to_utc();
        let seconds = Timestamp::from(instant);
        let milliseconds = Timestamp::with_milliseconds(instant);

        assert_eq!(seconds.to_string(), "20201007212236");
        assert!(!seconds.has_milliseconds());
        assert_eq!(seconds.datetime().timestamp_subsec_nanos(), 0);

        assert_eq!(milliseconds.to_string(), "20201007212236750");
        assert!(milliseconds.has_milliseconds());
        assert_eq!(milliseconds.datetime().timestamp_subsec_millis(), 750);
        assert_eq!(milliseconds, milliseconds.to_string().parse()?);

        Ok(())
    }

    #[test]
    fn timestamp_parsing_preserves_both_supported_forms() -> Result<(), Box<dyn std::error::Error>>
    {
        let seconds = "20201007212236".parse::<Timestamp>()?;
        let milliseconds = "20201007212236123".parse::<Timestamp>()?;

        assert_eq!(seconds.to_string(), "20201007212236");
        assert!(!seconds.has_milliseconds());
        assert_eq!(milliseconds.to_string(), "20201007212236123");
        assert!(milliseconds.has_milliseconds());
        assert_eq!(milliseconds.datetime().timestamp_subsec_millis(), 123);

        Ok(())
    }

    #[test]
    fn timestamps_of_both_precisions_order_chronologically()
    -> Result<(), Box<dyn std::error::Error>> {
        let previous = "20201007212235999".parse::<Timestamp>()?;
        let seconds = "20201007212236".parse::<Timestamp>()?;
        let zero_milliseconds = "20201007212236000".parse::<Timestamp>()?;
        let later = "20201007212236001".parse::<Timestamp>()?;

        assert!(previous < seconds);
        assert_eq!(seconds, zero_milliseconds);
        assert_eq!(seconds.cmp(&zero_milliseconds), std::cmp::Ordering::Equal);
        assert!(zero_milliseconds < later);

        Ok(())
    }

    #[test]
    fn read_index() -> Result<(), Box<dyn std::error::Error>> {
        let input = format!("{EXAMPLE}\n{EXAMPLE}\n");
        let items = IndexReader::new(input.as_bytes()).collect::<Result<Vec<_>, _>>()?;

        assert_eq!(items.len(), 2);
        assert_eq!(items[0], items[1]);

        Ok(())
    }

    #[test]
    fn search_key_reverses_and_lowercases() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            search_key("https://www.Example.com/Some/Path")?,
            "com,example,www)/some/path"
        );

        Ok(())
    }

    #[test]
    fn search_key_sorts_query_parameters() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            search_key("https://example.com/page?b=2&A=1")?,
            "com,example)/page?a=1&b=2"
        );

        Ok(())
    }

    #[test]
    fn search_key_keeps_non_default_ports() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            search_key("http://example.com:8080/")?,
            "com,example:8080)/"
        );
        assert_eq!(search_key("https://example.com:443/")?, "com,example)/");

        Ok(())
    }

    #[test]
    fn search_key_drops_trailing_host_dots_and_userinfo() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_eq!(search_key("https://example.com./x")?, "com,example)/x");
        assert_eq!(
            search_key("https://user:pass@example.com/")?,
            "com,example)/"
        );

        Ok(())
    }

    #[test]
    fn search_key_keeps_ip_hosts_in_order() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(search_key("http://127.0.0.1:8080/a")?, "127.0.0.1:8080)/a");
        assert_eq!(search_key("http://[2001:db8::1]/")?, "[2001:db8::1])/");

        Ok(())
    }

    #[test]
    fn search_key_keeps_braces_in_queries() -> Result<(), Box<dyn std::error::Error>> {
        // `{` is legal unencoded in a query string and must survive into the key.
        assert_eq!(
            search_key("https://example.com/?a={b}")?,
            "com,example)/?a={b}"
        );

        Ok(())
    }

    #[test]
    fn search_key_rejects_hostless_urls() {
        assert!(matches!(
            search_key("data:text/plain,hello"),
            Err(Error::MissingHost(_))
        ));
    }
}
