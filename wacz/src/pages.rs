//! The `pages/pages.jsonl` page list format.
//!
//! A page list is a JSON Lines file whose first line is a [`PageListHeader`] identifying the format
//! and naming the list, followed by one [`Page`] entry per line. The `pages/pages.jsonl` file is
//! required in every WACZ; additional lists (for example `extraPages.jsonl`) may sit alongside it
//! in the `pages/` directory using the same format.
//!
//! Entries are read at two levels: [`Page`] is the semantic form, whose timestamp is the RFC 3339
//! date-time the specification requires, while [`RawPage`] keeps the timestamp exactly as written.
//! [`PageListReader`] produces the former, and [`RawPageListReader`] the latter, for lists whose
//! timestamps another tool wrote in another form.

use std::borrow::Cow;
use std::io::{BufRead, Write};

use archivindex_warc::value::Supported as _;
use archivindex_warc::value::marker::Sha256;
use bounded_static::{IntoBoundedStatic, ToStatic};
use chrono::{DateTime, SecondsFormat, Utc};

use archivindex_cdx::properties::ExtraProperties;

use crate::LineContext;
use crate::lines::Lines;

/// The format identifier required in the header line of a page list.
pub const FORMAT: &str = "json-pages-1.0";

/// An error type for page list reading and writing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying stream could not be read or written.
    #[error(
        "{}",
        .context.as_ref().map_or_else(
            || .source.to_string(),
            |context| format!("failed to read {context}"),
        )
    )]
    Io {
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
        /// Location of the failed read, when available.
        context: Option<LineContext>,
    },
    /// The page list ended before a header line was read.
    #[error("missing page list header")]
    MissingHeader,
    /// The header line could not be parsed.
    #[error("invalid page list header at {context}")]
    InvalidHeader {
        /// Bounded source context.
        context: LineContext,
        /// Underlying deserialization error.
        #[source]
        source: serde_json::Error,
    },
    /// The header line declares a format other than [`FORMAT`].
    #[error("unsupported page list format: {0}")]
    UnsupportedFormat(String),
    /// A page entry line could not be parsed.
    #[error("invalid page list entry at {context}")]
    InvalidEntry {
        /// The underlying deserialization error.
        #[source]
        source: serde_json::Error,
        /// Bounded source context.
        context: LineContext,
    },
    /// A page entry has a timestamp that the specification does not permit.
    #[error("invalid page list entry at {context}")]
    InvalidTimestamp {
        /// The underlying conversion error.
        #[source]
        source: InvalidTimestamp,
        /// Bounded source context.
        context: LineContext,
    },
    /// A page entry could not be serialized.
    #[error("invalid page list entry")]
    Serialization(#[source] serde_json::Error),
    /// An extension property duplicates a modeled page-list property.
    #[error(transparent)]
    ExtraProperty(#[from] archivindex_cdx::properties::Error),
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Self::Io {
            source,
            context: None,
        }
    }
}

impl From<crate::lines::Error> for Error {
    fn from(error: crate::lines::Error) -> Self {
        Self::Io {
            source: error.source,
            context: Some(error.context),
        }
    }
}

/// The header line of a page list.
///
/// The specification only requires the format identifier; the list identifier and title are
/// conventional but optional.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
pub struct PageListHeader<'a> {
    /// The format identifier (always [`FORMAT`]).
    #[serde(borrow)]
    pub format: Cow<'a, str>,
    /// An identifier for the list (`pages` for the required list).
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<Cow<'a, str>>,
    /// A display name for the list.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl Default for PageListHeader<'static> {
    /// The conventional header of the required `pages/pages.jsonl` list.
    fn default() -> Self {
        Self {
            format: Cow::Borrowed(FORMAT),
            id: Some(Cow::Borrowed("pages")),
            title: Some(Cow::Borrowed("All Pages")),
            extra: ExtraProperties::default(),
        }
    }
}

