//! Disk-backed intermediate state for WARC-to-WACZ conversion.

use std::borrow::Cow;
use std::io::{Seek, Write};

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::io::write::index::IndexSpool;
use archivindex_wacz::pages::{Page, PageListHeader};
use redb::{ReadableTable as _, TableDefinition};

const PAGES: TableDefinition<'static, u64, &[u8]> = TableDefinition::new("pages");
const ANNOTATIONS: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("annotations");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("database operation failed")]
    Database(#[source] redb::Error),
    #[error("stored value serialization failed")]
    Serialization(#[from] serde_json::Error),
}

/// A page entry retaining its WARC record identity until linked metadata has been collected.
#[derive(serde::Deserialize, serde::Serialize)]
pub struct PageDraft {
    record_id: String,
    url: String,
    date: chrono::DateTime<chrono::Utc>,
    size: Option<u64>,
    title: Option<String>,
}

impl PageDraft {
    pub const fn new(
        record_id: String,
        url: String,
        date: chrono::DateTime<chrono::Utc>,
        size: Option<u64>,
        title: Option<String>,
    ) -> Self {
        Self {
            record_id,
            url,
            date,
            size,
            title,
        }
    }
}

#[derive(Default, serde::Deserialize, serde::Serialize)]
pub struct Annotation {
    title: Option<String>,
    via: bool,
    page_url: Option<String>,
}

impl Annotation {
    pub const fn new(title: Option<String>, via: bool, page_url: Option<String>) -> Self {
        Self {
            title,
            via,
            page_url,
        }
    }
}

pub struct ConversionSpool {
    _directory: tempfile::TempDir,
    transaction: redb::WriteTransaction,
    index: IndexSpool,
    captures: u64,
}

pub struct SpoolOutputs {
    pub index: IndexSpool,
    pub pages: std::fs::File,
    pub extra_page_file: std::fs::File,
    pub captures: usize,
    pub pages_count: usize,
    pub extra_pages: usize,
    pub main_page: Option<(String, chrono::DateTime<chrono::Utc>)>,
}

impl ConversionSpool {
    pub fn new() -> Result<Self, Error> {
        let directory = tempfile::tempdir()?;
        let database = redb::Database::create(directory.path().join("conversion.redb")).spool()?;
        let transaction = database.begin_write().spool()?;
        {
            transaction.open_table(PAGES).spool()?;
            transaction.open_table(ANNOTATIONS).spool()?;
        }
        Ok(Self {
            _directory: directory,
            transaction,
            index: IndexSpool::new(),
            captures: 0,
        })
    }

    pub fn add_capture(&mut self, item: &cdxj::Item<'_>, page: &PageDraft) -> Result<(), Error> {
        self.index.push(item)?;
        let bytes = serde_json::to_vec(page)?;
        let mut pages = self.transaction.open_table(PAGES).spool()?;
        pages.insert(self.captures, bytes.as_slice()).spool()?;
        self.captures += 1;
        Ok(())
    }

    pub fn annotate<I>(&self, record_ids: I, update: &Annotation) -> Result<(), Error>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut annotations = self.transaction.open_table(ANNOTATIONS).spool()?;
        for record_id in record_ids {
            let record_id = record_id.as_ref();
            let mut annotation: Annotation = annotations
                .get(record_id)
                .spool()?
                .map(|value| serde_json::from_slice(value.value()))
                .transpose()?
                .unwrap_or_default();
            if let Some(title) = &update.title {
                annotation.title = Some(title.clone());
            }
            annotation.via |= update.via;
            if let Some(page_url) = &update.page_url {
                annotation.page_url = Some(page_url.clone());
            }
            let bytes = serde_json::to_vec(&annotation)?;
            annotations.insert(record_id, bytes.as_slice()).spool()?;
        }
        Ok(())
    }

    pub fn finish(self, pages_from_metadata: bool) -> Result<SpoolOutputs, Error> {
        let Self {
            _directory,
            transaction,
            index,
            captures,
        } = self;
        let mut pages = tempfile::tempfile()?;
        let mut extra_page_file = tempfile::tempfile()?;
        write_json_line(&mut pages, &PageListHeader::default())?;
        write_json_line(&mut extra_page_file, &extra_page_list_header())?;
        let mut pages_count = 0;
        let mut extra_pages = 0;
        let mut main_page = None;

        {
            let stored_pages = transaction.open_table(PAGES).spool()?;
            let annotations = transaction.open_table(ANNOTATIONS).spool()?;
            for entry in stored_pages.iter().spool()? {
                let (_, value) = entry.spool()?;
                let draft: PageDraft = serde_json::from_slice(value.value())?;
                let annotation = annotations
                    .get(draft.record_id.as_str())
                    .spool()?
                    .map(|value| serde_json::from_slice::<Annotation>(value.value()))
                    .transpose()?
                    .unwrap_or_default();
                if pages_from_metadata && annotation.page_url.is_none() {
                    continue;
                }
                let url = annotation.page_url.unwrap_or(draft.url);
                let page = Page {
                    id: Some(Cow::Owned(archivindex_wacz::pages::synthetic_id(
                        &draft.date,
                        &url,
                        24,
                    ))),
                    url: Cow::Owned(url.clone()),
                    ts: draft.date,
                    title: annotation.title.or(draft.title).map(Cow::Owned),
                    text: None,
                    size: draft.size,
                    extra: ExtraProperties::default(),
                };
                let output = if annotation.via {
                    &mut extra_page_file
                } else {
                    &mut pages
                };
                write_json_line(output, &page)?;
                pages_count += 1;
                if annotation.via {
                    extra_pages += 1;
                } else if main_page.is_none() {
                    main_page = Some((url, draft.date));
                }
            }
        }
        transaction.commit().spool()?;
        pages.rewind()?;
        extra_page_file.rewind()?;
        Ok(SpoolOutputs {
            index,
            pages,
            extra_page_file,
            captures: usize::try_from(captures).expect("capture count fits in memory"),
            pages_count,
            extra_pages,
            main_page,
        })
    }
}

trait RedbResultExt<T> {
    fn spool(self) -> Result<T, Error>;
}

impl<T, E: Into<redb::Error>> RedbResultExt<T> for Result<T, E> {
    fn spool(self) -> Result<T, Error> {
        self.map_err(|error| Error::Database(error.into()))
    }
}

fn write_json_line(writer: &mut impl Write, value: &impl serde::Serialize) -> Result<(), Error> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    writer.write_all(b"\n")?;
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
