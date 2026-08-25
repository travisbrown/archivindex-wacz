//! The `warcinfo` and `metadata` records authored by the archiver.

use std::time::Duration;

use archivindex_warc::record::Record;
use archivindex_warc::record::fields::dcmi::DcmiTerm;
use archivindex_warc::record::fields::metadata::MetadataField;
use archivindex_warc::record::fields::warcinfo::WarcinfoField;
use archivindex_warc::value::WarcDate;
use chrono::Utc;
use fluent_uri::Uri;

use super::outcome::DATE_PRECISION;
use crate::Error;
use crate::session::{Operator, Software};

/// Information recorded in the WARC file's initial `warcinfo` record.
pub struct WarcinfoOptions<'a> {
    pub user_agent: &'a str,
    pub software: Option<&'a Software>,
    pub operator: Option<&'a Operator>,
    pub session_id: Option<&'a str>,
    pub title: Option<&'a str>,
}

impl<'a> WarcinfoOptions<'a> {
    /// Options for a one-shot run: this crate as software, with no operator or session.
    pub const fn archiver(user_agent: &'a str) -> Self {
        Self {
            user_agent,
            software: None,
            operator: None,
            session_id: None,
            title: None,
        }
    }
}

/// Values recorded in the `warc-fields` metadata accompanying one capture.
#[derive(Clone, Copy)]
pub struct MetadataValues<'a> {
    pub fetch_time: Duration,
    pub via: Option<&'a str>,
    pub title: Option<&'a str>,
}

/// Build the `warcinfo` record at the start of a WARC file.
///
/// `software` and `http-header-user-agent` are always included.
pub fn warcinfo_record(warc_name: &str, options: &WarcinfoOptions<'_>) -> Result<Record, Error> {
    let software = options.software.cloned().unwrap_or_default();
    let mut builder = Record::warcinfo(WarcDate::new(Utc::now(), DATE_PRECISION))
        .filename(warc_name)
        .expect("well-formed WARC file name")
        .software(&software.name, &software.version)?;
    if let Some(operator) = options.operator {
        builder = builder.operator(&operator.name, operator.email.as_deref())?;
    }
    builder = builder.http_header_user_agent(options.user_agent)?;
    if let Some(session_id) = options.session_id {
        builder = builder.is_part_of(session_id)?;
    }
    if let Some(title) = options.title {
        builder = builder.field(WarcinfoField::Dcmi(DcmiTerm::Title), title)?;
    }

    Ok(builder.build())
}

/// Build the metadata record linked to one captured response or revisit.
pub fn metadata_record(
    date: WarcDate,
    target_uri: Uri<String>,
    record_id: Uri<String>,
    warcinfo_id: &Uri<String>,
    values: MetadataValues<'_>,
) -> Result<Record, Error> {
    let mut builder = Record::metadata(date)
        .target_uri(target_uri)
        .concurrent_to(record_id)
        .warcinfo_id(warcinfo_id.clone());
    if let Some(via) = values.via {
        builder = builder.via(via)?;
    }
    builder = builder.fetch_time_ms(values.fetch_time);
    if let Some(title) = values.title {
        builder = builder.field(MetadataField::Dcmi(DcmiTerm::Title), title)?;
    }

    Ok(builder.build())
}