/// A single page entry in a page list.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
pub struct Page<'a> {
    /// The URL of the archived page.
    #[serde(borrow)]
    pub url: Cow<'a, str>,
    /// When the page was captured.
    #[serde(deserialize_with = "crate::attributes::rfc_3339_datetime")]
    pub ts: DateTime<Utc>,
    /// An arbitrary identifier for the page.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<Cow<'a, str>>,
    /// A title describing the page.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// Text content extracted from the page, used for search.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub text: Option<Cow<'a, str>>,
    /// The total size in bytes of the page and its resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl PageListHeader<'_> {
    fn validate(&self) -> Result<(), archivindex_cdx::properties::Error> {
        self.extra
            .validate("PageListHeader", &["format", "id", "title"])
    }
}

impl Page<'_> {
    fn validate(&self) -> Result<(), archivindex_cdx::properties::Error> {
        self.extra
            .validate("Page", &["url", "ts", "id", "title", "text", "size"])
    }
}

/// A page entry whose timestamp has not been interpreted.
///
/// Converting to [`Page`] requires an RFC 3339 timestamp. [`Self::into_page_compatible`] also
/// accepts the nonstandard date-time forms produced by some tools.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize)]
pub struct RawPage<'a> {
    /// The URL of the archived page.
    #[serde(borrow)]
    pub url: Cow<'a, str>,
    /// When the page was captured, as written.
    #[serde(borrow)]
    pub ts: Cow<'a, str>,
    /// An arbitrary identifier for the page.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<Cow<'a, str>>,
    /// A title describing the page.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// Text content extracted from the page, used for search.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub text: Option<Cow<'a, str>>,
    /// The total size in bytes of the page and its resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

/// A page timestamp that could not be interpreted as a date-time.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid page timestamp: {0}")]
pub struct InvalidTimestamp(pub String);

impl<'a> RawPage<'a> {
    /// Interpret the timestamp with the compatibility parser.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidTimestamp`] if the timestamp is not recognized.
    pub fn into_page_compatible(self) -> Result<Page<'a>, InvalidTimestamp> {
        self.into_page(crate::attributes::parse_compatible_datetime)
    }

    /// Interpret the timestamp with `parse`, moving the other properties across unchanged.
    fn into_page(
        self,
        parse: fn(&str) -> Option<DateTime<Utc>>,
    ) -> Result<Page<'a>, InvalidTimestamp> {
        let ts = parse(&self.ts).ok_or_else(|| InvalidTimestamp(self.ts.into_owned()))?;

        Ok(Page {
            url: self.url,
            ts,
            id: self.id,
            title: self.title,
            text: self.text,
            size: self.size,
            extra: self.extra,
        })
    }
}

impl<'a> TryFrom<RawPage<'a>> for Page<'a> {
    type Error = InvalidTimestamp;

    /// Interpret the timestamp as the RFC 3339 date-time the specification requires.
    fn try_from(page: RawPage<'a>) -> Result<Self, Self::Error> {
        page.into_page(crate::attributes::parse_rfc_3339)
    }
}

/// A page-list reader that preserves timestamp text and skips blank lines.
pub struct RawPageListReader<R> {
    lines: Lines<R>,
    header: PageListHeader<'static>,
}

impl<R: BufRead> RawPageListReader<R> {
    /// Create a new reader, reading and validating the header line.
    ///
    /// # Errors
    ///
    /// Fails if the stream has no non-blank lines, if the header line is not valid JSON, or if the
    /// header declares a format other than [`FORMAT`].
    pub fn new(reader: R) -> Result<Self, Error> {
        Self::with_source(reader, "<stream>")
    }

    /// Create a reader carrying a member path or source name for diagnostics.
    pub fn with_source(reader: R, source: impl Into<String>) -> Result<Self, Error> {
        Self::from_lines(Lines::with_source(reader, source))
    }

