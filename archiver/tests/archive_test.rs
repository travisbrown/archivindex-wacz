//! End-to-end archiving tests against a local HTTP server serving canned responses.

use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::thread;
use std::time::Duration;

use archivindex_archiver::client::{Archiver, Error};
use archivindex_archiver::config::Config;
use archivindex_packager::WarcToWacz;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::io::read::WaczReader;
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::metadata::MetadataField;
use archivindex_warc::record::header::truncated_type::TruncatedType;
use archivindex_warc::record::{FieldsBlock, Record};
use archivindex_warc::value::DigestAlgorithm;
use archivindex_warc::value::WarcDatePrecision;
use archivindex_warc::version::WarcVersion;
use chrono::SubsecRound as _;
use flate2::read::GzDecoder;
use fluent_uri::Uri;

struct PackagedWarc {
    _directory: tempfile::TempDir,
    reader: WaczReader<std::io::BufReader<std::fs::File>>,
}

impl std::ops::Deref for PackagedWarc {
    type Target = WaczReader<std::io::BufReader<std::fs::File>>;

    fn deref(&self) -> &Self::Target {
        &self.reader
    }
}

impl std::ops::DerefMut for PackagedWarc {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reader
    }
}

fn package_bytes(bytes: &[u8]) -> Result<PackagedWarc, Box<dyn std::error::Error>> {
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
    Ok(PackagedWarc {
        _directory: directory,
        reader,
    })
}

fn package_path(path: &std::path::Path) -> Result<PackagedWarc, Box<dyn std::error::Error>> {
    package_bytes(&std::fs::read(path)?)
}

/// The eight-byte PNG signature followed by a minimal IHDR prefix.
const PNG_PAYLOAD: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01";

/// A simple HTTP/1.1 response with a text body.
fn plain(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{headers}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A canned HTTP/1.1 response for a request path.
fn respond(path: &str) -> Vec<u8> {
    // Redirects to an address that refuses connections carry the target port in the path.
    if let Some(port) = path.strip_prefix("/dead/") {
        return plain(
            "302 Found",
            &format!("location: http://127.0.0.1:{port}/"),
            "",
        );
    }

    match path {
        "/" => plain("200 OK", "content-type: text/html", "<html>home</html>"),
        "/redirect" => plain(
            "302 Found",
            "content-type: text/plain\r\nlocation: /target",
            "",
        ),
        "/target" => plain(
            "200 OK",
            "content-type: text/plain; charset=utf-8",
            "arrived",
        ),
        "/loop" => plain(
            "302 Found",
            "content-type: text/plain\r\nlocation: /loop",
            "",
        ),
        "/bad-target" => plain(
            "302 Found",
            "content-type: text/plain\r\nlocation: ftp://127.0.0.1/file",
            "",
        ),
        "/multiple-choices" => plain(
            "300 Multiple Choices",
            "content-type: text/plain\r\nlocation: /target",
            "list",
        ),
        "/nonstandard" => plain("520 Origin Error", "content-type: text/plain", "err"),
        "/cookies" => plain(
            "200 OK",
            "content-type: text/plain\r\nset-cookie: a=1\r\nset-cookie: b=2",
            "ok",
        ),
        "/slow" => {
            thread::sleep(Duration::from_millis(500));
            plain("200 OK", "content-type: text/plain", "late")
        }
        // A chunked body, so that de-chunking is exercised against a real wire exchange.
        "/chunked" => b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                        transfer-encoding: chunked\r\nconnection: close\r\n\r\n\
                        6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n"
            .to_vec(),
        // A bodiless response whose headers describe the entity that was not sent.
        "/not-modified" => b"HTTP/1.1 304 Not Modified\r\netag: \"abc\"\r\n\
                             content-length: 42\r\nlocation: /target\r\n\
                             connection: close\r\n\r\n"
            .to_vec(),
        "/binary" => {
            let body = (0u8..=255).collect::<Vec<_>>();
            let mut response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            response
        }
        "/mislabelled" => {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                PNG_PAYLOAD.len()
            )
            .into_bytes();
            response.extend_from_slice(PNG_PAYLOAD);
            response
        }
        _ => plain("404 Not Found", "content-type: text/plain", "gone"),
    }
}

