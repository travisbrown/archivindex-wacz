//! Mapping captured HTTP exchanges to WARC records.

use std::io::Write;

use archivindex_warc::io::write::WarcWriter;
use archivindex_warc::record::Record;
use archivindex_warc::record::capture::{CaptureEvent, CaptureRecords};
use archivindex_warc::record::header::RevisitProfile;
use archivindex_warc::record::header::truncated_type::TruncatedType;
use archivindex_warc::recorder::CapturedExchange;
use archivindex_warc::value::{LabelledDigest, MediaType, WarcDate};
use archivindex_warc_revisit_index::payload::RevisitTarget;
use fluent_uri::Uri;

use super::warc_fields::{MetadataValues, metadata_record};
use super::{Error, Exchange};

/// Optional fields added to the metadata record accompanying an exchange.
#[derive(Clone, Copy)]
pub(super) struct MetadataOptions<'a> {
    pub(super) via: Option<&'a str>,
    pub(super) title: Option<&'a str>,
}

/// Write a record, optionally as an independent gzip member.
pub(super) fn write_record<W: Write>(
    writer: &mut WarcWriter<W>,
    record: Record,
    gzip: bool,
) -> Result<(), Error> {
    let record = record.into_raw()?;
    if gzip {
        writer.write_gzip(&record)?;
    } else {
        writer.write(&record)?;
    }
    Ok(())
}

/// Write one exchange's request, response, and metadata records.
///
/// When `revisit_of` names the earlier capture whose payload this exchange revisits, the response
/// is stored as a `revisit` record holding only the response head: under the `server-not-modified`
/// profile for a `304 Not Modified` answering a conditional request, and otherwise under the
/// `identical-payload-digest` profile. Otherwise the full response is stored, and when its payload
/// is digested the returned [`RevisitTarget`] identifies the new record so that later revisits can
/// reference it.
pub(super) fn write_exchange<W: Write>(
    writer: &mut WarcWriter<W>,
    exchange: Exchange,
    warcinfo_id: &Uri<String>,
    gzip: bool,
    metadata: MetadataOptions<'_>,
    revisit_of: Option<&RevisitTarget>,
) -> Result<Option<RevisitTarget>, Error> {
    let payload_length = exchange.payload_length();
    let Exchange {
        date,
        status: _,
        payload_digest,
        revalidated,
        captured,
        ..
    } = exchange;

    let (records, target_uri) = if let Some(original) = revisit_of {
        let profile = if revalidated.is_some() {
            RevisitProfile::SERVER_NOT_MODIFIED
        } else {
            RevisitProfile::IDENTICAL_PAYLOAD_DIGEST
        };

        revisit_records(captured, date, warcinfo_id, original, profile, metadata)?
    } else {
        full_records(
            captured,
            date,
            warcinfo_id,
            payload_digest.as_ref(),
            metadata,
        )?
    };

    // A revisit's payload is the original's, whatever the revisiting response itself carried.
    let digest = revisit_of
        .map(|original| original.payload_digest.clone())
        .or(payload_digest);
    let target = revisit_of
        .is_none()
        .then_some(digest)
        .flatten()
        .map(|payload_digest| RevisitTarget {
            record_id: records.response.core().record_id.clone(),
            target_uri: target_uri.clone(),
            warc_date: date,
            payload_digest,
            payload_length: Some(payload_length),
        });
    write_record(writer, records.request, gzip)?;
    write_record(writer, records.response, gzip)?;
    if let Some(metadata) = records.metadata {
        write_record(writer, metadata, gzip)?;
    }

    Ok(target)
}

/// Build a capture's request, response, and metadata records, returning its target URI.
fn full_records(
    captured: CapturedExchange,
    date: WarcDate,
    warcinfo_id: &Uri<String>,
    payload_digest: Option<&LabelledDigest>,
    metadata: MetadataOptions<'_>,
) -> Result<(CaptureRecords, Uri<String>), Error> {
    let CapturedExchange {
        request,
        response,
        target_uri,
        ip_address,
        date: _,
        fetch_time,
        truncated,
        response_metadata: _,
    } = captured;
    let mut event = CaptureEvent::new(target_uri.clone(), date)
        .warcinfo_id(warcinfo_id.clone())
        .ip_address(ip_address)
        .identify_payload_type();

    if let Some(digest) = payload_digest {
        event = event.payload_digest(digest.clone());
    }
    if let Some(reason) = truncated {
        event = event.truncated(reason);
    }

    let mut records = event.exchange(request, response)?;
    records.metadata = Some(metadata_record(
        date,
        target_uri.clone(),
        records.response.core().record_id.clone(),
        warcinfo_id,
        MetadataValues {
            fetch_time,
            via: metadata.via,
            title: metadata.title,
        },
    )?);

    Ok((records, target_uri))
}

/// Build the records of a capture revisiting an earlier record's payload: its request and metadata
/// records as usual, with a `revisit` record under `profile` referencing the original in place of a
/// full response record.
fn revisit_records(
    captured: CapturedExchange,
    date: WarcDate,
    warcinfo_id: &Uri<String>,
    original: &RevisitTarget,
    profile: RevisitProfile,
    metadata_options: MetadataOptions<'_>,
) -> Result<(CaptureRecords, Uri<String>), Error> {
    let body_offset = captured.response_metadata.body_offset;
    let CapturedExchange {
        request,
        mut response,
        target_uri,
        ip_address,
        date: _,
        fetch_time,
        truncated: _,
        response_metadata: _,
    } = captured;
    let request = Record::request(target_uri.as_str(), date)
        .expect("invariant violation: a parsed URI failed to reparse")
        .warcinfo_id(warcinfo_id.clone())
        .body(request)?;

    // A revisit stores only the response head, with the original's payload digest standing for the
    // payload it does not repeat. A `304 Not Modified` is nothing more than its head, while a
    // response repeating the payload is cut and must declare the truncation by length.
    let mut revisit = Record::revisit(target_uri.as_str(), date, profile)
        .expect("invariant violation: a parsed URI failed to reparse")
        .concurrent_to(request.core().record_id.clone())
        .content_type(MediaType::HTTP_RESPONSE)
        .payload_digest(original.payload_digest.clone())
        .warcinfo_id(warcinfo_id.clone())
        .ip_address(ip_address)
        .refers_to(original.record_id.clone())
        .refers_to_target_uri(original.target_uri.clone())
        .refers_to_date(original.warc_date);
    if body_offset < response.len() {
        response.truncate(body_offset);
        revisit = revisit.truncated(TruncatedType::Length);
    }
    let revisit = revisit.body(response)?;

    let metadata = metadata_record(
        date,
        target_uri.clone(),
        revisit.core().record_id.clone(),
        warcinfo_id,
        MetadataValues {
            fetch_time,
            via: metadata_options.via,
            title: metadata_options.title,
        },
    )?;

    Ok((
        CaptureRecords {
            request,
            response: revisit,
            metadata: Some(metadata),
        },
        target_uri,
    ))
}
