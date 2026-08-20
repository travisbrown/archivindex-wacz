//! Packaging existing WARC files as indexed WACZ distributions.
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::frictionless::DataPackageBuilder;
use archivindex_wacz::io::write::{
    MAX_ZIP_COMPRESSION_LEVEL, MIN_ZIP_COMPRESSION_LEVEL, WaczWriter, WriterConfig,
};
use archivindex_wacz::pages::{Page, PageListHeader};
use archivindex_warc::io::read::{self as warc_read, WarcReader};
use archivindex_warc::io::write::{
    DEFAULT_GZIP_COMPRESSION_LEVEL, MAX_GZIP_COMPRESSION_LEVEL, WarcWriter, Written,
};
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::dcmi::DcmiTerm;
use archivindex_warc::record::fields::metadata::MetadataField;
use archivindex_warc::record::fields::warcinfo::WarcinfoField;
use archivindex_warc::record::{FieldsBlock, Record, payload};
use archivindex_warc::value::{LabelledDigest, WarcDate};
use url::Url;

pub use archivindex_wacz::io::write::IndexFormat;

const INDEX_NAME: &str = "index.cdx";
const EXTRA_PAGES_NAME: &str = "extraPages.jsonl";
const REVISIT_MIME: &str = "warc/revisit";

/// An HTTP capture presented to a fallback page-title generator.
#[derive(Clone, Copy, Debug)]
pub struct Capture<'a> {
    /// The captured target URL.
    pub url: &'a str,
    /// The HTTP response status.
    pub status: u16,
    /// The decoded entity body, or stored body bytes when decoding fails.
    pub payload: &'a [u8],
    /// The complete recorded HTTP response.
    pub response: &'a [u8],
}

/// Generate a fallback page title from an existing captured response.
pub trait PageTitleGenerator {
    /// Return a title for `capture`, or `None` when no title can be derived.
    fn title(&mut self, capture: &Capture<'_>) -> Option<String>;
}

/// An error converting an existing WARC file into a WACZ package.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An input or temporary file could not be read or written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A WARC record could not be parsed.
    #[error(transparent)]
    WarcRead(#[from] warc_read::Error),
    /// A semantic WARC record could not be rendered again.
    #[error(transparent)]
    WarcRender(#[from] archivindex_warc::record::RenderError),
    /// A normalized WARC record could not be written.
    #[error(transparent)]
    WarcWrite(#[from] archivindex_warc::io::write::Error),
    /// The WACZ package could not be written.
    #[error(transparent)]
    Wacz(#[from] archivindex_wacz::io::write::Error),
    /// The requested gzip compression level is outside the supported range.
    #[error("gzip compression level must be between 0 and 9, got {0}")]
    InvalidGzipCompressionLevel(u32),
    /// The requested WACZ ZIP compression level is outside the supported range.
    #[error("ZIP compression level must be between 1 and 264, got {0}")]
    InvalidZipCompressionLevel(u32),
}

/// A non-fatal condition encountered while converting a WARC file.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ConversionWarning {
    /// The WARC contains multiple `warcinfo` records; package metadata came from the first.
    MultipleWarcinfo {
        /// The number of `warcinfo` records encountered.
        count: usize,
        /// Record IDs of the ignored `warcinfo` records, in source order.
        duplicate_record_ids: Vec<String>,
    },
}

impl std::fmt::Display for ConversionWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MultipleWarcinfo {
                count,
                duplicate_record_ids,
            } => {
                write!(
                    formatter,
                    "source WARC contains {count} warcinfo records; used the first for package metadata and ignored: {}",
                    duplicate_record_ids.join(", ")
                )
            }
        }
    }
}

/// Counts and non-fatal warnings from a WARC conversion.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConversionSummary {
    /// All WARC records copied into the package.
    pub records: usize,
    /// HTTP `response` and `revisit` records added to the CDXJ index.
    pub captures: usize,
    /// Capture entries added to the WACZ page list.
    pub pages: usize,
    /// Non-fatal source conditions encountered during conversion.
    pub warnings: Vec<ConversionWarning>,
}