/// Serve the given number of connections on an ephemeral local port, returning the raw bytes of
/// each request as received.
fn serve(connections: usize) -> std::io::Result<(u16, thread::JoinHandle<Vec<Vec<u8>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let handle = thread::spawn(move || {
        let mut requests = Vec::with_capacity(connections);

        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return requests;
            };

            let mut head = Vec::new();
            let mut buffer = [0; 4096];

            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => head.extend_from_slice(&buffer[..read]),
                }
            }

            let request = String::from_utf8_lossy(&head);
            let path = request.split(' ').nth(1).unwrap_or("/").to_owned();
            let _ = stream.write_all(&respond(&path));
            requests.push(head);
        }

        requests
    });

    Ok((port, handle))
}

#[test]
fn archive_and_read_back() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(4)?;
    let urls = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/redirect"),
        format!("http://127.0.0.1:{port}/missing"),
    ];

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(&urls, Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(
        summary
            .captures
            .iter()
            .map(|capture| (capture.status, capture.redirects))
            .collect::<Vec<_>>(),
        vec![(200, 0), (200, 1), (404, 0)]
    );

    let mut reader = package_bytes(&bytes)?;

    assert!(reader.verify_fixity()?.is_success());

    let package = reader.data_package()?;

    assert_eq!(package.main_page_url.as_deref(), Some(urls[0].as_str()));

    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        pages
            .iter()
            .map(|page| page.url.as_ref())
            .collect::<Vec<_>>(),
        urls.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(pages[1].size, Some("arrived".len() as u64));
    // Every page receives a synthetic id of the default 24-character length.
    assert!(
        pages
            .iter()
            .all(|page| page.id.as_deref().is_some_and(|id| id.len() == 24))
    );

    // One warcinfo record plus a request, response, and metadata record for each of the four
    // exchanges.
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 13);

    for record in &records {
        assert_eq!(record.version(), WarcVersion::V1_1);
    }

    // The warcinfo record carries its recommended fields and none of its prohibited ones.
    let Record::Warcinfo {
        header: warcinfo,
        body: warcinfo_body,
    } = &records[0]
    else {
        panic!("the first record should be a warcinfo record");
    };

    assert_eq!(
        warcinfo
            .filename
            .as_ref()
            .and_then(archivindex_warc::value::Text::to_str),
        Some("data.warc.gz")
    );
    assert!(records[0].target_uri().is_none());

    // The body is read back as typed `application/warc-fields`, so its content type follows from
    // the block rather than being declared separately.
    let FieldsBlock::Fields(warcinfo_body) = warcinfo_body else {
        panic!("the warcinfo body should parse as warc-fields");
    };

    assert!(
        warcinfo_body
            .software()
            .is_some_and(|software| software.starts_with("archivindex-archiver/"))
    );
    assert!(
        warcinfo
            .core
            .content_type
            .as_ref()
            .is_some_and(|content_type| content_type.is("application", "warc-fields"))
    );

    // Each exchange is written as its request, then the response naming it, then the metadata
    // record naming the response.
    let request = &records[1];
    let response = &records[2];
    let metadata = &records[3];

    assert_eq!(response.type_name(), "response");
    assert_eq!(
        response.target_uri().map(Uri::as_str),
        Some(urls[0].as_str())
    );
    assert!(response.body_bytes().ends_with(b"<html>home</html>"));

    assert!(
        response
            .core()
            .content_type
            .as_ref()
            .is_some_and(|content_type| content_type.is("application", "http"))
    );
    assert!(
        response
            .payload()
            .and_then(|payload| payload.payload_digest.as_ref())
            .is_some_and(|digest| *digest.algorithm() == DigestAlgorithm::Sha256)
    );
    assert!(
        response
            .payload()
            .and_then(|payload| payload.identified_payload_type.as_ref())
            .is_some_and(|media_type| media_type.is("text", "html"))
    );
    assert_eq!(response.ip_address(), Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert_eq!(response.warcinfo_id(), Some(&warcinfo.core.record_id));

    assert_eq!(request.type_name(), "request");
    assert!(request.concurrent_to().is_empty());
    assert_eq!(response.concurrent_to(), [request.core().record_id.clone()]);
    assert!(
        request
            .core()
            .content_type
            .as_ref()
            .is_some_and(|content_type| content_type.is("application", "http"))
    );

    // The metadata record describing the response reports how long the response took to collect.
    assert_eq!(metadata.type_name(), "metadata");
    assert_eq!(
        metadata.concurrent_to(),
        [response.core().record_id.clone()]
    );
    assert_eq!(
        metadata.target_uri().map(Uri::as_str),
        Some(urls[0].as_str())
    );

    let Record::Metadata {
        body: FieldsBlock::Fields(metadata_body),
        ..
    } = metadata
    else {
        panic!("the metadata body should parse as warc-fields");
    };

    assert_eq!(metadata_body.len(), 2);
    assert!(metadata_body.fetch_time_ms().is_some());
    assert_eq!(
        metadata_body.get(&MetadataField::Other("pageurl".to_owned())),
        Some(urls[0].as_str())
    );

    // Records of one capture event share a single WARC-Date, recorded at exactly microsecond
    // precision: the archiver stores every date with six fractional digits.
    assert_eq!(response.core().date, request.core().date);
    assert_eq!(response.core().date, metadata.core().date);
    assert_eq!(
        response.core().date.precision(),
        WarcDatePrecision::Fraction(6)
    );

    let request_message = String::from_utf8(request.body_bytes().into_owned())?;

    assert!(request_message.starts_with("GET / HTTP/1.1\r\n"));
    assert!(request_message.contains(&format!("host: 127.0.0.1:{port}\r\n")));
    assert!(request_message.contains("user-agent: archivindex-archiver/"));

    // The redirect chain is recorded hop by hop, three records to a hop.
    assert_eq!(
        records[4].target_uri().map(Uri::as_str),
        Some(urls[1].as_str())
    );
    assert_eq!(
        records[7].target_uri().map(Uri::as_str),
        Some(format!("http://127.0.0.1:{port}/target").as_str())
    );

    // The written form is checked at the raw layer, since URI angle brackets are applied when a
    // record is rendered rather than being part of its value: WARC 1.1 brackets record identifiers
    // and leaves target URIs bare.
    let raw_records = reader
        .warc("archive/data.warc.gz")?
        .iter_raw_records()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(raw_records.len(), 13);

    for record in &raw_records {
        // Raw field values are returned exactly as they were read, white space included.
        let record_id = record
            .header
            .get("WARC-Record-ID")
            .map(<[u8]>::trim_ascii)
            .expect("every record should carry an identifier");

        assert!(record_id.starts_with(b"<") && record_id.ends_with(b">"));
        assert!(
            record
                .header
                .get("WARC-Target-URI")
                .map(<[u8]>::trim_ascii)
                .is_none_or(|target| !target.starts_with(b"<"))
        );
    }

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 4);
    assert!(items.is_sorted_by_key(|item| item.key.clone()));
    assert!(items.iter().all(|item| item.timestamp.has_milliseconds()));
    assert!(
        items
            .iter()
            .all(|item| item.timestamp.to_string().len() == 17)
    );
    assert_eq!(
        items
            .iter()
            .find(|item| item.fields.url == urls[0])
            .map(|item| item.timestamp.datetime()),
        Some(response.core().date.date_time().trunc_subsecs(3))
    );
    assert_eq!(
        items
            .iter()
            .find(|item| item.fields.url == urls[0])
            .and_then(|item| item.fields.mime.as_deref()),
        Some("text/html")
    );
    assert_eq!(
        items
            .iter()
            .find(|item| item.fields.url == urls[2])
            .and_then(|item| item.fields.status),
        Some(404)
    );

    // Every index entry's offset and length must frame exactly one complete gzip member,
    // decompressible on its own, holding one parseable response record.
    let warc_bytes = &bytes;

    for item in &items {
        assert_eq!(item.fields.filename.as_deref(), Some("data.warc.gz"));

        let offset = usize::try_from(item.fields.offset.expect("offset should be indexed"))?;
        let length = usize::try_from(item.fields.length.expect("length should be indexed"))?;

        // The record digest covers exactly the framed range of stored (compressed) bytes.
        assert_eq!(
            item.fields.record_digest,
            Some(Sha256Digest::compute(&warc_bytes[offset..offset + length]))
        );

        let mut decompressed = Vec::new();
        GzDecoder::new(&warc_bytes[offset..offset + length]).read_to_end(&mut decompressed)?;

        let framed = WarcReader::new(decompressed.as_slice())
            .iter_records::<NoExtension>()
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(framed.len(), 1);
        assert_eq!(framed[0].type_name(), "response");
        assert_eq!(
            framed[0].target_uri().map(Uri::as_str),
            Some(item.fields.url.as_ref())
        );
    }

    Ok(())
}

