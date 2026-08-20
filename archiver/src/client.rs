//! The public archiving client facade and outcome types.

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use archivindex_wacz::cdxj;
use archivindex_wacz::writer::{WaczWriter, WriterConfig};
use archivindex_warc::record::BlockError;
use archivindex_warc::recorder::Recorder;
use archivindex_warc_revisit_index::Index as RevisitIndex;
use chrono::{DateTime, Utc};
use http::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};

use crate::config::Config;

mod capture;
mod collection;
mod pool;
mod warc_fields;
mod warc_mapping;

pub(crate) use capture::Exchange;
pub(crate) use collection::Collection;
use warc_fields::WarcinfoOptions;

const WARC_NAME: &str = "data.warc";
const GZIP_WARC_NAME: &str = "data.warc.gz";

/// An error type for archiving.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The archive could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An exchange could not be completed.
    #[error(transparent)]
    Fetch(#[from] archivindex_warc::recorder::Error),
    /// A URL to be archived could not be parsed.
    #[error(transparent)]
    InvalidUrl(#[from] url::ParseError),
    /// A URL to be archived contains credentials. The displayed URL has them removed.
    #[error("URL contains credentials: {0}")]
    CredentialedUrl(String),
    /// A URL to be archived does not have a host.
    #[error("URL has no host: {0}")]
    MissingHost(String),
    /// A parsed URL cannot be represented by the HTTP request URI grammar.
    #[error("URL is not a valid URI: {url}")]
    InvalidUri {
        /// The URL as requested.
        url: String,
        /// Where the URL departs from the URI grammar.
        #[source]
        source: http::uri::InvalidUri,
    },
    /// The configured `User-Agent` cannot be sent or recorded safely.
    #[error("invalid User-Agent header value: {0:?}")]
    InvalidUserAgent(String),
    /// A session identifier is empty or contains a non-URI-unreserved character.
    #[error("invalid session identifier: {0:?}")]
    InvalidSessionId(String),
    /// A CDXJ search key could not be derived for a URL.
    #[error(transparent)]
    Index(#[from] cdxj::Error),
    /// A revisit index could not be opened, queried, or updated.
    #[error(transparent)]
    RevisitIndex(#[from] archivindex_warc_revisit_index::Error),
    /// A WARC content block could not be attached to its record.
    #[error(transparent)]
    WarcBlock(#[from] BlockError),
    /// A `warc-fields` value could not be written.
    #[error(transparent)]
    WarcFields(#[from] archivindex_warc::record::fields::Error),
    /// Semantic `warc-fields` metadata could not be serialized.
    #[error(transparent)]
    WarcFieldsSerde(archivindex_warc::record::fields::serde::Error),
    /// A WARC record could not be rendered.
    #[error(transparent)]
    WarcRender(#[from] archivindex_warc::record::RenderError),
    /// A WARC record could not be written.
    #[error(transparent)]
    WarcWrite(#[from] archivindex_warc::io::write::Error),
    /// A completed WARC record could not be read for persistent indexing.
    #[error(transparent)]
    WarcRead(#[from] archivindex_warc::io::read::Error),
    /// The WACZ file could not be written.
    #[error(transparent)]
    Wacz(#[from] archivindex_wacz::writer::Error),
}

impl From<archivindex_warc::record::fields::serde::Error> for Error {
    fn from(source: archivindex_warc::record::fields::serde::Error) -> Self {
        match source {
            archivindex_warc::record::fields::serde::Error::Field(source) => {
                Self::WarcFields(source)
            }
            source => Self::WarcFieldsSerde(source),
        }
    }
}

/// The outcome of an archiving run.
#[derive(Debug, Default)]
pub struct ArchiveSummary {
    /// URLs archived successfully, in request order.
    pub captures: Vec<CaptureSummary>,
    /// URLs that could not be captured.
    pub failures: Vec<Failure>,
}

impl ArchiveSummary {
    /// Whether every URL was captured.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// The outcome of capturing one URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSummary {
    /// The requested URL.
    pub url: String,
    /// When capture of the final response began.
    pub date: DateTime<Utc>,
    /// The final response status.
    pub status: u16,
    /// The decoded entity-body length.
    pub size: u64,
    /// The number of redirects followed.
    pub redirects: usize,
}

/// A URL that could not be captured.
#[derive(Debug)]
pub struct Failure {
    /// The requested URL.
    pub url: String,
    /// The capture failure.
    pub error: Error,
}

/// An HTTP client that captures lists of URLs in WACZ files.
///
/// Each URL is fetched synchronously over HTTP/1.1. Redirect hops, wire-format messages, capture
/// metadata, CDXJ entries, and page-list entries are all retained. One-shot lists request every
/// URL unconditionally; only crawl sessions revalidate earlier captures.
#[derive(Clone, Debug)]
pub struct Archiver {
    recorder: Recorder,
    headers: HeaderMap,
    config: Config,
}

impl Archiver {
    /// Create a new archiving client.
    pub fn new(config: Config) -> Result<Self, Error> {
        let user_agent = HeaderValue::from_str(&config.user_agent)
            .map_err(|_| Error::InvalidUserAgent(config.user_agent.clone()))?;
        let mut headers = HeaderMap::with_capacity(2);
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(USER_AGENT, user_agent);

        let mut recorder = Recorder::new()
            .connect_timeout(config.timeout)
            .io_timeout(config.timeout);
        if let Some(length) = config.max_response_length {
            recorder = recorder.max_response_length(length);
        }

        Ok(Self {
            recorder,
            headers,
            config,
        })
    }

    /// Download URLs and write a new WACZ at `path`, refusing to overwrite an existing file.
    pub fn archive_to_path<P: AsRef<Path>, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        path: P,
    ) -> Result<ArchiveSummary, Error> {
        self.archive_into(
            urls,
            WaczWriter::create_with_config(path, self.writer_config())?,
        )
    }

    /// Download URLs and write a WACZ to `writer`.
    pub fn archive<W: Write + Seek, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        writer: W,
    ) -> Result<ArchiveSummary, Error> {
        self.archive_into(urls, WaczWriter::with_config(writer, self.writer_config()))
    }

    /// Start the collection used by a crawl session.
    pub(crate) fn session_collection(
        &self,
        id: &str,
        software: (&str, &str),
        operator: (&str, Option<&str>),
        persistent_index: Option<RevisitIndex>,
    ) -> Result<Collection, Error> {
        let gzip = self.config.gzip_warc;
        let suffix = if gzip { ".warc.gz" } else { ".warc" };

        Collection::new(
            format!("{id}{suffix}"),
            gzip,
            &WarcinfoOptions {
                user_agent: &self.config.user_agent,
                software: Some(software),
                operator: Some(operator),
                session_id: Some(id),
            },
            persistent_index,
        )
    }

    /// Create a WACZ writer using this client's output configuration.
    pub(crate) fn wacz_to_path(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<WaczWriter<BufWriter<File>>, Error> {
        Ok(WaczWriter::create_with_config(path, self.writer_config())?)
    }

    /// The WACZ configuration derived from this client.
    pub(crate) fn writer_config(&self) -> WriterConfig {
        WriterConfig {
            index_format: self.config.index_format,
            ..WriterConfig::default()
        }
    }

    fn archive_into<W: Write + Seek, I: IntoIterator<Item = S>, S: AsRef<str>>(
        &self,
        urls: I,
        wacz: WaczWriter<W>,
    ) -> Result<ArchiveSummary, Error> {
        let gzip = self.config.gzip_warc;
        let warc_name = if gzip { GZIP_WARC_NAME } else { WARC_NAME };
        let mut collection = Collection::new(
            warc_name.to_owned(),
            gzip,
            &WarcinfoOptions::archiver(&self.config.user_agent),
            None,
        )?;

        let concurrency = self.config.concurrency.max(1);
        if concurrency == 1 {
            for url in urls {
                let url = url.as_ref();
                let (exchanges, error) = self.capture(url, None);
                collection.record(url.to_owned(), exchanges, error, None, false, None)?;
            }
        } else {
            self.capture_concurrently(urls, concurrency, &mut collection)?;
        }

        collection.finish(wacz, None)
    }
}