/// A conversion from one existing WARC file to a new WACZ package.
///
/// Records are parsed semantically and normalized into a new WARC member. This makes offsets and
/// lengths independently addressable even when the input was a continuously compressed gzip
/// stream. Metadata fields linked by `WARC-Refers-To` or `WARC-Concurrent-To` classify captures:
/// those with `via` enter `extraPages.jsonl`, while the rest enter the required page list. A
/// metadata `title` takes precedence over one supplied by an optional [`PageTitleGenerator`].
/// WARCs declaring `pageList: metadata` in their first `warcinfo` include only captures whose
/// linked metadata has a `pageUrl`; other WARCs retain the legacy one-page-per-capture behavior.
pub struct WarcToWacz<'a> {
    input: PathBuf,
    output: PathBuf,
    title_generator: Option<Box<dyn PageTitleGenerator + 'a>>,
    index_format: IndexFormat,
    gzip_warc: bool,
    gzip_compression_level: u32,
    zip_compression_level: Option<u32>,
}

impl<'a> WarcToWacz<'a> {
    /// Create a conversion from `input` to a new WACZ at `output`.
    #[must_use]
    pub fn new(input: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
            title_generator: None,
            index_format: IndexFormat::Plain,
            gzip_warc: false,
            gzip_compression_level: DEFAULT_GZIP_COMPRESSION_LEVEL,
            zip_compression_level: None,
        }
    }

    /// Use `generator` to generate fallback page titles from captured HTTP responses.
    ///
    /// A title recorded in linked WARC metadata always replaces the generated title.
    #[must_use]
    pub fn title_generator<G: PageTitleGenerator + 'a>(mut self, generator: G) -> Self {
        self.title_generator = Some(Box::new(generator));
        self
    }

    /// Select the plain or compressed CDXJ representation.
    #[must_use]
    pub const fn index_format(mut self, index_format: IndexFormat) -> Self {
        self.index_format = index_format;
        self
    }

    /// Gzip the packaged WARC, writing every record as an independently compressed member.
    ///
    /// This option affects plain WARC input. Gzip-compressed input is always normalized to
    /// record-at-a-time compression for random access, regardless of this setting.
    #[must_use]
    pub const fn gzip_warc(mut self, gzip_warc: bool) -> Self {
        self.gzip_warc = gzip_warc;
        self
    }

    /// Set the gzip compression level used for packaged WARC records.
    ///
    /// Levels range from 0 (no compression) through 9 (best compression), and default to 6. This
    /// setting applies whenever the output WARC is gzip-compressed, including when the input is
    /// already gzip-compressed.
    #[must_use]
    pub const fn gzip_compression_level(mut self, level: u32) -> Self {
        self.gzip_compression_level = level;
        self
    }

    /// Set the ZIP DEFLATE compression level used for compressible WACZ members.
    ///
    /// Levels range from 1 through 264. Levels up to 9 use `miniz_oxide`, while higher levels use
    /// Zopfli. WARC and gzip-compressed members use ZIP `STORE` and are not affected.
    #[must_use]
    pub const fn zip_compression_level(mut self, level: u32) -> Self {
        self.zip_compression_level = Some(level);
        self
    }

    /// Parse the WARC and write the completed WACZ, refusing to overwrite `output`.
    pub fn run(mut self) -> Result<ConversionSummary, Error> {
        let input_gzip = is_gzip_file(&self.input)?;
        let output_gzip = input_gzip || self.gzip_warc;
        if output_gzip && self.gzip_compression_level > MAX_GZIP_COMPRESSION_LEVEL {
            return Err(Error::InvalidGzipCompressionLevel(
                self.gzip_compression_level,
            ));
        }
        if let Some(level) = self.zip_compression_level
            && !(MIN_ZIP_COMPRESSION_LEVEL..=MAX_ZIP_COMPRESSION_LEVEL).contains(&level)
        {
            return Err(Error::InvalidZipCompressionLevel(level));
        }
        let warc_name = if output_gzip {
            "data.warc.gz"
        } else {
            "data.warc"
        };

        if input_gzip {
            let reader = WarcReader::from_path_gzip(&self.input)?;
            self.convert(reader, warc_name, output_gzip)
        } else {
            let reader = WarcReader::from_path(&self.input)?;
            self.convert(reader, warc_name, output_gzip)
        }
    }

    fn convert<R: BufRead>(
        &mut self,
        reader: WarcReader<R>,
        warc_name: &str,
        output_gzip: bool,
    ) -> Result<ConversionSummary, Error> {
        let mut warc = WarcWriter::new(BufWriter::new(tempfile::tempfile()?)).with_digests();
        let mut items = Vec::new();
        let mut pages = Vec::new();
        let mut annotations = HashMap::new();
        let mut package_info = PackageInfo::default();
        let mut records = 0;

        for record in reader.iter_records::<NoExtension>() {
            let record = record?;
            package_info.inspect(&record);
            collect_metadata(&record, &mut annotations);
            let capture = capture_info(&record, self.title_generator.as_deref_mut());
            let raw = record.into_raw()?;
            let written = if output_gzip {
                warc.write_gzip_with_level(&raw, self.gzip_compression_level)?
            } else {
                warc.write(&raw)?
            };
            records += 1;

            if let Some(capture) = capture {
                items.push(capture.item(warc_name, &written));
                pages.push(capture.page());
            }
        }

        pages.retain_mut(|page| {
            let annotation = annotations.get(&page.record_id);
            if package_info.pages_from_metadata
                && annotation.is_none_or(|item| item.page_url.is_none())
            {
                return false;
            }
            if let Some(annotation) = annotation {
                if let Some(title) = &annotation.title {
                    page.title = Some(title.clone());
                }
                if let Some(url) = &annotation.page_url {
                    page.url.clone_from(url);
                }
                page.extra = annotation.via;
            }
            true
        });

        let mut file = warc.finish().map_err(std::io::IntoInnerError::into_error)?;
        file.rewind()?;
        let writer_config = WriterConfig {
            index_format: self.index_format,
            zip_compression_level: self.zip_compression_level,
            ..WriterConfig::default()
        };
        let mut wacz = WaczWriter::create_with_config(&self.output, writer_config)?;
        wacz.add_warc(warc_name, file)?;
        // Malformed or payload-less WARC responses are retained by conversion. Their CDXJ entries
        // intentionally use the reader-compatible lenient field model.
        wacz.add_index_lenient(INDEX_NAME, &items)?;
        let page_entries = pages
            .iter()
            .filter(|page| !page.extra)
            .map(PageDraft::as_page)
            .collect::<Vec<_>>();
        let extra_page_entries = pages
            .iter()
            .filter(|page| page.extra)
            .map(PageDraft::as_page)
            .collect::<Vec<_>>();
        wacz.add_pages(&PageListHeader::default(), &page_entries)?;
        if !extra_page_entries.is_empty() {
            wacz.add_page_list(
                EXTRA_PAGES_NAME,
                &extra_page_list_header(),
                &extra_page_entries,
            )?;
        }
        let warnings = package_info.warnings();
        let mut metadata = DataPackageBuilder::new();
        if let Some(title) = package_info.title {
            metadata = metadata.title(title);
        }
        if let Some(description) = package_info.description {
            metadata = metadata.description(description);
        }
        if let Some(page) = page_entries.first() {
            metadata = metadata
                .main_page_url(page.url.clone().into_owned())
                .main_page_date(page.ts);
        }
        wacz.finish(metadata)?.flush()?;

        Ok(ConversionSummary {
            records,
            captures: items.len(),
            pages: pages.len(),
            warnings,
        })
    }
}

