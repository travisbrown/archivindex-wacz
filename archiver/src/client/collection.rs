//! WARC spooling and revisit-state accumulation.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use archivindex_warc::io::write::WarcWriter;
use archivindex_warc_revisit_index::db::Index;
use archivindex_warc_revisit_index::payload::RevisitTarget;
use archivindex_warc_revisit_index::resource::{ResourceKey, ResourceState, ResourceStateUpdate};
use fluent_uri::Uri;
use tempfile::{NamedTempFile, TempPath};

use super::capture::Original;
use super::capture::{CaptureOutcome, Exchange};
use super::warc_fields::{WarcinfoOptions, warcinfo_record};
use super::warc_mapping::{MetadataOptions, write_exchange, write_record};
use super::{ArchiveSummary, CaptureSummary, Error, Failure};

/// Files accumulated while captures are written to a spooled WARC file.
pub struct Collection {
    warc: WarcWriter<BufWriter<File>>,
    spool_path: Option<TempPath>,
    warcinfo_id: Uri<String>,
    gzip: bool,
    summary: ArchiveSummary,
    /// Payload and conditional-request state created by this collection.
    session_index: Index,
    /// Earlier durable crawl state, published to only after the WARC is durable.
    persistent_index: Option<Index>,
}

impl Collection {
    /// Start a collection by writing its initial `warcinfo` record.
    pub(super) fn new(
        warc_name: &str,
        gzip: bool,
        warcinfo: &WarcinfoOptions<'_>,
        persistent_index: Option<Index>,
    ) -> Result<Self, Error> {
        Self::with_spool(
            tempfile::tempfile()?,
            None,
            warc_name,
            gzip,
            warcinfo,
            persistent_index,
        )
    }

