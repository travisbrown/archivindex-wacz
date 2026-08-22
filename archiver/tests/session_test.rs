//! End-to-end crawl session tests against a local HTTP server serving canned responses.

use std::collections::HashSet;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use archivindex_archiver::client::{Archiver, CaptureControl, CaptureEvent, Error};
use archivindex_archiver::config::Config;
use archivindex_archiver::session::{
    Capture, CaptureProcessor, Inspection, Operator, RetryConfig, Session,
};
use archivindex_packager::WarcToWacz;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::io::read::WaczReader;
use archivindex_wacz::io::read::validate::ValidationOptions;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::Field;
use archivindex_warc::record::fields::dcmi::DcmiTerm;
use archivindex_warc::record::fields::metadata::MetadataField;
use archivindex_warc::record::fields::warcinfo::WarcinfoField;
use archivindex_warc::record::header::RevisitProfile;
use archivindex_warc::record::header::truncated_type::TruncatedType;
use archivindex_warc::record::{FieldsBlock, Record};
use archivindex_warc::value::{DigestAlgorithm, LabelledDigest, MediaType, WarcDate};
use archivindex_warc::version::WarcVersion;
use archivindex_warc_revisit_index::db::Index;
use archivindex_warc_revisit_index::payload::RevisitTarget;
use archivindex_warc_revisit_index::resource::{ResourceKey, ResourceStateUpdate};
use fluent_uri::Uri;

mod support;

use support::{plain, request_header, request_path, serve_concurrently_with, serve_with};

fn gzip_config() -> Config {
    Config {
        gzip_warc: true,
        ..Config::default()
    }
}

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

fn package_path(path: &std::path::Path) -> Result<PackagedWarc, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
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
    serve_with(connections, |head| {
        let path = request_path(head);
        (respond(path), path.to_owned())
    })
}

const LAST_MODIFIED: &str = "Wed, 01 Jan 2025 00:00:00 GMT";
const EXTERNAL_RECORD_ID: &str = "urn:uuid:00000000-0000-4000-8000-000000000001";

fn uri(value: &str) -> Uri<String> {
    Uri::parse(value).expect("test URI").to_owned()
}

fn warc_date(value: &str) -> WarcDate {
    WarcDate::parse(value, WarcVersion::V1_1).expect("test WARC date")
}

fn sha256(payload: &[u8]) -> LabelledDigest {
    let digest = Sha256Digest::compute(payload);
    LabelledDigest::from_digest(DigestAlgorithm::Sha256, &digest.0)
}

/// Answer a request for a versioned page, whose `ETag` advances once: an unconditional request or
/// one for a stale version gets the current page in full, while one for the current version gets
/// `304 Not Modified`, carrying the page's validators without a body.
fn respond_versioned(head: &str, versions: usize) -> Vec<u8> {
    let requested = request_header(head, "if-none-match")
        .and_then(|etag| etag.trim_matches('"').parse::<usize>().ok());
    let current = requested.map_or(1, |etag| versions.min(etag + 1));

    if requested == Some(current) {
        format!(
            "HTTP/1.1 304 Not Modified\r\netag: \"{current}\"\r\nlast-modified: {LAST_MODIFIED}\r\n\
             connection: close\r\n\r\n"
        )
        .into_bytes()
    } else {
        plain(
            "200 OK",
            &format!(
                "content-type: text/html\r\netag: \"{current}\"\r\nlast-modified: {LAST_MODIFIED}"
            ),
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
            ..Inspection::default()
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
            ..Inspection::default()
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

struct FailingProcessor;

impl CaptureProcessor for FailingProcessor {
    fn inspect(&mut self, _capture: &Capture<'_>) -> Inspection {
        Inspection {
            error: Some("cannot continue traversal".to_owned()),
            ..Inspection::default()
        }
    }
}

impl CaptureProcessor for FixedLinksProcessor {
    fn inspect(&mut self, _capture: &Capture<'_>) -> Inspection {
        Inspection {
            links: self.links.clone(),
            title: None,
            ..Inspection::default()
        }
    }
}

/// Ask for the first successful URL a fixed number of additional times, optionally recording the
/// status and payload of every capture seen.
struct RecaptureProcessor<'a> {
    remaining: usize,
    observed: Option<&'a mut Vec<(u16, String)>>,
}

impl CaptureProcessor for RecaptureProcessor<'_> {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        if let Some(observed) = self.observed.as_deref_mut() {
            observed.push((
                capture.status,
                String::from_utf8_lossy(capture.payload).into_owned(),
            ));
        }
        let recaptures = if self.remaining == 0 {
            Vec::new()
        } else {
            self.remaining -= 1;
            vec![capture.url.to_owned()]
        };

        Inspection {
            recaptures,
            ..Inspection::default()
        }
    }
}

