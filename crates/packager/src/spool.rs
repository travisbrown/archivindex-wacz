//! Disk-backed intermediate state for WARC-to-WACZ conversion.
//!
//! Metadata may precede or follow a capture. A temporary file stores page drafts in capture
//! order, and a redb database stores annotations by record ID. They are joined after the source
//! has been read.

use std::borrow::Cow;
use std::io::{BufReader, BufWriter, Seek, Write};

use archivindex_cdx::format::cdxj;
use archivindex_cdx::properties::ExtraProperties;
use archivindex_wacz::io::write::index::IndexSpool;
use archivindex_wacz::pages::{Page, PageListHeader, PageListWriter};
use redb::{ReadableTable as _, Table, TableDefinition, WriteTransaction};

const ANNOTATIONS: TableDefinition<'static, &str, &[u8]> = TableDefinition::new("annotations");

/// A failure of the temporary conversion state.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("database operation failed")]
    Database(#[source] redb::Error),
    #[error("stored value serialization failed")]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    PageList(#[from] archivindex_wacz::pages::Error),
    #[error(transparent)]
    Index(#[from] archivindex_wacz::io::write::Error),
}

/// A page entry retaining its WARC record identity until linked metadata has been collected.
#[derive(Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PageDraft {
    record_id: String,
    url: String,
    date: chrono::DateTime<chrono::Utc>,
    title: Option<String>,
}

impl PageDraft {
    /// Describe the page a capture may become.
    pub const fn new(
        record_id: String,
        url: String,
        date: chrono::DateTime<chrono::Utc>,
        title: Option<String>,
    ) -> Self {
        Self {
            record_id,
            url,
            date,
            title,
        }
    }
}

/// Page properties contributed by `metadata` records.
#[derive(Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct Annotation {
    title: Option<String>,
    via: bool,
    page_url: Option<String>,
}

impl Annotation {
    /// Collect the page properties of one `metadata` record.
    pub const fn new(title: Option<String>, via: bool, page_url: Option<String>) -> Self {
        Self {
            title,
            via,
            page_url,
        }
    }

    /// Apply later title and URL values, retaining `via` if any annotation sets it.
    fn merge(&mut self, update: &Self) {
        if let Some(title) = &update.title {
            self.title = Some(title.clone());
        }
        self.via |= update.via;
        if let Some(page_url) = &update.page_url {
            self.page_url = Some(page_url.clone());
        }
    }
}

/// The private database holding a conversion's annotations.
///
/// Its anonymous temporary file is removed when the store is dropped.
pub struct SpoolStore {
    database: redb::Database,
}

impl SpoolStore {
    /// Create the database in an anonymous temporary file.
    pub fn new() -> Result<Self, Error> {
        let database = redb::Database::builder()
            .create_file(tempfile::tempfile()?)
            .spool()?;
        Ok(Self { database })
    }

    /// Begin the uncommitted transaction used by a conversion spool.
    pub fn begin(&self) -> Result<WriteTransaction, Error> {
        self.database.begin_write().spool()
    }
}

/// The index lines, page drafts and annotations gathered while a source is read.
///
/// Its temporary page file and annotation table remain open for the duration of the conversion.
pub struct ConversionSpool<'txn> {
    pages: BufWriter<std::fs::File>,
    annotations: Table<'txn, &'static str, &'static [u8]>,
    index: IndexSpool,
    captures: usize,
}

/// The members and counts a finished spool contributes to the package.
pub struct SpoolOutputs {
    pub index: IndexSpool,
    pub pages: std::fs::File,
    pub extra_page_file: std::fs::File,
    pub captures: usize,
    pub pages_count: usize,
    pub extra_pages: usize,
    pub main_page: Option<(String, chrono::DateTime<chrono::Utc>)>,
}

impl<'txn> ConversionSpool<'txn> {
    /// Create a temporary page file and open the annotation table in `transaction`.
    pub fn new(transaction: &'txn WriteTransaction) -> Result<Self, Error> {
        Ok(Self {
            pages: BufWriter::new(tempfile::tempfile()?),
            annotations: transaction.open_table(ANNOTATIONS).spool()?,
            index: IndexSpool::new(),
            captures: 0,
        })
    }

    /// Record an indexed capture and the page it may become.
    pub fn add_capture(
        &mut self,
        item: &cdxj::ConformingEntry<'_>,
        page: &PageDraft,
    ) -> Result<(), Error> {
        self.index.push(item)?;
        serde_json::to_writer(&mut self.pages, page)?;
        self.pages.write_all(b"\n")?;
        self.captures += 1;
        Ok(())
    }