#[derive(Default)]
struct PackageInfo {
    title: Option<String>,
    description: Option<String>,
    warcinfo_count: usize,
    duplicate_warcinfo_record_ids: Vec<String>,
    pages_from_metadata: bool,
}

impl PackageInfo {
    fn inspect(&mut self, record: &Record) {
        let Record::Warcinfo { header, body } = record else {
            return;
        };
        self.warcinfo_count += 1;
        if self.warcinfo_count != 1 {
            self.duplicate_warcinfo_record_ids
                .push(header.core.record_id.as_str().to_owned());
            return;
        }
        let FieldsBlock::Fields(fields) = body else {
            return;
        };
        self.title = fields
            .get(&WarcinfoField::Dcmi(DcmiTerm::Title))
            .map(str::to_owned);
        self.description = fields
            .get(&WarcinfoField::Dcmi(DcmiTerm::Description))
            .map(str::to_owned);
        self.pages_from_metadata = fields
            .get(&WarcinfoField::Other("pagelist".to_owned()))
            .is_some_and(|value| value == "metadata");
    }

    fn warnings(&self) -> Vec<ConversionWarning> {
        (self.warcinfo_count > 1)
            .then_some(ConversionWarning::MultipleWarcinfo {
                count: self.warcinfo_count,
                duplicate_record_ids: self.duplicate_warcinfo_record_ids.clone(),
            })
            .into_iter()
            .collect()
    }
}