#[test]
fn persistent_index_supplies_historical_and_same_session_revisit_targets()
-> Result<(), Box<dyn std::error::Error>> {
    const HISTORICAL: &str = "historical payload";
    const NEW: &str = "new shared payload";

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("revisits.sqlite3");
    let database_for_server = database.clone();
    let historical_target = RevisitTarget {
        payload_digest: sha256(HISTORICAL.as_bytes()),
        payload_length: Some(HISTORICAL.len() as u64),
        record_id: uri(EXTERNAL_RECORD_ID),
        target_uri: uri("https://archive.example/historical"),
        warc_date: warc_date("2025-01-01T00:00:00Z"),
    };
    Index::open(&database)?.insert_payload(&historical_target)?;
    let (port, server) = serve_with(3, move |head| {
        let path = request_path(head);
        let body = if path == "/historical" {
            HISTORICAL
        } else {
            NEW
        };
        let new_payload_is_durable = Index::open(&database_for_server)
            .and_then(|index| index.lookup_payload(&sha256(NEW.as_bytes())))
            .expect("inspect durable revisit index")
            .is_some();
        (
            plain("200 OK", "content-type: text/plain", body),
            format!("{path}:{new_payload_is_durable}"),
        )
    })?;
    let historical_url = format!("http://127.0.0.1:{port}/historical");
    let new_a_url = format!("http://127.0.0.1:{port}/new-a");
    let new_b_url = format!("http://127.0.0.1:{port}/new-b");
    let output = directory.path().join("persistent-revisits.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
        "persistent-revisits",
        operator(),
        [&historical_url, &new_a_url, &new_b_url],
        &output,
    )?
    .revisit_index(&database)
    .run()?;

    assert_eq!(
        server.join().expect("server thread"),
        ["/historical:false", "/new-a:false", "/new-b:false"]
    );
    assert!(summary.is_complete());

    let mut reader = package_path(&output)?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        records.iter().map(Record::type_name).collect::<Vec<_>>(),
        [
            "warcinfo", "request", "revisit", "metadata", "request", "response", "metadata",
            "request", "revisit", "metadata",
        ]
    );

    let Record::Revisit {
        header: historical, ..
    } = &records[2]
    else {
        panic!("the historical duplicate should be a revisit");
    };
    assert_eq!(historical.profile, RevisitProfile::IDENTICAL_PAYLOAD_DIGEST);
    assert_eq!(
        historical.refers_to.as_ref(),
        Some(&uri(EXTERNAL_RECORD_ID))
    );
    assert_eq!(
        historical.refers_to_target_uri.as_ref().map(Uri::as_str),
        Some("https://archive.example/historical")
    );

    let Record::Response {
        header: new_original,
        ..
    } = &records[5]
    else {
        panic!("the first new payload should be stored in full");
    };
    let Record::Revisit {
        header: new_revisit,
        ..
    } = &records[8]
    else {
        panic!("the second new payload should be a revisit");
    };
    assert_eq!(
        new_revisit.refers_to.as_ref(),
        Some(&new_original.core.record_id)
    );
    assert_eq!(
        new_revisit.refers_to_target_uri.as_ref().map(Uri::as_str),
        Some(new_a_url.as_str())
    );

    let persisted = Index::open(&database)?
        .lookup_payload(&sha256(NEW.as_bytes()))?
        .expect("new payload should be persisted");
    assert_eq!(persisted.record_id, new_original.core.record_id);
    assert_eq!(persisted.target_uri.as_str(), new_a_url);

    Ok(())
}