    /// Start a collection in `<output>.partial` so its growth is visible while it is written.
    pub(super) fn new_for_path(
        output: &Path,
        warc_name: &str,
        gzip: bool,
        warcinfo: &WarcinfoOptions<'_>,
        persistent_index: Option<Index>,
    ) -> Result<Self, Error> {
        if output.try_exists()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("output already exists: {}", output.display()),
            )
            .into());
        }

        let partial_path = std::path::absolute(partial_path(output))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&partial_path)?;
        let spool_path = TempPath::try_from_path(&partial_path)?;

        Self::with_spool(
            file,
            Some(spool_path),
            warc_name,
            gzip,
            warcinfo,
            persistent_index,
        )
    }

    fn with_spool(
        file: File,
        spool_path: Option<TempPath>,
        warc_name: &str,
        gzip: bool,
        warcinfo: &WarcinfoOptions<'_>,
        persistent_index: Option<Index>,
    ) -> Result<Self, Error> {
        let mut warc = WarcWriter::new(BufWriter::new(file));
        let warcinfo = warcinfo_record(warc_name, warcinfo)?;
        let warcinfo_id = warcinfo.core().record_id.clone();
        write_record(&mut warc, warcinfo, gzip)?;
        if spool_path.is_some() {
            warc.flush()?;
        }

        Ok(Self {
            warc,
            spool_path,
            warcinfo_id,
            gzip,
            summary: ArchiveSummary::default(),
            session_index: Index::open_in_memory()?,
            persistent_index,
        })
    }

    /// The earlier capture of a target URI that a new capture may ask the server to revalidate.
    pub(super) fn original(&self, target_uri: Uri<String>) -> Result<Option<Original>, Error> {
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
            ResourceStateUpdate::Representation {
                etag: state.etag.clone(),
                last_modified: state.last_modified.clone(),
                payload_digest: state.payload_digest.clone(),
                record_id: state.record_id.clone(),
                warc_date: state.warc_date,
            },
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
    ) -> Result<Option<RevisitTarget>, Error> {
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
        outcome: CaptureOutcome,
        title: Option<&str>,
        via: Option<&str>,
    ) -> Result<(), Error> {
        match outcome {
            CaptureOutcome::Captured(exchanges) => {
                let redirects = exchanges.len().saturating_sub(1);
                let last = self.record_exchanges(exchanges, title, via, redirects)?;
                let (date, status, size) =
                    last.expect("a successful capture has at least one exchange");
                self.summary.captures.push(CaptureSummary {
                    url,
                    date,
                    status,
                    size,
                    redirects,
                });
            }
            CaptureOutcome::Failed { exchanges, error } => {
                let redirects = exchanges.len().saturating_sub(1);
                self.record_exchanges(exchanges, title, via, redirects)?;
                self.summary.failures.push(Failure { url, error });
            }
        }

        if self.spool_path.is_some() {
            self.warc.flush()?;
        }

        Ok(())
    }

    fn record_exchanges(
        &mut self,
        exchanges: Vec<Exchange>,
        title: Option<&str>,
        via: Option<&str>,
        redirects: usize,
    ) -> Result<Option<(chrono::DateTime<chrono::Utc>, u16, u64)>, Error> {
        let mut last = None;

        for (hop, exchange) in exchanges.into_iter().enumerate() {
            last = Some((
                exchange.date.date_time(),
                exchange.status,
                exchange.payload_length(),
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
            let target = write_exchange(
                &mut self.warc,
                exchange,
                &self.warcinfo_id,
                self.gzip,
                MetadataOptions {
                    via: via.filter(|_| hop == 0),
                    title: title.filter(|_| hop == redirects),
                },
                revisit_of.as_ref(),
            )?;

            if key.is_some()
                && let Some(target) = &target
            {
                self.session_index.insert_payload(target)?;
            }

            if status == 304 && revalidated.is_some() {
                self.session_index.update_resource(
                    &resource_key,
                    ResourceStateUpdate::NotModified {
                        etag,
                        last_modified,
                    },
                )?;
            } else if status == 200
                && key.is_some()
                && let Some(original) = revisit_of.as_ref().or(target.as_ref())
            {
                self.session_index.update_resource(
                    &resource_key,
                    ResourceStateUpdate::Representation {
                        etag,
                        last_modified,
                        payload_digest: Some(original.payload_digest.clone()),
                        record_id: Some(original.record_id.clone()),
                        warc_date: Some(original.warc_date),
                    },
                )?;
            }
        }

        Ok(last)
    }

    /// Copy the completed WARC to `output`.
    pub(crate) fn finish<W: Write>(self, mut output: W) -> Result<ArchiveSummary, Error> {
        let Self {
            warc,
            spool_path: _,
            warcinfo_id: _,
            summary,
            persistent_index: _,
            gzip: _,
            session_index: _,
        } = self;
        let mut file = warc.finish().map_err(std::io::IntoInnerError::into_error)?;
        file.rewind()?;
        std::io::copy(&mut file, &mut output)?;
        output.flush()?;
        Ok(summary)
    }

    /// Atomically publish the completed WARC at `path`, then update durable revisit state.
    pub(crate) fn finish_to_path(self, path: &Path) -> Result<ArchiveSummary, Error> {
        let Self {
            warc,
            spool_path,
            warcinfo_id: _,
            summary,
            mut persistent_index,
            gzip: _,
            session_index,
        } = self;
        let mut source = warc.finish().map_err(std::io::IntoInnerError::into_error)?;
        source.rewind()?;

        if let Some(spool_path) = spool_path {
            source.sync_all()?;
            NamedTempFile::from_parts(source, spool_path)
                .persist_noclobber(path)
                .map_err(|error| error.error)?;
        } else {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let mut temporary = NamedTempFile::new_in(parent)?;
            std::io::copy(&mut source, &mut temporary)?;
            temporary.flush()?;
            temporary.as_file().sync_all()?;
            temporary
                .persist_noclobber(path)
                .map_err(|error| error.error)?;
        }

        // The session index already holds every row the published WARC would yield, so it is
        // copied into durable state rather than re-read from the file.
        if let Some(index) = &mut persistent_index {
            let transaction = index.begin()?;
            transaction.merge_from(&session_index)?;
            transaction.commit()?;
        }
        Ok(summary)
    }
}

fn partial_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(".partial");
    path.into()
}
