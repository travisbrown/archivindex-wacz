//! Packaging of WARC files written by the archiver, from single captures to crawl sessions.

use std::io::{Cursor, Read};
use std::path::Path;
use std::thread;

use archivindex_archiver::session::{
    Capture, CaptureProcessor, Crawl, Discovery, Operator, Session,
};
use archivindex_archiver::{Archiver, Config};
use archivindex_cdx::format::cdxj::Fields;
use archivindex_packager::WarcToWacz;
use archivindex_surt::Surt;
use archivindex_test_support::http::{Request, dead_port, response, serve_with};
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::io::read::WaczReader;
use archivindex_wacz::io::read::validate::ValidationOptions;
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::record::extension::NoExtension;
use chrono::SubsecRound as _;
use flate2::read::GzDecoder;
use fluent_uri::Uri;

/// The eight-byte PNG signature followed by a minimal IHDR prefix.
const PNG_PAYLOAD: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01";
const LAST_MODIFIED: &str = "Wed, 01 Jan 2025 00:00:00 GMT";

type Reader = WaczReader<std::io::BufReader<std::fs::File>>;

/// A converted package and the temporary directory containing it and its source WARC.
struct Package {
    _directory: tempfile::TempDir,
    reader: Reader,
}

impl std::ops::Deref for Package {
    type Target = Reader;

    fn deref(&self) -> &Self::Target {
        &self.reader
    }
}

impl std::ops::DerefMut for Package {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reader
    }
}

/// Convert an in-memory WARC to a package, detecting gzip from the input bytes.
fn package(bytes: &[u8]) -> Result<Package, Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let warc = directory.path().join(if bytes.starts_with(&[0x1f, 0x8b]) {
        "capture.warc.gz"
    } else {
        "capture.warc"
    });
    let wacz = directory.path().join("capture.wacz");
    std::fs::write(&warc, bytes)?;
    WarcToWacz::new(&warc, &wacz).run()?;
    let reader = WaczReader::open(&wacz)?;
    Ok(Package {
        _directory: directory,
        reader,
    })
}

/// Convert a WARC file to a package.
fn package_path(path: &Path) -> Result<Package, Box<dyn std::error::Error>> {
    package(&std::fs::read(path)?)
}

/// Archive URLs into memory, requiring every capture to succeed.
fn archive(config: Config, urls: &[String]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let summary = Archiver::new(config)?.archive(urls, Cursor::new(&mut bytes))?;
    assert!(summary.is_complete());
    Ok(bytes)
}

fn gzip_config() -> Config {
    Config {
        gzip_warc: true,
        operator: Some(operator()),
        ..Config::default()
    }
}

/// A gzip configuration that also turns short duplicate fixtures into revisits.
fn gzip_revisit_config() -> Config {
    Config {
        min_revisit_payload_length: 0,
        ..gzip_config()
    }
}

fn operator() -> Operator {
    Operator {
        name: "Test Operator".to_owned(),
        email: Some("operator@example.com".to_owned()),
    }
}

/// A canned HTTP/1.1 response for a request path: a small site whose home page links to two other
/// pages, one of which links back, plus a redirect and an image served as text.
fn respond(path: &str) -> Vec<u8> {
    // Redirects to an address that refuses connections carry the target port in the path.
    if let Some(port) = path.strip_prefix("/dead/") {
        return response(
            "302 Found",
            &[("location", &format!("http://127.0.0.1:{port}/"))],
            "",
        );
    }

    match path {
        "/" => response(
            "200 OK",
            &[("content-type", "text/html")],
            "<html>home links: /about /missing</html>",
        ),
        "/about" => response(
            "200 OK",
            &[("content-type", "text/html")],
            "<html>about links: /</html>",
        ),
        "/redirect" => response(
            "302 Found",
            &[("content-type", "text/plain"), ("location", "/about")],
            "",
        ),
        "/mislabelled" => {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n",
                PNG_PAYLOAD.len()
            )
            .into_bytes();
            response.extend_from_slice(PNG_PAYLOAD);
            response
        }
        _ => response("404 Not Found", &[("content-type", "text/plain")], "gone"),
    }
}

/// Serve the canned site for a fixed number of connections, returning the request paths.
fn serve(connections: usize) -> std::io::Result<(u16, thread::JoinHandle<Vec<String>>)> {
    serve_with(connections, |request| {
        let path = request.path();
        (respond(path), path.to_owned())
    })
}

