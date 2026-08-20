//! WARC spooling, CDX/page accumulation, and final WACZ assembly.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, BufWriter, Seek, Write};

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::pages::{Page, PageListHeader};
use archivindex_wacz::writer::{PackageMetadata, WaczWriter};
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::io::write::WarcWriter;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc_revisit_index::{
    Index, ResourceKey, ResourceState, ResourceStateUpdate, Transaction,
};
use flate2::bufread::MultiGzDecoder;
use fluent_uri::Uri;

use super::capture::Original;
use super::warc_fields::{WarcinfoOptions, warcinfo_record};
use super::warc_mapping::{MetadataOptions, write_exchange, write_record};
use super::{ArchiveSummary, CaptureSummary, Error, Exchange, Failure};
use crate::config::DEFAULT_USER_AGENT;

const INDEX_NAME: &str = "index.cdx";
const EXTRA_PAGES_NAME: &str = "extraPages.jsonl";

/// Files accumulated while captures are written to a spooled WARC file.
pub struct Collection {
    warc: WarcWriter<BufWriter<File>>,
    warcinfo_id: Uri<String>,
    warc_name: String,
    gzip: bool,
    summary: ArchiveSummary,
    items: Vec<cdxj::Item<'static>>,
    page_list: Vec<Page<'static>>,
    extra_page_list: Vec<Page<'static>>,
    /// Payload and conditional-request state created by this collection.
    session_index: Index,
    /// Earlier durable crawl state, published to only after the WACZ is complete.
    persistent_index: Option<Index>,
}

impl Collection {
    /// Start a collection by writing its initial `warcinfo` record.
    pub(super) fn new(
        warc_name: String,
        gzip: bool,
        warcinfo: &WarcinfoOptions<'_>,
        persistent_index: Option<Index>,
    ) -> Result<Self, Error> {
        let mut warc = WarcWriter::new(BufWriter::new(tempfile::tempfile()?)).with_digests();
        let warcinfo = warcinfo_record(&warc_name, warcinfo)?;
        let warcinfo_id = warcinfo.core().record_id.clone();
        write_record(&mut warc, warcinfo, gzip)?;

        Ok(Self {
            warc,
            warcinfo_id,
            warc_name,
            gzip,
            summary: ArchiveSummary::default(),
            items: Vec::new(),
            page_list: Vec::new(),
            extra_page_list: Vec::new(),
            session_index: Index::open_in_memory()?,
            persistent_index,
        })
    }

    /// The earlier capture of a target URI that a new capture may ask the server to revalidate.
    pub(super) fn original(&self, url: &str) -> Result<Option<Original>, Error> {
        let target_uri = Uri::parse(url)
            .expect("invariant violation: a parsed URL failed to reparse as a URI")
            .to_owned();
        let key = ResourceKey::new(target_uri);

        if let Some(state) = self.session_index.lookup_resource(&key)? {
            return self.original_from_state(state);
        }
        let Some(state) = self
            .persistent_index
            .as_ref()
            .map(|index| index.lookup_resource(&key))
            .transpose()?
            .flatten()
        else {
            return Ok(None);
        };
        self.session_index.update_resource(
            &key,
            ResourceStateUpdate::representation(
                state.etag.clone(),
                state.last_modified.clone(),
                state.payload_digest.clone(),
                state.record_id.clone(),
                state.warc_date,
            ),
        )?;

        self.original_from_state(state)
    }

    /// Resolve a resource state's canonical payload target across the session overlay and durable
    /// index.
    fn original_from_state(&self, state: ResourceState) -> Result<Option<Original>, Error> {
        let canonical = state
            .payload_digest
            .as_ref()
            .map(|digest| self.lookup_payload(digest))
            .transpose()?
            .flatten();

        Ok(Original::from_state(state, canonical))
    }

