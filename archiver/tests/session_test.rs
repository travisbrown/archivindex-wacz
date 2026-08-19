//! End-to-end crawl session tests against a local HTTP server serving canned responses.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use archivindex_archiver::client::{Archiver, Error};
use archivindex_archiver::config::Config;
use archivindex_archiver::session::{
    Capture, CaptureProcessor, Inspection, Operator, RetryConfig, Session,
};
use archivindex_wacz::reader::WaczReader;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::Field;
use archivindex_warc::record::fields::dcmi::DcmiTerm;
use archivindex_warc::record::fields::warcinfo::WarcinfoField;
use archivindex_warc::record::{FieldsBlock, Record};

/// The operator most tests run their sessions as.
fn operator() -> Operator {
    Operator {
        name: "Test Operator".to_owned(),
        email: Some("operator@example.com".to_owned()),
    }
}

fn archiver(config: Config) -> Archiver {
    Archiver::new(config).expect("test archiver configuration should be valid")
}

/// A simple HTTP/1.1 response with a text body.
fn plain(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\n{headers}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A canned HTTP/1.1 response for a request path: a small site whose home page links to two other
/// pages, one of which links back to the home page.
fn respond(path: &str) -> Vec<u8> {
    match path {
        "/" => plain(
            "200 OK",
            "content-type: text/html",
            "<html>home links: /about /missing</html>",
        ),
        "/about" => plain(
            "200 OK",
            "content-type: text/html",
            "<html>about links: /</html>",
        ),
        "/redirect" => plain(
            "302 Found",
            "content-type: text/plain\r\nlocation: /about",
            "",
        ),
        _ => plain("404 Not Found", "content-type: text/plain", "gone"),
    }
}

/// Serve the given number of connections on an ephemeral local port, returning the request paths in
/// the order they arrived.
fn serve(connections: usize) -> std::io::Result<(u16, thread::JoinHandle<Vec<String>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let handle = thread::spawn(move || {
        let mut paths = Vec::with_capacity(connections);

        for _ in 0..connections {
            let Ok((mut stream, _)) = listener.accept() else {
                return paths;
            };

            let path = read_request_path(&mut stream);
            let _ = stream.write_all(&respond(&path));
            paths.push(path);
        }

        paths
    });

    Ok((port, handle))
}

/// Read a request's header section from a stream and return its target path.
fn read_request_path(stream: &mut (impl Read + Write)) -> String {
    let mut head = Vec::new();
    let mut buffer = [0; 4096];

    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => head.extend_from_slice(&buffer[..read]),
        }
    }

    String::from_utf8_lossy(&head)
        .split(' ')
        .nth(1)
        .unwrap_or("/")
        .to_owned()
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
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        let text = std::str::from_utf8(capture.payload).ok();
        let title = text.and_then(|text| {
            text.contains("home")
                .then(|| "Home".to_owned())
                .or_else(|| text.contains("about").then(|| "About".to_owned()))
        });

        Inspection {
            links: extract_links(capture.payload, self.port),
            title,
        }
    }
}

/// Return the same small link set for every capture, including the capture itself.
struct DeduplicationProcessor {
    port: u16,
}

impl CaptureProcessor for DeduplicationProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        Inspection {
            links: vec![
                format!("http://127.0.0.1:{}/", self.port),
                capture.url.to_owned(),
                format!("http://127.0.0.1:{}/about", self.port),
            ],
            title: None,
        }
    }
}

/// Record what the processor sees without discovering further links.
struct ObservingProcessor<'a> {
    observed: &'a mut Vec<(String, String, u16, String)>,
}

impl CaptureProcessor for ObservingProcessor<'_> {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        self.observed.push((
            capture.url.to_owned(),
            capture.final_url.to_owned(),
            capture.status,
            String::from_utf8_lossy(capture.payload).into_owned(),
        ));

        Inspection::default()
    }
}