#[test]
fn persistent_resource_state_drives_conditional_requests_and_not_modified_revisits()
-> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve_with(1, |head| (respond_versioned(head, 1), head.to_owned()))?;
    let url = format!("http://127.0.0.1:{port}/page");
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("resource-state.sqlite3");
    let output = directory.path().join("persistent-revalidation.warc.gz");
    let digest = sha256(b"<html>version 1</html>");
    let original_date = warc_date("2025-01-01T00:00:00Z");
    let index = Index::open(&database)?;
    index.insert_payload(&RevisitTarget {
        payload_digest: digest.clone(),
        payload_length: Some(22),
        record_id: uri(EXTERNAL_RECORD_ID),
        target_uri: uri(&url),
        warc_date: original_date,
    })?;
    index.update_resource(
        &ResourceKey::new(uri(&url)),
        ResourceStateUpdate::Representation {
            etag: Some("\"1\"".to_owned()),
            last_modified: Some(LAST_MODIFIED.to_owned()),
            payload_digest: Some(digest.clone()),
            record_id: Some(uri(EXTERNAL_RECORD_ID)),
            warc_date: Some(original_date),
        },
    )?;
    drop(index);

    let summary = Session::new(
        archiver(gzip_config()),
        "persistent-revalidation",
        operator(),
        [&url],
        &output,
    )?
    .revisit_index(&database)
    .run()?;

    let requests = server.join().expect("server thread");
    assert_eq!(
        request_header(&requests[0], "if-none-match").as_deref(),
        Some("\"1\"")
    );
    assert_eq!(
        request_header(&requests[0], "if-modified-since").as_deref(),
        Some(LAST_MODIFIED.to_ascii_lowercase().as_str())
    );
    assert_eq!(summary.seed_captures[0].status, 304);

    let mut reader = package_path(&output)?;
    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;
    let Record::Revisit { header, .. } = &records[2] else {
        panic!("the persisted original should produce a revisit");
    };
    assert_eq!(header.profile, RevisitProfile::SERVER_NOT_MODIFIED);
    assert_eq!(header.refers_to.as_ref(), Some(&uri(EXTERNAL_RECORD_ID)));
    assert_eq!(header.refers_to_date, Some(original_date));
    assert_eq!(header.payload.payload_digest.as_ref(), Some(&digest));

    let state = Index::open(&database)?
        .lookup_resource(&ResourceKey::new(uri(&url)))?
        .expect("resource state should remain indexed");
    assert_eq!(state.payload_digest, Some(digest));
    assert_eq!(state.record_id, Some(uri(EXTERNAL_RECORD_ID)));
    assert_eq!(state.warc_date, Some(original_date));

    Ok(())
}

