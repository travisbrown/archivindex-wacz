//! Mapping captured HTTP exchanges to WARC records and CDXJ entries.

use std::borrow::Cow;
use std::io::Write;

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_warc::io::write::{WarcWriter, Written};
use archivindex_warc::record::capture::CaptureEvent;
use archivindex_warc::record::fields::metadata::MetadataField;
use archivindex_warc::record::{FieldsBlock, Record};
use archivindex_warc::value::{DigestAlgorithm, LabelledDigest, WarcDate, WarcDatePrecision};
use chrono::Utc;
use fluent_uri::Uri;

use super::{Error, Exchange};

const DATE_PRECISION: WarcDatePrecision = WarcDatePrecision::Fraction(6);

/// Information recorded in the WARC file's initial `warcinfo` record.
pub struct WarcinfoOptions<'a> {
    pub(crate) user_agent: &'a str,
    pub(crate) software: Option<(&'a str, &'a str)>,
    pub(crate) operator: Option<(&'a str, Option<&'a str>)>,
    pub(crate) session_id: Option<&'a str>,
}

impl<'a> WarcinfoOptions<'a> {
    /// Options for a one-shot run: this crate as software, with no operator or session.
    pub(crate) const fn archiver(user_agent: &'a str) -> Self {
        Self {
            user_agent,
            software: None,
            operator: None,
            session_id: None,
        }
    }
}

/// Write a record, optionally as an independent gzip member.
pub(super) fn write_record<W: Write>(
    writer: &mut WarcWriter<W>,
    record: Record,
    gzip: bool,
) -> Result<Written, Error> {
    let record = record.into_raw()?;
    if gzip {
        writer.write_gzip(&record)
    } else {
        writer.write(&record)
    }
    .map_err(Error::from)
}

/// Write one exchange's request, response, and metadata records and build its CDXJ item.
pub(super) fn write_exchange<W: Write>(
    writer: &mut WarcWriter<W>,
    exchange: Exchange,
    warcinfo_id: &Uri<String>,
    warc_name: &str,
    gzip: bool,
    via: Option<&str>,
) -> Result<cdxj::Item<'static>, Error> {
    let mut event = CaptureEvent::new(exchange.captured.target_uri.clone(), exchange.date)
        .warcinfo_id(warcinfo_id.clone())
        .ip_address(exchange.captured.ip_address)
        .identify_payload_type()
        .fetch_time(exchange.captured.fetch_time);

    if let Some(digest) = &exchange.payload_digest {
        event = event.payload_digest(LabelledDigest::from_digest(
            DigestAlgorithm::Sha256,
            &digest.0,
        ));
    }
    if let Some(reason) = exchange.captured.truncated.clone() {
        event = event.truncated(reason);
    }

    let mut records = event.exchange(exchange.captured.request, exchange.captured.response)?;
    if let (
        Some(via),
        Some(Record::Metadata {
            header,
            body: FieldsBlock::Fields(fields),
        }),
    ) = (via, records.metadata.as_mut())
    {
        fields.push(MetadataField::Via, via)?;
        header.core.content_length = Some(fields.rendered_len() as u64);
    }

    let mime = records
        .response
        .payload()
        .and_then(|payload| payload.identified_payload_type.as_ref())
        .map(ToString::to_string);
    write_record(writer, records.request, gzip)?;
    let response = write_record(writer, records.response, gzip)?;
    if let Some(metadata) = records.metadata {
        write_record(writer, metadata, gzip)?;
    }

    Ok(cdxj::Item {
        key: Cow::Owned(exchange.key),
        timestamp: cdxj::Timestamp::with_milliseconds(exchange.date.date_time()),
        fields: cdxj::Fields {
            url: Cow::Owned(exchange.captured.target_uri.into_string()),
            digest: exchange
                .payload_digest
                .map(|digest| Cow::Owned(digest.to_string())),
            mime: mime.map(Cow::Owned),
            status: Some(exchange.status),
            offset: Some(response.offset),
            length: Some(response.length),
            filename: Some(Cow::Owned(warc_name.to_owned())),
            record_digest: Some(stored_digest(&response)),
            extra: ExtraProperties::default(),
        },
    })
}

/// Build the `warcinfo` record at the start of a WARC file.
pub(super) fn warcinfo_record(
    warc_name: &str,
    options: &WarcinfoOptions<'_>,
) -> Result<Record, Error> {
    let (software_name, software_version) = options
        .software
        .unwrap_or((env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")));
    let mut builder = Record::warcinfo(record_date())
        .filename(warc_name)
        .expect("well-formed WARC file name")
        .software(software_name, software_version)?;

    if let Some((name, email)) = options.operator {
        builder = builder.operator(name, email)?;
    }
    builder = builder
        .http_header_user_agent(options.user_agent)
        .map_err(|_| Error::InvalidUserAgent(options.user_agent.to_owned()))?;
    if let Some(session_id) = options.session_id {
        builder = builder
            .is_part_of(session_id)
            .expect("well-formed session identifier");
    }

    Ok(builder.build())
}

fn record_date() -> WarcDate {
    WarcDate::new(Utc::now(), DATE_PRECISION)
}

fn stored_digest(written: &Written) -> Sha256Digest {
    written
        .digest
        .as_ref()
        .and_then(LabelledDigest::decoded)
        .and_then(|bytes| bytes.try_into().ok())
        .map(Sha256Digest)
        .expect("invariant violation: a digesting writer reports a 32-byte SHA-256 digest")
}