    /// Read the header line and build a reader around the remaining lines.
    fn from_lines(mut lines: Lines<R>) -> Result<Self, Error> {
        let (location, line_text) = lines.next_content()?.ok_or(Error::MissingHeader)?;

        let header = serde_json::from_str::<PageListHeader<'_>>(line_text).map_err(|source| {
            Error::InvalidHeader {
                context: location,
                source,
            }
        })?;

        if header.format != FORMAT {
            return Err(Error::UnsupportedFormat(header.format.into_owned()));
        }

        let header = header.into_static();

        Ok(Self { lines, header })
    }

    /// The parsed header line.
    #[must_use]
    pub const fn header(&self) -> &PageListHeader<'static> {
        &self.header
    }

    /// Read the next entry together with the source context it came from.
    fn next_located(&mut self) -> Option<Result<(LineContext, RawPage<'static>), Error>> {
        match self.lines.next_content() {
            Ok(Some((location, line_text))) => Some(
                serde_json::from_str::<RawPage<'_>>(line_text)
                    .map(|page| (location.clone(), page.into_static()))
                    .map_err(|source| Error::InvalidEntry {
                        source,
                        context: location,
                    }),
            ),
            Ok(None) => None,
            Err(error) => Some(Err(error.into())),
        }
    }
}

impl<R: BufRead> Iterator for RawPageListReader<R> {
    type Item = Result<RawPage<'static>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_located()?.map(|(_, page)| page))
    }
}

/// A page-list reader that requires RFC 3339 timestamps and rejects blank lines.
pub struct PageListReader<R> {
    raw: RawPageListReader<R>,
}

impl<R: BufRead> PageListReader<R> {
    /// Create a new reader, reading and validating the header line.
    ///
    /// # Errors
    ///
    /// Fails if the stream is empty, if the header line is blank or is not valid JSON, or if the
    /// header declares a format other than [`FORMAT`].
    pub fn new(reader: R) -> Result<Self, Error> {
        Self::with_source(reader, "<stream>")
    }

    /// Create a reader carrying a member path or source name for diagnostics.
    pub fn with_source(reader: R, source: impl Into<String>) -> Result<Self, Error> {
        RawPageListReader::from_lines(Lines::with_source(reader, source).rejecting_blank_lines())
            .map(|raw| Self { raw })
    }

    /// The parsed header line.
    #[must_use]
    pub const fn header(&self) -> &PageListHeader<'static> {
        self.raw.header()
    }
}

impl<R: BufRead> Iterator for PageListReader<R> {
    type Item = Result<Page<'static>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.raw.next_located()?.and_then(|(context, page)| {
            Page::try_from(page).map_err(|source| Error::InvalidTimestamp { source, context })
        }))
    }
}

/// The synthetic identifier for a page: a truncated SHA-256 hash of its timestamp and URL.
///
/// The hash input is the concatenation of the timestamp in RFC 3339 format (UTC, `Z` suffix,
/// exactly as it is serialized in the page entry) and the URL; the identifier is the first `length`
/// characters of the lowercase hexadecimal digest. Lengths above 64 yield the full digest.
#[must_use]
pub fn synthetic_id(ts: &DateTime<Utc>, url: &str, length: usize) -> String {
    let mut hasher = Sha256::hasher();
    hasher.update(ts.to_rfc3339_opts(SecondsFormat::AutoSi, true).as_bytes());
    hasher.update(url.as_bytes());

    let mut id = data_encoding::HEXLOWER.encode(&hasher.finalize());
    id.truncate(length);
    id
}

/// Write a page list: a header line followed by one JSON line per page.
///
/// # Errors
///
/// Fails if the underlying stream cannot be written or if an entry cannot be serialized.
pub fn write_page_list<'p, W: Write, I: IntoIterator<Item = &'p Page<'p>>>(
    writer: W,
    header: &PageListHeader<'_>,
    pages: I,
) -> Result<(), Error> {
    write_page_list_with_policy(writer, header, pages, IdPolicy::Preserve)
}

/// A streaming page-list writer that emits the header on construction and one page per call.
pub struct PageListWriter<W> {
    writer: W,
    id_policy: IdPolicy,
}

impl<W: Write> PageListWriter<W> {
    /// Start a page list that preserves page identifiers as supplied.
    pub fn new(writer: W, header: &PageListHeader<'_>) -> Result<Self, Error> {
        Self::with_policy(writer, header, IdPolicy::Preserve)
    }

