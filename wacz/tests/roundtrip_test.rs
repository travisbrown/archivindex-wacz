//! Round-trip tests for WACZ files containing compressed and uncompressed WARC files.

use std::borrow::Cow;
use std::io::{Cursor, Read, Write};

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_wacz::frictionless::DataPackageBuilder;
use archivindex_wacz::io::read::{self as reader, WaczReader};
use archivindex_wacz::io::write::{
    self as writer, IndexFormat, PackageMetadata, WaczWriter, WriterConfig,
};
use archivindex_wacz::pages;
use archivindex_wacz::pages::{Page, PageListHeader};
use archivindex_warc::io::write::WarcWriter;
use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;
use chrono::{TimeZone, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::{DeflateEncoder, GzEncoder};
use fluent_uri::Uri;

const URL: &str = "https://www.example.com/page";
const BODY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html>hello</html>";

/// Build the serialized bytes of a single-record WARC file for the test capture.
fn warc_bytes() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let record: Record = Record::response(URL, capture_time())?.body(BODY)?;

    let mut bytes = Vec::new();
    let mut writer = WarcWriter::new(&mut bytes);
    writer.write(&record.into_raw()?)?;

    Ok(bytes)
}

/// The conventional capture time used by index and page fixtures.
fn capture_time() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2020, 10, 7, 21, 22, 36).unwrap()
}

/// Build a minimal CDXJ item for a URL captured at [`capture_time`].
fn item_for(url: &str) -> Result<cdxj::Item<'static>, cdxj::Error> {
    Ok(cdxj::Item {
        key: Cow::Owned(cdxj::search_key(url)?),
        timestamp: capture_time().into(),
        fields: cdxj::Fields {
            url: Cow::Owned(url.to_owned()),
            digest: Some(Cow::Borrowed(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            )),
            mime: Some(Cow::Borrowed("text/html")),
            status: Some(200),
            offset: Some(0),
            length: Some(10),
            filename: Some(Cow::Borrowed("data.warc.gz")),
            record_digest: None,
            extra: ExtraProperties::default(),
        },
    })
}

fn conforming_items(
    items: &[cdxj::Item<'static>],
) -> Result<Vec<cdxj::ConformingItem<'static>>, cdxj::ConformanceError> {
    items.iter().map(cdxj::ConformingItem::try_from).collect()
}

/// Build a searchable item at a particular time without requiring a corresponding WARC record.
fn item_at(
    url: &str,
    timestamp: chrono::DateTime<Utc>,
) -> Result<cdxj::Item<'static>, cdxj::Error> {
    let mut item = item_for(url)?;
    item.timestamp = timestamp.into();
    Ok(item)
}

/// Build an item whose fields resolve to a real record in a named WARC member.
fn resolvable_item(
    url: &str,
    filename: &'static str,
    length: u64,
) -> Result<cdxj::Item<'static>, cdxj::Error> {
    let mut item = item_for(url)?;
    item.fields.filename = Some(Cow::Borrowed(filename));
    item.fields.length = Some(length);
    Ok(item)
}

/// Build a ZIP file from `(path, contents)` pairs.
fn zip_of(members: &[(&str, &[u8])]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();

    for (path, contents) in members {
        zip.start_file(*path, options)?;
        zip.write_all(contents)?;
    }

    Ok(zip.finish()?.into_inner())
}

/// Build a ZIP file whose members all use `STORE`, as WACZ random access requires.
fn stored_zip_of(members: &[(&str, &[u8])]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    for (path, contents) in members {
        zip.start_file(*path, options)?;
        zip.write_all(contents)?;
    }

    Ok(zip.finish()?.into_inner())
}

struct FailingReader(bool);

impl Read for FailingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.0 {
            Err(std::io::Error::other("deliberate test read failure"))
        } else {
            self.0 = true;
            buffer[..7].copy_from_slice(b"partial");
            Ok(7)
        }
    }
}

/// A minimal valid manifest with no resources, for hand-rolled containers.
const EMPTY_MANIFEST: &str =
    r#"{"profile": "data-package", "wacz_version": "1.1.1", "resources": []}"#;

/// Build an in-memory WACZ containing one WARC file, one index, and one page.
fn build_wacz(warc_name: &str, warc_data: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    writer.add_warc(warc_name, warc_data)?;

    let capture_time = capture_time();

    let item = cdxj::Item {
        key: Cow::Owned(cdxj::search_key(URL)?),
        timestamp: capture_time.into(),
        fields: cdxj::Fields {
            url: Cow::Borrowed(URL),
            digest: Some(Cow::Borrowed(
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            )),
            mime: Some(Cow::Borrowed("text/html")),
            status: Some(200),
            offset: Some(0),
            length: Some(warc_data.len() as u64),
            filename: Some(Cow::Borrowed(warc_name)),
            record_digest: None,
            extra: ExtraProperties::default(),
        },
    };

    writer.add_index("index.cdx", [&cdxj::ConformingItem::try_from(&item)?])?;

    let page = Page {
        url: Cow::Borrowed(URL),
        ts: capture_time,
        id: Some(Cow::Borrowed("1db0ef709a")),
        title: Some(Cow::Borrowed("Example Domain")),
        text: None,
        size: Some(BODY.len() as u64),
        extra: ExtraProperties::default(),
    };

    writer.add_pages(&PageListHeader::default(), [&page])?;

    let metadata = DataPackageBuilder::new()
        .title("Test collection")
        .main_page_url(URL)
        .main_page_date(capture_time);

    Ok(writer.finish(metadata)?.into_inner())
}

/// Assert that a built WACZ file round trips through the reader.
fn assert_round_trip(warc_name: &str, warc_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let wacz = build_wacz(warc_name, warc_data)?;
    let mut reader = WaczReader::new(Cursor::new(wacz))?;

    let package = reader.data_package()?;
    let warc_path = format!("archive/{warc_name}");

    assert_eq!(package.wacz_version, "1.1.1");
    assert_eq!(package.title.as_deref(), Some("Test collection"));
    assert_eq!(package.main_page_url.as_deref(), Some(URL));
    assert!(package.created.is_some());
    assert_eq!(package.resources.len(), 3);
    assert!(
        package
            .resources
            .iter()
            .any(|resource| resource.path == warc_path)
    );

    let digest = reader
        .data_package_digest()?
        .expect("digest file should be present");

    assert_eq!(digest.path, "datapackage.json");

    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        vec![warc_path.clone()]
    );
    assert_eq!(
        reader.index_paths().collect::<Vec<_>>(),
        vec!["indexes/index.cdx"]
    );

    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].url, URL);
    assert_eq!(pages[0].title.as_deref(), Some("Example Domain"));

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "com,example,www)/page");
    assert_eq!(items[0].fields.filename.as_deref(), Some(warc_name));

    let records = reader
        .warc(&warc_path)?
        .iter_records::<NoExtension>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].body_bytes().as_ref(), BODY);
    assert_eq!(records[0].target_uri().map(Uri::as_str), Some(URL));

    let fixity = reader.verify_fixity()?;

    assert!(fixity.is_success());
    // The three resources plus the manifest itself, which is covered by the digest file.
    assert_eq!(fixity.verified.len(), 4);

    Ok(())
}