    /// Look up a payload created in this collection before consulting earlier durable state.
    fn lookup_payload(
        &self,
        digest: &archivindex_warc::value::LabelledDigest,
    ) -> Result<Option<archivindex_warc_revisit_index::RevisitTarget>, Error> {
        if let Some(target) = self.session_index.lookup_payload(digest)? {
            return Ok(Some(target));
        }

        self.persistent_index
            .as_ref()
            .map(|index| index.lookup_payload(digest))
            .transpose()
            .map(Option::flatten)
            .map_err(Error::from)
    }

    /// Record every captured hop and add either a page summary or failure.
    ///
    /// A hop whose payload digest matches an earlier capture in this collection, or whose `304 Not
    /// Modified` confirms an earlier capture's payload unchanged, is stored as a `revisit` record
    /// referencing the original, instead of repeating the payload.
    pub(crate) fn record(
        &mut self,
        url: String,
        exchanges: Vec<Exchange>,
        error: Option<Error>,
        title: Option<String>,
        extra: bool,
        via: Option<&str>,
    ) -> Result<(), Error> {
        let redirects = exchanges.len().saturating_sub(1);
        let mut last = None;

        for (hop, exchange) in exchanges.into_iter().enumerate() {
            last = Some((
                exchange.date.date_time(),
                exchange.status,
                exchange.payload_length,
            ));
            let key = exchange.revisit_key();
            let resource_key = exchange.resource_key();
            let etag = exchange.validator("etag");
            let last_modified = exchange.validator("last-modified");
            let status = exchange.status;
            let revalidated = exchange.revalidated.clone();
            let revisit_of = match &revalidated {
                Some(target) => Some(target.clone()),
                None => key
                    .as_ref()
                    .map(|digest| self.lookup_payload(digest))
                    .transpose()?
                    .flatten(),
            };
            let (item, target) = write_exchange(
                &mut self.warc,
                exchange,
                &self.warcinfo_id,
                &self.warc_name,
                self.gzip,
                MetadataOptions {
                    via: via.filter(|_| hop == 0),
                    title: title.as_deref().filter(|_| hop == redirects),
                },
                revisit_of.as_ref(),
            )?;
            self.items.push(item);

            if key.is_some()
                && let Some(target) = &target
            {
                self.session_index.insert_payload(target)?;
            }

            if status == 304 && revalidated.is_some() {
                self.session_index.update_resource(
                    &resource_key,
                    ResourceStateUpdate::not_modified(etag, last_modified),
                )?;
            } else if status == 200
                && key.is_some()
                && let Some(original) = revisit_of.as_ref().or(target.as_ref())
            {
                self.session_index.update_resource(
                    &resource_key,
                    ResourceStateUpdate::representation(
                        etag,
                        last_modified,
                        Some(original.payload_digest.clone()),
                        Some(original.record_id.clone()),
                        Some(original.warc_date),
                    ),
                )?;
            }
        }

        if let Some(error) = error {
            self.summary.failures.push(Failure { url, error });
        } else {
            let (date, status, size) =
                last.expect("a capture without an error has at least one exchange");
            let page = Page {
                url: Cow::Owned(url.clone()),
                ts: date,
                id: None,
                title: title.map(Cow::Owned),
                text: None,
                size: Some(size),
                extra: ExtraProperties::default(),
            };

            if extra {
                self.extra_page_list.push(page);
            } else {
                self.page_list.push(page);
            }
            self.summary.captures.push(CaptureSummary {
                url,
                date,
                status,
                size,
                redirects,
            });
        }

        Ok(())
    }