/// Answer a request for a versioned page, whose `ETag` advances once: an unconditional request or
/// one for a stale version gets the current page in full, while one for the current version gets
/// `304 Not Modified`, carrying the page's validators without a body.
fn respond_versioned(request: &Request, versions: usize) -> Vec<u8> {
    let requested = request
        .header("if-none-match")
        .and_then(|etag| etag.trim_matches('"').parse::<usize>().ok());
    let current = requested.map_or(1, |etag| versions.min(etag + 1));

    if requested == Some(current) {
        format!(
            "HTTP/1.1 304 Not Modified\r\netag: \"{current}\"\r\nlast-modified: {LAST_MODIFIED}\r\n\
             connection: close\r\n\r\n"
        )
        .into_bytes()
    } else {
        response(
            "200 OK",
            &[
                ("content-type", "text/html"),
                ("etag", &format!("\"{current}\"")),
                ("last-modified", LAST_MODIFIED),
            ],
            &format!("<html>version {current}</html>"),
        )
    }
}

/// The space-separated tokens of a payload that name paths (start with `/`), as absolute URLs.
fn extract_links(payload: &[u8], port: u16) -> Vec<String> {
    String::from_utf8_lossy(payload)
        .split_whitespace()
        .filter(|token| token.starts_with('/'))
        .map(|path| {
            let path = path.trim_end_matches("</html>");
            format!("http://127.0.0.1:{port}{path}")
        })
        .collect()
}

/// Inspect the canned site's HTML once for both links and its title.
struct SiteProcessor {
    port: u16,
}

impl CaptureProcessor for SiteProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Discovery {
        let text = std::str::from_utf8(capture.payload).ok();
        let title = text.and_then(|text| {
            text.contains("home")
                .then(|| "Home".to_owned())
                .or_else(|| text.contains("about").then(|| "About".to_owned()))
        });

        Discovery {
            links: extract_links(capture.payload, self.port),
            title,
            ..Discovery::default()
        }
    }
}

/// Ask for the first successful URL a fixed number of additional times.
///
/// The crawl must repeat discoveries for the same URL to be requested again.
struct RecaptureProcessor {
    remaining: usize,
}

impl CaptureProcessor for RecaptureProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Discovery {
        let links = if self.remaining == 0 {
            Vec::new()
        } else {
            self.remaining -= 1;
            vec![capture.url.to_owned()]
        };

        Discovery {
            links,
            ..Discovery::default()
        }
    }
}

/// Assert that a package is internally consistent and conforms to the WACZ specification.
fn assert_conformant(reader: &mut Reader) -> Result<(), Box<dyn std::error::Error>> {
    assert!(reader.verify_fixity()?.is_success());
    let validation = reader.validate(ValidationOptions::all())?;
    assert!(validation.is_conformant(), "{validation:#?}");
    Ok(())
}

/// Assert that an index entry's offset and length frame exactly one complete record within the WARC
/// member (one gzip member, decompressible on its own, when the member is compressed), that it is
/// the response for the entry's URL, and that the record digest covers the framed bytes.
fn assert_frames_one_response(
    member: &[u8],
    fields: &Fields<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let offset = usize::try_from(fields.offset.expect("offset should be indexed"))?;
    let length = usize::try_from(fields.length.expect("length should be indexed"))?;
    let framed = &member[offset..offset + length];

    assert_eq!(
        fields.record_digest.as_deref(),
        Some(Sha256Digest::compute(framed).to_string().as_str())
    );

    let mut record = Vec::new();
    if framed.starts_with(&[0x1f, 0x8b]) {
        GzDecoder::new(framed).read_to_end(&mut record)?;
    } else {
        record.extend_from_slice(framed);
    }

    let records = WarcReader::new(record.as_slice())
        .iter_records::<NoExtension>()
        .records()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].type_name(), "response");
    assert_eq!(
        records[0].target_uri().map(Uri::as_str),
        Some(fields.url.as_ref())
    );
    Ok(())
}

