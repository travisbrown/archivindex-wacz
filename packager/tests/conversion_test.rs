//! End-to-end conversion of plain and continuously gzip-compressed WARC files.

use std::io::{Read, Write};

use archivindex_packager::{
    Capture, ConversionWarning, Error, PageTitleGenerator, SkipReason, WarcToWacz,
};
use archivindex_wacz::io::read::WaczReader;
use archivindex_wacz::io::read::validate::ValidationOptions;
use archivindex_warc::io::write::WarcWriter;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::metadata::MetadataBody;
use archivindex_warc::record::fields::warcinfo::WarcinfoBody;
use archivindex_warc::record::header::RevisitProfile;
use archivindex_warc::record::header::truncated_type::TruncatedType;
use archivindex_warc::record::{FieldsBlock, Record};
use archivindex_warc::value::{Algorithm, LabelledDigest, MediaType, WarcDate};
use chrono::Utc;
use flate2::Compression;
use flate2::write::GzEncoder;

struct PayloadTitle;

impl PageTitleGenerator for PayloadTitle {
    fn title(&mut self, capture: &Capture<'_>) -> Option<String> {
        Some(format!(
            "Generated: {}",
            String::from_utf8_lossy(capture.payload)
        ))
    }
}

fn response(url: &str, body: &str, date: WarcDate) -> Record {
    let message = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    Record::<NoExtension>::response(url, date)
        .expect("test response")
        .body(message.into_bytes())
        .expect("test response body")
}

fn metadata(response: &Record, fields: &[u8], date: WarcDate) -> Record {
    let fields = MetadataBody::parse(fields).expect("test metadata fields");
    let mut record = Record::<NoExtension>::metadata(date)
        .target_uri(response.target_uri().expect("response target").clone())
        .concurrent_to(response.core().record_id.clone())
        .build();
    let Record::Metadata { header, body } = &mut record else {
        unreachable!("metadata builder returned another record type");
    };
    header.core.content_length = Some(fields.rendered_len() as u64);
    *body = FieldsBlock::Fields(fields);
    record
}

fn warcinfo(fields: &[u8], date: WarcDate) -> Record {
    let fields = WarcinfoBody::parse(fields).expect("test warcinfo fields");
    let mut record = Record::<NoExtension>::warcinfo(date).build();
    let Record::Warcinfo { header, body } = &mut record else {
        unreachable!("warcinfo builder returned another record type");
    };
    header.core.content_length = Some(fields.rendered_len() as u64);
    *body = FieldsBlock::Fields(fields);
    record
}