#[test]
fn processor_can_explicitly_recapture_a_seen_url() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(2)?;
    let url = format!("http://127.0.0.1:{port}/about");
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("recapture.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
        "recapture",
        operator(),
        [&url],
        &output,
    )?
    .processor(RecaptureProcessor {
        remaining: 1,
        observed: None,
    })
    .run()?;

    assert_eq!(server.join().expect("server thread"), ["/about", "/about"]);
    assert_eq!(summary.seed_captures.len(), 2);
    assert!(summary.is_complete());

    // The second capture's payload matches the first, so it is stored as a revisit record while
    // the collection remains fully conformant.
    let mut reader = package_path(&output)?;

    assert!(reader.validate(ValidationOptions::all())?.is_conformant());

    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        records
            .iter()
            .map(|record| record.type_name())
            .collect::<Vec<_>>(),
        [
            "warcinfo", "request", "response", "metadata", "request", "revisit", "metadata",
        ]
    );

    let Record::Response {
        header: original,
        body: original_body,
    } = &records[2]
    else {
        panic!("the first capture should store a full response record");
    };
    let Record::Revisit {
        header: revisit,
        body: revisit_body,
    } = &records[5]
    else {
        panic!("the second capture should store a revisit record");
    };

    // The revisit uses the identical-payload-digest profile and points back at the original
    // record, capture URI, and date, sharing the original's payload digest.
    assert_eq!(revisit.profile, RevisitProfile::IDENTICAL_PAYLOAD_DIGEST);
    assert_eq!(revisit.target_uri.as_str(), url);
    assert_eq!(revisit.refers_to.as_ref(), Some(&original.core.record_id));
    assert_eq!(
        revisit
            .refers_to_target_uri
            .as_ref()
            .map(|uri| uri.as_str()),
        Some(url.as_str())
    );
    assert_eq!(revisit.refers_to_date, Some(original.core.date));
    assert!(original.payload.payload_digest.is_some());
    assert_eq!(
        revisit.payload.payload_digest,
        original.payload.payload_digest
    );
    assert_eq!(revisit.core.content_type, Some(MediaType::HTTP_RESPONSE));
    assert_eq!(revisit.core.truncated, Some(TruncatedType::Length));

    // The revisit block is exactly the head of the (identical) response it stands for.
    assert!(revisit_body.ends_with(b"\r\n\r\n"));
    assert!(revisit_body.len() < original_body.len());
    assert_eq!(
        revisit_body.as_slice(),
        &original_body[..revisit_body.len()]
    );

    // Both captures are indexed under the shared payload digest, the revisit entry marked by the
    // conventional media type.
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 2);
    assert!(items[0].fields.digest.is_some());
    assert_eq!(items[0].fields.digest, items[1].fields.digest);
    assert_eq!(items[0].fields.mime.as_deref(), Some("text/html"));
    assert_eq!(items[1].fields.mime.as_deref(), Some("warc/revisit"));
    assert_eq!(items[1].fields.status, Some(200));

    Ok(())
}

#[test]
fn recapture_of_a_validated_response_is_a_server_not_modified_revisit()
-> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve_with(2, |head| (respond_versioned(head, 1), head.to_owned()))?;
    let url = format!("http://127.0.0.1:{port}/page");
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("revalidate.warc.gz");
    let mut observed = Vec::new();

    let summary = Session::new(
        archiver(gzip_config()),
        "revalidate",
        operator(),
        [&url],
        &output,
    )?
    .processor(RecaptureProcessor {
        remaining: 1,
        observed: Some(&mut observed),
    })
    .run()?;

    // The first request is unconditional; the recapture carries the stored response's validators.
    let heads = server.join().expect("server thread");
    assert_eq!(heads.len(), 2);
    assert_eq!(request_header(&heads[0], "if-none-match"), None);
    assert_eq!(request_header(&heads[0], "if-modified-since"), None);
    assert_eq!(
        request_header(&heads[1], "if-none-match").as_deref(),
        Some("\"1\"")
    );
    assert_eq!(
        request_header(&heads[1], "if-modified-since").as_deref(),
        Some(LAST_MODIFIED.to_ascii_lowercase().as_str())
    );

    // The processor sees the revalidated recapture as a 304 with no payload.
    assert_eq!(
        observed,
        [
            (200, "<html>version 1</html>".to_owned()),
            (304, String::new())
        ]
    );
    assert!(summary.is_complete());
    assert_eq!(
        summary
            .seed_captures
            .iter()
            .map(|capture| capture.status)
            .collect::<Vec<_>>(),
        [200, 304]
    );

    let mut reader = package_path(&output)?;

    assert!(reader.validate(ValidationOptions::all())?.is_conformant());

    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        records
            .iter()
            .map(|record| record.type_name())
            .collect::<Vec<_>>(),
        [
            "warcinfo", "request", "response", "metadata", "request", "revisit", "metadata",
        ]
    );

    let Record::Response {
        header: original, ..
    } = &records[2]
    else {
        panic!("the first capture should store a full response record");
    };
    let Record::Revisit {
        header: revisit,
        body: revisit_body,
    } = &records[5]
    else {
        panic!("the revalidated recapture should store a revisit record");
    };

    // The revisit uses the server-not-modified profile, points back at the original record, and
    // stands for the original payload, whose digest it repeats.
    assert_eq!(revisit.profile, RevisitProfile::SERVER_NOT_MODIFIED);
    assert_eq!(revisit.target_uri.as_str(), url);
    assert_eq!(revisit.refers_to.as_ref(), Some(&original.core.record_id));
    assert_eq!(
        revisit
            .refers_to_target_uri
            .as_ref()
            .map(|uri| uri.as_str()),
        Some(url.as_str())
    );
    assert_eq!(revisit.refers_to_date, Some(original.core.date));
    assert!(original.payload.payload_digest.is_some());
    assert_eq!(
        revisit.payload.payload_digest,
        original.payload.payload_digest
    );
    assert_eq!(revisit.core.content_type, Some(MediaType::HTTP_RESPONSE));

    // The revisit block is the whole `304` response as received, which is nothing but its head,
    // so it is not truncated.
    assert_eq!(revisit.core.truncated, None);
    assert!(revisit_body.starts_with(b"HTTP/1.1 304 Not Modified\r\n"));
    assert!(revisit_body.ends_with(b"\r\n\r\n"));

    // The index entry for the revisit carries its own status and the original's digest.
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 2);
    assert!(items[0].fields.digest.is_some());
    assert_eq!(items[0].fields.digest, items[1].fields.digest);
    assert_eq!(items[1].fields.mime.as_deref(), Some("warc/revisit"));
    assert_eq!(items[1].fields.status, Some(304));

    Ok(())
}