    /// Add the spooled WARC and supporting resources and finish the WACZ.
    pub(crate) fn finish<W: Write + Seek>(
        self,
        mut wacz: WaczWriter<W>,
        title: Option<String>,
    ) -> Result<ArchiveSummary, Error> {
        let Self {
            warc,
            warcinfo_id: _,
            warc_name,
            summary,
            items,
            page_list,
            extra_page_list,
            persistent_index,
            gzip,
            session_index: _,
        } = self;
        let mut file = warc.finish().map_err(std::io::IntoInnerError::into_error)?;
        file.rewind()?;

        wacz.add_warc(&warc_name, &mut file)?;
        wacz.add_index(INDEX_NAME, &items)?;
        wacz.add_pages(&PageListHeader::default(), &page_list)?;
        if !extra_page_list.is_empty() {
            wacz.add_page_list(
                EXTRA_PAGES_NAME,
                &extra_page_list_header(),
                &extra_page_list,
            )?;
        }

        let metadata = PackageMetadata {
            title,
            software: Some(DEFAULT_USER_AGENT.to_owned()),
            main_page_url: page_list.first().map(|page| page.url.clone().into_owned()),
            main_page_date: page_list.first().map(|page| page.ts),
            ..PackageMetadata::default()
        };
        wacz.finish(metadata)?.flush()?;
        if let Some(mut persistent_index) = persistent_index {
            publish_warc(&mut persistent_index, &mut file, gzip)?;
        }
        Ok(summary)
    }
}

/// Publish the completed WARC's records to durable crawl state as one atomic update.
fn publish_warc(index: &mut Index, file: &mut File, gzip: bool) -> Result<(), Error> {
    file.rewind()?;
    let transaction = index.begin()?;

    if gzip {
        let decoder = MultiGzDecoder::new(BufReader::new(file));
        index_records(BufReader::new(decoder), &transaction)?;
    } else {
        index_records(BufReader::new(file), &transaction)?;
    }

    transaction.commit()?;
    Ok(())
}

/// Parse and index every semantic record from one completed WARC stream.
fn index_records<R: std::io::BufRead>(
    reader: R,
    transaction: &Transaction<'_>,
) -> Result<(), Error> {
    for record in WarcReader::new(reader).iter_records::<NoExtension>() {
        transaction.index_record(&record?)?;
    }
    Ok(())
}

fn extra_page_list_header() -> PageListHeader<'static> {
    PageListHeader {
        format: Cow::Borrowed(archivindex_wacz::pages::FORMAT),
        id: Some(Cow::Borrowed("extra-pages")),
        title: Some(Cow::Borrowed("Extra Pages")),
        extra: ExtraProperties::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Error as IoError, SeekFrom};

    use archivindex_wacz::digest::Sha256Digest;
    use archivindex_warc::record::Record;
    use archivindex_warc::value::{DigestAlgorithm, LabelledDigest, WarcDate};
    use archivindex_warc::version::WarcVersion;
    use archivindex_warc_revisit_index::RevisitTarget;

    use super::*;

    /// A seekable WACZ sink that accepts the ZIP bytes but cannot flush them durably.
    struct FailingWriter {
        cursor: Cursor<Vec<u8>>,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.cursor.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(IoError::other("injected WACZ failure"))
        }
    }

    impl Seek for FailingWriter {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(position)
        }
    }

    #[test]
    fn failed_wacz_is_not_published_to_the_persistent_index()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("revisits.sqlite3");
        let digest = Sha256Digest::compute(b"body");
        let labelled = LabelledDigest::from_digest(DigestAlgorithm::Sha256, &digest.0);
        let date =
            WarcDate::parse("2025-01-01T00:00:00Z", WarcVersion::V1_1).expect("test WARC date");
        let mut collection = Collection::new(
            "failed.warc".to_owned(),
            false,
            &WarcinfoOptions::archiver("test-agent/1.0"),
            Some(Index::open(&database)?),
        )?;
        let response = Record::<NoExtension>::response("https://example.com/", date)?
            .payload_digest(labelled.clone())
            .body(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody".to_vec())?;
        collection.session_index.insert_payload(&RevisitTarget {
            payload_digest: labelled.clone(),
            payload_length: Some(4),
            record_id: response.core().record_id.clone(),
            target_uri: Uri::parse("https://example.com/")?.to_owned(),
            warc_date: date,
        })?;
        write_record(&mut collection.warc, response, false)?;

        let result = collection.finish(
            WaczWriter::new(FailingWriter {
                cursor: Cursor::new(Vec::new()),
            }),
            None,
        );

        assert!(result.is_err());
        assert!(Index::open(&database)?.lookup_payload(&labelled)?.is_none());
        Ok(())
    }
}