fn write_source(path: &std::path::Path, records: Vec<Record>, gzip: bool) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    let mut writer = WarcWriter::new(&mut bytes);
    for record in records {
        writer
            .write(&record.into_raw().expect("render test record"))
            .expect("write test record");
    }
    writer.flush()?;

    if gzip {
        let mut encoder = GzEncoder::new(std::fs::File::create(path)?, Compression::default());
        encoder.write_all(&bytes)?;
        encoder.finish()?;
    } else {
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

fn conversion_fixture(
    input_gzip: bool,
    gzip_warc: bool,
    gzip_compression_level: Option<u32>,
    expected_xfl: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let input_name = if input_gzip {
        "input.warc.gz"
    } else {
        "input.warc"
    };
    let input = directory.path().join(input_name);
    let output = directory.path().join("output.wacz");
    let date = WarcDate::from(Utc::now());
    let first_warcinfo = warcinfo(
        b"title: First collection\r\ndescription: First description\r\n",
        date,
    );
    let second_warcinfo = warcinfo(
        b"title: Ignored collection\r\ndescription: Ignored description\r\n",
        date,
    );
    let second_warcinfo_id = second_warcinfo.core().record_id.as_str().to_owned();
    let third_warcinfo = warcinfo(b"title: Also ignored\r\n", date);
    let third_warcinfo_id = third_warcinfo.core().record_id.as_str().to_owned();
    let root = response("https://example.com/", "root", date);
    let root_metadata = metadata(&root, b"title: Recorded root\r\n", date);
    let extra = response("https://example.com/extra", "extra", date);
    let extra_metadata = metadata(&extra, b"via: https://example.com/\r\n", date);
    write_source(
        &input,
        vec![
            first_warcinfo,
            second_warcinfo,
            third_warcinfo,
            root,
            root_metadata,
            extra,
            extra_metadata,
        ],
        input_gzip,
    )?;

    let mut conversion = WarcToWacz::new(&input, &output)
        .title_generator(PayloadTitle)
        .gzip_warc(gzip_warc);
    if let Some(level) = gzip_compression_level {
        conversion = conversion.gzip_compression_level(level);
    }
    let summary = conversion.run()?;
    assert_eq!(summary.records, 7);
    assert_eq!(summary.captures, 2);
    assert_eq!(summary.pages, 2);
    assert_eq!(
        summary.warnings,
        [ConversionWarning::MultipleWarcinfo {
            count: 3,
            duplicate_record_ids: vec![second_warcinfo_id, third_warcinfo_id],
        }]
    );

    let mut reader = WaczReader::open(&output)?;
    let validation = reader.validate(ValidationOptions::all())?;
    assert!(validation.is_conformant(), "{validation:#?}");
    assert_eq!(
        reader.warc_paths().collect::<Vec<_>>(),
        [if input_gzip || gzip_warc {
            "archive/data.warc.gz"
        } else {
            "archive/data.warc"
        }]
    );
    if input_gzip || gzip_warc {
        assert_independent_gzip_members(&mut reader, summary.records, expected_xfl)?;
    }
    let package = reader.data_package()?;
    assert_eq!(package.title.as_deref(), Some("First collection"));
    assert_eq!(package.description.as_deref(), Some("First description"));

    let pages = reader.pages()?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].url, "https://example.com/");
    assert_eq!(pages[0].title.as_deref(), Some("Recorded root"));

    let extra_pages = reader
        .page_list("pages/extraPages.jsonl")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(extra_pages.len(), 1);
    assert_eq!(extra_pages[0].url, "https://example.com/extra");
    assert_eq!(extra_pages[0].title.as_deref(), Some("Generated: extra"));

    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(items.len(), 2);
    for item in &items {
        assert_eq!(reader.read_capture(&item.fields)?.type_name(), "response");
    }

    Ok(())
}

fn assert_independent_gzip_members(
    reader: &mut WaczReader<std::io::BufReader<std::fs::File>>,
    expected: usize,
    expected_xfl: u8,
) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = reader.member_bytes("archive/data.warc.gz")?;
    let mut remaining = bytes.as_slice();
    let mut members = 0;
    let expected_header = [
        0x1f,
        0x8b,
        8, // CM = DEFLATE
        0, // FLG: FTEXT, FHCRC, FEXTRA, FNAME, and FCOMMENT are absent
        0,
        0,
        0,
        0, // MTIME = 0
        expected_xfl,
        255, // XFL and OS = unknown
    ];

    while !remaining.is_empty() {
        assert!(remaining.starts_with(&expected_header));
        let before = remaining.len();
        let mut decoder = flate2::bufread::GzDecoder::new(remaining);
        let mut record = Vec::new();
        decoder.read_to_end(&mut record)?;
        remaining = decoder.into_inner();
        assert!(remaining.len() < before);
        assert!(record.starts_with(b"WARC/"));
        members += 1;
    }

    assert_eq!(members, expected);
    Ok(())
}

#[test]
fn converts_plain_warc_with_recorded_and_generated_titles() -> Result<(), Box<dyn std::error::Error>>
{
    conversion_fixture(false, false, None, 0)
}

#[test]
fn converts_continuous_gzip_warc_to_random_access_members() -> Result<(), Box<dyn std::error::Error>>
{
    conversion_fixture(true, false, None, 0)
}