#[test]
fn round_trip_with_plain_warc_member() -> Result<(), Box<dyn std::error::Error>> {
    assert_round_trip("data.warc", &warc_bytes()?)
}

#[test]
fn round_trip_with_gzip_warc_member() -> Result<(), Box<dyn std::error::Error>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&warc_bytes()?)?;
    let compressed = encoder.finish()?;

    assert_round_trip("data.warc.gz", &compressed)
}

/// Public member APIs distinguish stored, raw ZIP, and content-level decoded representations and
/// permit bounded random access only where offsets are meaningful.
#[test]
fn member_access_apis() -> Result<(), Box<dyn std::error::Error>> {
    let warc = warc_bytes()?;
    let wacz = build_wacz("data.warc", &warc)?;
    let mut reader = WaczReader::new(Cursor::new(wacz))?;

    let metadata = reader.member_metadata("archive/data.warc")?;
    assert_eq!(metadata.compression, zip::CompressionMethod::Stored);
    assert_eq!(metadata.size, warc.len() as u64);
    assert_eq!(reader.member_bytes("archive/data.warc")?, warc);
    assert_eq!(reader.decoded_member_bytes("archive/data.warc")?, warc);
    assert_eq!(reader.member_range("archive/data.warc", 5, 7)?, warc[5..12]);
    assert!(matches!(
        reader.member_range("archive/data.warc", warc.len() as u64, 1),
        Err(reader::Error::RangeOutOfBounds { .. })
    ));

    // The manifest is DEFLATE-compressed in the ZIP, so logical byte-range access is rejected.
    assert!(matches!(
        reader.member_range("datapackage.json", 0, 1),
        Err(reader::Error::CompressedMember(_))
    ));
    assert!(reader.raw_member("datapackage.json")?.compressed_size() > 0);

    Ok(())
}

/// Manifest resources can be retrieved generically with their declared size and digest checked.
#[test]
fn arbitrary_resource_read_is_verified() -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    writer.add_resource("extras/metadata.txt", b"custom metadata".as_slice())?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();
    let mut reader = WaczReader::new(Cursor::new(wacz))?;

    assert_eq!(
        reader.resource_bytes("extras/metadata.txt")?,
        b"custom metadata"
    );
    assert!(matches!(
        reader.resource_bytes("not-listed.txt"),
        Err(reader::Error::UnlistedResource(_))
    ));

    Ok(())
}

/// Plain-index lookup uses the SURT key, honors time bounds, and resolves the descriptor to a WARC
/// record through the same range API.
#[test]
fn plain_lookup_and_capture_resolution() -> Result<(), Box<dyn std::error::Error>> {
    let warc = warc_bytes()?;
    let wacz = build_wacz("data.warc", &warc)?;
    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let timestamp: cdxj::Timestamp = capture_time().into();

    assert!(reader.lookup(URL, ..timestamp)?.is_empty());

    let captures = reader.lookup(URL, timestamp..=timestamp)?;
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].index_path, "indexes/index.cdx");
    assert_eq!(reader.capture_bytes(&captures[0].item.fields)?, warc);

    let raw = reader.read_capture_raw(&captures[0].item.fields)?;
    assert_eq!(raw.body, BODY);
    let record = reader.read_capture(&captures[0].item.fields)?;
    assert_eq!(record.body_bytes().as_ref(), BODY);

    let mut mismatched = captures[0].item.fields.clone();
    mismatched.record_digest = Some(Sha256Digest::compute(b"different"));
    assert!(matches!(
        reader.capture_bytes(&mismatched),
        Err(reader::Error::DigestMismatch { .. })
    ));

    Ok(())
}

/// Gzip capture ranges are verified while compressed and decoded as one member for WARC parsing.
#[test]
fn gzip_capture_resolution() -> Result<(), Box<dyn std::error::Error>> {
    let warc = warc_bytes()?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&warc)?;
    let compressed = encoder.finish()?;
    let wacz = build_wacz("data.warc.gz", &compressed)?;
    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let capture = reader.lookup(URL, ..)?.into_iter().next().expect("capture");

    assert_eq!(reader.capture_bytes(&capture.item.fields)?, compressed);
    assert_eq!(
        reader
            .read_capture(&capture.item.fields)?
            .body_bytes()
            .as_ref(),
        BODY
    );

    Ok(())
}

/// Pages written without an identifier receive a synthetic one of the configured length; explicitly
/// supplied identifiers are preserved.
#[test]
fn synthetic_page_ids() -> Result<(), Box<dyn std::error::Error>> {
    let capture_time = capture_time();
    let with_id = Page {
        url: Cow::Borrowed(URL),
        ts: capture_time,
        id: Some(Cow::Borrowed("explicit-id")),
        title: None,
        text: None,
        size: None,
        extra: ExtraProperties::default(),
    };
    let without_id = Page {
        url: Cow::Borrowed("https://www.example.com/other"),
        id: None,
        ..with_id.clone()
    };

    for (length, config) in [
        (24, WriterConfig::default()),
        (
            16,
            WriterConfig {
                page_id_length: 16,
                ..WriterConfig::default()
            },
        ),
    ] {
        let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
        writer.add_pages(&PageListHeader::default(), [&with_id, &without_id])?;
        let wacz = writer
            .finish_unchecked(PackageMetadata::default())?
            .into_inner();

        let mut reader = WaczReader::new(Cursor::new(wacz))?;
        let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

        assert_eq!(pages[0].id.as_deref(), Some("explicit-id"));
        assert_eq!(
            pages[1].id.as_deref(),
            Some(pages::synthetic_id(&capture_time, &pages[1].url, length).as_str())
        );
        assert_eq!(pages[1].id.as_deref().map(str::len), Some(length));
    }

    Ok(())
}