/// Return a fixed set of links for every capture.
struct FixedLinksProcessor {
    links: Vec<String>,
}

impl CaptureProcessor for FixedLinksProcessor {
    fn inspect(&mut self, _capture: &Capture<'_>) -> Inspection {
        Inspection {
            links: self.links.clone(),
            title: None,
        }
    }
}

#[test]
fn session_crawls_discovered_urls_into_extra_pages() -> Result<(), Box<dyn std::error::Error>> {
    // The seeds are the home page and a redirect whose final URL is /about. The home page links
    // directly to /about and /missing; both are discoveries because seed identity uses the
    // requested URL rather than a redirect target.
    let (port, server) = serve(5)?;
    let seeds = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/redirect"),
    ];

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let summary = Session::new(
        archiver(Config {
            user_agent: "session-test/1.0".to_owned(),
            ..Config::default()
        }),
        "crawl-2026.08",
        operator(),
        &seeds,
        &path,
    )?
    .software("session-test-crawler", "9.9")
    .processor(SiteProcessor { port })
    .run()?;
    let request_paths = server.join().expect("server thread should not panic");

    // Seeds are captured first, including the redirect to /about, followed by discoveries in
    // processor order. Rediscovered seed URLs are discarded.
    assert_eq!(
        request_paths,
        ["/", "/redirect", "/about", "/about", "/missing"]
    );

    assert!(summary.is_complete());
    assert_eq!(
        summary
            .seed_captures
            .iter()
            .map(|capture| capture.url.as_str())
            .collect::<Vec<_>>(),
        seeds.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(
        summary
            .extra_captures
            .iter()
            .map(|capture| (capture.url.as_str(), capture.status))
            .collect::<Vec<_>>(),
        vec![
            (format!("http://127.0.0.1:{port}/about").as_str(), 200),
            (format!("http://127.0.0.1:{port}/missing").as_str(), 404),
        ]
    );

    let mut reader = WaczReader::new(std::io::Cursor::new(std::fs::read(&path)?))?;

    assert!(reader.verify()?.is_success());

    // The WARC file is named after the session identifier.
    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        ["archive/crawl-2026.08.warc.gz"]
    );

    // The manifest is titled by the identifier, and the main page is the first seed.
    let package = reader.data_package()?;

    assert_eq!(package.title.as_deref(), Some("crawl-2026.08"));
    assert_eq!(package.main_page_url.as_deref(), Some(seeds[0].as_str()));

    // Only the seeds appear in the required page list, with their titles; the pages discovered
    // during the crawl are listed in `extraPages.jsonl`.
    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        pages
            .iter()
            .map(|page| (page.url.as_ref(), page.title.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (seeds[0].as_str(), Some("Home")),
            (seeds[1].as_str(), Some("About")),
        ]
    );

    let extra = reader.page_list("pages/extraPages.jsonl")?;

    assert_eq!(extra.header().id.as_deref(), Some("extra-pages"));

    let extra_pages = extra.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        extra_pages
            .iter()
            .map(|page| (page.url.as_ref(), page.title.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (
                format!("http://127.0.0.1:{port}/about").as_str(),
                Some("About")
            ),
            (format!("http://127.0.0.1:{port}/missing").as_str(), None),
        ]
    );

    // The warcinfo record names the session and the User-Agent sent with every request.
    let records = reader
        .warc("archive/crawl-2026.08.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    let Record::Warcinfo { header, body } = &records[0] else {
        panic!("the first record should be a warcinfo record");
    };
    let FieldsBlock::Fields(fields) = body else {
        panic!("the warcinfo body should parse as warc-fields");
    };

    assert_eq!(
        fields
            .iter()
            .map(|(field, _)| field.name())
            .collect::<Vec<_>>(),
        [
            "format",
            "conformsTo",
            "software",
            "operator",
            "http-header-user-agent",
            "isPartOf",
        ]
    );

    assert_eq!(
        header
            .filename
            .as_ref()
            .and_then(archivindex_warc::value::Text::to_str),
        Some("crawl-2026.08.warc.gz")
    );
    assert_eq!(fields.http_header_user_agent(), Some("session-test/1.0"));
    assert_eq!(
        fields.get(&WarcinfoField::Dcmi(DcmiTerm::IsPartOf)),
        Some("crawl-2026.08")
    );
    assert_eq!(fields.software(), Some("session-test-crawler/9.9"));
    assert_eq!(
        fields.operator(),
        Some("Test Operator <operator@example.com>")
    );

    // One warcinfo record, then request, response, and metadata records for each of the five
    // exchanges (the redirect seed contributes two hops).
    assert_eq!(records.len(), 16);

    // Discovered captures carry the URI of the page they were discovered on as `via` in their
    // metadata records; seed captures (redirect hops included) carry none. Both discoveries came
    // from the home page's payload.
    let vias = records
        .iter()
        .filter(|record| record.type_name() == "metadata")
        .map(|record| {
            let Record::Metadata {
                body: FieldsBlock::Fields(fields),
                ..
            } = record
            else {
                panic!("the metadata body should parse as warc-fields");
            };

            fields.via()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        vias,
        vec![
            None,
            None,
            None,
            Some(seeds[0].as_str()),
            Some(seeds[0].as_str()),
        ]
    );

    // Every capture (seed hops and discovered pages alike) is indexed.
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 5);
    assert!(
        items
            .iter()
            .all(|item| item.fields.filename.as_deref() == Some("crawl-2026.08.warc.gz"))
    );

    Ok(())
}