#[test]
fn gzips_each_record_from_a_plain_warc_independently() -> Result<(), Box<dyn std::error::Error>> {
    conversion_fixture(false, true, None, 0)
}

#[test]
fn configured_gzip_compression_levels_are_used() -> Result<(), Box<dyn std::error::Error>> {
    conversion_fixture(false, true, Some(0), 4)?;
    conversion_fixture(false, true, Some(1), 4)?;
    conversion_fixture(false, true, Some(9), 2)
}

#[test]
fn rejects_an_invalid_gzip_compression_level() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("input.warc");
    let output = directory.path().join("output.wacz");
    write_source(
        &input,
        vec![warcinfo(b"title: Test\r\n", WarcDate::from(Utc::now()))],
        false,
    )?;

    let error = WarcToWacz::new(&input, &output)
        .gzip_warc(true)
        .gzip_compression_level(10)
        .run()
        .expect_err("level 10 must be rejected");

    assert!(matches!(
        error,
        Error::Wacz(archivindex_wacz::io::write::Error::InvalidGzipCompressionLevel(10))
    ));
    assert!(!output.exists());
    Ok(())
}

#[test]
fn rejects_an_invalid_zip_compression_level() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("input.warc");
    let output = directory.path().join("output.wacz");
    write_source(
        &input,
        vec![warcinfo(b"title: Test\r\n", WarcDate::from(Utc::now()))],
        false,
    )?;

    let error = WarcToWacz::new(&input, &output)
        .zip_compression_level(265)
        .run()
        .expect_err("level 265 must be rejected");

    assert!(matches!(
        error,
        Error::Wacz(archivindex_wacz::io::write::Error::InvalidZipCompressionLevel(265))
    ));
    assert!(!output.exists());
    Ok(())
}

#[test]
fn index_fields_follow_cdxj_conventions() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("input.warc");
    let output = directory.path().join("output.wacz");
    let date = WarcDate::from(Utc::now());
    let html = Record::<NoExtension>::response("https://example.com/html", date)?.body(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: 4\r\n\r\nhtml"
            .to_vec(),
    )?;
    let html_digest =
        LabelledDigest::compute(Algorithm::Sha256, b"html").expect("sha256 is enabled");
    let identified = Record::<NoExtension>::response("https://example.com/identified", date)?
        .identified_payload_type(MediaType::parse(b"image/png")?)
        .body(b"HTTP/1.1 200 OK\r\ncontent-length: 3\r\n\r\npng".to_vec())?;
    let unknown = Record::<NoExtension>::response("https://example.com/unknown", date)?
        .body(b"HTTP/1.1 404 Not Found\r\ncontent-length: 4\r\n\r\ngone".to_vec())?;
    // A truncated response never receives a payload digest from the WARC writer.
    let truncated = Record::<NoExtension>::response("https://example.com/truncated", date)?
        .truncated(TruncatedType::Length)
        .body(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 100\r\n\r\ntr"
                .to_vec(),
        )?;
    let revisit = Record::<NoExtension>::revisit(
        "https://example.com/revisit",
        date,
        RevisitProfile::IDENTICAL_PAYLOAD_DIGEST,
    )?
    .payload_digest(html_digest.clone())
    .refers_to(html.core().record_id.clone())
    .refers_to_target_uri(html.target_uri().expect("response target").clone())
    .refers_to_date(date)
    .truncated(TruncatedType::Length)
    .body(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: 4\r\n\r\n"
            .to_vec(),
    )?;
    write_source(
        &input,
        vec![html, identified, unknown, truncated, revisit],
        false,
    )?;

    let summary = WarcToWacz::new(&input, &output).run()?;

    assert_eq!(summary.captures, 5);
    assert!(summary.warnings.is_empty());
    let mut reader = WaczReader::open(&output)?;
    let validation = reader.validate(ValidationOptions::all())?;
    assert!(validation.is_conformant(), "{validation:#?}");
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    for item in &items {
        assert!(archivindex_wacz::cdxj::ConformingItem::try_from(item).is_ok());
    }
    let fields = |path: &str| {
        let url = format!("https://example.com/{path}");
        &items
            .iter()
            .find(|item| item.fields.url == url)
            .expect("capture is indexed")
            .fields
    };
    assert_eq!(fields("html").mime.as_deref(), Some("text/html"));
    assert_eq!(
        fields("html").digest.as_deref(),
        Some(html_digest.to_string().as_str())
    );
    assert_eq!(fields("identified").mime.as_deref(), Some("image/png"));
    assert_eq!(fields("unknown").mime.as_deref(), Some("unk"));
    assert_eq!(fields("unknown").status, Some(404));
    assert_eq!(
        fields("truncated").digest.as_deref(),
        Some(
            LabelledDigest::compute(Algorithm::Sha256, b"tr")
                .expect("sha256 is enabled")
                .to_string()
                .as_str()
        )
    );
    assert_eq!(fields("revisit").mime.as_deref(), Some("warc/revisit"));
    assert_eq!(
        fields("revisit").digest.as_deref(),
        Some(html_digest.to_string().as_str())
    );

    Ok(())
}