/// A configured ZIP level is passed to the DEFLATE encoder for compressible WACZ members.
#[test]
fn zip_compression_level_controls_deflated_members() -> Result<(), Box<dyn std::error::Error>> {
    let contents = [b'a'; 8192];

    for level in [1, 6, 9] {
        let config = WriterConfig {
            zip_compression_level: Some(level),
            ..WriterConfig::default()
        };
        let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
        writer.add_resource("extras/content.txt", contents.as_slice())?;
        let wacz = writer
            .finish_unchecked(PackageMetadata::default())?
            .into_inner();
        let mut reader = WaczReader::new(Cursor::new(wacz))?;
        let mut compressed = Vec::new();
        reader
            .raw_member("extras/content.txt")?
            .read_to_end(&mut compressed)?;

        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
        encoder.write_all(&contents)?;
        assert_eq!(compressed, encoder.finish()?);
    }

    let config = WriterConfig {
        zip_compression_level: Some(10),
        ..WriterConfig::default()
    };
    let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
    writer.add_resource("extras/content.txt", contents.as_slice())?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();
    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    assert_eq!(reader.resource_bytes("extras/content.txt")?, contents);

    Ok(())
}

#[test]
fn invalid_zip_compression_level_is_rejected() {
    for level in [0, 265] {
        let config = WriterConfig {
            zip_compression_level: Some(level),
            ..WriterConfig::default()
        };
        let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);

        assert!(matches!(
            writer.add_resource("extras/content.txt", &b"content"[..]),
            Err(writer::Error::InvalidZipCompressionLevel(actual)) if actual == level
        ));
    }
}

/// A `ZipNum` index following the `py-wacz` layout: `index.cdx.gz` holds independent gzip members
/// of at most `lines` CDX lines each, and `index.idx` locates every block by offset, length, and
/// digest behind a `!meta` header line.
#[test]
fn zipnum_index() -> Result<(), Box<dyn std::error::Error>> {
    // Five items across a two-line block size: blocks of 2, 2, and 1 lines.
    let items = (0..5)
        .map(|i| item_for(&format!("https://www.example.com/page{i}")))
        .collect::<Result<Vec<_>, cdxj::Error>>()?;

    let config = WriterConfig {
        index_format: IndexFormat::ZipNum { lines: 2 },
        ..WriterConfig::default()
    };
    let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
    let conforming = conforming_items(&items)?;
    writer.add_index("index.cdx", &conforming)?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    // The gzip data file uses STORE; the plain-text summary uses DEFLATE.
    let mut archive = zip::ZipArchive::new(Cursor::new(&wacz))?;
    assert_eq!(
        archive.by_name("indexes/index.cdx.gz")?.compression(),
        zip::CompressionMethod::Stored
    );
    assert_eq!(
        archive.by_name("indexes/index.idx")?.compression(),
        zip::CompressionMethod::Deflated
    );

    let mut summary = String::new();
    archive
        .by_name("indexes/index.idx")?
        .read_to_string(&mut summary)?;
    let mut data = Vec::new();
    archive
        .by_name("indexes/index.cdx.gz")?
        .read_to_end(&mut data)?;

    let summary_lines = summary.lines().collect::<Vec<_>>();

    assert_eq!(
        summary_lines[0],
        "!meta 0 {\"format\": \"cdxj-gzip-1.0\", \"filename\": \"index.cdx.gz\"}"
    );
    assert_eq!(summary_lines.len(), 4);

    // Each summary line locates a complete, independently decompressible gzip member.
    let mut expected_offset = 0;
    let mut block_line_counts = Vec::new();

    for line in &summary_lines[1..] {
        let brace = line.find('{').expect("summary line should hold JSON");
        let value = serde_json::from_str::<serde_json::Value>(&line[brace..])?;

        let offset = usize::try_from(value["offset"].as_u64().expect("offset"))?;
        let length = usize::try_from(value["length"].as_u64().expect("length"))?;
        assert_eq!(offset, expected_offset);
        expected_offset += length;

        let block = &data[offset..offset + length];
        assert_eq!(
            value["digest"].as_str().expect("digest"),
            Sha256Digest::compute(block).to_string()
        );

        let mut decoded = String::new();
        GzDecoder::new(block).read_to_string(&mut decoded)?;
        block_line_counts.push(decoded.lines().count());

        // The prefix is the search key and timestamp of the block's first line.
        assert!(decoded.starts_with(line[..brace].trim_end()));
    }

    assert_eq!(expected_offset, data.len());
    assert_eq!(block_line_counts, vec![2, 2, 1]);

    // The data file reads back as the full sorted index, and manifest verification passes.
    let mut reader = WaczReader::new(Cursor::new(&wacz))?;
    let read_items = reader
        .index("indexes/index.cdx.gz")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(read_items.len(), items.len());
    assert!(read_items.is_sorted_by_key(|item| item.key.clone()));

    let parsed_summary = reader.zipnum_summary("indexes/index.idx")?;
    assert_eq!(parsed_summary.data_path, "indexes/index.cdx.gz");
    assert_eq!(parsed_summary.blocks.len(), 3);
    let first_block = reader.zipnum_block(&parsed_summary.blocks[0])?;
    assert_eq!(std::str::from_utf8(&first_block)?.lines().count(), 2);
    let mut bad_block = parsed_summary.blocks[0].clone();
    bad_block.digest = Sha256Digest::compute(b"not this block");
    assert!(matches!(
        reader.zipnum_block(&bad_block),
        Err(reader::Error::DigestMismatch { .. })
    ));

    let captures = reader.lookup("https://www.example.com/page3", ..)?;
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].index_path, "indexes/index.idx");
    assert_eq!(captures[0].item.fields.url, "https://www.example.com/page3");
    assert!(reader.verify_fixity()?.is_success());

    Ok(())
}

/// Lookup results are chronological even when index input is not, and range bounds use timestamp
/// semantics rather than the textual precision of a CDXJ value.
#[test]
fn lookup_orders_and_filters_captures_chronologically() -> Result<(), Box<dyn std::error::Error>> {
    let first = capture_time();
    let second = first + chrono::TimeDelta::milliseconds(500);
    let third = first + chrono::TimeDelta::seconds(1);
    let mut items = [
        item_at(URL, third)?,
        item_at(URL, first)?,
        item_at(URL, second)?,
    ];
    items[1].timestamp = cdxj::Timestamp::new(first);
    items[2].timestamp = cdxj::Timestamp::with_milliseconds(second);

    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    let conforming = conforming_items(&items)?;
    writer.add_index("index.cdx", &conforming)?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();
    let mut reader = WaczReader::new(Cursor::new(wacz))?;

    let captures = reader.lookup(
        URL,
        cdxj::Timestamp::new(first)..cdxj::Timestamp::new(third),
    )?;
    assert_eq!(captures.len(), 2);
    assert!(captures.is_sorted_by_key(|capture| capture.item.timestamp));
    assert_eq!(captures[0].item.timestamp.datetime(), first);
    assert_eq!(captures[1].item.timestamp.datetime(), second);

    Ok(())
}

