//! End-to-end archiving tests against a local HTTP server serving canned responses.

use std::io::{BufReader, Cursor};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

mod support;

use support::{plain, records, request_header, request_path, serve_with, sha256};

use archivindex_archiver::capture::{CaptureControl, CaptureEvent};
use archivindex_archiver::{Archiver, Config, CookieError, Error};
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::record::header::truncated_type::TruncatedType;
use archivindex_warc::record::{FieldsBlock, Record};
use archivindex_warc::value::Algorithm;
use archivindex_warc::value::WarcDatePrecision;
use archivindex_warc::version::WarcVersion;
use data_encoding::BASE64;
use flate2::bufread::MultiGzDecoder;
use fluent_uri::Uri;

fn gzip_config() -> Config {
    Config {
        gzip_warc: true,
        ..Config::default()
    }
}

/// The eight-byte PNG signature followed by a minimal IHDR prefix.
const PNG_PAYLOAD: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01";

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

    // Canned responses are chosen by path alone, so a query string never changes them.
    match path.split('?').next().unwrap_or(path) {
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
    serve_with(connections, |head| {
        (respond(request_path(head)), head.as_bytes().to_vec())
    })
}

#[test]
fn archive_and_read_back() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(4)?;
    let urls = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/redirect"),
        format!("http://127.0.0.1:{port}/missing"),
    ];

    let archiver = Archiver::new(gzip_config())?;
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

    // One warcinfo record plus a request, response, and metadata record for each of the four
    // exchanges.
    let records = records(&bytes)?;

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
            .is_some_and(|digest| digest.algorithm() == Some(Algorithm::Sha256))
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

    assert_eq!(metadata_body.len(), 1);
    assert!(metadata_body.fetch_time_ms().is_some());

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
    let raw_records = WarcReader::new(BufReader::new(MultiGzDecoder::new(bytes.as_slice())))
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

    Ok(())
}

#[test]
fn archive_solves_and_retains_sucuri_cookie_challenges() -> Result<(), Box<dyn std::error::Error>> {
    let script = "v='cookie-value';\
        document.cookie='sucuri_cloudproxy_uuid_test=' + v + \
        ';path=/;max-age=86400;SameSite=Lax'; location.reload();";
    let encoded = BASE64.encode(script.as_bytes());
    let challenge =
        format!("<html><script>var sucuri_cloudproxy_js='',S='{encoded}';</script></html>");
    let attempt = AtomicUsize::new(0);
    let (port, server) = serve_with(3, move |head| {
        let response = if attempt.fetch_add(1, Ordering::Relaxed) == 0 {
            plain(
                "307 Temporary Redirect",
                "content-type: text/html\r\nx-sucuri-id: 12005",
                &challenge,
            )
        } else {
            plain("200 OK", "content-type: text/plain", "accepted")
        };
        (
            response,
            (
                request_path(head).to_owned(),
                request_header(head, "cookie"),
            ),
        )
    })?;
    let urls = [
        format!("http://127.0.0.1:{port}/first"),
        format!("http://127.0.0.1:{port}/second"),
    ];
    let mut output = Vec::new();

    let summary = Archiver::new(gzip_config())?.archive(&urls, Cursor::new(&mut output))?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures.len(), 2);
    // The challenge page and the response it guarded are both captured.
    assert_eq!(summary.captures[0].redirects, 1);
    assert_eq!(summary.captures[1].redirects, 0);
    assert_eq!(
        requests,
        [
            ("/first".to_owned(), None),
            (
                "/first".to_owned(),
                Some("sucuri_cloudproxy_uuid_test=cookie-value".to_owned())
            ),
            (
                "/second".to_owned(),
                Some("sucuri_cloudproxy_uuid_test=cookie-value".to_owned())
            )
        ]
    );

    Ok(())
}