/// A response or revisit's data needed after its record has been written.
struct CaptureInfo {
    record_id: String,
    key: String,
    url: String,
    date: WarcDate,
    status: Option<u16>,
    digest: Option<LabelledDigest>,
    mime: Option<String>,
    size: Option<u64>,
    generated_title: Option<String>,
}

impl CaptureInfo {
    fn item(&self, warc_name: &str, written: &Written) -> cdxj::Item<'static> {
        cdxj::Item {
            key: Cow::Owned(self.key.clone()),
            timestamp: cdxj::Timestamp::with_milliseconds(self.date.date_time()),
            fields: cdxj::Fields {
                url: Cow::Owned(self.url.clone()),
                digest: self
                    .digest
                    .as_ref()
                    .map(|digest| Cow::Owned(digest.to_string())),
                mime: self.mime.as_ref().map(|mime| Cow::Owned(mime.clone())),
                status: self.status,
                offset: Some(written.offset),
                length: Some(written.length),
                filename: Some(Cow::Owned(warc_name.to_owned())),
                record_digest: stored_digest(written),
                extra: ExtraProperties::default(),
            },
        }
    }

    fn page(self) -> PageDraft {
        PageDraft {
            record_id: self.record_id,
            url: self.url,
            date: self.date,
            size: self.size,
            title: self.generated_title,
            extra: false,
        }
    }
}

/// A page entry retaining its WARC record identity until linked metadata has been collected.
struct PageDraft {
    record_id: String,
    url: String,
    date: WarcDate,
    size: Option<u64>,
    title: Option<String>,
    extra: bool,
}

impl PageDraft {
    fn as_page(&self) -> Page<'_> {
        Page {
            url: Cow::Borrowed(&self.url),
            ts: self.date.date_time(),
            id: None,
            title: self.title.as_deref().map(Cow::Borrowed),
            text: None,
            size: self.size,
            extra: ExtraProperties::default(),
        }
    }
}