    fn with_policy(
        mut writer: W,
        header: &PageListHeader<'_>,
        id_policy: IdPolicy,
    ) -> Result<Self, Error> {
        header.validate()?;
        write_line(&mut writer, header)?;
        Ok(Self { writer, id_policy })
    }

    /// Write one validated page entry.
    pub fn write(&mut self, page: &Page<'_>) -> Result<(), Error> {
        page.validate()?;
        self.write_validated(page)
    }

    fn write_validated(&mut self, page: &Page<'_>) -> Result<(), Error> {
        match self.id_policy {
            IdPolicy::Synthetic(id_length) if page.id.is_none() => {
                let id = synthetic_id(&page.ts, &page.url, id_length);
                write_line(&mut self.writer, &IdentifiedPage { id: &id, page })
            }
            IdPolicy::Preserve | IdPolicy::Synthetic(_) => write_line(&mut self.writer, page),
        }
    }

    /// Return the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

/// A page entry serialized with a synthetic identifier when it has none of its own.
#[derive(serde::Serialize)]
struct IdentifiedPage<'a> {
    id: &'a str,
    #[serde(flatten)]
    page: &'a Page<'a>,
}

/// Write a page list, giving pages without identifiers synthetic ones of `id_length` characters
/// (see [`synthetic_id`]).
pub(crate) fn write_page_list_with_synthetic_ids<
    'p,
    W: Write,
    I: IntoIterator<Item = &'p Page<'p>>,
>(
    writer: W,
    header: &PageListHeader<'_>,
    pages: I,
    id_length: usize,
) -> Result<(), Error> {
    write_page_list_with_policy(writer, header, pages, IdPolicy::Synthetic(id_length))
}

#[derive(Clone, Copy)]
enum IdPolicy {
    Preserve,
    Synthetic(usize),
}

fn write_page_list_with_policy<'p, W: Write, I: IntoIterator<Item = &'p Page<'p>>>(
    writer: W,
    header: &PageListHeader<'_>,
    pages: I,
    id_policy: IdPolicy,
) -> Result<(), Error> {
    header.validate()?;
    let pages = pages.into_iter().collect::<Vec<_>>();
    for page in &pages {
        page.validate()?;
    }
    let mut writer = PageListWriter::with_policy(writer, header, id_policy)?;
    for page in pages {
        writer.write_validated(page)?;
    }

    Ok(())
}

