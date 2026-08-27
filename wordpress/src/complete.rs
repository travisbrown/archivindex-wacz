//! Completing missing pages in archived `WordPress` comment collections.

use std::collections::BTreeSet;
use std::io::{BufRead, BufWriter, Write};
use std::path::{Path, PathBuf};

use archivindex_archiver::Archiver;
use archivindex_archiver::capture::ArchiveSummary;
use archivindex_warc::io::read::{self as warc_read, WarcReader};
use archivindex_warc::io::write::{self as warc_write, Compression, WarcWriter};
use archivindex_warc::parse::raw;
use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;

use crate::read::{check_comment_completeness, comment_page, is_comment_endpoint, is_gzip_file};

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
/// capture record that carries `WARC-Warcinfo-ID` is retargeted to that original record. Output
/// compression follows the input archive's actual contents rather than either file extension.
/// When no pages are missing, the output contains only the original `warcinfo` record.
pub fn complete_comments(
    archiver: &Archiver,
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
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
    let requested_urls = paging.map_or_else(Vec::new, |paging| {
        missing_pages.iter().map(|page| paging.url(*page)).collect()
    });

    let capture_directory = tempfile::tempdir()?;
    let capture_path = capture_directory.path().join("completion-captures.warc");
    let archive = if requested_urls.is_empty() {
        None
    } else {
        Some(archiver.archive_to_path(&requested_urls, &capture_path)?)
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

    write_output(
        output,
        source_gzip,
        &warcinfo,
        &warcinfo_id,
        archive.as_ref().map(|_| capture_path.as_path()),
    )?;

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
    for result in reader.iter_records::<NoExtension>() {
        let record = result.map_err(|source| Error::WarcRead {
            path: path.to_owned(),
            source,
        })?;
        let Record::Response { header, body } = record else {
            continue;
        };
        let url = header.target_uri.as_str();
        if !is_comment_endpoint(url)
            || !header
                .payload
                .identified_payload_type
                .as_ref()
                .is_some_and(|media_type| media_type.is("application", "json"))
        {
            continue;
        }
        let Some(response) = archivindex_warc::record::http::ResponseMetadata::parse(&body) else {
            continue;
        };
        if response.status != 200 || comment_page(url).is_none() {
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

fn write_output(
    output: &Path,
    gzip: bool,
    warcinfo: &raw::Record,
    warcinfo_id: &[u8],
    captures: Option<&Path>,
) -> Result<(), Error> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let compression = if gzip {
            Compression::gzip()
        } else {
            Compression::NONE
        };
        let mut writer =
            WarcWriter::new(BufWriter::new(temporary.as_file_mut())).with_compression(compression);
        writer.write(warcinfo)?;

        if let Some(captures) = captures {
            let capture_gzip = is_gzip_file(captures)?;
            if capture_gzip {
                copy_capture_records(
                    WarcReader::from_path_gzip(captures)?,
                    captures,
                    &mut writer,
                    warcinfo_id,
                )?;
            } else {
                copy_capture_records(
                    WarcReader::from_path(captures)?,
                    captures,
                    &mut writer,
                    warcinfo_id,
                )?;
            }
        }
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(output)
        .map_err(|error| error.error)?;

    Ok(())
}

fn copy_capture_records<R: BufRead, W: Write>(
    reader: WarcReader<R>,
    path: &Path,
    writer: &mut WarcWriter<W>,
    warcinfo_id: &[u8],
) -> Result<(), Error> {
    for result in reader.iter_raw_records() {
        let mut record = result.map_err(|source| Error::WarcRead {
            path: path.to_owned(),
            source,
        })?;
        if record
            .header
            .get("WARC-Type")
            .map(trim_ascii)
            .is_some_and(|value| value.eq_ignore_ascii_case(b"warcinfo"))
        {
            continue;
        }
        for (name, value) in &mut record.header.headers {
            if name.eq_ignore_ascii_case("WARC-Warcinfo-ID") {
                *value = [b" ".as_slice(), warcinfo_id].concat();
            }
        }
        writer.write(&record)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use archivindex_archiver::{Archiver, Config};
    use archivindex_warc::io::read::WarcReader;
    use archivindex_warc::io::write::WarcWriter;
    use archivindex_warc::record::Record;
    use archivindex_warc::value::MediaType;
    use chrono::Utc;

    use super::{PagingUrl, complete_comments, trim_ascii, write_output};

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
        let response: Record = Record::response("https://example.com/", Utc::now())?
            .body(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_vec())?;
        let mut response = response.into_raw()?;
        response.header.headers.push((
            "WARC-Warcinfo-ID".to_owned(),
            b" <urn:uuid:generated>".to_vec(),
        ));
        capture_writer.write(&response)?;
        capture_writer.flush()?;

        write_output(&output, false, &source, &source_id, Some(&captures))?;

        let records = WarcReader::from_path(output)?
            .iter_raw_records()
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(records.len(), 2);
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

        Ok(())
    }
}
