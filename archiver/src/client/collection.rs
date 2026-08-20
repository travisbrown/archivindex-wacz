//! WARC spooling, CDX/page accumulation, and final WACZ assembly.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Seek, Write};

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::pages::{Page, PageListHeader};
use archivindex_wacz::writer::{PackageMetadata, WaczWriter};
use archivindex_warc::io::write::WarcWriter;
use fluent_uri::Uri;

use super::capture::Original;
use super::warc_mapping::{
    RevisitTarget, WarcinfoOptions, warcinfo_record, write_exchange, write_record,
};
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
    /// Stored `response` records eligible as revisit originals, by payload digest.
    revisits: HashMap<Sha256Digest, RevisitTarget>,
    /// The latest complete capture carrying validators for each target URI, which a later capture
    /// of the URI asks the server to revalidate.
    originals: HashMap<String, Original>,
}

impl Collection {
    /// Start a collection by writing its initial `warcinfo` record.
    pub(crate) fn new(
        warc_name: String,
        gzip: bool,
        warcinfo: &WarcinfoOptions<'_>,
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
            revisits: HashMap::new(),
            originals: HashMap::new(),
        })
    }

    /// The earlier capture of a target URI that a new capture may ask the server to revalidate.
    pub(super) fn original(&self, url: &str) -> Option<&Original> {
        self.originals.get(url)
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
            let original = exchange.original();
            let (item, target) = write_exchange(
                &mut self.warc,
                exchange,
                &self.warcinfo_id,
                &self.warc_name,
                self.gzip,
                via.filter(|_| hop == 0),
                key.and_then(|key| self.revisits.get(&key)),
            )?;
            if let Some(original) = original {
                self.originals.insert(item.fields.url.to_string(), original);
            }
            self.items.push(item);
            if let (Some(key), Some(target)) = (key, target) {
                self.revisits.insert(key, target);
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
            warc_name,
            summary,
            items,
            page_list,
            extra_page_list,
            ..
        } = self;
        let mut file = warc.finish().map_err(std::io::IntoInnerError::into_error)?;
        file.rewind()?;

        wacz.add_warc(&warc_name, file)?;
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
        Ok(summary)
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