#[test]
fn archive_solves_simply_clearance_challenges_behind_a_proxy()
-> Result<(), Box<dyn std::error::Error>> {
    let token = "021c7f24e8c1ed8c4472a22aa9b441b223a08cb15fa889293574500e190960dc";
    let challenge = format!(
        "<html><script>var T=\"{token}\",TS=\"1787470051\",D=4;\
         x.open(\"POST\",\"/.sc-verify/\");</script></html>"
    );
    let attempt = AtomicUsize::new(0);
    let (port, server) = serve_with(4, move |head| {
        let response = match attempt.fetch_add(1, Ordering::Relaxed) {
            0 => plain(
                "454 Request blocked",
                "content-type: text/html\r\nserver: cloudflare",
                &challenge,
            ),
            1 => plain(
                "200 OK",
                "content-type: application/json",
                r#"{"ok":true,"cookie":"clearance-value"}"#,
            ),
            _ => plain("200 OK", "content-type: text/plain", "accepted"),
        };
        (
            response,
            (
                head.lines().next().unwrap_or_default().to_owned(),
                request_header(head, "cookie"),
            ),
        )
    })?;
    let urls = [
        format!("http://127.0.0.1:{port}/first"),
        format!("http://127.0.0.1:{port}/second"),
    ];
    let mut output = Vec::new();

    let summary = Archiver::new(gzip_config())?.archive(&urls, Cursor::new(&mut output))?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures.len(), 2);
    // The challenge page, the proof of work, and the response it guarded are all captured.
    assert_eq!(summary.captures[0].redirects, 2);
    assert_eq!(summary.captures[1].redirects, 0);
    assert!(requests[0].0.starts_with("GET /first HTTP/1.1"));
    assert!(requests[1].0.starts_with("POST /.sc-verify/ HTTP/1.1"));
    assert_eq!(
        requests[2].1.as_deref(),
        Some("sc_clearance=clearance-value")
    );
    assert_eq!(
        requests[3].1.as_deref(),
        Some("sc_clearance=clearance-value")
    );

    // The submitted proof of work is recorded as sent, nonce included.
    let records = records(&output)?;
    let proof = records
        .iter()
        .find_map(|record| match record {
            Record::Request { body, .. } => {
                let body = String::from_utf8_lossy(body);
                body.contains("POST /.sc-verify/")
                    .then(|| body.into_owned())
            }
            _ => None,
        })
        .expect("the proof of work should be recorded");
    assert!(proof.contains("ts=1787470051&nonce="));
    assert!(proof.contains(&format!("&token={token}")));

    Ok(())
}

#[test]
fn archive_solves_and_retains_varnish_proof_of_work_challenges()
-> Result<(), Box<dyn std::error::Error>> {
    let nonce = "83462578e314e3b20855f1cb32d30a09";
    let trace_nonce = "e71e658fa0f38f0361551e676842c933";
    let issued_at = "1787485140";
    let challenge = format!(
        "<script>window.POW_CHALLENGE_DATA={{\
         challenge_nonce:'{nonce}',challenge_hmac:'22d6f9feb179b6b7e9616ede',\
         difficulty:'1',difficulty_char:'b',issued_at:'{issued_at}',\
         cookie_duration:'3600',cookie_domain:'127.0.0.1'}};</script>"
    );
    let attempt = AtomicUsize::new(0);
    let (port, server) = serve_with(3, move |head| {
        let response = if attempt.fetch_add(1, Ordering::Relaxed) == 0 {
            plain(
                "202 Verifying",
                &format!(
                    "content-type: text/html\r\nserver: Varnish\r\n\
                     set-cookie: pow_trace={trace_nonce}|{issued_at}; path=/; Secure"
                ),
                &challenge,
            )
        } else {
            plain("200 OK", "content-type: text/plain", "accepted")
        };
        (
            response,
            (
                request_path(head).to_owned(),
                request_header(head, "cookie"),
            ),
        )
    })?;
    let urls = [
        format!("http://127.0.0.1:{port}/first"),
        format!("http://127.0.0.1:{port}/second"),
    ];
    let mut output = Vec::new();

    let summary = Archiver::new(gzip_config())?.archive(&urls, Cursor::new(&mut output))?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures.len(), 2);
    assert_eq!(summary.captures[0].redirects, 1);
    assert_eq!(summary.captures[1].redirects, 0);
    assert_eq!(requests[0], ("/first".to_owned(), None));
    for (path, cookie) in &requests[1..] {
        let cookie = cookie.as_deref().expect("the proof-of-work cookies");
        assert!(matches!(path.as_str(), "/first" | "/second"));
        assert!(cookie.starts_with(&format!(
            "pow_trace={trace_nonce}|{issued_at}; pow_bypass={nonce}|{issued_at}|"
        )));
        assert!(cookie.ends_with("|22d6f9feb179b6b7e9616ede"));
    }

    Ok(())
}