#[test]
fn changed_content_is_recaptured_in_full_and_revalidated_by_its_new_validators()
-> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve_with(3, |head| (respond_versioned(head, 2), head.to_owned()))?;
    let url = format!("http://127.0.0.1:{port}/page");
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("changed.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
        "changed",
        operator(),
        [&url],
        &output,
    )?
    .processor(RecaptureProcessor {
        remaining: 2,
        observed: None,
    })
    .run()?;

    // Each recapture is conditional on the latest stored version: the first finds the page
    // changed and is answered in full, and the second confirms the new version unchanged.
    let heads = server.join().expect("server thread");
    assert_eq!(
        heads
            .iter()
            .map(|head| request_header(head, "if-none-match"))
            .collect::<Vec<_>>(),
        [None, Some("\"1\"".to_owned()), Some("\"2\"".to_owned())]
    );
    assert!(summary.is_complete());
    assert_eq!(
        summary
            .seed_captures
            .iter()
            .map(|capture| capture.status)
            .collect::<Vec<_>>(),
        [200, 200, 304]
    );

    let mut reader = package_path(&output)?;

    assert!(reader.validate(ValidationOptions::all())?.is_conformant());

    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        records
            .iter()
            .map(|record| record.type_name())
            .collect::<Vec<_>>(),
        [
            "warcinfo", "request", "response", "metadata", "request", "response", "metadata",
            "request", "revisit", "metadata",
        ]
    );

    let (Record::Response { header: first, .. }, Record::Response { header: second, .. }) =
        (&records[2], &records[5])
    else {
        panic!("both changed versions should store full response records");
    };
    let Record::Revisit {
        header: revisit, ..
    } = &records[8]
    else {
        panic!("the revalidated recapture should store a revisit record");
    };

    assert_ne!(first.payload.payload_digest, second.payload.payload_digest);
    assert_eq!(revisit.profile, RevisitProfile::SERVER_NOT_MODIFIED);
    assert_eq!(revisit.refers_to.as_ref(), Some(&second.core.record_id));
    assert_eq!(
        revisit.payload.payload_digest,
        second.payload.payload_digest
    );

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 3);
    assert_ne!(items[0].fields.digest, items[1].fields.digest);
    assert_eq!(items[1].fields.digest, items[2].fields.digest);
    assert_eq!(
        items
            .iter()
            .map(|item| item.fields.status)
            .collect::<Vec<_>>(),
        [Some(200), Some(200), Some(304)]
    );

    Ok(())
}