#[test]
fn archive_with_plain_warc_member() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config {
        gzip_warc: false,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = package_bytes(&bytes)?;

    assert!(reader.verify_fixity()?.is_success());

    let records = reader
        .warc("archive/data.warc")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 4);

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.filename.as_deref(), Some("data.warc"));

    // The offset and length frame the uncompressed response record directly.
    let warc_bytes = &bytes;

    let offset = usize::try_from(items[0].fields.offset.expect("offset should be indexed"))?;
    let length = usize::try_from(items[0].fields.length.expect("length should be indexed"))?;

    // The record digest covers exactly the framed range of stored (plain) bytes.
    assert_eq!(
        items[0].fields.record_digest,
        Some(Sha256Digest::compute(&warc_bytes[offset..offset + length]))
    );

    let framed = WarcReader::new(&warc_bytes[offset..offset + length])
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(framed.len(), 1);
    assert_eq!(framed[0].type_name(), "response");

    Ok(())
}

#[test]
fn archive_records_unreachable_urls_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    // Bind and immediately drop a listener so that the port refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;

    assert!(!summary.is_complete());
    assert!(summary.captures.is_empty());
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].url, url);

    // The collection is still written and internally consistent.
    let mut reader = package_bytes(&bytes)?;

    assert!(reader.verify_fixity()?.is_success());
    assert_eq!(reader.pages()?.count(), 0);

    Ok(())
}

