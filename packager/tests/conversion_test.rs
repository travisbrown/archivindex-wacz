//! End-to-end conversion of plain and continuously gzip-compressed WARC files.

use std::io::Write;

use archivindex_packager::{Capture, ConversionWarning, PageTitleGenerator, WarcToWacz};
use archivindex_wacz::reader::{ValidationOptions, WaczReader};
use archivindex_warc::io::write::WarcWriter;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::fields::metadata::MetadataBody;
use archivindex_warc::record::fields::warcinfo::WarcinfoBody;
use archivindex_warc::record::{FieldsBlock, Record};
use archivindex_warc::value::WarcDate;
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

fn conversion_fixture(gzip: bool) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let input_name = if gzip { "input.warc.gz" } else { "input.warc" };
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
        gzip,
    )?;

    let summary = WarcToWacz::new(&input, &output)
        .title_generator(PayloadTitle)
        .run()?;
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
        [if gzip {
            "archive/data.warc.gz"
        } else {
            "archive/data.warc"
        }]
    );
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

#[test]
fn converts_plain_warc_with_recorded_and_generated_titles() -> Result<(), Box<dyn std::error::Error>>
{
    conversion_fixture(false)
}

#[test]
fn converts_continuous_gzip_warc_to_random_access_members() -> Result<(), Box<dyn std::error::Error>>
{
    conversion_fixture(true)
}