#[test]
fn session_captures_each_url_once() -> Result<(), Box<dyn std::error::Error>> {
    // Both seeds link to each other and themselves, and one seed repeats; every URL is still
    // captured exactly once.
    let (port, server) = serve(2)?;
    let seeds = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/about"),
    ];

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let summary = Session::new(
        archiver(Config::default()),
        "dedup",
        Operator {
            name: "Solo".to_owned(),
            email: None,
        },
        &seeds,
        &path,
    )?
    .processor(DeduplicationProcessor { port })
    .run()?;
    let request_paths = server.join().expect("server thread should not panic");

    assert_eq!(request_paths, ["/", "/about"]);
    assert!(summary.is_complete());
    assert_eq!(summary.seed_captures.len(), 2);
    assert!(summary.extra_captures.is_empty());

    let mut reader = WaczReader::new(std::io::Cursor::new(std::fs::read(&path)?))?;
    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        pages
            .iter()
            .map(|page| page.url.as_ref())
            .collect::<Vec<_>>(),
        [seeds[0].as_str(), seeds[2].as_str()]
    );

    // With no discovered pages, the extra page list is omitted.
    assert!(reader.page_list("pages/extraPages.jsonl").is_err());

    // An operator without an email is recorded by name alone; the software defaults to this crate.
    let records = reader
        .warc("archive/dedup.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;
    let Record::Warcinfo {
        body: FieldsBlock::Fields(fields),
        ..
    } = &records[0]
    else {
        panic!("the first record should be a warcinfo record with warc-fields");
    };

    assert_eq!(fields.operator(), Some("Solo"));
    assert!(
        fields
            .software()
            .is_some_and(|software| software.starts_with("archivindex-archiver/"))
    );

    Ok(())
}

#[test]
fn session_limit_stops_with_discoveries_still_queued() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("limited.wacz");

    let summary = Session::new(
        archiver(Config::default()),
        "limited",
        operator(),
        [&url],
        &path,
    )?
    .processor(SiteProcessor { port })
    .limit(1)
    .run()?;
    let request_paths = server.join().expect("server thread should not panic");

    assert_eq!(request_paths, ["/"]);
    assert!(summary.is_complete());
    assert_eq!(summary.seed_captures.len(), 1);
    assert_eq!(summary.extra_captures.len(), 0);

    let mut reader = WaczReader::new(std::io::Cursor::new(std::fs::read(&path)?))?;

    assert_eq!(reader.pages()?.count(), 1);
    assert!(reader.page_list("pages/extraPages.jsonl").is_err());

    Ok(())
}