#[test]
fn archive_stops_following_at_the_redirect_limit() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/redirect");

    let archiver = Archiver::new(Config {
        max_redirects: 0,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 302);
    assert_eq!(summary.captures[0].redirects, 0);

    let mut reader = package_bytes(&bytes)?;
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, cdxj::search_key(&url)?);

    Ok(())
}

#[test]
fn archive_to_path_refuses_an_existing_output() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.warc.gz");
    std::fs::write(&path, b"existing")?;

    let archiver = Archiver::new(Config::default())?;

    assert!(archiver.archive_to_path::<_, _, &str>([], &path).is_err());

    Ok(())
}

#[test]
fn archive_to_path_writes_a_collection() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.warc.gz");

    let archiver = Archiver::new(Config::default())?;
    let summary = archiver.archive_to_path([format!("http://127.0.0.1:{port}/")], &path)?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = package_path(&path)?;

    assert!(reader.verify_fixity()?.is_success());

    Ok(())
}

#[test]
fn recorded_request_matches_the_wire_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config {
        user_agent: "fidelity-test/1.0".into(),
        gzip_warc: false,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = package_bytes(&bytes)?;
    let records = reader
        .warc("archive/data.warc")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    // The request record replays the received request byte for byte.
    assert_eq!(records[1].type_name(), "request");
    assert_eq!(records[1].body_bytes().as_ref(), requests[0].as_slice());

    Ok(())
}

#[test]
fn archive_records_chunked_responses_verbatim() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/chunked");

    let archiver = Archiver::new(Config {
        gzip_warc: false,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    // The reported size describes the payload (the de-chunked entity body), even though the record
    // stores the chunk framing as it crossed the wire.
    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].size, "hello world".len() as u64);

    let mut reader = package_bytes(&bytes)?;
    let records = reader
        .warc("archive/data.warc")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    let message = String::from_utf8(records[2].body_bytes().into_owned())?;

    assert!(message.contains("transfer-encoding: chunked\r\n"));
    assert!(message.ends_with("6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n"));

    // The payload digest likewise covers the entity body, with the chunk framing removed.
    assert_eq!(
        records[2]
            .payload()
            .and_then(|payload| payload.payload_digest.as_ref())
            .map(ToString::to_string),
        Some(Sha256Digest::compute(b"hello world").to_string())
    );

    Ok(())
}