#[test]
fn archive_captures_an_unrecognized_challenge_as_the_response_it_is()
-> Result<(), Box<dyn std::error::Error>> {
    // A Sucuri challenge whose script sets a cookie under an unexpected name is not answered.
    let script = "document.cookie='other=value;path=/'; location.reload();";
    let encoded = BASE64.encode(script.as_bytes());
    let challenge = format!("<html><script>var sucuri_cloudproxy_js='{encoded}';</script></html>");
    let (port, server) = serve_with(1, move |head| {
        (
            plain(
                "307 Temporary Redirect",
                "content-type: text/html\r\nx-sucuri-id: 12005",
                &challenge,
            ),
            request_header(head, "cookie"),
        )
    })?;
    let mut output = Vec::new();

    let summary = Archiver::new(gzip_config())?.archive(
        [format!("http://127.0.0.1:{port}/first")],
        Cursor::new(&mut output),
    )?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 307);
    assert_eq!(summary.captures[0].redirects, 0);
    assert_eq!(requests, [None]);

    Ok(())
}

#[test]
fn event_sink_can_cancel_and_finalize_a_partial_archive() -> Result<(), Box<dyn std::error::Error>>
{
    let (port, server) = serve(1)?;
    let urls = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/missing"),
    ];
    let archiver = Archiver::new(Config {
        concurrency: 1,
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let mut events = Vec::new();
    let summary = {
        let mut sink = |event: CaptureEvent<'_>| {
            events.push(match event {
                CaptureEvent::Started { .. } => "started",
                CaptureEvent::Captured { .. } => "captured",
                CaptureEvent::Written { .. } => "written",
                CaptureEvent::Retrying { .. } => "retrying",
                CaptureEvent::Failed { .. } => "failed",
            });
            if matches!(event, CaptureEvent::Written { .. }) {
                CaptureControl::Cancel
            } else {
                CaptureControl::Continue
            }
        };
        archiver.archive_with_events(&urls, Cursor::new(&mut bytes), &mut sink)?
    };
    server.join().expect("server thread should not panic");

    assert!(summary.cancelled);
    assert!(!summary.is_complete());
    assert_eq!(summary.captures.len(), 1);
    assert_eq!(events, ["started", "captured", "written"]);
    // The partial archive is a complete WARC: its warcinfo record and the one exchange.
    assert_eq!(records(&bytes)?.len(), 4);

    Ok(())
}

#[test]
fn event_sink_can_cancel_before_the_first_dispatch() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(0)?;
    let urls = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/missing"),
    ];
    // Two workers, so the cancellation lands in the pool's initial dispatch loop.
    let archiver = Archiver::new(Config {
        concurrency: 2,
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let mut events = Vec::new();
    let summary = {
        let mut sink = |event: CaptureEvent<'_>| {
            events.push(match event {
                CaptureEvent::Started { .. } => "started",
                CaptureEvent::Captured { .. } => "captured",
                CaptureEvent::Written { .. } => "written",
                CaptureEvent::Retrying { .. } => "retrying",
                CaptureEvent::Failed { .. } => "failed",
            });
            CaptureControl::Cancel
        };
        archiver.archive_with_events(&urls, Cursor::new(&mut bytes), &mut sink)?
    };
    server.join().expect("server thread should not panic");

    assert!(summary.cancelled);
    assert!(summary.captures.is_empty());
    assert_eq!(events, ["started"]);

    Ok(())
}