fn capture_info(
    record: &Record,
    generator: Option<&mut (dyn PageTitleGenerator + '_)>,
) -> Option<CaptureInfo> {
    match record {
        Record::Response { header, body } => capture_info_from_http(
            header.core.record_id.as_str(),
            header.target_uri.as_str(),
            header.core.date,
            header.payload.payload_digest.clone(),
            header
                .payload
                .identified_payload_type
                .as_ref()
                .map(ToString::to_string),
            body,
            false,
            generator,
        ),
        Record::Revisit { header, body } => capture_info_from_http(
            header.core.record_id.as_str(),
            header.target_uri.as_str(),
            header.core.date,
            header.payload.payload_digest.clone(),
            Some(REVISIT_MIME.to_owned()),
            body,
            true,
            generator,
        ),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_info_from_http(
    record_id: &str,
    target_uri: &str,
    date: WarcDate,
    digest: Option<LabelledDigest>,
    mime: Option<String>,
    message: &[u8],
    revisit: bool,
    generator: Option<&mut (dyn PageTitleGenerator + '_)>,
) -> Option<CaptureInfo> {
    let url = Url::parse(target_uri).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let key = cdxj::search_key(target_uri).ok()?;
    let head = response_head(message);
    let status = head.as_ref().map(|head| head.status);
    let entity: Cow<'_, [u8]> = if revisit {
        Cow::Borrowed(&[])
    } else {
        payload::entity_body(message).unwrap_or_else(|_| {
            head.as_ref().map_or(Cow::Borrowed(&[]), |head| {
                Cow::Borrowed(&message[head.body_offset..])
            })
        })
    };
    let generated_title = generator.and_then(|generator| {
        status.and_then(|status| {
            generator.title(&Capture {
                url: target_uri,
                status,
                payload: &entity,
                response: message,
            })
        })
    });

    Some(CaptureInfo {
        record_id: record_id.to_owned(),
        key,
        url: target_uri.to_owned(),
        date,
        status,
        digest,
        mime,
        size: (!revisit).then_some(entity.len() as u64),
        generated_title,
    })
}

#[derive(Default)]
struct MetadataAnnotation {
    title: Option<String>,
    via: bool,
    page_url: Option<String>,
}

fn collect_metadata(record: &Record, annotations: &mut HashMap<String, MetadataAnnotation>) {
    let Record::Metadata {
        header,
        body: FieldsBlock::Fields(fields),
    } = record
    else {
        return;
    };
    let title = fields
        .get(&MetadataField::Dcmi(DcmiTerm::Title))
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let via = fields.via().is_some();
    let page_url = fields
        .get(&MetadataField::Other("pageurl".to_owned()))
        .map(str::to_owned);

    for record_id in header.refers_to.iter().chain(&header.concurrent_to) {
        let annotation = annotations
            .entry(record_id.as_str().to_owned())
            .or_default();
        if title.is_some() {
            annotation.title.clone_from(&title);
        }
        annotation.via |= via;
        if page_url.is_some() {
            annotation.page_url.clone_from(&page_url);
        }
    }
}

fn extra_page_list_header() -> PageListHeader<'static> {
    PageListHeader {
        format: Cow::Borrowed(archivindex_wacz::pages::FORMAT),
        id: Some(Cow::Borrowed("extra-pages")),
        title: Some(Cow::Borrowed("Extra Pages")),
        extra: ExtraProperties::default(),
    }
}

fn stored_digest(written: &Written) -> Option<Sha256Digest> {
    written
        .digest
        .as_ref()
        .and_then(LabelledDigest::decoded)
        .and_then(|bytes| bytes.try_into().ok())
        .map(Sha256Digest)
}

fn is_gzip_file(path: &Path) -> Result<bool, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut magic = [0; 2];
    Ok(file.read(&mut magic)? == magic.len() && magic == [0x1f, 0x8b])
}

struct ResponseHead {
    status: u16,
    body_offset: usize,
}

fn response_head(message: &[u8]) -> Option<ResponseHead> {
    let first_line_end = message.windows(2).position(|bytes| bytes == b"\r\n")?;
    let first_line = std::str::from_utf8(&message[..first_line_end]).ok()?;
    let mut parts = first_line.split_ascii_whitespace();
    let version = parts.next()?;
    let status = parts.next()?.parse().ok()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let body_offset = message.windows(4).position(|bytes| bytes == b"\r\n\r\n")? + 4;
    Some(ResponseHead {
        status,
        body_offset,
    })
}