/// Files under `archive/` use `STORE`, while plain-text files may use `DEFLATE`.
#[test]
fn spec_compression_methods() -> Result<(), Box<dyn std::error::Error>> {
    use zip::CompressionMethod;

    let expectations = [
        ("indexes/index.cdx", CompressionMethod::Deflated),
        ("pages/pages.jsonl", CompressionMethod::Deflated),
        ("datapackage.json", CompressionMethod::Deflated),
        ("datapackage-digest.json", CompressionMethod::Deflated),
    ];

    // Uncompressed and compressed WARC files both use STORE.
    let wacz = build_wacz("data.warc", &warc_bytes()?)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(wacz))?;
    assert_eq!(
        archive.by_name("archive/data.warc")?.compression(),
        CompressionMethod::Stored
    );
    for (name, expected) in expectations {
        assert_eq!(archive.by_name(name)?.compression(), expected, "{name}");
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&warc_bytes()?)?;
    let compressed = encoder.finish()?;

    let wacz = build_wacz("data.warc.gz", &compressed)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(wacz))?;
    assert_eq!(
        archive.by_name("archive/data.warc.gz")?.compression(),
        CompressionMethod::Stored
    );

    Ok(())
}

#[test]
fn create_refuses_an_existing_output() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("test.wacz");
    std::fs::write(&path, b"existing")?;

    assert!(WaczWriter::create(&path).is_err());

    Ok(())
}

#[test]
fn failed_member_write_poisons_writer_and_leaves_no_final_path()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failed.wacz");
    let mut writer = WaczWriter::create(&path)?;

    assert!(!path.exists());
    assert!(
        writer
            .add_resource("partial.bin", FailingReader(false))
            .is_err()
    );
    assert!(matches!(
        writer.add_resource("replacement.bin", &b"replacement"[..]),
        Err(writer::Error::Poisoned)
    ));
    assert!(matches!(
        writer.finish_unchecked(PackageMetadata::default()),
        Err(writer::Error::Poisoned)
    ));
    assert!(!path.exists());

    Ok(())
}

#[test]
fn write_and_open_from_paths() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let warc_path = directory.path().join("data.warc");
    std::fs::write(&warc_path, warc_bytes()?)?;

    let wacz_path = directory.path().join("test.wacz");
    let mut writer = WaczWriter::create(&wacz_path)?;
    assert!(!wacz_path.exists());
    writer.add_warc_from_path(&warc_path)?;
    writer.add_pages(&PageListHeader::default(), [])?;
    writer.finish_unchecked(PackageMetadata::default())?;

    let mut reader = WaczReader::open(&wacz_path)?;

    assert!(reader.verify_fixity()?.is_success());
    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        vec!["archive/data.warc"]
    );

    Ok(())
}

#[test]
fn verify_fixity_reports_missing_and_mismatched_members() -> Result<(), Box<dyn std::error::Error>>
{
    // The manifest lists one absent file and gives the wrong hash for another.
    let manifest = concat!(
        "{\"profile\": \"data-package\", \"wacz_version\": \"1.1.1\", \"resources\": [",
        "{\"name\": \"pages.jsonl\", \"path\": \"pages/pages.jsonl\", ",
        "\"hash\": \"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\", ",
        "\"bytes\": 0}, ",
        "{\"name\": \"missing.warc\", \"path\": \"archive/missing.warc\", ",
        "\"hash\": \"sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\", ",
        "\"bytes\": 0}]}",
    );

    let bytes = zip_of(&[
        ("datapackage.json", manifest.as_bytes()),
        (
            "pages/pages.jsonl",
            "{\"format\": \"json-pages-1.0\", \"id\": \"pages\", \"title\": \"t\"}\n".as_bytes(),
        ),
    ])?;

    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let fixity = reader.verify_fixity()?;

    assert!(!fixity.is_success());
    assert_eq!(fixity.mismatched, vec!["pages/pages.jsonl"]);
    assert_eq!(fixity.missing, vec!["archive/missing.warc"]);

    Ok(())
}

/// Plain indexes are sorted by rendered line and deduplicated, matching the `ZipNum` behavior (and
/// `py-wacz`).
#[test]
fn plain_index_is_sorted_and_deduplicated() -> Result<(), Box<dyn std::error::Error>> {
    let urls = [
        "https://www.example.com/page2",
        "https://www.example.com/page0",
        "https://www.example.com/page1",
        "https://www.example.com/page1",
    ];
    let items = urls
        .iter()
        .map(|url| item_for(url))
        .collect::<Result<Vec<_>, cdxj::Error>>()?;

    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    let conforming = conforming_items(&items)?;
    writer.add_index("index.cdx", &conforming)?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let read_items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        read_items
            .iter()
            .map(|item| item.key.as_ref())
            .collect::<Vec<_>>(),
        vec![
            "com,example,www)/page0",
            "com,example,www)/page1",
            "com,example,www)/page2",
        ]
    );

    Ok(())
}

/// An index written with no items is still readable in both formats, and a `ZipNum` summary holds
/// only its `!meta` line.
#[test]
fn empty_indexes_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let no_items = std::iter::empty::<&cdxj::ConformingItem<'static>>();

    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    writer.add_index("index.cdx", no_items.clone())?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    assert!(
        reader
            .index("indexes/index.cdx")?
            .collect::<Result<Vec<_>, _>>()?
            .is_empty()
    );
    assert!(reader.verify_fixity()?.is_success());

    let config = WriterConfig {
        index_format: IndexFormat::ZipNum { lines: 2 },
        ..WriterConfig::default()
    };
    let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
    writer.add_index("index.cdx", no_items)?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    let mut archive = zip::ZipArchive::new(Cursor::new(&wacz))?;
    let mut summary = String::new();
    archive
        .by_name("indexes/index.idx")?
        .read_to_string(&mut summary)?;

    assert_eq!(summary.lines().count(), 1);
    assert!(summary.starts_with("!meta 0 "));

    // The data file still contains a valid empty gzip stream.
    let mut reader = WaczReader::new(Cursor::new(&wacz))?;
    assert!(
        reader
            .index("indexes/index.cdx.gz")?
            .collect::<Result<Vec<_>, _>>()?
            .is_empty()
    );
    assert!(reader.verify_fixity()?.is_success());

    Ok(())
}