#[test]
fn session_rejects_an_unwritable_operator_before_writing() -> Result<(), Box<dyn std::error::Error>>
{
    // Reject the invalid operator before creating output or contacting the deliberately unreachable
    // seed.
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let result = Session::new(
        archiver(Config::default()),
        "bad-operator",
        Operator {
            name: "Line\r\nBreak".to_owned(),
            email: None,
        },
        ["http://127.0.0.1:9/"],
        &path,
    )?
    .run();

    assert!(matches!(result, Err(Error::WarcFields(_))));
    assert!(!path.exists());

    Ok(())
}

#[test]
fn session_retries_transient_failures_with_backoff() -> Result<(), Box<dyn std::error::Error>> {
    // The first connection stalls past the client timeout before responding; the retry is then
    // served promptly. Only the successful attempt's exchange is recorded.
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    // Each connection is served on its own thread, so that the stalling first attempt cannot delay
    // the accept (and prompt response) of the retry.
    let server = thread::spawn(move || {
        let mut handlers = Vec::new();

        for attempt in 0..2 {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };

            handlers.push(thread::spawn(move || {
                let path = read_request_path(&mut stream);

                if attempt == 0 {
                    thread::sleep(Duration::from_millis(300));
                }

                let _ = stream.write_all(&respond(&path));
            }));
        }

        for handler in handlers {
            let _ = handler.join();
        }
    });

    let url = format!("http://127.0.0.1:{port}/");
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let summary = Session::new(
        archiver(Config {
            timeout: Duration::from_millis(100),
            ..Config::default()
        }),
        "retry",
        operator(),
        [&url],
        &path,
    )?
    .retry(RetryConfig {
        attempts: 3,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(50),
    })
    .run()?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(summary.seed_captures.len(), 1);
    assert_eq!(summary.seed_captures[0].status, 200);

    // The failed attempt leaves no trace: one warcinfo record plus one exchange's records.
    let mut reader = WaczReader::new(std::io::Cursor::new(std::fs::read(&path)?))?;

    assert!(reader.verify()?.is_success());
    assert_eq!(
        reader
            .warc("archive/retry.warc.gz")?
            .iter_records::<NoExtension>()
            .count(),
        4
    );

    Ok(())
}

#[test]
fn session_reports_exhausted_retries_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    // Bind and immediately drop a listener so that the port refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/");

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let summary = Session::new(
        archiver(Config::default()),
        "unreachable",
        operator(),
        [&url],
        &path,
    )?
    .retry(RetryConfig {
        attempts: 2,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(10),
    })
    .run()?;

    assert!(!summary.is_complete());
    assert!(summary.fatal_error.is_none());
    assert_eq!(summary.failures.len(), 1);
    assert_eq!(summary.failures[0].url, url);

    // The collection is still written and internally consistent.
    let mut reader = WaczReader::new(std::io::Cursor::new(std::fs::read(&path)?))?;

    assert!(reader.verify()?.is_success());
    assert_eq!(reader.pages()?.count(), 0);

    Ok(())
}

#[test]
fn session_does_not_retry_permanent_failures() -> Result<(), Box<dyn std::error::Error>> {
    // A hostless URL is a permanent error, regardless of the retry settings.
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let summary = Session::new(
        archiver(Config::default()),
        "no-retry",
        operator(),
        ["data:text/plain,hi"],
        &path,
    )?
    .retry(RetryConfig {
        attempts: 100,
        initial_backoff: Duration::from_secs(60),
        max_backoff: Duration::from_secs(60),
    })
    .run()?;

    assert!(!summary.is_complete());
    assert!(matches!(summary.failures[0].error, Error::MissingHost(_)));

    Ok(())
}