#[test]
fn session_waits_between_queued_requests() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve_with(2, |_| {
        (
            plain("200 OK", "content-type: text/plain", "ok"),
            Instant::now(),
        )
    })?;
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("delayed.warc.gz");
    let delay = Duration::from_millis(50);

    let summary = Session::new(
        archiver(gzip_config()),
        "delayed",
        operator(),
        [
            format!("http://127.0.0.1:{port}/first"),
            format!("http://127.0.0.1:{port}/second"),
        ],
        output,
    )?
    .request_delay(delay)
    .run()?;
    let requests = server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert_eq!(requests.len(), 2);
    assert!(requests[1].duration_since(requests[0]) >= delay);

    Ok(())
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
    let path = directory.path().join("session.warc.gz");

    let summary = Session::new(
        archiver(Config {
            user_agent: "session-test/1.0".to_owned(),
            ..gzip_config()
        }),
        "crawl-2026.08",
        operator(),
        &seeds,
        &path,
    )?
    .software("session-test-crawler", "9.9")
    .processor(SiteProcessor { port })
    .titles()
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

    let mut reader = package_path(&path)?;

    assert!(reader.verify_fixity()?.is_success());
    assert!(reader.validate(ValidationOptions::all())?.is_conformant());

    // The WARC file is named after the session identifier.
    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        ["archive/data.warc.gz"]
    );

    // The manifest is titled by the identifier, and the main page is the first seed.
    let package = reader.data_package()?;

    assert_eq!(package.title.as_deref(), Some("crawl-2026.08"));
    assert_eq!(package.main_page_url.as_deref(), Some(seeds[0].as_str()));

    // Captures without discovery metadata enter the required page list. The redirect and its
    // destination remain separate captures because the WARC contains no page-list directives.
    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        pages
            .iter()
            .map(|page| (page.url.as_ref(), page.title.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            (seeds[0].as_str(), Some("Home")),
            (seeds[1].as_str(), None),
            (
                format!("http://127.0.0.1:{port}/about").as_str(),
                Some("About")
            ),
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
        .warc("archive/data.warc.gz")?
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
            "title",
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
    assert_eq!(
        fields.get(&WarcinfoField::Dcmi(DcmiTerm::Title)),
        Some("crawl-2026.08")
    );
    assert_eq!(fields.software(), Some("session-test-crawler/9.9"));
    assert_eq!(
        fields.operator(),
        Some("Test Operator <operator@example.com>")
    );

    // One warcinfo record, then request, response, and metadata records for each of the five
    // exchanges (the redirect seed contributes two hops). The second /about capture repeats the
    // first's payload, so its response is stored as a revisit record.
    assert_eq!(records.len(), 16);
    assert_eq!(
        records
            .iter()
            .filter(|record| record.type_name() == "revisit")
            .count(),
        1
    );

    // Discovered captures carry the URI of the page they were discovered on as `via` in their
    // metadata records; seed captures (redirect hops included) carry none. Both discoveries came
    // from the home page's payload.
    let metadata = records
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

            (
                fields.via(),
                fields.get(&MetadataField::Dcmi(DcmiTerm::Title)),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        metadata,
        vec![
            (None, Some("Home")),
            (None, None),
            (None, Some("About")),
            (Some(seeds[0].as_str()), Some("About")),
            (Some(seeds[0].as_str()), None),
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
            .all(|item| item.fields.filename.as_deref() == Some("data.warc.gz"))
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
    let path = directory.path().join("session.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
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

    let mut reader = package_path(&path)?;
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
        .warc("archive/data.warc.gz")?
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
    let path = directory.path().join("limited.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
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

    let mut reader = package_path(&path)?;

    assert_eq!(reader.pages()?.count(), 1);
    assert!(reader.page_list("pages/extraPages.jsonl").is_err());

    let records = reader
        .warc("archive/data.warc.gz")?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;
    let Record::Warcinfo {
        body: FieldsBlock::Fields(warcinfo),
        ..
    } = &records[0]
    else {
        panic!("the first record should be a warcinfo record with warc-fields");
    };
    assert!(
        warcinfo
            .get(&WarcinfoField::Dcmi(DcmiTerm::Title))
            .is_none()
    );
    assert!(records.iter().all(|record| {
        let Record::Metadata {
            body: FieldsBlock::Fields(fields),
            ..
        } = record
        else {
            return true;
        };
        fields.get(&MetadataField::Dcmi(DcmiTerm::Title)).is_none()
    }));

    Ok(())
}

#[test]
fn session_rejects_an_unwritable_operator_before_writing() -> Result<(), Box<dyn std::error::Error>>
{
    // Reject the invalid operator before creating output or contacting the deliberately unreachable
    // seed.
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.warc.gz");

    let result = Session::new(
        archiver(gzip_config()),
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
    let (port, server) = serve_concurrently_with(2, |attempt, head| {
        if attempt == 0 {
            thread::sleep(Duration::from_millis(300));
        }
        (respond(request_path(head)), ())
    })?;

    let url = format!("http://127.0.0.1:{port}/");
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.warc.gz");

    let summary = Session::new(
        archiver(Config {
            timeout: Duration::from_millis(100),
            ..gzip_config()
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
    let mut reader = package_path(&path)?;

    assert!(reader.verify_fixity()?.is_success());
    assert_eq!(
        reader
            .warc("archive/data.warc.gz")?
            .iter_records::<NoExtension>()
            .count(),
        4
    );

    Ok(())
}

#[test]
fn session_retries_retryable_http_statuses() -> Result<(), Box<dyn std::error::Error>> {
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = Arc::clone(&attempts);
    let (port, server) = serve_with(2, move |head| {
        let attempt = server_attempts.fetch_add(1, Ordering::Relaxed);
        let response = if attempt == 0 {
            plain(
                "503 Service Unavailable",
                "content-type: text/plain\r\nretry-after: 0",
                "try later",
            )
        } else {
            plain("200 OK", "content-type: text/plain", "complete")
        };
        (response, request_path(head).to_owned())
    })?;
    let url = format!("http://127.0.0.1:{port}/");
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("status-retry.warc.gz");
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_sink = Arc::clone(&events);

    let summary = Session::new(
        archiver(gzip_config()),
        "status-retry",
        operator(),
        [&url],
        &path,
    )?
    .events(move |event: CaptureEvent<'_>| {
        events_for_sink
            .lock()
            .expect("event lock")
            .push(match event {
                CaptureEvent::Started { attempt, .. } => format!("started:{attempt}"),
                CaptureEvent::Retrying { attempt, .. } => format!("retrying:{attempt}"),
                CaptureEvent::Captured { .. } => "captured".to_owned(),
                CaptureEvent::Written { .. } => "written".to_owned(),
                CaptureEvent::Failed { .. } => "failed".to_owned(),
            });
        CaptureControl::Continue
    })
    .retry(RetryConfig {
        attempts: 2,
        initial_backoff: Duration::from_secs(30),
        max_backoff: Duration::from_secs(30),
    })
    .run()?;
    let requests = server.join().expect("server thread should not panic");

    assert_eq!(attempts.load(Ordering::Relaxed), 2);
    assert_eq!(requests, ["/", "/"]);
    assert!(summary.is_complete());
    assert_eq!(summary.seed_captures[0].status, 200);
    assert_eq!(
        *events.lock().expect("event lock"),
        [
            "started:1",
            "retrying:2",
            "started:2",
            "captured",
            "written"
        ]
    );

    Ok(())
}

#[test]
fn session_reports_exhausted_http_status_retries() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve_with(2, |head| {
        (
            plain(
                "503 Service Unavailable",
                "content-type: text/plain",
                "busy",
            ),
            request_path(head).to_owned(),
        )
    })?;
    let url = format!("http://127.0.0.1:{port}/");
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("status-exhausted.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
        "status-exhausted",
        operator(),
        [&url],
        &path,
    )?
    .retry(RetryConfig {
        attempts: 2,
        initial_backoff: Duration::ZERO,
        max_backoff: Duration::ZERO,
    })
    .run()?;
    server.join().expect("server thread should not panic");

    assert!(!summary.is_complete());
    assert!(matches!(
        summary.failures[0].error,
        Error::HttpStatus { status: 503, .. }
    ));

    Ok(())
}

#[test]
fn processor_failure_stops_with_an_incomplete_summary() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let url = format!("http://127.0.0.1:{port}/");
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("processor-failure.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
        "processor-failure",
        operator(),
        [&url],
        &path,
    )?
    .processor(FailingProcessor)
    .run()?;
    server.join().expect("server thread should not panic");

    assert!(!summary.is_complete());
    assert!(matches!(summary.failures[0].error, Error::Processor { .. }));

    Ok(())
}

#[test]
fn session_reports_exhausted_retries_as_failures() -> Result<(), Box<dyn std::error::Error>> {
    // Bind and immediately drop a listener so that the port refuses connections.
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let url = format!("http://127.0.0.1:{port}/");

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
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
    let mut reader = package_path(&path)?;

    assert!(reader.verify_fixity()?.is_success());
    assert_eq!(reader.pages()?.count(), 0);

    Ok(())
}

#[test]
fn session_does_not_retry_permanent_failures() -> Result<(), Box<dyn std::error::Error>> {
    // A hostless URL is a permanent error, regardless of the retry settings.
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.warc.gz");

    let summary = Session::new(
        archiver(gzip_config()),
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
                    archiver(gzip_config()),
                    id,
                    operator(),
                    seeds,
                    "out.warc.gz"
                ),
                Err(Error::InvalidSessionId(_))
            ),
            "identifier {id:?} should be rejected"
        );
    }

    assert!(
        Session::new(
            archiver(gzip_config()),
            "ok-id_1.2~3",
            operator(),
            seeds,
            "out.warc.gz"
        )
        .is_ok()
    );
}

#[test]
fn session_refuses_an_existing_output() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.warc.gz");
    let database = directory.path().join("state.sqlite3");
    std::fs::write(&path, b"existing")?;
    let url = "https://example.com/";

    let result = Session::new(
        archiver(gzip_config()),
        "existing",
        operator(),
        [url],
        &path,
    )?
    .revisit_index(&database)
    .run();

    assert!(result.is_err());
    let key = ResourceKey::new(Uri::parse(url)?.to_owned());
    assert!(Index::open(database)?.lookup_resource(&key)?.is_none());

    Ok(())
}

#[test]
fn session_with_no_seeds_writes_an_empty_collection() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.warc.gz");

    let seeds: [&str; 0] = [];
    let summary =
        Session::new(archiver(gzip_config()), "empty", operator(), seeds, &path)?.run()?;

    assert!(summary.is_complete());
    assert!(summary.seed_captures.is_empty());

    let mut reader = package_path(&path)?;

    assert!(reader.verify_fixity()?.is_success());
    assert_eq!(reader.pages()?.count(), 0);

    Ok(())
}

#[test]
fn session_writes_to_named_partial_before_publishing() -> Result<(), Box<dyn std::error::Error>> {
    let (port, server) = serve(1)?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("session.warc.gz");
    let partial_path = directory.path().join("session.warc.gz.partial");
    let url = format!("http://127.0.0.1:{port}/");
    let mut saw_partial = false;

    let summary = Session::new(
        archiver(gzip_config()),
        "visible-partial",
        operator(),
        [&url],
        &path,
    )?
    .events(|event: CaptureEvent<'_>| {
        if matches!(event, CaptureEvent::Started { .. }) {
            saw_partial = true;
            assert!(partial_path.exists());
            assert!(std::fs::metadata(&partial_path).is_ok_and(|metadata| metadata.len() > 0));
            assert!(!path.exists());
        }
        CaptureControl::Continue
    })
    .run()?;
    server.join().expect("server thread should not panic");

    assert!(summary.is_complete());
    assert!(saw_partial);
    assert!(path.exists());
    assert!(!partial_path.exists());

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
    let path = directory.path().join("session.warc.gz");

    let mut observed = Vec::new();
    let summary = Session::new(
        archiver(gzip_config()),
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
    let path = directory.path().join("session.warc.gz");

    let about = seeds[1].clone();
    let missing = format!("http://127.0.0.1:{port}/missing");
    let discovered = vec![about, missing.clone()];
    let summary = Session::new(
        archiver(gzip_config()),
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