/// A `{` is legal unencoded in a URL query string, so a `ZipNum` summary prefix must end at the
/// second space-separated field rather than at the first brace.
#[test]
fn zipnum_summary_prefixes_survive_braces_in_keys() -> Result<(), Box<dyn std::error::Error>> {
    let item = item_for("https://example.com/?a={b}")?;

    let config = WriterConfig {
        index_format: IndexFormat::zipnum(),
        ..WriterConfig::default()
    };
    let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
    writer.add_index("index.cdx", [&cdxj::ConformingItem::try_from(&item)?])?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    let mut archive = zip::ZipArchive::new(Cursor::new(&wacz))?;
    let mut summary = String::new();
    archive
        .by_name("indexes/index.idx")?
        .read_to_string(&mut summary)?;

    let summary_lines = summary.lines().collect::<Vec<_>>();

    assert_eq!(summary_lines.len(), 2);
    assert!(summary_lines[1].starts_with("com,example)/?a={b} 20201007212236 {\"offset\": "));

    drop(archive);
    let mut reader = WaczReader::new(Cursor::new(&wacz))?;
    let captures = reader.lookup("https://example.com/?a={b}", ..)?;
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].item.key, "com,example)/?a={b}");

    Ok(())
}

/// The convenience constructor uses the `py-wacz` standard block size.
#[test]
fn zipnum_default_block_size() {
    assert_eq!(IndexFormat::zipnum(), IndexFormat::ZipNum { lines: 1024 });
}

/// A page list written under a custom name round trips through `page_list`, including its header
/// properties.
#[test]
fn named_page_lists_round_trip() -> Result<(), Box<dyn std::error::Error>> {
    let page = Page {
        url: Cow::Borrowed(URL),
        ts: capture_time(),
        id: Some(Cow::Borrowed("extra-page-id")),
        title: None,
        text: None,
        size: None,
        extra: ExtraProperties::default(),
    };
    let header = PageListHeader {
        id: Some(Cow::Borrowed("extra-pages")),
        title: Some(Cow::Borrowed("Extra Pages")),
        ..PageListHeader::default()
    };

    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    writer.add_page_list("extraPages.jsonl", &header, [&page])?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let list = reader.page_list("pages/extraPages.jsonl")?;

    assert_eq!(list.header().id.as_deref(), Some("extra-pages"));
    assert_eq!(list.header().title.as_deref(), Some("Extra Pages"));

    let read_pages = list.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(read_pages.len(), 1);
    assert_eq!(read_pages[0].id.as_deref(), Some("extra-page-id"));

    Ok(())
}

/// Assigning a synthetic identifier preserves a page's additional properties.
#[test]
fn synthetic_ids_preserve_extra_properties() -> Result<(), Box<dyn std::error::Error>> {
    let mut extra = serde_json::Map::new();
    extra.insert("custom".to_owned(), serde_json::Value::Bool(true));

    let page = Page {
        url: Cow::Borrowed(URL),
        ts: capture_time(),
        id: None,
        title: None,
        text: None,
        size: None,
        extra: ExtraProperties::from(extra),
    };

    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    writer.add_pages(&PageListHeader::default(), [&page])?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let read_pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;

    assert_eq!(read_pages[0].id.as_deref().map(str::len), Some(24));
    assert_eq!(
        read_pages[0].extra.get("custom"),
        Some(&serde_json::Value::Bool(true))
    );

    Ok(())
}

/// A custom file added outside the reserved directories is recorded in the manifest and verifies.
#[test]
fn custom_resources_are_recorded_and_verified() -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    writer.add_resource("extra/notes.txt", &b"notes"[..])?;
    let wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let package = reader.data_package()?;

    assert_eq!(package.resources.len(), 1);
    assert_eq!(package.resources[0].path, "extra/notes.txt");
    assert_eq!(package.resources[0].name, "notes.txt");
    assert_eq!(package.resources[0].bytes, 5);
    assert!(reader.verify_fixity()?.is_success());

    Ok(())
}

/// A path without a UTF-8 file name cannot be added under `archive/`.
#[test]
fn add_warc_from_path_requires_a_usable_file_name() {
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));

    assert!(matches!(
        writer.add_warc_from_path("/"),
        Err(writer::Error::InvalidFileName(_))
    ));
}

/// Paths that escape the WACZ, name directories, or repeat existing files are rejected.
#[test]
fn member_paths_are_validated() -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    writer.add_warc("data.warc", &b""[..])?;
    let item = item_for(URL)?;

    assert!(matches!(
        writer.add_warc("data.warc", &b""[..]),
        Err(writer::Error::DuplicateMemberPath(path)) if path == "archive/data.warc"
    ));
    assert!(matches!(
        writer.add_resource("datapackage.json", &b""[..]),
        Err(writer::Error::DuplicateMemberPath(_))
    ));
    assert!(matches!(
        writer.add_warc("../evil.warc", &b""[..]),
        Err(writer::Error::InvalidMemberPath(path)) if path == "archive/../evil.warc"
    ));
    for name in ["nested/index.cdx", "index.cdx.gz", "index.idx", "index"] {
        assert!(
            matches!(
                writer.add_index_lenient(name, [&item]),
                Err(writer::Error::InvalidIndexName(value)) if value == name
            ),
            "{name:?} should be rejected"
        );
    }

    for path in ["/absolute.txt", "dir\\file.txt", "trailing/", "", "./x.txt"] {
        assert!(
            matches!(
                writer.add_resource(path, &b""[..]),
                Err(writer::Error::InvalidMemberPath(_))
            ),
            "{path:?} should be rejected"
        );
    }

    Ok(())
}

#[test]
fn validate_rejects_mistyped_required_member_paths() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = zip_of(&[
        ("datapackage.json", EMPTY_MANIFEST.as_bytes()),
        ("pages/pages.jsonl", b"{}\n"),
        ("archive/not-a-warc.txt", b""),
        ("indexes/not-an-index.txt", b""),
        ("indexes/orphan.cdx.gz", b""),
    ])?;
    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let report = reader.validate(reader::ValidationOptions::default())?;

    assert!(
        report
            .layout
            .contains(&reader::LayoutProblem::InvalidWarcMember(
                "archive/not-a-warc.txt".to_owned()
            ))
    );
    assert!(
        report
            .layout
            .contains(&reader::LayoutProblem::InvalidIndexMember(
                "indexes/not-an-index.txt".to_owned()
            ))
    );
    assert!(
        report
            .layout
            .contains(&reader::LayoutProblem::InvalidIndexMember(
                "indexes/orphan.cdx.gz".to_owned()
            ))
    );
    assert!(
        report
            .layout
            .contains(&reader::LayoutProblem::NoWarcMembers)
    );
    assert!(
        report
            .layout
            .contains(&reader::LayoutProblem::NoIndexMembers)
    );

    Ok(())
}

