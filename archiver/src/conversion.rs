//! Conversion of existing WARC files into indexed WACZ packages.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::pages::{Page, PageListHeader};
use archivindex_wacz::writer::{PackageMetadata, WaczWriter, WriterConfig};
use archivindex_warc::io::read::{self as warc_read, WarcReader};
use archivindex_warc::io::write::{WarcWriter, Written};
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::dcmi::DcmiTerm;
use archivindex_warc::record::fields::metadata::MetadataField;
use archivindex_warc::record::{FieldsBlock, Record, payload};
use archivindex_warc::value::{LabelledDigest, WarcDate};
use url::Url;

use crate::config::IndexFormat;
use crate::response;
use crate::session::{Capture, CaptureProcessor};

const INDEX_NAME: &str = "index.cdx";
const EXTRA_PAGES_NAME: &str = "extraPages.jsonl";
const REVISIT_MIME: &str = "warc/revisit";

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
    Wacz(#[from] archivindex_wacz::writer::Error),
}

/// Counts of records and replay captures written by a WARC conversion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConversionSummary {
    /// All WARC records copied into the package.
    pub records: usize,
    /// HTTP `response` and `revisit` records added to the CDXJ index.
    pub captures: usize,
    /// Capture entries added to the WACZ page list.
    pub pages: usize,
}

/// A conversion from one existing WARC file to a new WACZ package.
///
/// Records are parsed semantically and normalized into a new WARC member. This makes offsets and
/// lengths independently addressable even when the input was a continuously compressed gzip
/// stream. Metadata fields linked by `WARC-Refers-To` or `WARC-Concurrent-To` classify captures:
/// those with `via` enter `extraPages.jsonl`, while the rest enter the required page list. A
/// metadata `title` takes precedence over one supplied by an optional [`CaptureProcessor`].
pub struct WarcToWacz<'a> {
    input: PathBuf,
    output: PathBuf,
    processor: Option<Box<dyn CaptureProcessor + 'a>>,
    index_format: IndexFormat,
}

impl<'a> WarcToWacz<'a> {
    /// Create a conversion from `input` to a new WACZ at `output`.
    #[must_use]
    pub fn new(input: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
            processor: None,
            index_format: IndexFormat::Plain,
        }
    }

    /// Use `processor` to generate fallback page titles from captured HTTP responses.
    ///
    /// Links and recaptures returned by the processor are ignored because conversion has no crawl
    /// queue. A title recorded in linked WARC metadata always replaces the generated title.
    #[must_use]
    pub fn processor<P: CaptureProcessor + 'a>(mut self, processor: P) -> Self {
        self.processor = Some(Box::new(processor));
        self
    }

    /// Select the plain or compressed CDXJ representation.
    #[must_use]
    pub const fn index_format(mut self, index_format: IndexFormat) -> Self {
        self.index_format = index_format;
        self
    }

    /// Parse the WARC and write the completed WACZ, refusing to overwrite `output`.
    pub fn run(mut self) -> Result<ConversionSummary, Error> {
        let gzip = is_gzip_path(&self.input);
        let warc_name = if gzip { "data.warc.gz" } else { "data.warc" };

        if gzip {
            let reader = WarcReader::from_path_gzip(&self.input)?;
            self.convert(reader, warc_name, true)
        } else {
            let reader = WarcReader::from_path(&self.input)?;
            self.convert(reader, warc_name, false)
        }
    }

    fn convert<R: BufRead>(
        &mut self,
        reader: WarcReader<R>,
        warc_name: &str,
        gzip: bool,
    ) -> Result<ConversionSummary, Error> {
        let mut warc = WarcWriter::new(BufWriter::new(tempfile::tempfile()?)).with_digests();
        let mut items = Vec::new();
        let mut pages = Vec::new();
        let mut annotations = HashMap::new();
        let mut records = 0;

        for record in reader.iter_records::<NoExtension>() {
            let record = record?;
            collect_metadata(&record, &mut annotations);
            let capture = capture_info(&record, self.processor.as_deref_mut());
            let raw = record.into_raw()?;
            let written = if gzip {
                warc.write_gzip(&raw)?
            } else {
                warc.write(&raw)?
            };
            records += 1;

            if let Some(capture) = capture {
                items.push(capture.item(warc_name, &written));
                pages.push(capture.page());
            }
        }

        for page in &mut pages {
            if let Some(annotation) = annotations.get(&page.record_id) {
                if let Some(title) = &annotation.title {
                    page.title = Some(title.clone());
                }
                page.extra = annotation.via;
            }
        }

        let mut file = warc.finish().map_err(std::io::IntoInnerError::into_error)?;
        file.rewind()?;
        let writer_config = WriterConfig {
            index_format: self.index_format,
            ..WriterConfig::default()
        };
        let mut wacz = WaczWriter::create_with_config(&self.output, writer_config)?;
        wacz.add_warc(warc_name, file)?;
        wacz.add_index(INDEX_NAME, &items)?;
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
        let metadata = PackageMetadata {
            main_page_url: page_entries
                .first()
                .map(|page| page.url.clone().into_owned()),
            main_page_date: page_entries.first().map(|page| page.ts),
            ..PackageMetadata::default()
        };
        wacz.finish(metadata)?.flush()?;

        Ok(ConversionSummary {
            records,
            captures: items.len(),
            pages: pages.len(),
        })
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
    processor: Option<&mut (dyn CaptureProcessor + '_)>,
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
            processor,
        ),
        Record::Revisit { header, body } => capture_info_from_http(
            header.core.record_id.as_str(),
            header.target_uri.as_str(),
            header.core.date,
            header.payload.payload_digest.clone(),
            Some(REVISIT_MIME.to_owned()),
            body,
            true,
            processor,
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
    processor: Option<&mut (dyn CaptureProcessor + '_)>,
) -> Option<CaptureInfo> {
    let url = Url::parse(target_uri).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let key = cdxj::search_key(target_uri).ok()?;
    let head = response::head(message);
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
    let generated_title = processor.and_then(|processor| {
        status.and_then(|status| {
            processor
                .inspect(&Capture {
                    url: target_uri,
                    final_url: target_uri,
                    status,
                    payload: &entity,
                    response: message,
                })
                .title
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

    for record_id in header.refers_to.iter().chain(&header.concurrent_to) {
        let annotation = annotations
            .entry(record_id.as_str().to_owned())
            .or_default();
        if title.is_some() {
            annotation.title.clone_from(&title);
        }
        annotation.via |= via;
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

fn is_gzip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gz"))
}
