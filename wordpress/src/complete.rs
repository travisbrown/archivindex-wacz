//! Completing missing pages in archived `WordPress` comment collections.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use archivindex_archiver::Archiver;
use archivindex_archiver::capture::{ArchiveSummary, CaptureControl, CaptureEvent};
use archivindex_warc::io::read::{self as warc_read, WarcReader};
use archivindex_warc::io::write::{self as warc_write, Compression, WarcWriter};
use archivindex_warc::parse::raw;
use archivindex_warc::record as warc_record;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::metadata::{MetadataBody, MetadataField};
use archivindex_warc::value::{DigestError, DigestFormat, LabelledDigest};
use tempfile::{NamedTempFile, TempPath};

use crate::read::{
    check_comment_completeness, comment_page, is_gzip_file, qualifying_comment_capture,
};

/// The result of requesting the comment pages absent from an input archive.
#[derive(Debug)]
pub struct CommentCompletionSummary {
    /// Pages found to be missing before any requests were made.
    pub missing_pages: Vec<usize>,
    /// Exact URLs generated from the paging URL found in the input archive.
    pub requested_urls: Vec<String>,
    /// Result of the HTTP capture, absent when the input had no missing pages.
    pub archive: Option<ArchiveSummary>,
    /// Requested pages that did not produce a qualifying successful JSON response.
    pub uncaptured_pages: Vec<usize>,
}

impl CommentCompletionSummary {
    /// Whether every missing page produced a complete qualifying capture.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.uncaptured_pages.is_empty()
            && self
                .archive
                .as_ref()
                .is_none_or(ArchiveSummary::is_complete)
    }
}