#[test]
fn unparsable_http_message_is_copied_but_not_indexed() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("input.warc");
    let output = directory.path().join("output.wacz");
    let date = WarcDate::from(Utc::now());
    let good = response("https://example.com/", "root", date);
    let bad = Record::<NoExtension>::response("https://example.com/bad", date)?
        .body(b"not an HTTP message".to_vec())?;
    let bad_id = bad.core().record_id.as_str().to_owned();
    write_source(&input, vec![good, bad], false)?;

    let summary = WarcToWacz::new(&input, &output).run()?;

    assert_eq!(summary.records, 2);
    assert_eq!(summary.captures, 1);
    assert_eq!(
        summary.warnings,
        [ConversionWarning::CaptureNotIndexed {
            record_id: bad_id,
            reason: SkipReason::UnparsableHttpMessage,
        }]
    );
    let mut reader = WaczReader::open(&output)?;
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.url, "https://example.com/");
    let copied = reader
        .warc("archive/data.warc")?
        .iter_records::<NoExtension>()
        .count();
    assert_eq!(copied, 2);

    Ok(())
}

/// Source records are copied byte for byte, and only the record types the conversion reads are
/// parsed semantically, so a payload digest missing from the source is computed for the index
/// without being added to the record.
#[test]
fn records_are_copied_verbatim() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("input.warc");
    let output = directory.path().join("output.wacz");
    let date = WarcDate::from(Utc::now());
    let mut request = Record::<NoExtension>::request("https://example.com/", date)?
        .body(b"GET / HTTP/1.1\r\n\r\n".to_vec())?
        .into_raw()?;
    // Without the mandatory `WARC-Date` the request is not a valid semantic record, but it is
    // still a well-formed raw record, and request records are never parsed.
    request
        .header
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("WARC-Date"));
    let mut source = Vec::new();
    let mut writer = WarcWriter::new(&mut source);
    writer.write(&warcinfo(b"title: Verbatim\r\n", date).into_raw()?)?;
    writer.write(&request)?;
    writer.write(&response("https://example.com/", "raw", date).into_raw_without_digests()?)?;
    writer.flush()?;
    std::fs::write(&input, &source)?;

    let summary = WarcToWacz::new(&input, &output).run()?;

    assert_eq!(summary.records, 3);
    assert_eq!(summary.captures, 1);
    assert!(summary.warnings.is_empty());
    let mut reader = WaczReader::open(&output)?;
    assert_eq!(reader.member_bytes("archive/data.warc")?, source);
    let items = reader
        .index("indexes/index.cdx")?
        .collect::<Result<Vec<_>, _>>()?;
    let expected = LabelledDigest::compute(Algorithm::Sha256, b"raw")
        .expect("sha256 digest")
        .to_string();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].fields.digest.as_deref(), Some(expected.as_str()));
    Ok(())
}