#[test]
fn archive_writes_a_plain_warc_when_gzip_is_off() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config {
        gzip_warc: false,
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    // The WARC opens with its first record rather than a gzip member.
    assert!(bytes.starts_with(b"WARC/1.1\r\n"));
    assert_eq!(records(&bytes)?.len(), 4);

    Ok(())
}

#[test]
fn archive_records_unreachable_urls_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    // Bind and immediately drop a listener so that the port refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(gzip_config())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;

    assert!(!summary.is_complete());
    assert!(summary.captures.is_empty());
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].url, url);

    // The WARC is still written, holding only its warcinfo record.
    assert_eq!(records(&bytes)?.len(), 1);

    Ok(())
}

#[test]
fn archive_stops_following_at_the_redirect_limit() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/redirect");

    let archiver = Archiver::new(Config {
        max_redirects: 0,
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 302);
    assert_eq!(summary.captures[0].redirects, 0);

    // Only the redirect itself is recorded: one exchange after the warcinfo record.
    assert_eq!(records(&bytes)?.len(), 4);

    Ok(())
}

#[test]
fn archive_to_path_refuses_an_existing_output() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.warc.gz");
    std::fs::write(&path, b"existing")?;

    let archiver = Archiver::new(gzip_config())?;

    assert!(archiver.archive_to_path::<_, _, &str>([], &path).is_err());

    Ok(())
}

#[test]
fn archive_to_path_refuses_an_existing_partial() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.warc.gz");
    let partial_path = directory.path().join("test.warc.gz.partial");
    std::fs::write(&partial_path, b"existing partial")?;

    let archiver = Archiver::new(gzip_config())?;

    assert!(archiver.archive_to_path::<_, _, &str>([], &path).is_err());
    assert_eq!(std::fs::read(partial_path)?, b"existing partial");

    Ok(())
}

#[test]
fn archive_to_path_writes_a_collection() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.warc.gz");
    let partial_path = directory.path().join("test.warc.gz.partial");

    let archiver = Archiver::new(gzip_config())?;
    let mut saw_partial = false;
    let mut events = |event: CaptureEvent<'_>| {
        if matches!(event, CaptureEvent::Started { .. }) {
            saw_partial = true;
            assert!(partial_path.exists());
            assert!(std::fs::metadata(&partial_path).is_ok_and(|metadata| metadata.len() > 0));
            assert!(!path.exists());
        }
        CaptureControl::Continue
    };
    let summary = archiver.archive_to_path_with_events(
        [format!("http://127.0.0.1:{port}/")],
        &path,
        &mut events,
    )?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert!(saw_partial);
    assert!(!partial_path.exists());

    assert_eq!(records(&std::fs::read(&path)?)?.len(), 4);

    Ok(())
}

#[test]
fn recorded_request_matches_the_wire_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    let archiver = Archiver::new(Config {
        user_agent: "fidelity-test/1.0".into(),
        gzip_warc: false,
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let records = records(&bytes)?;

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
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    // The reported size describes the payload (the de-chunked entity body), even though the record
    // stores the chunk framing as it crossed the wire.
    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].size, "hello world".len() as u64);

    let records = records(&bytes)?;

    let message = String::from_utf8(records[2].body_bytes().into_owned())?;

    assert!(message.contains("transfer-encoding: chunked\r\n"));
    assert!(message.ends_with("6\r\nhello \r\n5\r\nworld\r\n0\r\n\r\n"));

    // The payload digest likewise covers the entity body, with the chunk framing removed.
    assert_eq!(
        records[2]
            .payload()
            .and_then(|payload| payload.payload_digest.as_ref()),
        Some(&sha256(b"hello world"))
    );

    Ok(())
}