#[test]
fn writer_rejects_nonconforming_layout_and_reserved_custom_resources() {
    let writer = WaczWriter::new(Cursor::new(Vec::new()));
    assert!(matches!(
        writer.finish(PackageMetadata::default()),
        Err(writer::Error::MissingRequiredMembers(_))
    ));

    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    assert!(matches!(
        writer.add_resource("archive/not-a-warc.txt", &b"data"[..]),
        Err(writer::Error::ReservedResourcePath(_))
    ));
}

#[test]
fn writer_rejects_invalid_warc_names_and_gzip_streams() {
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    assert!(matches!(
        writer.add_warc("data.bin", &b"data"[..]),
        Err(writer::Error::InvalidWarcName(_))
    ));
    assert!(matches!(
        writer.add_warc("data.warc.gz", &b"not gzip"[..]),
        Err(writer::Error::InvalidGzip(_))
    ));
}

#[test]
fn normal_index_writing_requires_normative_fields() -> Result<(), Box<dyn std::error::Error>> {
    let mut item = item_for(URL)?;
    let mut conforming = cdxj::ConformingItem::try_from(&item)?;
    conforming
        .fields
        .extra
        .insert("offset".to_owned(), serde_json::Value::from(1));
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));
    assert!(matches!(
        writer.add_index("index.cdx", [&conforming]),
        Err(writer::Error::ExtraProperty(_))
    ));

    item.fields.digest = None;
    let mut writer = WaczWriter::new(Cursor::new(Vec::new()));

    assert!(cdxj::ConformingItem::try_from(&item).is_err());
    writer.add_index_lenient("index.cdx", [&item])?;
    Ok(())
}

/// Requesting an absent file reports its path rather than an opaque ZIP error.
#[test]
fn missing_members_are_reported() -> Result<(), Box<dyn std::error::Error>> {
    let wacz = build_wacz("data.warc", &warc_bytes()?)?;
    let mut reader = WaczReader::new(Cursor::new(wacz))?;

    assert!(matches!(
        reader.warc("archive/absent.warc"),
        Err(reader::Error::MissingMember(path)) if path == "archive/absent.warc"
    ));

    Ok(())
}

/// The digest file is only recommended by the specification, so its absence is not an error.
#[test]
fn absent_digest_files_read_as_none() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = zip_of(&[("datapackage.json", EMPTY_MANIFEST.as_bytes())])?;
    let mut reader = WaczReader::new(Cursor::new(bytes))?;

    assert!(reader.data_package_digest()?.is_none());
    assert!(reader.verify_fixity()?.is_success());

    Ok(())
}

/// A corrupt stored file (whose bytes no longer match the ZIP checksum) is reported as mismatched
/// rather than failing verification with an error.
#[test]
fn verify_fixity_reports_corrupt_members() -> Result<(), Box<dyn std::error::Error>> {
    let mut wacz = build_wacz("data.warc", &warc_bytes()?)?;

    // The WARC file uses STORE, so its contents appear literally in the ZIP exactly once; every
    // other file uses DEFLATE.
    let needle = b"<html>hello";
    let position = wacz
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("stored WARC body should appear in the container");
    wacz[position] ^= 0x01;

    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let fixity = reader.verify_fixity()?;

    assert!(!fixity.is_success());
    assert_eq!(fixity.mismatched, vec!["archive/data.warc"]);
    assert!(fixity.missing.is_empty());

    Ok(())
}

/// A digest file that cannot be parsed cannot corroborate the manifest, so the manifest is reported
/// as mismatched.
#[test]
fn verify_fixity_reports_unparseable_digest_files() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = zip_of(&[
        ("datapackage.json", EMPTY_MANIFEST.as_bytes()),
        ("datapackage-digest.json", b"not json".as_slice()),
    ])?;

    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let fixity = reader.verify_fixity()?;

    assert!(!fixity.is_success());
    assert_eq!(fixity.mismatched, vec!["datapackage.json"]);

    Ok(())
}

/// A digest file naming a path other than `datapackage.json` does not corroborate the manifest,
/// even when its hash matches.
#[test]
fn verify_fixity_rejects_digests_naming_another_path() -> Result<(), Box<dyn std::error::Error>> {
    let digest = format!(
        r#"{{"path": "other.json", "hash": "{}"}}"#,
        Sha256Digest::compute(EMPTY_MANIFEST.as_bytes())
    );
    let bytes = zip_of(&[
        ("datapackage.json", EMPTY_MANIFEST.as_bytes()),
        ("datapackage-digest.json", digest.as_bytes()),
    ])?;

    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let fixity = reader.verify_fixity()?;

    assert!(!fixity.is_success());
    assert_eq!(fixity.mismatched, vec!["datapackage.json"]);

    Ok(())
}

/// ZIP directory entries under the reserved prefixes are not returned as file paths.
#[test]
fn directory_entries_are_not_member_paths() -> Result<(), Box<dyn std::error::Error>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    zip.add_directory("archive/subdir", options)?;
    zip.add_directory("indexes", options)?;
    zip.start_file("archive/data.warc", options)?;
    zip.write_all(&warc_bytes()?)?;
    let bytes = zip.finish()?.into_inner();

    let reader = WaczReader::new(Cursor::new(bytes))?;

    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        vec!["archive/data.warc"]
    );
    assert_eq!(reader.index_paths().count(), 0);

    Ok(())
}

/// A writer-produced WACZ passes every validation layer.
#[test]
fn validate_passes_for_conforming_archives() -> Result<(), Box<dyn std::error::Error>> {
    let wacz = build_wacz("data.warc", &warc_bytes()?)?;
    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let report = reader.validate(reader::ValidationOptions::all())?;

    assert!(report.is_conformant());
    assert!(report.layout.is_empty());
    assert!(report.manifest.is_empty());
    assert_eq!(report.signature, reader::SignatureStatus::Unsigned);
    assert_eq!(report.content, Some(Vec::new()));
    assert_eq!(report.index, Some(Vec::new()));
    assert!(report.fixity.is_some_and(|fixity| fixity.is_success()));

    Ok(())
}