#[test]
fn archive_rejects_credentialed_urls_without_leaking_the_secret()
-> Result<(), Box<dyn std::error::Error>> {
    // Nothing listens on the port: the URL is rejected before any request is made.
    let url = "http://user:secret@127.0.0.1:9/";

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([url], Cursor::new(&mut bytes))?;

    assert!(!summary.is_complete());
    assert!(matches!(
        summary.failures[0].error,
        Error::CredentialedUrl(_)
    ));
    assert!(!summary.failures[0].error.to_string().contains("secret"));
    assert!(!summary.failures[0].error.to_string().contains("user"));

    Ok(())
}

#[test]
fn archive_records_hops_captured_before_a_failure() -> Result<(), Box<dyn std::error::Error>> {
    // Bind and immediately drop a listener so that the redirect target refuses connections.
    let dead_port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/dead/{dead_port}");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(!summary.is_complete());
    assert!(summary.captures.is_empty());
    assert_eq!(summary.failures[0].url, url);

    let mut reader = package_bytes(&bytes)?;

    // The URL failed, so it gets no page entry, but the hop captured before the failure is still
    // recorded and indexed.
    assert_eq!(reader.pages()?.count(), 0);

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.status, Some(302));

    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 4);
    assert_eq!(records[2].type_name(), "response");

    Ok(())
}

#[test]
fn archive_treats_multiple_choices_and_not_modified_as_final()
-> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(2)?;
    let urls = [
        format!("http://127.0.0.1:{port}/multiple-choices"),
        format!("http://127.0.0.1:{port}/not-modified"),
    ];

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(&urls, Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    // Neither response is followed, despite the redirection-class status and location header.
    assert!(summary.is_complete());
    assert_eq!(
        summary
            .captures
            .iter()
            .map(|capture| (capture.status, capture.redirects))
            .collect::<Vec<_>>(),
        vec![(300, 0), (304, 0)]
    );

    let mut reader = package_bytes(&bytes)?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    // The bodiless 304 keeps its headers exactly as received, with no fabricated zero
    // content-length replacing the one describing the entity that was not sent.
    let message = String::from_utf8(records[5].body_bytes().into_owned())?;

    assert!(message.starts_with("HTTP/1.1 304 Not Modified\r\n"));
    assert!(message.contains("content-length: 42\r\n"));
    assert!(!message.contains("content-length: 0"));
    assert!(message.ends_with("\r\n\r\n"));

    Ok(())
}

#[test]
fn archive_preserves_a_nonstandard_reason_phrase() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/nonstandard");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 520);

    let mut reader = package_bytes(&bytes)?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    // The origin's own reason phrase is stored, not the status code's canonical one.
    let message = String::from_utf8(records[2].body_bytes().into_owned())?;

    assert!(message.starts_with("HTTP/1.1 520 Origin Error\r\n"));

    Ok(())
}

#[test]
fn archive_preserves_repeated_set_cookie_headers() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/cookies");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = package_bytes(&bytes)?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    let message = String::from_utf8(records[2].body_bytes().into_owned())?;

    assert!(message.contains("set-cookie: a=1\r\n"));
    assert!(message.contains("set-cookie: b=2\r\n"));

    Ok(())
}

#[test]
fn archive_records_binary_bodies() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/binary");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].size, 256);

    let body = (0u8..=255).collect::<Vec<_>>();
    let mut reader = package_bytes(&bytes)?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert!(records[2].body_bytes().ends_with(&body));
    // The payload digest of a record and the digest recorded in the index share an encoding.
    assert_eq!(
        records[2]
            .payload()
            .and_then(|payload| payload.payload_digest.as_ref())
            .map(ToString::to_string),
        Some(Sha256Digest::compute(&body).to_string())
    );

    Ok(())
}