#[test]
fn archive_rejects_credentialed_urls_without_leaking_the_secret()
-> Result<(), Box<dyn std::error::Error>> {
    // Nothing listens on the port: the URL is rejected before any request is made.
    let url = "http://user:secret@127.0.0.1:9/";

    let archiver = Archiver::new(gzip_config())?;
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

    let archiver = Archiver::new(gzip_config())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(!summary.is_complete());
    assert!(summary.captures.is_empty());
    assert_eq!(summary.failures[0].url, url);

    // The completed redirect hop is recorded even though the following request failed.
    let records = records(&bytes)?;

    assert_eq!(records.len(), 4);
    assert_eq!(records[2].type_name(), "response");
    assert_eq!(records[2].target_uri().map(Uri::as_str), Some(url.as_str()));
    assert!(
        records[2]
            .body_bytes()
            .starts_with(b"HTTP/1.1 302 Found\r\n")
    );

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

    let archiver = Archiver::new(gzip_config())?;
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

    let records = records(&bytes)?;

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

    let archiver = Archiver::new(gzip_config())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 520);

    let records = records(&bytes)?;

    // The origin's own reason phrase is stored, not the status code's canonical one.
    let message = String::from_utf8(records[2].body_bytes().into_owned())?;

    assert!(message.starts_with("HTTP/1.1 520 Origin Error\r\n"));

    Ok(())
}

#[test]
fn archive_preserves_repeated_set_cookie_headers() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/cookies");

    let archiver = Archiver::new(gzip_config())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let records = records(&bytes)?;

    let message = String::from_utf8(records[2].body_bytes().into_owned())?;

    assert!(message.contains("set-cookie: a=1\r\n"));
    assert!(message.contains("set-cookie: b=2\r\n"));

    Ok(())
}

#[test]
fn archive_records_binary_bodies() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/binary");

    let archiver = Archiver::new(gzip_config())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].size, 256);

    let body = (0u8..=255).collect::<Vec<_>>();
    let records = records(&bytes)?;

    assert!(records[2].body_bytes().ends_with(&body));
    // The payload digest of a record and the digest recorded in the index share an encoding.
    assert_eq!(
        records[2]
            .payload()
            .and_then(|payload| payload.payload_digest.as_ref()),
        Some(&sha256(&body))
    );

    Ok(())
}

#[test]
fn archive_identifies_payload_types_from_content() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/mislabelled");

    let archiver = Archiver::new(gzip_config())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let records = records(&bytes)?;
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
        ..gzip_config()
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
        ..gzip_config()
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

    let records = records(&bytes)?;

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
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.captures[0].status, 302);
    assert_eq!(summary.captures[0].redirects, 2);

    // Three hops, each a request, response, and metadata record after the warcinfo record.
    assert_eq!(records(&bytes)?.len(), 10);

    Ok(())
}

#[test]
fn archive_records_an_unusable_redirect_target_as_final() -> Result<(), Box<dyn std::error::Error>>
{
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/bad-target");

    let archiver = Archiver::new(gzip_config())?;
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
    let archiver = Archiver::new(gzip_config())?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(["data:text/plain,hi"], Cursor::new(&mut bytes))?;

    assert!(!summary.is_complete());
    assert!(matches!(summary.failures[0].error, Error::MissingHost(_)));

    Ok(())
}