/// Absent required members are reported by the always-on layout layer, and unselected layers stay
/// `None` so they cannot be mistaken for clean results.
#[test]
fn validate_reports_missing_required_members() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = zip_of(&[("datapackage.json", EMPTY_MANIFEST.as_bytes())])?;
    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let report = reader.validate(reader::ValidationOptions::default())?;

    assert!(!report.is_conformant());
    assert_eq!(
        report.layout,
        vec![
            reader::LayoutProblem::MissingPages,
            reader::LayoutProblem::NoWarcMembers,
            reader::LayoutProblem::NoIndexMembers,
        ]
    );
    assert_eq!(report.manifest, vec![reader::ManifestProblem::NoResources]);
    assert_eq!(report.signature, reader::SignatureStatus::Absent);
    assert_eq!(report.fixity, None);
    assert_eq!(report.content, None);
    assert_eq!(report.index, None);

    // Without a manifest, the fixity layer is skipped even when selected, since there are no
    // declared digests to check.
    let bytes = zip_of(&[])?;
    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let report = reader.validate(reader::ValidationOptions {
        fixity: true,
        ..reader::ValidationOptions::default()
    })?;

    assert_eq!(
        report.layout.first(),
        Some(&reader::LayoutProblem::MissingDataPackage)
    );
    assert!(report.manifest.is_empty());
    assert_eq!(report.fixity, None);

    Ok(())
}

#[test]
fn validate_reports_duplicate_zip_member_names() -> Result<(), Box<dyn std::error::Error>> {
    let first = b"duplicate-a.txt";
    let second = b"duplicate-b.txt";
    let mut bytes = zip_of(&[
        ("duplicate-a.txt", b"first"),
        ("duplicate-b.txt", b"second"),
    ])?;
    let positions = bytes
        .windows(second.len())
        .enumerate()
        .filter_map(|(position, value)| (value == second).then_some(position))
        .collect::<Vec<_>>();
    for position in positions {
        bytes[position..position + first.len()].copy_from_slice(first);
    }
    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let report = reader.validate(Default::default())?;

    assert!(
        report
            .layout
            .contains(&reader::LayoutProblem::DuplicateMember(
                "duplicate-a.txt".to_owned()
            ))
    );
    assert!(!report.is_conformant());

    Ok(())
}

/// Manifest conformance violations are reported individually and in declaration order.
#[test]
fn validate_reports_manifest_problems() -> Result<(), Box<dyn std::error::Error>> {
    const EMPTY_HASH: &str =
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let resource = |name: &str, path: &str| {
        format!(r#"{{"name": "{name}", "path": "{path}", "hash": "{EMPTY_HASH}", "bytes": 0}}"#)
    };
    let manifest = format!(
        r#"{{"profile": "wacz", "wacz_version": "1.2.0", "resources": [{}, {}, {}, {}, {}]}}"#,
        resource("Pages.JSONL", "pages/pages.jsonl"),
        resource("pages.jsonl", "pages/pages.jsonl"),
        resource("pages.jsonl", "archive/missing.warc"),
        resource("datapackage.json", "datapackage.json"),
        resource("escape", "../escape"),
    );
    let bytes = zip_of(&[
        ("datapackage.json", manifest.as_bytes()),
        ("pages/pages.jsonl", b"placeholder".as_slice()),
        ("extra/notes.txt", b"notes".as_slice()),
    ])?;

    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let report = reader.validate(reader::ValidationOptions::default())?;

    assert_eq!(
        report.manifest,
        vec![
            reader::ManifestProblem::Profile("wacz".to_owned()),
            reader::ManifestProblem::WaczVersion("1.2.0".to_owned()),
            reader::ManifestProblem::InvalidResourceName("Pages.JSONL".to_owned()),
            reader::ManifestProblem::DuplicateResourcePath("pages/pages.jsonl".to_owned()),
            reader::ManifestProblem::DuplicateResourceName("pages.jsonl".to_owned()),
            reader::ManifestProblem::MissingResourceMember("archive/missing.warc".to_owned()),
            reader::ManifestProblem::ReservedResourcePath("datapackage.json".to_owned()),
            reader::ManifestProblem::InvalidResourcePath("../escape".to_owned()),
            reader::ManifestProblem::MissingResourceMember("../escape".to_owned()),
            reader::ManifestProblem::UnlistedMember("extra/notes.txt".to_owned()),
        ]
    );

    // An unparseable manifest is reported rather than failing validation.
    let bytes = zip_of(&[("datapackage.json", b"not json".as_slice())])?;
    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let report = reader.validate(reader::ValidationOptions::default())?;

    assert!(matches!(
        report.manifest.as_slice(),
        [reader::ManifestProblem::Unparseable(_)]
    ));

    Ok(())
}

/// The signature layer distinguishes unsigned, unverified, and internally inconsistent digest
/// files, without attempting cryptographic verification.
#[test]
fn validate_reports_signature_status() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_hash = Sha256Digest::compute(EMPTY_MANIFEST.as_bytes());
    let signed = |signed_hash: Sha256Digest| {
        format!(
            concat!(
                r#"{{"path": "datapackage.json", "hash": "{}", "signedData": {{"#,
                r#""hash": "{}", "created": "2020-10-07T21:22:36Z", "#,
                r#""software": "example-signer", "version": "0.1.0", "#,
                r#""signature": "c2ln", "publicKey": "a2V5"}}}}"#,
            ),
            manifest_hash, signed_hash,
        )
    };
    let validate = |digest: &str| -> Result<reader::SignatureStatus, Box<dyn std::error::Error>> {
        let bytes = zip_of(&[
            ("datapackage.json", EMPTY_MANIFEST.as_bytes()),
            ("datapackage-digest.json", digest.as_bytes()),
        ])?;
        let mut reader = WaczReader::new(Cursor::new(bytes))?;
        Ok(reader
            .validate(reader::ValidationOptions::default())?
            .signature)
    };

    // A consistent signature is reported as present but not cryptographically checked.
    assert_eq!(
        validate(&signed(manifest_hash))?,
        reader::SignatureStatus::Unverified
    );

    // A signature covering a different hash invalidates the digest file.
    let other = Sha256Digest::compute(b"other");
    assert_eq!(
        validate(&signed(other))?,
        reader::SignatureStatus::Invalid(vec![reader::SignatureProblem::SignedHash {
            declared: manifest_hash,
            signed: other,
        }])
    );

    // A declared hash that does not match the manifest bytes invalidates the digest file.
    assert_eq!(
        validate(&format!(
            r#"{{"path": "datapackage.json", "hash": "{other}"}}"#
        ))?,
        reader::SignatureStatus::Invalid(vec![reader::SignatureProblem::ManifestHash {
            declared: other,
            computed: manifest_hash,
        }])
    );

    // An unparseable digest file is reported rather than failing validation.
    assert!(matches!(
        validate("not json")?,
        reader::SignatureStatus::Invalid(problems)
            if matches!(problems.as_slice(), [reader::SignatureProblem::Unparseable(_)])
    ));

    Ok(())
}

