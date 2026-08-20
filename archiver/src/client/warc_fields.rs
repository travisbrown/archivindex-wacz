//! Semantic schemas for the `warc-fields` record bodies authored by the archiver.

use std::time::Duration;

use archivindex_warc::record::fields::metadata::MetadataBody;
use archivindex_warc::record::fields::serde::to_body;
use archivindex_warc::record::fields::warcinfo::WarcinfoBody;
use archivindex_warc::record::{FieldsBlock, Record};
use archivindex_warc::value::{WarcDate, WarcDatePrecision};
use chrono::Utc;
use fluent_uri::Uri;

use super::Error;

const DATE_PRECISION: WarcDatePrecision = WarcDatePrecision::Fraction(6);
const WARC_FORMAT: &str = "WARC file version 1.1";
const WARC_SPECIFICATION: &str =
    "http://iipc.github.io/warc-specifications/specifications/warc-format/warc-1.1/";

/// Information recorded in the WARC file's initial `warcinfo` record.
pub(super) struct WarcinfoOptions<'a> {
    pub(super) user_agent: &'a str,
    pub(super) software: Option<(&'a str, &'a str)>,
    pub(super) operator: Option<(&'a str, Option<&'a str>)>,
    pub(super) session_id: Option<&'a str>,
}

impl<'a> WarcinfoOptions<'a> {
    /// Options for a one-shot run: this crate as software, with no operator or session.
    pub(super) const fn archiver(user_agent: &'a str) -> Self {
        Self {
            user_agent,
            software: None,
            operator: None,
            session_id: None,
        }
    }
}

/// The archiver's schema for the body of its initial `warcinfo` record.
///
/// Non-optional fields are deliberately required by our output format even though WARC permits
/// every individual `warcinfo` field to be omitted.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Warcinfo<'a> {
    format: &'static str,
    conforms_to: &'static str,
    software: String,
    operator: Option<String>,
    #[serde(rename = "http-header-user-agent")]
    http_header_user_agent: &'a str,
    is_part_of: Option<&'a str>,
}

impl<'a> Warcinfo<'a> {
    fn from_options(options: &WarcinfoOptions<'a>) -> Self {
        let (software_name, software_version) = options
            .software
            .unwrap_or((env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")));
        let operator = options.operator.map(|(name, email)| {
            email.map_or_else(|| name.to_owned(), |email| format!("{name} <{email}>"))
        });

        Self {
            format: WARC_FORMAT,
            conforms_to: WARC_SPECIFICATION,
            software: format!("{software_name}/{software_version}"),
            operator,
            http_header_user_agent: options.user_agent,
            is_part_of: options.session_id,
        }
    }
}

/// The archiver's schema for per-capture metadata.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Metadata<'a> {
    via: Option<&'a str>,
    fetch_time_ms: u128,
}

/// Build the `warcinfo` record at the start of a WARC file.
pub(super) fn warcinfo_record(
    warc_name: &str,
    options: &WarcinfoOptions<'_>,
) -> Result<Record, Error> {
    let fields: WarcinfoBody = to_body(&Warcinfo::from_options(options))?;
    let mut record = Record::warcinfo(record_date())
        .filename(warc_name)
        .expect("well-formed WARC file name")
        .build();
    let Record::Warcinfo { header, body } = &mut record else {
        unreachable!("a warcinfo builder returned another record type");
    };
    header.core.content_length = Some(fields.rendered_len() as u64);
    *body = FieldsBlock::Fields(fields);

    Ok(record)
}

/// Build the metadata record linked to one captured response or revisit.
pub(super) fn metadata_record(
    date: WarcDate,
    target_uri: Uri<String>,
    record_id: Uri<String>,
    warcinfo_id: &Uri<String>,
    fetch_time: Duration,
    via: Option<&str>,
) -> Result<Record, Error> {
    let fields: MetadataBody = to_body(&Metadata {
        via,
        fetch_time_ms: fetch_time.as_millis(),
    })?;
    let mut record = Record::metadata(date)
        .target_uri(target_uri)
        .concurrent_to(record_id)
        .warcinfo_id(warcinfo_id.clone())
        .build();
    let Record::Metadata { header, body } = &mut record else {
        unreachable!("a metadata builder returned another record type");
    };
    header.core.content_length = Some(fields.rendered_len() as u64);
    *body = FieldsBlock::Fields(fields);

    Ok(record)
}

fn record_date() -> WarcDate {
    WarcDate::new(Utc::now(), DATE_PRECISION)
}