#[test]
fn supplied_cookie_is_scoped_to_its_host_and_recorded() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve_with(1, |head| {
        (
            plain("200 OK", "content-type: text/plain", "accepted"),
            request_header(head, "cookie"),
        )
    })?;
    let url = format!("http://127.0.0.1:{port}/");
    let archiver = Archiver::new(Config {
        gzip_warc: false,
        ..gzip_config()
    })?
    .cookie_for(&url, "session=clearance")?;
    let mut output = Vec::new();

    let summary = archiver.archive([&url], Cursor::new(&mut output))?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(requests, [Some("session=clearance".to_owned())]);
    assert!(String::from_utf8_lossy(&output).contains("cookie: session=clearance"));

    Ok(())
}

#[test]
fn supplied_cookie_is_withheld_from_other_hosts() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve_with(1, |head| {
        (
            plain("200 OK", "content-type: text/plain", "accepted"),
            request_header(head, "cookie"),
        )
    })?;
    let archiver = Archiver::new(gzip_config())?
        .cookie_for("http://elsewhere.example/", "session=clearance")?;
    let mut output = Vec::new();

    archiver.archive(
        [format!("http://127.0.0.1:{port}/")],
        Cursor::new(&mut output),
    )?;
    let requests = server.join().expect("server thread should not panic");

    assert_eq!(requests, [None]);

    Ok(())
}

#[test]
fn supplied_cookie_rejects_header_injection() -> Result<(), Box<dyn std::error::Error>> {
    let result = Archiver::new(gzip_config())?
        .cookie_for("https://example.com/", "safe=yes\r\nx-injected: true");

    assert!(matches!(result, Err(CookieError::InvalidCookie(_))));

    Ok(())
}

#[test]
fn new_rejects_an_invalid_user_agent() {
    let result = Archiver::new(Config {
        user_agent: "bad\r\nagent".into(),
        ..gzip_config()
    });

    assert!(result.is_err());
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
        ..gzip_config()
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

    // Response records follow input order, exactly as in a sequential run.
    let records = records(&bytes)?;

    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.type_name(), "response" | "revisit"))
            .map(|record| record.target_uri().map(Uri::as_str))
            .collect::<Vec<_>>(),
        urls.iter()
            .map(|url| Some(url.as_str()))
            .collect::<Vec<_>>()
    );

    Ok(())
}

#[test]
fn archive_encodes_url_characters_the_uri_grammar_rejects() -> Result<(), Box<dyn std::error::Error>>
{
    let (port, server) = serve(1)?;
    // A WHATWG URL serializes `|` unencoded, which the URI grammar does not allow.
    let url = format!("http://127.0.0.1:{port}/target?x=1|2");

    let archiver = Archiver::new(Config {
        gzip_warc: false,
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive([&url], Cursor::new(&mut bytes))?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert!(requests[0].starts_with(b"GET /target?x=1%7C2 HTTP/1.1\r\n"));

    let records = records(&bytes)?;
    let Record::Response { header, .. } = &records[2] else {
        panic!("the capture should store a response record");
    };

    assert_eq!(
        header.target_uri.as_str(),
        format!("http://127.0.0.1:{port}/target?x=1%7C2")
    );

    Ok(())
}

#[test]
fn archive_never_revisits_a_truncated_capture() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(2)?;
    // Two URLs with byte-identical responses, both cut short by the limit.
    let urls = [
        format!("http://127.0.0.1:{port}/first"),
        format!("http://127.0.0.1:{port}/second"),
    ];
    let limit = respond("/first").len() as u64 - 2;

    let archiver = Archiver::new(Config {
        max_response_length: Some(limit),
        gzip_warc: false,
        ..gzip_config()
    })?;
    let mut bytes = Vec::new();
    let summary = archiver.archive(&urls, Cursor::new(&mut bytes))?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());

    let records = records(&bytes)?;

    // The second response is stored in full rather than as a revisit of the truncated first.
    assert_eq!(records.len(), 7);
    assert!(matches!(records[2], Record::Response { .. }));
    assert!(matches!(records[5], Record::Response { .. }));
    assert_eq!(records[5].core().truncated, Some(TruncatedType::Length));

    Ok(())
}