#[test]
fn archive_identifies_payload_types_from_content() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/mislabelled");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let mut reader = package_bytes(&bytes)?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;
    let response = &records[2];

    // Identification examines the PNG signature instead of copying the declared `text/plain`.
    assert!(
        response
            .body_bytes()
            .starts_with(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain")
    );
    assert!(
        response
            .payload()
            .and_then(|payload| payload.identified_payload_type.as_ref())
            .is_some_and(|media_type| media_type.is("image", "png"))
    );

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items[0].fields.mime.as_deref(), Some("image/png"));

    Ok(())
}

#[test]
fn archive_records_timeouts_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    // The slow endpoint stalls before sending anything, so the timeout occurs while the response
    // head is awaited and fails the capture (a timeout mid-body would truncate it instead).
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/slow");

    let archiver = Archiver::new(Config {
        timeout: Duration::from_millis(100),
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(!summary.is_complete());
    assert!(matches!(summary.failures[0].error, Error::Fetch(_)));

    Ok(())
}

#[test]
fn archive_truncates_responses_at_the_configured_limit() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    // The limit cuts five bytes off the canned response, partway into its body.
    let full = respond("/");
    let limit = full.len() as u64 - 5;

    let archiver = Archiver::new(Config {
        max_response_length: Some(limit),
        gzip_warc: false,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    // A response cut short by the limit is a capture, not a failure, and the reported size
    // describes the payload bytes actually stored.
    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 200);
    assert_eq!(
        summary.captures[0].size,
        "<html>home</html>".len() as u64 - 5
    );

    let mut reader = package_bytes(&bytes)?;
    let records = reader
        .warc("archive/data.warc")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    // The response record holds exactly the bytes received up to the limit and declares why it was
    // truncated; the request and metadata records are unaffected.
    assert_eq!(records[2].core().truncated, Some(TruncatedType::Length),);
    assert_eq!(records[2].body_bytes().as_ref(), &full[..limit as usize]);
    assert_eq!(records[1].core().truncated, None);
    assert_eq!(records.len(), 4);

    Ok(())
}

#[test]
fn archive_stops_following_a_redirect_cycle() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(3)?;
    let url = format!("http://127.0.0.1:{port}/loop");

    let archiver = Archiver::new(Config {
        max_redirects: 2,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 302);
    assert_eq!(summary.captures[0].redirects, 2);

    let mut reader = package_bytes(&bytes)?;
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 3);

    Ok(())
}

#[test]
fn archive_records_an_unusable_redirect_target_as_final() -> Result<(), Box<dyn std::error::Error>>
{
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/bad-target");

    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 302);
    assert_eq!(summary.captures[0].redirects, 0);

    Ok(())
}

#[test]
fn archive_records_urls_without_a_host_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    let archiver = Archiver::new(Config::default())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(["data:text/plain,hi"], Cursor::new(&mut bytes))?;

    assert!(!summary.is_complete());
    assert!(matches!(summary.failures[0].error, Error::MissingHost(_)));

    Ok(())
}

#[test]
fn new_rejects_an_invalid_user_agent() {
    let result = Archiver::new(Config {
        user_agent: "bad\r\nagent".into(),
        ..Config::default()
    });

    assert!(matches!(result, Err(Error::InvalidUserAgent(_))));
}

#[test]
fn archive_concurrently_preserves_input_order() -> Result<(), Box<dyn std::error::Error>> {
    let paths = [
        "/",
        "/target",
        "/missing",
        "/cookies",
        "/",
        "/nonstandard",
        "/target",
        "/",
    ];
    let (port, server) = serve(paths.len())?;
    let urls = paths
        .iter()
        .map(|path| format!("http://127.0.0.1:{port}{path}"))
        .collect::<Vec<_>>();

    let archiver = Archiver::new(Config {
        concurrency: 4,
        ..Config::default()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(&urls, Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(
        summary
            .captures
            .iter()
            .map(|capture| capture.url.as_str())
            .collect::<Vec<_>>(),
        urls.iter().map(String::as_str).collect::<Vec<_>>()
    );

    let mut reader = package_bytes(&bytes)?;

    assert!(reader.verify_fixity()?.is_success());

    // Page entries follow input order, exactly as in a sequential run.
    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        pages
            .iter()
            .map(|page| page.url.as_ref())
            .collect::<Vec<_>>(),
        urls.iter().map(String::as_str).collect::<Vec<_>>()
    );

    Ok(())
}