/// A failure while planning, capturing, or writing a comments completion archive.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An ordinary file operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The input WARC could not be interpreted while its coverage was checked.
    #[error(transparent)]
    ReadComments(#[from] crate::read::Error),
    /// A WARC record could not be parsed.
    #[error("invalid WARC file {path}")]
    WarcRead {
        /// The file being read.
        path: PathBuf,
        /// The parsing failure.
        #[source]
        source: warc_read::Error,
    },
    /// A WARC record could not be written.
    #[error("cannot write completion WARC: {0}")]
    WarcWrite(#[from] warc_write::Error),
    /// Generated capture metadata could not be read or updated.
    #[error("invalid generated capture metadata: {0}")]
    CaptureMetadata(#[from] warc_record::fields::Error),
    /// A generated metadata record carried a malformed block digest.
    #[error("invalid generated capture metadata digest: {0}")]
    CaptureMetadataDigest(#[from] DigestError),
    /// A generated metadata record's block digest could not be recomputed.
    #[error("generated capture metadata has an unsupported or ambiguous block digest")]
    UnusableCaptureMetadataDigest,
    /// A generated capture record does not occupy complete gzip members.
    #[error("generated capture WARC record is not independently framed")]
    UnframedCaptureRecord,
    /// The HTTP capture could not be run.
    #[error("cannot capture missing comment pages: {0}")]
    Archive(#[from] archivindex_archiver::Error),
    /// The input starts with something other than a `warcinfo` record.
    #[error("input WARC does not begin with a warcinfo record")]
    MissingWarcinfo,
    /// The source `warcinfo` has no identifier for new records to reference.
    #[error("input WARC's warcinfo record has no WARC-Record-ID")]
    MissingWarcinfoId,
    /// No response advertised how many comment pages exist.
    #[error("input WARC has no valid X-WP-TotalPages value")]
    MissingPageTotal,
    /// Missing pages were known, but no captured URL could be reused as their template.
    #[error("input WARC has no usable WordPress comments paging URL")]
    MissingPagingUrl,
    /// The destination already exists and was left untouched.
    #[error("output already exists: {}", .0.display())]
    OutputExists(PathBuf),
}

/// Request missing comment pages and atomically write their captures to a new WARC.
///
/// The output begins with the input archive's first `warcinfo` record. Every subsequently written
/// capture record that carries `WARC-Warcinfo-ID` is retargeted to that original record. Metadata
/// for a missing page after page one names the preceding page URL as its `via`. Output
/// compression is selected from the output filename: names ending in `.gz` are gzip-compressed and
/// all other names are plain WARC files. When captures are in progress, each completed capture is
/// flushed to `<output>.partial`. When no pages are missing, the output contains only the original
/// `warcinfo` record.
pub fn complete_comments(
    archiver: &Archiver,
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<CommentCompletionSummary, Error> {
    complete_comments_with_delay(archiver, input, output, Duration::ZERO)
}

/// Request missing comment pages, spacing capture starts by `request_delay`.
///
/// This is the command-oriented variant of [`complete_comments`].
pub fn complete_comments_with_delay(
    archiver: &Archiver,
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    request_delay: Duration,
) -> Result<CommentCompletionSummary, Error> {
    let input = input.as_ref();
    let output = output.as_ref();
    if output.try_exists()? {
        return Err(Error::OutputExists(output.to_owned()));
    }

    let coverage = check_comment_completeness(input)?;
    let _total_pages = coverage.total_pages.ok_or(Error::MissingPageTotal)?;
    let missing_pages = coverage.missing_pages().collect::<Vec<_>>();
    let source_gzip = is_gzip_file(input)?;
    let (warcinfo, warcinfo_id) = source_warcinfo(input, source_gzip)?;

    let paging = if missing_pages.is_empty() {
        None
    } else {
        Some(source_paging_url(input, source_gzip)?.ok_or(Error::MissingPagingUrl)?)
    };
    let requested_urls = paging.as_ref().map_or_else(Vec::new, |paging| {
        missing_pages.iter().map(|page| paging.url(*page)).collect()
    });
    let via_urls = paging.as_ref().map_or_else(BTreeMap::new, |paging| {
        completion_via_urls(paging, &missing_pages)
    });

    let mut completion = CompletionOutput::new(output, &warcinfo)?;
    let capture_directory = tempfile::tempdir()?;
    let capture_path = capture_directory.path().join("completion-captures.warc");
    let archive = if requested_urls.is_empty() {
        None
    } else {
        let partial_path = partial_path(&capture_path);
        let mut copied = 0;
        let mut capture_gzip = None;
        let mut copy_error = None;
        let mut delay = RequestDelay::new(request_delay);
        let archive = archiver.archive_to_path_with_events(
            &requested_urls,
            &capture_path,
            &mut |event: CaptureEvent<'_>| {
                delay.before(&event);
                if matches!(event, CaptureEvent::Written { .. }) {
                    let gzip = match capture_gzip {
                        Some(gzip) => gzip,
                        None => match is_gzip_file(&partial_path) {
                            Ok(gzip) => {
                                capture_gzip = Some(gzip);
                                gzip
                            }
                            Err(error) => {
                                copy_error = Some(error.into());
                                return CaptureControl::Cancel;
                            }
                        },
                    };
                    if let Err(error) = copy_new_capture_records(
                        &partial_path,
                        gzip,
                        &mut copied,
                        &mut completion.writer,
                        &warcinfo_id,
                        &via_urls,
                    ) {
                        copy_error = Some(error);
                        return CaptureControl::Cancel;
                    }
                }
                CaptureControl::Continue
            },
        )?;
        if let Some(error) = copy_error {
            return Err(error);
        }
        Some(archive)
    };
    let captured_pages = if archive.is_some() {
        check_comment_completeness(&capture_path)?
            .captured_pages
            .into_iter()
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    let uncaptured_pages = missing_pages
        .iter()
        .copied()
        .filter(|page| !captured_pages.contains(page))
        .collect();

    completion.publish(output)?;

    Ok(CommentCompletionSummary {
        missing_pages,
        requested_urls,
        archive,
        uncaptured_pages,
    })
}

/// The portions of an archived URL surrounding its decimal page value.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PagingUrl {
    prefix: String,
    suffix: String,
}

impl PagingUrl {
    /// Recover the URL's `page` parameter without parsing and re-encoding any other byte.
    fn explicit(url: &str) -> Option<Self> {
        let query = url.find('?')? + 1;
        let end = url[query..]
            .find('#')
            .map_or(url.len(), |offset| query + offset);
        let mut start = query;
        let mut found = None;

        while start <= end {
            let segment_end = url[start..end]
                .find('&')
                .map_or(end, |offset| start + offset);
            let segment = &url[start..segment_end];
            if let Some(value_offset) = segment.strip_prefix("page=") {
                if found.is_some()
                    || value_offset.is_empty()
                    || !value_offset.bytes().all(|byte| byte.is_ascii_digit())
                    || value_offset
                        .parse::<usize>()
                        .ok()
                        .is_none_or(|page| page == 0)
                {
                    return None;
                }
                let value_start = start + "page=".len();
                found = Some(Self {
                    prefix: url[..value_start].to_owned(),
                    suffix: url[segment_end..].to_owned(),
                });
            }
            if segment_end == end {
                break;
            }
            start = segment_end + 1;
        }

        found
    }

    /// Add an explicit page parameter to a page-one URL that omitted it.
    fn from_implicit_first_page(url: &str) -> Self {
        let fragment = url.find('#').unwrap_or(url.len());
        let before_fragment = &url[..fragment];
        let separator = match before_fragment.split_once('?') {
            None => "?",
            Some((_, "")) => "",
            Some(_) if before_fragment.ends_with('&') => "",
            Some(_) => "&",
        };

        Self {
            prefix: format!("{before_fragment}{separator}page="),
            suffix: url[fragment..].to_owned(),
        }
    }

    fn url(&self, page: usize) -> String {
        format!("{}{page}{}", self.prefix, self.suffix)
    }
}

fn completion_via_urls(paging: &PagingUrl, missing_pages: &[usize]) -> BTreeMap<String, String> {
    missing_pages
        .iter()
        .copied()
        .filter(|page| *page > 1)
        .map(|page| (paging.url(page), paging.url(page - 1)))
        .collect()
}

struct RequestDelay {
    duration: Duration,
    requested: bool,
}

impl RequestDelay {
    const fn new(duration: Duration) -> Self {
        Self {
            duration,
            requested: false,
        }
    }

    fn before(&mut self, event: &CaptureEvent<'_>) {
        if matches!(event, CaptureEvent::Started { .. }) {
            if self.requested {
                std::thread::sleep(self.duration);
            }
            self.requested = true;
        }
    }
}

fn partial_path(output: &Path) -> PathBuf {
    let mut path = output.as_os_str().to_os_string();
    path.push(".partial");
    path.into()
}

fn source_paging_url(path: &Path, gzip: bool) -> Result<Option<PagingUrl>, Error> {
    if gzip {
        find_paging_url(WarcReader::from_path_gzip(path)?, path)
    } else {
        find_paging_url(WarcReader::from_path(path)?, path)
    }
}

fn find_paging_url<R: BufRead>(
    reader: WarcReader<R>,
    path: &Path,
) -> Result<Option<PagingUrl>, Error> {
    let mut implicit = None;
    for result in reader.iter_records::<NoExtension>().records() {
        let record = result.map_err(|source| Error::WarcRead {
            path: path.to_owned(),
            source,
        })?;
        let Some((url, _response)) = qualifying_comment_capture(&record) else {
            continue;
        };
        if comment_page(url).is_none() {
            continue;
        }

        if let Some(paging) = PagingUrl::explicit(url) {
            return Ok(Some(paging));
        }
        implicit.get_or_insert_with(|| PagingUrl::from_implicit_first_page(url));
    }

    Ok(implicit)
}

fn source_warcinfo(path: &Path, gzip: bool) -> Result<(raw::Record, Vec<u8>), Error> {
    if gzip {
        first_warcinfo(WarcReader::from_path_gzip(path)?, path)
    } else {
        first_warcinfo(WarcReader::from_path(path)?, path)
    }
}

fn first_warcinfo<R: BufRead>(
    reader: WarcReader<R>,
    path: &Path,
) -> Result<(raw::Record, Vec<u8>), Error> {
    let first = reader
        .iter_raw_records()
        .records()
        .next()
        .ok_or(Error::MissingWarcinfo)?
        .map_err(|source| Error::WarcRead {
            path: path.to_owned(),
            source,
        })?;
    let record_type = first.header.get("WARC-Type").map(trim_ascii);
    if !record_type.is_some_and(|value| value.eq_ignore_ascii_case(b"warcinfo")) {
        return Err(Error::MissingWarcinfo);
    }
    let warcinfo_id = first
        .header
        .get("WARC-Record-ID")
        .map(trim_ascii)
        .filter(|value| !value.is_empty())
        .ok_or(Error::MissingWarcinfoId)?
        .to_vec();

    Ok((first, warcinfo_id))
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

struct CompletionOutput {
    writer: WarcWriter<BufWriter<File>>,
    path: TempPath,
}

impl CompletionOutput {
    fn new(output: &Path, warcinfo: &raw::Record) -> Result<Self, Error> {
        let path = std::path::absolute(partial_path(output))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let path = TempPath::try_from_path(&path)?;
        let compression = if output
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gz"))
        {
            Compression::gzip()
        } else {
            Compression::NONE
        };
        let mut writer = WarcWriter::new(BufWriter::new(file)).with_compression(compression);
        writer.write(warcinfo)?;
        writer.flush()?;

        Ok(Self { writer, path })
    }

    fn publish(self, output: &Path) -> Result<(), Error> {
        let Self { writer, path } = self;
        let file = writer
            .finish()
            .map_err(std::io::IntoInnerError::into_error)?;
        file.sync_all()?;
        NamedTempFile::from_parts(file, path)
            .persist_noclobber(output)
            .map_err(|error| error.error)?;

        Ok(())
    }
}

fn copy_new_capture_records(
    path: &Path,
    gzip: bool,
    offset: &mut u64,
    writer: &mut WarcWriter<BufWriter<File>>,
    warcinfo_id: &[u8],
    via_urls: &BTreeMap<String, String>,
) -> Result<(), Error> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(*offset))?;
    let reader = BufReader::new(file);
    let consumed = if gzip {
        copy_capture_records(
            WarcReader::from_gzip(reader),
            path,
            writer,
            warcinfo_id,
            via_urls,
        )?
    } else {
        copy_capture_records(WarcReader::new(reader), path, writer, warcinfo_id, via_urls)?
    };
    *offset += consumed;
    writer.flush()?;

    Ok(())
}

fn copy_capture_records<R: BufRead, W: Write>(
    reader: WarcReader<R>,
    path: &Path,
    writer: &mut WarcWriter<W>,
    warcinfo_id: &[u8],
    via_urls: &BTreeMap<String, String>,
) -> Result<u64, Error> {
    let mut records = reader.iter_raw_records();
    let mut consumed = 0;
    for located in &mut records {
        let frame = located.frame().ok_or(Error::UnframedCaptureRecord)?;
        let mut record = located.value.map_err(|source| Error::WarcRead {
            path: path.to_owned(),
            source,
        })?;
        consumed = frame.offset + frame.length;
        if !record
            .header
            .get("WARC-Type")
            .map(trim_ascii)
            .is_some_and(|value| value.eq_ignore_ascii_case(b"warcinfo"))
        {
            for (name, value) in &mut record.header.headers {
                if name.eq_ignore_ascii_case("WARC-Warcinfo-ID") {
                    *value = [b" ".as_slice(), warcinfo_id].concat();
                }
            }
            add_completion_via(&mut record, via_urls)?;
            writer.write(&record)?;
        }
    }

    Ok(consumed)
}

/// Add the predecessor link to metadata describing a requested missing page.
fn add_completion_via(
    record: &mut raw::Record,
    via_urls: &BTreeMap<String, String>,
) -> Result<(), Error> {
    if !record
        .header
        .get("WARC-Type")
        .map(trim_ascii)
        .is_some_and(|value| value.eq_ignore_ascii_case(b"metadata"))
    {
        return Ok(());
    }
    let Some(via) = record
        .header
        .get("WARC-Target-URI")
        .map(trim_ascii)
        .and_then(|target| std::str::from_utf8(target).ok())
        .and_then(|target| via_urls.get(target))
    else {
        return Ok(());
    };

    let mut fields = MetadataBody::parse(&record.body)?;
    fields.set(MetadataField::Via, via)?;
    record.body = fields.to_string().into_bytes();

    for (name, value) in &mut record.header.headers {
        if name.eq_ignore_ascii_case("Content-Length") {
            *value = format!(" {}", record.body.len()).into_bytes();
        } else if name.eq_ignore_ascii_case("WARC-Block-Digest") {
            let current = LabelledDigest::parse(trim_ascii(value))?;
            let format = DigestFormat {
                algorithm: current
                    .algorithm()
                    .ok_or(Error::UnusableCaptureMetadataDigest)?,
                encoding: current
                    .encoding()
                    .ok_or(Error::UnusableCaptureMetadataDigest)?,
            };
            let digest = format
                .algorithm
                .digest(&record.body)
                .ok_or(Error::UnusableCaptureMetadataDigest)?;
            *value = format!(" {}", LabelledDigest::from_digest_in(format, &digest)).into_bytes();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use archivindex_archiver::capture::CaptureEvent;
    use archivindex_archiver::{Archiver, Config};
    use archivindex_warc::io::read::WarcReader;
    use archivindex_warc::io::write::WarcWriter;
    use archivindex_warc::record::Record;
    use archivindex_warc::value::MediaType;
    use chrono::Utc;

    use super::{
        CompletionOutput, MetadataBody, PagingUrl, RequestDelay, complete_comments,
        completion_via_urls, copy_new_capture_records, is_gzip_file, partial_path, trim_ascii,
    };

    fn assert_completion_via(
        records: &[archivindex_warc::parse::raw::Record],
        target: &str,
        expected_via: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let metadata = records
            .iter()
            .find(|record| {
                record
                    .header
                    .get("WARC-Type")
                    .map(trim_ascii)
                    .is_some_and(|kind| kind.eq_ignore_ascii_case(b"metadata"))
                    && record.header.get("WARC-Target-URI").map(trim_ascii)
                        == Some(target.as_bytes())
            })
            .expect("metadata describing the completed page");
        let fields = MetadataBody::parse(&metadata.body)?;
        assert_eq!(fields.via(), Some(expected_via));

        if let Some(block_digest) = metadata
            .header
            .get("WARC-Block-Digest")
            .map(trim_ascii)
            .map(archivindex_warc::value::LabelledDigest::parse)
            .transpose()?
        {
            let computed = block_digest
                .algorithm()
                .and_then(|algorithm| algorithm.digest(&metadata.body))
                .expect("the archiver uses a supported digest algorithm");
            assert_eq!(block_digest.decoded().as_deref(), Some(&*computed));
        }

        Ok(())
    }

    #[test]
    fn explicit_page_replacement_preserves_every_surrounding_byte() {
        let url = "https://example.com/wp-json/wp/v2/comments?before=2026-08-20T00%3A00%3A00Z&orderby=id&page=003&per_page=100#part";
        let paging = PagingUrl::explicit(url).expect("an explicit page parameter");

        assert_eq!(
            paging.url(12),
            "https://example.com/wp-json/wp/v2/comments?before=2026-08-20T00%3A00%3A00Z&orderby=id&page=12&per_page=100#part"
        );
    }

    #[test]
    fn page_one_without_a_parameter_gets_one_without_reencoding() {
        let paging = PagingUrl::from_implicit_first_page(
            "https://example.com/wp-json/wp/v2/comments?before=a%2Fb&order=asc#part",
        );

        assert_eq!(
            paging.url(2),
            "https://example.com/wp-json/wp/v2/comments?before=a%2Fb&order=asc&page=2#part"
        );
    }

    #[test]
    fn completion_vias_skip_page_one_and_name_each_predecessor() {
        let paging = PagingUrl::explicit("https://example.com/comments?page=3")
            .expect("an explicit page parameter");

        assert_eq!(
            completion_via_urls(&paging, &[1, 2, 4]),
            BTreeMap::from([
                (
                    "https://example.com/comments?page=2".to_owned(),
                    "https://example.com/comments?page=1".to_owned(),
                ),
                (
                    "https://example.com/comments?page=4".to_owned(),
                    "https://example.com/comments?page=3".to_owned(),
                ),
            ])
        );
    }

    #[test]
    fn completion_partial_is_staged_beside_the_output() {
        let output = Path::new("archive/comments.warc.gz");
        let partial = partial_path(output);

        assert_eq!(partial, Path::new("archive/comments.warc.gz.partial"));
        assert_eq!(partial.parent(), output.parent());
    }

    #[test]
    fn completion_compression_follows_the_output_filename() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let source: Record = Record::warcinfo(Utc::now()).build();
        let source = source.into_raw()?;

        for (name, expected_gzip) in [("output.warc", false), ("output.warc.gz", true)] {
            let output = directory.path().join(name);
            CompletionOutput::new(&output, &source)?.publish(&output)?;

            assert_eq!(is_gzip_file(&output)?, expected_gzip, "{name}");
        }

        Ok(())
    }

    #[test]
    fn completion_waits_between_request_starts() {
        let duration = Duration::from_millis(20);
        let mut delay = RequestDelay::new(duration);
        let started = CaptureEvent::Started {
            url: "https://example.com/",
            attempt: 1,
        };

        delay.before(&started);
        let before_second = Instant::now();
        delay.before(&started);

        assert!(before_second.elapsed() >= duration);
    }

    #[test]
    fn output_replaces_generated_warcinfo_with_the_source_record()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let captures = directory.path().join("captures.warc");
        let output = directory.path().join("output.warc");
        let source: Record = Record::warcinfo(Utc::now())
            .filename("source.warc")?
            .build();
        let source = source.into_raw()?;
        let source_id = source
            .header
            .get("WARC-Record-ID")
            .map(trim_ascii)
            .expect("a generated record identifier")
            .to_vec();

        let mut capture_writer = WarcWriter::new(std::fs::File::create(&captures)?);
        let generated: Record = Record::warcinfo(Utc::now()).build();
        capture_writer.write(&generated.into_raw()?)?;
        let response: Record = Record::response("https://example.com/page=2", Utc::now())?
            .body(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_vec())?;
        let metadata: Record = Record::metadata(Utc::now())
            .target_uri(response.target_uri().expect("response target").clone())
            .concurrent_to(response.core().record_id.clone())
            .fetch_time_ms(Duration::from_millis(1))
            .build();
        let mut response = response.into_raw()?;
        response.header.headers.push((
            "WARC-Warcinfo-ID".to_owned(),
            b" <urn:uuid:generated>".to_vec(),
        ));
        capture_writer.write(&response)?;
        capture_writer.write(&metadata.into_raw()?)?;
        capture_writer.flush()?;

        let via_urls = BTreeMap::from([(
            "https://example.com/page=2".to_owned(),
            "https://example.com/page=1".to_owned(),
        )]);
        let mut completion = CompletionOutput::new(&output, &source)?;
        let mut offset = 0;
        copy_new_capture_records(
            &captures,
            false,
            &mut offset,
            &mut completion.writer,
            &source_id,
            &via_urls,
        )?;
        let partial_records = WarcReader::from_path(partial_path(&output))?
            .iter_raw_records()
            .records()
            .collect::<Result<Vec<_>, _>>()?;
        assert_completion_via(
            &partial_records,
            "https://example.com/page=2",
            "https://example.com/page=1",
        )?;
        completion.publish(&output)?;

        let records = WarcReader::from_path(output)?
            .iter_raw_records()
            .records()
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(records.len(), 3);
        assert_eq!(records[0], source);
        assert_eq!(
            records[1].header.get("WARC-Warcinfo-ID").map(trim_ascii),
            Some(source_id.as_slice())
        );

        Ok(())
    }

    #[test]
    fn completion_reuses_the_exact_url_and_original_warcinfo()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let authority = listener.local_addr()?;
        let expected_target = "/wp-json/wp/v2/comments?before=a%2Fb&orderby=id&page=2&per_page=100";
        let server = thread::spawn(move || -> Result<String, std::io::Error> {
            let (mut stream, _) = listener.accept()?;
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request_line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                  x-wp-total: 0\r\nx-wp-totalpages: 3\r\ncontent-length: 2\r\n\
                  connection: close\r\n\r\n[]",
            )?;

            Ok(request_line)
        });

        let directory = tempfile::tempdir()?;
        let input = directory.path().join("input.warc");
        let output = directory.path().join("output.warc");
        let mut writer = WarcWriter::new(std::fs::File::create(&input)?);
        let warcinfo: Record = Record::warcinfo(Utc::now())
            .filename("original.warc")?
            .build();
        let warcinfo = warcinfo.into_raw()?;
        let source_id = warcinfo
            .header
            .get("WARC-Record-ID")
            .map(trim_ascii)
            .expect("a generated record identifier")
            .to_vec();
        writer.write(&warcinfo)?;
        for page in [1, 3] {
            let url = format!(
                "http://{authority}/wp-json/wp/v2/comments?\
                 before=a%2Fb&orderby=id&page={page}&per_page=100"
            );
            let response = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                x-wp-totalpages: 3\r\ncontent-length: 2\r\n\r\n[]";
            let record: Record = Record::response(&url, Utc::now())?
                .identified_payload_type(MediaType::parse(b"application/json")?)
                .body(response.to_vec())?;
            writer.write(&record.into_raw()?)?;
        }
        writer.flush()?;

        let archiver = Archiver::new(Config::default())?;
        let summary = complete_comments(&archiver, &input, &output)?;
        let request_line = server.join().expect("the test server thread")?;
        assert!(!partial_path(&output).exists());

        assert_eq!(summary.missing_pages, [2]);
        assert_eq!(
            summary.requested_urls,
            [format!("http://{authority}{expected_target}")]
        );
        assert!(summary.uncaptured_pages.is_empty());
        assert!(summary.is_complete());
        assert_eq!(request_line, format!("GET {expected_target} HTTP/1.1"));

        let records = WarcReader::from_path(&output)?
            .iter_raw_records()
            .records()
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(records.first(), Some(&warcinfo));
        assert_eq!(
            records
                .iter()
                .filter(|record| {
                    record
                        .header
                        .get("WARC-Type")
                        .map(trim_ascii)
                        .is_some_and(|kind| kind.eq_ignore_ascii_case(b"warcinfo"))
                })
                .count(),
            1
        );
        for record in &records[1..] {
            assert_eq!(
                record.header.get("WARC-Warcinfo-ID").map(trim_ascii),
                Some(source_id.as_slice())
            );
        }
        let requested_url = format!("http://{authority}{expected_target}");
        let predecessor_url = format!(
            "http://{authority}/wp-json/wp/v2/comments?\
             before=a%2Fb&orderby=id&page=1&per_page=100"
        );
        assert_completion_via(&records, &requested_url, &predecessor_url)?;

        Ok(())
    }
}