#[test]
fn session_rejects_invalid_identifiers() {
    let seeds: [&str; 0] = [];

    for id in ["", "has space", "sl/ash", "qu?ery", "ünïcode"] {
        assert!(
            matches!(
                Session::new(
                    archiver(Config::default()),
                    id,
                    operator(),
                    seeds,
                    "out.wacz"
                ),
                Err(Error::InvalidSessionId(_))
            ),
            "identifier {id:?} should be rejected"
        );
    }

    assert!(
        Session::new(
            archiver(Config::default()),
            "ok-id_1.2~3",
            operator(),
            seeds,
            "out.wacz"
        )
        .is_ok()
    );
}

#[test]
fn session_refuses_an_existing_output() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");
    std::fs::write(&path, b"existing")?;

    let seeds: [&str; 0] = [];
    let result = Session::new(
        archiver(Config::default()),
        "existing",
        operator(),
        seeds,
        &path,
    )?
    .run();

    assert!(result.is_err());

    Ok(())
}

#[test]
fn session_with_no_seeds_writes_an_empty_collection() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let seeds: [&str; 0] = [];
    let summary = Session::new(
        archiver(Config::default()),
        "empty",
        operator(),
        seeds,
        &path,
    )?
    .run()?;

    assert!(summary.is_complete());
    assert!(summary.seed_captures.is_empty());

    let mut reader = WaczReader::new(std::io::Cursor::new(std::fs::read(&path)?))?;

    assert!(reader.verify()?.is_success());
    assert_eq!(reader.pages()?.count(), 0);

    Ok(())
}

#[test]
fn session_processor_sees_the_final_response_of_a_chain() -> Result<(), Box<dyn std::error::Error>>
{
    // The redirect seed's processor runs on /about's payload (the final hop), and the reported
    // final URL names the hop rather than the seed.
    let (port, server) = serve(2)?;
    let url = format!("http://127.0.0.1:{port}/redirect");

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let mut observed = Vec::new();
    let summary = Session::new(
        archiver(Config::default()),
        "final-hop",
        operator(),
        [&url],
        &path,
    )?
    .processor(ObservingProcessor {
        observed: &mut observed,
    })
    .run()?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].0, url);
    assert_eq!(observed[0].1, format!("http://127.0.0.1:{port}/about"));
    assert_eq!(observed[0].2, 200);
    assert!(observed[0].3.contains("about links"));

    Ok(())
}

#[test]
fn session_seed_set_is_by_requested_url() -> Result<(), Box<dyn std::error::Error>> {
    // A discovered URL that repeats a seed is dropped even when found before the seed itself is
    // captured; membership is by the requested URL, not the final one.
    let (port, server) = serve(3)?;
    let seeds = [
        format!("http://127.0.0.1:{port}/"),
        format!("http://127.0.0.1:{port}/about"),
    ];
    let seed_set = seeds.iter().cloned().collect::<HashSet<_>>();

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.wacz");

    let about = seeds[1].clone();
    let missing = format!("http://127.0.0.1:{port}/missing");
    let discovered = vec![about, missing.clone()];
    let summary = Session::new(
        archiver(Config::default()),
        "seed-set",
        operator(),
        &seeds,
        &path,
    )?
    .processor(FixedLinksProcessor { links: discovered })
    .run()?;
    server.join().expect("server thread should not panic");

    assert!(
        summary.is_complete(),
        "failures: {:?}, fatal: {:?}",
        summary
            .failures
            .iter()
            .map(|failure| (failure.url.as_str(), failure.error.to_string()))
            .collect::<Vec<_>>(),
        summary.fatal_error
    );
    assert!(
        summary
            .seed_captures
            .iter()
            .all(|capture| seed_set.contains(&capture.url))
    );
    assert_eq!(
        summary
            .extra_captures
            .iter()
            .map(|capture| capture.url.as_str())
            .collect::<Vec<_>>(),
        [missing.as_str()]
    );

    Ok(())
}