/// Write one value as a JSON line, distinguishing stream failures from serialization failures.
fn write_line<W: Write, T: serde::ser::Serialize>(writer: &mut W, value: &T) -> Result<(), Error> {
    serde_json::to_writer(&mut *writer, value).map_err(|error| {
        if error.is_io() {
            Error::from(std::io::Error::from(error))
        } else {
            Error::Serialization(error)
        }
    })?;

    writer.write_all(b"\n")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    #[test_strategy::proptest]
    fn page_lists_round_trip(
        #[strategy(strategies::page_list_header())] header: PageListHeader<'static>,
        #[strategy(proptest::collection::vec(strategies::page(), 0..=4))] pages: Vec<Page<'static>>,
    ) {
        let mut output = Vec::new();
        write_page_list(&mut output, &header, &pages).unwrap();

        let mut reader = PageListReader::new(output.as_slice()).unwrap();

        prop_assert_eq!(reader.header(), &header);

        let read = reader
            .by_ref()
            .collect::<Result<Vec<_>, _>>()
            .expect("invariant violation: written entries parse");

        prop_assert_eq!(read, pages);
    }

    #[test_strategy::proptest]
    fn synthetic_identifiers_are_hexadecimal_of_the_requested_length(
        #[strategy(strategies::datetime())] ts: DateTime<Utc>,
        #[strategy(".*")] url: String,
        #[strategy(0..=80_usize)] length: usize,
    ) {
        let id = synthetic_id(&ts, &url, length);

        prop_assert_eq!(id.len(), length.min(64));
        prop_assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    /// A page list holding one entry whose timestamp is written as `ts`.
    fn page_list(ts: &str) -> String {
        format!(
            "{{\"format\":\"{FORMAT}\"}}\n{{\"url\":\"https://example.com/\",\"ts\":\"{ts}\"}}\n"
        )
    }

    #[test]
    fn zoneless_timestamps_are_rejected_at_the_semantic_level() {
        let list = page_list("2020-10-07T21:22:36");
        let mut reader = PageListReader::new(list.as_bytes()).expect("a valid header");

        assert!(matches!(
            reader.next(),
            Some(Err(Error::InvalidTimestamp { context, .. })) if context.line == 2
        ));
    }

    /// JSON Lines has no blank records, so the semantic level rejects a blank line that the raw
    /// level skips.
    #[test]
    fn blank_lines_are_read_only_at_the_raw_level() {
        let list = page_list("2020-10-07T21:22:36Z").replace('\n', "\n\n");

        let mut raw = RawPageListReader::new(list.as_bytes()).expect("a valid header");
        assert!(raw.next().expect("one entry").is_ok());
        assert!(raw.next().is_none());

        let error = PageListReader::new(list.as_bytes())
            .expect("a valid header")
            .next()
            .expect("the blank second line")
            .expect_err("blank lines are not entries");

        assert!(
            matches!(&error, Error::Io { context: Some(context), .. } if context.line == 2),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn zoneless_timestamps_are_read_at_the_raw_level() {
        let list = page_list("2020-10-07T21:22:36");
        let mut reader = RawPageListReader::new(list.as_bytes()).expect("a valid header");
        let raw = reader
            .next()
            .expect("one entry")
            .expect("a parseable entry");

        assert_eq!(raw.ts, "2020-10-07T21:22:36");
        assert_eq!(
            Page::try_from(raw.clone()),
            Err(InvalidTimestamp("2020-10-07T21:22:36".to_owned()))
        );
        assert_eq!(
            raw.into_page_compatible().map(|page| page.ts),
            DateTime::parse_from_rfc3339("2020-10-07T21:22:36Z")
                .map(|value| value.with_timezone(&Utc))
                .map_err(|_| InvalidTimestamp(String::new()))
        );
    }

    #[test_strategy::proptest]
    fn written_entries_are_read_at_both_levels(
        #[strategy(strategies::page_list_header())] header: PageListHeader<'static>,
        #[strategy(proptest::collection::vec(strategies::page(), 0..=4))] pages: Vec<Page<'static>>,
    ) {
        let mut output = Vec::new();
        write_page_list(&mut output, &header, &pages).unwrap();

        let read = RawPageListReader::new(output.as_slice())
            .unwrap()
            .map(|page| Page::try_from(page.expect("invariant violation: written entries parse")))
            .collect::<Result<Vec<_>, _>>()
            .expect("invariant violation: written timestamps are RFC 3339");

        prop_assert_eq!(read, pages);
    }

    #[test]
    fn line_io_errors_retain_context() {
        let error = Error::from(crate::lines::Error {
            context: LineContext {
                source: "pages/pages.jsonl".to_owned(),
                line: 3,
                excerpt: None,
            },
            source: std::io::Error::other("failed"),
        });

        assert_eq!(error.to_string(), "failed to read pages/pages.jsonl:3");
        assert!(matches!(
            error,
            Error::Io {
                context: Some(context),
                ..
            } if context.line == 3
        ));
    }

    #[test]
    fn writing_rejects_collisions_before_emitting_any_lines() {
        let mut header = PageListHeader::default();
        header
            .extra
            .insert("format".to_owned(), serde_json::Value::Null);
        let mut output = Vec::new();
        assert!(write_page_list(&mut output, &header, []).is_err());
        assert_eq!(output, b"");

        let header = PageListHeader::default();
        let mut page = Page {
            url: "https://example.com/".into(),
            ts: Utc::now(),
            id: None,
            title: None,
            text: None,
            size: None,
            extra: ExtraProperties::default(),
        };
        page.extra
            .insert("size".to_owned(), serde_json::Value::from(1));
        assert!(write_page_list(&mut output, &header, [&page]).is_err());
        assert_eq!(output, b"");
    }

    const EXAMPLE: &str = concat!(
        "{\"format\": \"json-pages-1.0\", \"id\": \"pages\", \"title\": \"All Pages\"}\n",
        "{\"id\": \"1db0ef709a\", \"url\": \"https://www.example.com/page\", \"size\": 1256, ",
        "\"ts\": \"2020-10-07T21:22:36Z\", \"title\": \"Example Domain\", \"custom\": true}\n",
        "{\"url\": \"https://www.example.com/another\", \"ts\": \"2020-10-07T21:23:36Z\"}\n",
    );

    #[test]
    fn read_example_page_list() -> Result<(), Box<dyn std::error::Error>> {
        let reader = PageListReader::new(EXAMPLE.as_bytes())?;

        assert_eq!(reader.header().id.as_deref(), Some("pages"));

        let pages = reader.collect::<Result<Vec<_>, _>>()?;

        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].url, "https://www.example.com/page");
        assert_eq!(pages[0].size, Some(1256));
        assert_eq!(pages[0].extra["custom"], serde_json::Value::Bool(true));
        assert_eq!(pages[1].id, None);

        Ok(())
    }

    #[test]
    fn read_rejects_empty_streams() {
        assert!(matches!(
            PageListReader::new(&b""[..]),
            Err(Error::MissingHeader)
        ));
    }

    #[test]
    fn read_accepts_a_format_only_header() -> Result<(), Box<dyn std::error::Error>> {
        // The specification only requires the format property in the header.
        let reader = PageListReader::new(&b"{\"format\": \"json-pages-1.0\"}\n"[..])?;

        assert_eq!(reader.header().id, None);
        assert_eq!(reader.header().title, None);

        Ok(())
    }

    #[test]
    fn read_rejects_unsupported_format() {
        let result =
            PageListReader::new(&b"{\"format\": \"other\", \"id\": \"x\", \"title\": \"y\"}\n"[..]);

        assert!(matches!(result, Err(Error::UnsupportedFormat(format)) if format == "other"));
    }

    #[test]
    fn read_reports_entry_line_numbers() -> Result<(), Box<dyn std::error::Error>> {
        let mut reader = PageListReader::with_source(
            &b"{\"format\": \"json-pages-1.0\", \"id\": \"pages\", \"title\": \"t\"}\nnot json\n"[..],
            "pages/pages.jsonl",
        )?;

        assert!(matches!(
            reader.next(),
            Some(Err(Error::InvalidEntry { context, .. }))
                if context.source == "pages/pages.jsonl" && context.line == 2
        ));

        Ok(())
    }

    #[test]
    fn synthetic_id_matches_known_value() {
        // Externally computed: sha256("2020-10-07T21:22:36Zhttps://www.example.com/page").
        const DIGEST: &str = "f5ca709e5e9363c834323853295995cc0df353276b4811df37034f2bab360bbd";

        let ts = DateTime::parse_from_rfc3339("2020-10-07T21:22:36Z")
            .expect("valid timestamp")
            .to_utc();
        let url = "https://www.example.com/page";

        assert_eq!(synthetic_id(&ts, url, 10), DIGEST[..10]);
        assert_eq!(synthetic_id(&ts, url, 64), DIGEST);
        // Lengths above the digest length yield the full digest.
        assert_eq!(synthetic_id(&ts, url, 100), DIGEST);
    }

    #[test]
    fn write_read_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let original = PageListReader::new(EXAMPLE.as_bytes())?;
        let header = original.header().clone();
        let pages = original.collect::<Result<Vec<_>, _>>()?;

        let mut buffer = Vec::new();
        write_page_list(&mut buffer, &header, &pages)?;

        let reader = PageListReader::new(buffer.as_slice())?;

        assert_eq!(reader.header(), &header);
        assert_eq!(reader.collect::<Result<Vec<_>, _>>()?, pages);

        Ok(())
    }
}