#[test]
fn packages_archived_captures_with_a_random_access_index() -> Result<(), Box<dyn std::error::Error>>
{
    let (port, server) = serve(5)?;
    let urls = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/redirect"),
        format!("http://127.0.0.1:{port}/missing"),
        format!("http://127.0.0.1:{port}/mislabelled"),
    ];
    let bytes = archive(gzip_config(), &urls)?;
    server.join().expect("server thread should not panic");

    let mut reader = package(&bytes)?;
    assert_conformant(&mut reader)?;
    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        ["archive/data.warc.gz"]
    );

    // The first URL is the collection's main page; every hop of a redirect chain is a page.
    let data_package = reader.data_package()?;
    assert_eq!(
        data_package.main_page_url.as_deref(),
        Some(urls[0].as_str())
    );

    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        pages
            .iter()
            .map(|page| page.url.as_ref())
            .collect::<Vec<_>>(),
        [
            urls[0].as_str(),
            urls[1].as_str(),
            &format!("http://127.0.0.1:{port}/about"),
            urls[2].as_str(),
            urls[3].as_str(),
        ]
    );
    assert!(pages.iter().all(|page| page.size.is_none()));
    assert!(
        pages
            .iter()
            .all(|page| page.id.as_deref().is_some_and(|id| id.len() == 24))
    );

    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .records()
        .collect::<Result<Vec<_>, _>>()?;
    let home = &records[2];
    assert_eq!(home.type_name(), "response");
    let mislabelled = records
        .iter()
        .find(|record| {
            record.type_name() == "response"
                && record
                    .target_uri()
                    .is_some_and(|uri| uri.as_str() == urls[3])
        })
        .expect("the mislabelled capture is recorded");
    assert!(
        mislabelled
            .payload()
            .and_then(|payload| payload.identified_payload_type.as_ref())
            .is_some_and(|media_type| media_type.is("image", "png"))
    );

    // Index entries are sorted by SURT key and timestamped to the millisecond.
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(items.len(), 5);
    assert!(items.is_sorted_by_key(|item| item.key.clone()));
    for item in &items {
        assert_eq!(item.key, Surt::from_url(&item.fields.url)?.as_str());
        assert!(item.timestamp.has_milliseconds());
        assert_eq!(item.timestamp.to_string().len(), 17);
        assert_eq!(item.fields.filename.as_deref(), Some("data.warc.gz"));
    }

    let fields = |url: &str| {
        &items
            .iter()
            .find(|item| item.fields.url == url)
            .expect("every capture is indexed")
            .fields
    };
    assert_eq!(
        items
            .iter()
            .find(|item| item.fields.url == urls[0])
            .map(|item| item.timestamp.datetime()),
        Some(home.core().date.date_time().trunc_subsecs(3))
    );
    assert_eq!(fields(&urls[0]).mime.as_deref(), Some("text/html"));
    assert_eq!(fields(&urls[2]).status, Some(404));
    // The index mirrors the declared `Content-Type`, as cdxj-indexer does; the identified type
    // stays on the record.
    assert_eq!(fields(&urls[3]).mime.as_deref(), Some("text/plain"));

    let member = reader.member_bytes("archive/data.warc.gz")?;
    for item in &items {
        assert_frames_one_response(&member, &item.fields)?;
    }

    Ok(())
}

#[test]
fn packages_a_plain_warc_member() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let urls = [format!("http://127.0.0.1:{port}/")];
    let bytes = archive(Config::default(), &urls)?;
    server.join().expect("server thread should not panic");

    let mut reader = package(&bytes)?;
    assert_conformant(&mut reader)?;
    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        ["archive/data.warc"]
    );

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.filename.as_deref(), Some("data.warc"));

    let member = reader.member_bytes("archive/data.warc")?;
    assert!(member.starts_with(b"WARC/1.1\r\n"));
    assert_frames_one_response(&member, &items[0].fields)?;

    Ok(())
}

#[test]
fn packages_a_hop_captured_before_a_failure() -> Result<(), Box<dyn std::error::Error>> {
    let dead_port = dead_port()?;
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/dead/{dead_port}");
    let mut bytes = Vec::new();
    let summary = Archiver::new(gzip_config())?.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");
    assert!(!summary.is_complete());

    // The completed redirect hop is a page and an index entry even though the following request
    // failed.
    let mut reader = package(&bytes)?;
    assert_conformant(&mut reader)?;

    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].url, url);

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.status, Some(302));

    Ok(())
}