    /// Attach page properties to every record ID a `metadata` record refers to.
    pub fn annotate<I>(&mut self, record_ids: I, update: &Annotation) -> Result<(), Error>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        for record_id in record_ids {
            let record_id = record_id.as_ref();
            let mut annotation: Annotation = self
                .annotations
                .get(record_id)
                .spool()?
                .map(|value| serde_json::from_slice(value.value()))
                .transpose()?
                .unwrap_or_default();
            annotation.merge(update);
            let bytes = serde_json::to_vec(&annotation)?;
            self.annotations
                .insert(record_id, bytes.as_slice())
                .spool()?;
        }
        Ok(())
    }

    /// Join the drafts with their annotations and render both page lists in capture order.
    pub fn finish(self, pages_from_metadata: bool) -> Result<SpoolOutputs, Error> {
        let Self {
            pages: stored_pages,
            annotations,
            index,
            captures,
        } = self;
        let stored_pages = finish_file(stored_pages)?;
        let drafts = serde_json::Deserializer::from_reader(BufReader::new(stored_pages))
            .into_iter::<PageDraft>();
        let mut pages = BufWriter::new(tempfile::tempfile()?);
        let mut extra_page_file = BufWriter::new(tempfile::tempfile()?);
        let mut pages_count = 0;
        let mut extra_pages = 0;
        let mut main_page = None;

        {
            let extra_header = PageListHeader {
                id: Some(Cow::Borrowed("extra-pages")),
                title: Some(Cow::Borrowed("Extra Pages")),
                ..PageListHeader::default()
            };
            let mut page_writer = PageListWriter::new(&mut pages, &PageListHeader::default())?;
            let mut extra_page_writer = PageListWriter::new(&mut extra_page_file, &extra_header)?;
            for draft in drafts {
                let draft = draft?;
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
                    size: None,
                    extra: ExtraProperties::default(),
                };
                pages_count += 1;
                if annotation.via {
                    extra_page_writer.write(&page)?;
                    extra_pages += 1;
                } else {
                    page_writer.write(&page)?;
                    if main_page.is_none() {
                        main_page = Some((url, draft.date));
                    }
                }
            }
        }
        Ok(SpoolOutputs {
            index,
            pages: finish_file(pages)?,
            extra_page_file: finish_file(extra_page_file)?,
            captures,
            pages_count,
            extra_pages,
            main_page,
        })
    }
}

/// Flush a temporary file and rewind it for reading, propagating any buffered write failure.
fn finish_file(writer: BufWriter<std::fs::File>) -> Result<std::fs::File, std::io::Error> {
    let mut file = writer
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?;
    file.rewind()?;
    Ok(file)
}

trait RedbResultExt<T> {
    fn spool(self) -> Result<T, Error>;
}

impl<T, E: Into<redb::Error>> RedbResultExt<T> for Result<T, E> {
    fn spool(self) -> Result<T, Error> {
        self.map_err(|error| Error::Database(error.into()))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    /// Buffered writes can appear to succeed even when the file is unwritable. Finishing the
    /// spool must surface the flush failure before attempting to read the drafts.
    #[test]
    fn finishing_propagates_page_flush_errors() -> Result<(), Error> {
        let store = SpoolStore::new()?;
        let transaction = store.begin()?;
        let mut spool = ConversionSpool::new(&transaction)?;
        let file = tempfile::NamedTempFile::new()?;
        spool.pages = BufWriter::new(std::fs::File::open(file.path())?);
        spool.pages.write_all(b"{}\n")?;

        assert!(matches!(spool.finish(false), Err(Error::Io(_))));
        Ok(())
    }

    #[proptest::property_test]
    fn annotations_merge_to_the_last_present_values(
        #[strategy = proptest::collection::vec(strategies::annotation(), 0..=6)] updates: Vec<
            Annotation,
        >,
    ) {
        let mut merged = Annotation::default();

        for update in &updates {
            merged.merge(update);
        }

        let last_title = updates.iter().rev().find_map(|update| update.title.clone());
        let last_page_url = updates
            .iter()
            .rev()
            .find_map(|update| update.page_url.clone());

        prop_assert_eq!(merged.title, last_title);
        prop_assert_eq!(merged.via, updates.iter().any(|update| update.via));
        prop_assert_eq!(merged.page_url, last_page_url);
    }

    #[proptest::property_test]
    fn spooled_values_round_trip_through_json(
        #[strategy = strategies::annotation()] annotation: Annotation,
        #[strategy = strategies::page_draft()] draft: PageDraft,
    ) {
        let bytes = serde_json::to_vec(&annotation).unwrap();

        prop_assert_eq!(
            serde_json::from_slice::<Annotation>(&bytes).unwrap(),
            annotation
        );

        let bytes = serde_json::to_vec(&draft).unwrap();

        prop_assert_eq!(serde_json::from_slice::<PageDraft>(&bytes).unwrap(), draft);
    }
}
