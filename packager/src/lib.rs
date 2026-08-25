//! Packaging existing WARC files as indexed WACZ distributions.
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

mod spool;

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use archivindex_surt::url::Canonicalizer;
use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::frictionless::DataPackageBuilder;
use archivindex_wacz::io::write::{IndexFormat, WaczWriter, WriterConfig};
use archivindex_warc::io::read::{self as warc_read, WarcReader};
use archivindex_warc::io::write::{DEFAULT_GZIP_COMPRESSION_LEVEL, Written};
use archivindex_warc::parse::{raw, untyped};
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::dcmi::DcmiTerm;
use archivindex_warc::record::fields::metadata::MetadataField;
use archivindex_warc::record::fields::warcinfo::WarcinfoField;
use archivindex_warc::record::http::ResponseMetadata;
use archivindex_warc::record::record_type::RecordType;
use archivindex_warc::record::{FieldsBlock, Record};
use archivindex_warc::value::{Algorithm, LabelledDigest};

use spool::{Annotation, ConversionSpool, PageDraft, SpoolStore};

const INDEX_NAME: &str = "index.cdx";
const EXTRA_PAGES_NAME: &str = "extraPages.jsonl";
const REVISIT_MIME: &str = "warc/revisit";
const UNKNOWN_MIME: &str = "unk";

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
    /// A normalized WARC record could not be written.
    #[error(transparent)]
    WarcWrite(#[from] archivindex_warc::io::write::Error),
    /// The WACZ package could not be written.
    #[error(transparent)]
    Wacz(#[from] archivindex_wacz::io::write::Error),
    /// Temporary conversion metadata could not be stored or queried.
    #[error(transparent)]
    Spool(SpoolError),
}

/// A failure in the private disk-backed conversion store.
#[derive(Debug, thiserror::Error)]
#[error("conversion metadata spool error")]
pub struct SpoolError {
    #[source]
    source: spool::Error,
}

impl From<spool::Error> for Error {
    fn from(source: spool::Error) -> Self {
        Self::Spool(SpoolError { source })
    }
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
    /// A `response` or `revisit` record was copied into the WARC but left out of the CDXJ index
    /// because a field the index requires could not be determined.
    CaptureNotIndexed {
        /// The record ID of the capture.
        record_id: String,
        /// The field that could not be determined.
        reason: SkipReason,
    },
}

/// Why a capture could not be given a CDXJ line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SkipReason {
    /// The block is not a parseable HTTP message, so the status is unknown.
    UnparsableHttpMessage,
    /// No payload digest is declared and none could be computed from the block.
    UndeterminedPayload,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UnparsableHttpMessage => "its block is not a parseable HTTP message",
            Self::UndeterminedPayload => "its payload digest is neither declared nor computable",
        })
    }
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
            Self::CaptureNotIndexed { record_id, reason } => {
                write!(formatter, "capture {record_id} was not indexed: {reason}")
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
/// Records are copied byte for byte into a random-access WARC member; only gzip framing may
/// change. The converter parses only the record types needed for package metadata and indexing.
/// Metadata linked by `WARC-Refers-To` or `WARC-Concurrent-To` classifies captures:
/// those with `via` enter `extraPages.jsonl`, while the rest enter the required page list. A
/// metadata `title` takes precedence over one supplied by an optional [`PageTitleGenerator`].
/// Missing index digests are computed without changing the copied record. Captures without a
/// usable status or digest are copied but reported as [`ConversionWarning`] and not indexed.
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
        let warc_name = if output_gzip {
            "data.warc.gz"
        } else {
            "data.warc"
        };

        if input_gzip {
            let reader = WarcReader::from_path_gzip(&self.input)?;
            self.convert(reader, warc_name)
        } else {
            let reader = WarcReader::from_path(&self.input)?;
            self.convert(reader, warc_name)
        }
    }

    fn convert<R: BufRead>(
        &mut self,
        reader: WarcReader<R>,
        warc_name: &str,
    ) -> Result<ConversionSummary, Error> {
        let writer_config = WriterConfig {
            index_format: self.index_format,
            zip_compression_level: self.zip_compression_level,
            gzip_compression_level: self.gzip_compression_level,
            ..WriterConfig::default()
        };
        let mut wacz = WaczWriter::create_with_config(&self.output, writer_config)?;
        let mut warc = wacz.start_warc(warc_name)?;
        let store = SpoolStore::new()?;
        let transaction = store.begin()?;
        let mut spool = ConversionSpool::new(&transaction)?;
        let mut package_info = PackageInfo::default();
        let mut records = 0;

        for record in reader.iter_raw_records() {
            let record = record?;
            let written = warc.write_record(&record)?;
            records += 1;
            if !is_inspected(&record.header) {
                continue;
            }
            // Writing is complete, so semantic parsing can consume the raw record.
            let record = parse_record(record)?;
            package_info.inspect(&record);
            collect_metadata(&mut spool, &record)?;
            match capture_parts(
                &record,
                self.title_generator.as_deref_mut(),
                warc_name,
                &written,
            ) {
                Ok(Some((item, page))) => spool.add_capture(&item, &page)?,
                Ok(None) => {}
                Err(reason) => {
                    package_info.skip_capture(record.core().record_id.as_str(), reason);
                }
            }
        }

        warc.finish()?;
        let mut outputs = spool.finish(package_info.pages_from_metadata)?;
        wacz.add_spooled_index(INDEX_NAME, outputs.index)?;
        wacz.add_page_list_file("pages.jsonl", BufReader::new(&mut outputs.pages))?;
        if outputs.extra_pages > 0 {
            wacz.add_page_list_file(
                EXTRA_PAGES_NAME,
                BufReader::new(&mut outputs.extra_page_file),
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
        if let Some((url, date)) = outputs.main_page {
            metadata = metadata.main_page_url(url).main_page_date(date);
        }
        wacz.finish(metadata)?.flush()?;

        Ok(ConversionSummary {
            records,
            captures: outputs.captures,
            pages: outputs.pages_count,
            warnings,
        })
    }
}

/// The record types whose fields the conversion reads; all others are copied unparsed.
const INSPECTED_TYPES: [RecordType; 4] = [
    RecordType::Warcinfo,
    RecordType::Metadata,
    RecordType::Response,
    RecordType::Revisit,
];

/// Whether a raw record declares one of the [`INSPECTED_TYPES`].
///
/// This preliminary check ignores case and surrounding whitespace; semantic parsing validates it.
fn is_inspected(header: &raw::RecordHeader) -> bool {
    header.get("WARC-Type").is_some_and(|value| {
        let value = value.trim_ascii();
        INSPECTED_TYPES
            .iter()
            .any(|record_type| value.eq_ignore_ascii_case(record_type.as_str().as_bytes()))
    })
}

/// Convert an already written raw record to its semantic representation.
fn parse_record(record: raw::Record) -> Result<Record, warc_read::Error> {
    Ok(Record::<NoExtension>::try_from(untyped::Record::try_from(
        record,
    )?)?)
}

#[derive(Default)]
struct PackageInfo {
    title: Option<String>,
    description: Option<String>,
    warcinfo_count: usize,
    duplicate_warcinfo_record_ids: Vec<String>,
    pages_from_metadata: bool,
    skipped_captures: Vec<ConversionWarning>,
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

    fn skip_capture(&mut self, record_id: &str, reason: SkipReason) {
        self.skipped_captures
            .push(ConversionWarning::CaptureNotIndexed {
                record_id: record_id.to_owned(),
                reason,
            });
    }

    fn warnings(&self) -> Vec<ConversionWarning> {
        (self.warcinfo_count > 1)
            .then_some(ConversionWarning::MultipleWarcinfo {
                count: self.warcinfo_count,
                duplicate_record_ids: self.duplicate_warcinfo_record_ids.clone(),
            })
            .into_iter()
            .chain(self.skipped_captures.iter().cloned())
            .collect()
    }
}

fn collect_metadata(spool: &mut ConversionSpool<'_>, record: &Record) -> Result<(), Error> {
    let Record::Metadata {
        header,
        body: FieldsBlock::Fields(fields),
    } = record
    else {
        return Ok(());
    };
    let annotation = Annotation::new(
        fields
            .get(&MetadataField::Dcmi(DcmiTerm::Title))
            .filter(|title| !title.is_empty())
            .map(str::to_owned),
        fields.via().is_some(),
        fields
            .get(&MetadataField::Other("pageurl".to_owned()))
            .map(str::to_owned),
    );
    spool.annotate(
        header.refers_to.iter().chain(&header.concurrent_to),
        &annotation,
    )?;
    Ok(())
}

/// Describe an HTTP `response` or `revisit` record, written at `written`, as the CDXJ line
/// locating it and the page it may become.
///
/// Returns `Ok(None)` for records of other types and for captures of non-HTTP URLs, and an error
/// naming the field that keeps an HTTP capture out of the index.
fn capture_parts(
    record: &Record,
    generator: Option<&mut (dyn PageTitleGenerator + '_)>,
    warc_name: &str,
    written: &Written,
) -> Result<Option<(cdxj::ConformingItem<'static>, PageDraft)>, SkipReason> {
    let (message, revisit) = match record {
        Record::Response { body, .. } => (body.as_slice(), false),
        Record::Revisit { body, .. } => (body.as_slice(), true),
        _ => return Ok(None),
    };
    let (Some(target_uri), Some(payload_headers)) = (record.target_uri(), record.payload()) else {
        return Ok(None);
    };
    let Ok(canonical) = Canonicalizer::WAYBACK.canonicalize(target_uri.as_str()) else {
        return Ok(None);
    };
    if !matches!(canonical.scheme(), "http" | "https") {
        return Ok(None);
    }
    let head = ResponseMetadata::parse(message).ok_or(SkipReason::UnparsableHttpMessage)?;
    // A revisit has no local payload; a response's is its entity body when it can be extracted.
    let payload = (!revisit)
        .then(|| record.payload_bytes().ok().flatten())
        .flatten();
    let digest = match &payload_headers.payload_digest {
        Some(digest) => digest.to_string(),
        None => payload
            .as_deref()
            .and_then(|payload| LabelledDigest::compute(Algorithm::Sha256, payload))
            .ok_or(SkipReason::UndeterminedPayload)?
            .to_string(),
    };
    let mime = if revisit {
        REVISIT_MIME.to_owned()
    } else {
        content_type_essence(&head)
            .map(str::to_owned)
            .or_else(|| {
                payload_headers
                    .identified_payload_type
                    .as_ref()
                    .map(|media_type| {
                        format!("{}/{}", media_type.type_name(), media_type.subtype())
                    })
            })
            .unwrap_or_else(|| UNKNOWN_MIME.to_owned())
    };
    let entity: Cow<'_, [u8]> = if revisit {
        Cow::Borrowed(&[])
    } else {
        payload.unwrap_or_else(|| Cow::Borrowed(&message[head.body_offset..]))
    };
    let generated_title = generator.and_then(|generator| {
        generator.title(&Capture {
            url: target_uri.as_str(),
            status: head.status,
            payload: &entity,
            response: message,
        })
    });

    let date = record.core().date.date_time();
    let url = target_uri.as_str().to_owned();
    let page = PageDraft::new(
        record.core().record_id.as_str().to_owned(),
        url.clone(),
        date,
        generated_title,
    );
    let item = cdxj::Item {
        key: Cow::Owned(Cow::from(canonical.surt()).into_owned()),
        timestamp: cdxj::Timestamp::with_milliseconds(date),
        fields: cdxj::ConformingFields {
            url: Cow::Owned(url),
            digest: Cow::Owned(digest),
            mime: Cow::Owned(mime),
            status: head.status,
            offset: written.offset,
            length: written.length,
            filename: Cow::Owned(warc_name.to_owned()),
            record_digest: stored_digest(written),
            extra: ExtraProperties::default(),
        },
    };
    Ok(Some((item, page)))
}

/// The `type/subtype` of the response's `Content-Type`, without its parameters.
fn content_type_essence(head: &ResponseMetadata) -> Option<&str> {
    head.header("content-type")
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|essence| !essence.is_empty())
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