#[test]
fn packages_a_crawl_session_with_extra_pages() -> Result<(), Box<dyn std::error::Error>> {
    // The seeds are the home page and a redirect whose final URL is /about; the home page links to
    // /about and /missing, which are crawled as discoveries.
    let (port, server) = serve(5)?;
    let seeds = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/redirect"),
    ];
    let about = format!("http://127.0.0.1:{port}/about");
    let missing = format!("http://127.0.0.1:{port}/missing");

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.warc.gz");
    let summary = Session::new(
        Archiver::new(gzip_revisit_config())?,
        "crawl-2026.08",
        Crawl::seeds(&seeds).processor(SiteProcessor { port }),
        &path,
    )?
    .run()?;
    server.join().expect("server thread should not panic");
    assert!(summary.is_complete());

    let mut reader = package_path(&path)?;
    assert_conformant(&mut reader)?;
    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        ["archive/data.warc.gz"]
    );

    // The session identifier titles the collection, whose main page is the first seed.
    let data_package = reader.data_package()?;
    assert_eq!(data_package.title.as_deref(), Some("crawl-2026.08"));
    assert_eq!(
        data_package.main_page_url.as_deref(),
        Some(seeds[0].as_str())
    );

    // Seed hops are pages, titled from the processor's inspection where it found one.
    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        pages
            .iter()
            .map(|page| (page.url.as_ref(), page.title.as_deref()))
            .collect::<Vec<_>>(),
        [
            (seeds[0].as_str(), Some("Home")),
            (seeds[1].as_str(), None),
            (about.as_str(), Some("About")),
        ]
    );

    // Discovered pages are listed separately.
    let extra_pages = reader
        .page_list("pages/extraPages.jsonl")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        extra_pages
            .iter()
            .map(|page| (page.url.as_ref(), page.title.as_deref()))
            .collect::<Vec<_>>(),
        [(about.as_str(), Some("About")), (missing.as_str(), None)]
    );

    // Every capture (seed hops and discovered pages alike) is indexed; the rediscovered /about
    // repeats the seed hop's payload, so its entry is a revisit.
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(items.len(), 5);
    assert!(
        items
            .iter()
            .all(|item| item.fields.filename.as_deref() == Some("data.warc.gz"))
    );
    assert_eq!(
        items
            .iter()
            .filter(|item| item.fields.mime.as_deref() == Some("warc/revisit"))
            .count(),
        1
    );

    Ok(())
}

#[test]
fn packages_an_identical_payload_revisit() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(2)?;
    let url = format!("http://127.0.0.1:{port}/about");
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("recapture.warc.gz");
    let summary = Session::new(
        Archiver::new(gzip_revisit_config())?,
        "recapture",
        Crawl::seeds([&url])
            .dedupe_discoveries(false)
            .processor(RecaptureProcessor { remaining: 1 }),
        &path,
    )?
    .run()?;
    server.join().expect("server thread should not panic");
    assert!(summary.is_complete());

    // Both captures are indexed under the shared payload digest, the revisit entry marked by the
    // conventional media type and carrying the original's status.
    let mut reader = package_path(&path)?;
    assert_conformant(&mut reader)?;

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(items.len(), 2);
    assert!(items[0].fields.digest.is_some());
    assert_eq!(items[0].fields.digest, items[1].fields.digest);
    assert_eq!(items[0].fields.mime.as_deref(), Some("text/html"));
    assert_eq!(items[1].fields.mime.as_deref(), Some("warc/revisit"));
    assert_eq!(items[1].fields.status, Some(200));
    assert_eq!(
        reader.read_capture(&items[1].fields)?.type_name(),
        "revisit"
    );

    Ok(())
}

#[test]
fn packages_server_not_modified_revisits() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve_with(3, |request| (respond_versioned(request, 2), ()))?;
    let url = format!("http://127.0.0.1:{port}/page");
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("revalidate.warc.gz");
    let summary = Session::new(
        Archiver::new(gzip_config())?,
        "revalidate",
        Crawl::seeds([&url])
            .dedupe_discoveries(false)
            .processor(RecaptureProcessor { remaining: 2 }),
        &path,
    )?
    .run()?;
    server.join().expect("server thread should not panic");
    assert!(summary.is_complete());

    // The first recapture finds the page changed and is stored in full under its new digest; the
    // second is revalidated, so its revisit entry carries its own status and that new digest.
    let mut reader = package_path(&path)?;
    assert_conformant(&mut reader)?;

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(items.len(), 3);
    assert!(items[0].fields.digest.is_some());
    assert_ne!(items[0].fields.digest, items[1].fields.digest);
    assert_eq!(items[1].fields.digest, items[2].fields.digest);
    assert_eq!(
        items
            .iter()
            .map(|item| (item.fields.mime.as_deref(), item.fields.status))
            .collect::<Vec<_>>(),
        [
            (Some("text/html"), Some(200)),
            (Some("text/html"), Some(200)),
            (Some("warc/revisit"), Some(304)),
        ]
    );

    Ok(())
}