/// The content layer reports the first parse failure in each page list, index, and WARC member.
#[test]
fn validate_reports_content_problems() -> Result<(), Box<dyn std::error::Error>> {
    let page0 = item_for("https://www.example.com/page0")?;
    let page1 = item_for("https://www.example.com/page1")?;
    let unsorted_index = format!("{page1}\n{page0}\n");

    let bytes = stored_zip_of(&[
        ("datapackage.json", EMPTY_MANIFEST.as_bytes()),
        (
            "pages/pages.jsonl",
            "{\"format\": \"json-pages-1.0\", \"id\": \"pages\", \"title\": \"t\"}\n".as_bytes(),
        ),
        ("pages/bad.jsonl", b"not a page list".as_slice()),
        ("indexes/unsorted.cdx", unsorted_index.as_bytes()),
        ("indexes/bad.cdx", b"garbage\n".as_slice()),
        ("indexes/bad.idx", b"garbage\n".as_slice()),
        (
            "archive/truncated.warc",
            b"WARC/1.1\r\nWARC-Type: response\r\n".as_slice(),
        ),
    ])?;

    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let report = reader.validate(reader::ValidationOptions {
        content: true,
        ..reader::ValidationOptions::default()
    })?;
    let content = report.content.expect("content layer should run");

    assert_eq!(content.len(), 5);
    assert!(content.iter().any(|problem| matches!(
        problem,
        reader::ContentProblem::Pages { path, .. } if path == "pages/bad.jsonl"
    )));
    assert!(content.iter().any(|problem| matches!(
        problem,
        reader::ContentProblem::IndexOrder { path } if path == "indexes/unsorted.cdx"
    )));
    assert!(content.iter().any(|problem| matches!(
        problem,
        reader::ContentProblem::Index { path, .. } if path == "indexes/bad.cdx"
    )));
    assert!(content.iter().any(|problem| matches!(
        problem,
        reader::ContentProblem::ZipNum { path, .. } if path == "indexes/bad.idx"
    )));
    assert!(content.iter().any(|problem| matches!(
        problem,
        reader::ContentProblem::Warc { path, .. } if path == "archive/truncated.warc"
    )));

    Ok(())
}

/// The index layer resolves each entry to its record, reporting digest mismatches and ranges that
/// do not resolve while accepting entries that do.
#[test]
fn validate_resolves_index_entries_to_records() -> Result<(), Box<dyn std::error::Error>> {
    let warc = warc_bytes()?;
    let length = warc.len() as u64;

    let good = resolvable_item("https://www.example.com/page0", "data.warc", length)?;
    let mut bad_digest = resolvable_item("https://www.example.com/page1", "data.warc", length)?;
    bad_digest.fields.record_digest = Some(Sha256Digest::compute(b"wrong"));
    let out_of_bounds = resolvable_item("https://www.example.com/page2", "data.warc", length + 10)?;

    let index = format!("{good}\n{bad_digest}\n{out_of_bounds}\n");
    let bytes = stored_zip_of(&[
        ("datapackage.json", EMPTY_MANIFEST.as_bytes()),
        ("archive/data.warc", &warc),
        ("indexes/index.cdx", index.as_bytes()),
    ])?;

    let mut reader = WaczReader::new(Cursor::new(bytes))?;
    let report = reader.validate(reader::ValidationOptions {
        index: true,
        ..reader::ValidationOptions::default()
    })?;

    // The index layer implies the content layer, which finds nothing wrong here.
    assert_eq!(report.content, Some(Vec::new()));

    let problems = report.index.expect("index layer should run");

    assert_eq!(problems.len(), 2);
    assert!(matches!(
        &problems[0],
        reader::IndexProblem::Capture { index_path, key, message, .. }
            if index_path == "indexes/index.cdx"
                && key == "com,example,www)/page1"
                && message.contains("digest mismatch")
    ));
    assert!(matches!(
        &problems[1],
        reader::IndexProblem::Capture { key, message, .. }
            if key == "com,example,www)/page2" && message.contains("outside member")
    ));

    Ok(())
}

/// A `ZipNum` block whose stored bytes no longer match the summary's digest is reported, while
/// entries in intact blocks still resolve.
#[test]
fn validate_reports_corrupt_zipnum_blocks() -> Result<(), Box<dyn std::error::Error>> {
    let warc = warc_bytes()?;
    let length = warc.len() as u64;
    let items = (0..4)
        .map(|i| {
            resolvable_item(
                &format!("https://www.example.com/page{i}"),
                "data.warc",
                length,
            )
        })
        .collect::<Result<Vec<_>, cdxj::Error>>()?;

    let config = WriterConfig {
        index_format: IndexFormat::ZipNum { lines: 2 },
        ..WriterConfig::default()
    };
    let mut writer = WaczWriter::with_config(Cursor::new(Vec::new()), config);
    let conforming = conforming_items(&items)?;
    writer.add_index("index.cdx", &conforming)?;
    writer.add_warc("data.warc", warc.as_slice())?;
    let mut wacz = writer
        .finish_unchecked(PackageMetadata::default())?
        .into_inner();

    // Locate the first block's stored bytes and flip its gzip header OS byte, which changes the
    // block digest without invalidating the gzip framing.
    let mut reader = WaczReader::new(Cursor::new(wacz.clone()))?;
    let summary = reader.zipnum_summary("indexes/index.idx")?;

    assert_eq!(summary.blocks.len(), 2);

    let block = summary.blocks[0].clone();
    let stored = reader.member_range(&block.data_path, block.offset, block.length)?;
    let position = wacz
        .windows(stored.len())
        .position(|window| window == stored)
        .expect("stored block bytes should appear in the container");
    wacz[position + 9] ^= 0xff;

    let mut reader = WaczReader::new(Cursor::new(wacz))?;
    let report = reader.validate(reader::ValidationOptions {
        index: true,
        ..reader::ValidationOptions::default()
    })?;
    let problems = report.index.expect("index layer should run");

    assert_eq!(problems.len(), 1);
    assert!(matches!(
        &problems[0],
        reader::IndexProblem::Block { summary_path, offset, message }
            if summary_path == "indexes/index.idx"
                && *offset == 0
                && message.contains("digest mismatch")
    ));

    Ok(())
}
