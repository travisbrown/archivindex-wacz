//! Mapping captured HTTP exchanges to WARC records and CDXJ entries.

use std::borrow::Cow;
use std::io::Write;

use archivindex_wacz::ExtraProperties;
use archivindex_wacz::cdxj;
use archivindex_wacz::digest::Sha256Digest;
use archivindex_warc::io::write::{WarcWriter, Written};
use archivindex_warc::record::Record;
use archivindex_warc::record::capture::{CaptureEvent, CaptureRecords};
use archivindex_warc::record::header::RevisitProfile;
use archivindex_warc::record::header::truncated_type::TruncatedType;
use archivindex_warc::recorder::CapturedExchange;
use archivindex_warc::value::{DigestAlgorithm, LabelledDigest, MediaType, WarcDate};
use fluent_uri::Uri;

use super::warc_fields::metadata_record;
use super::{Error, Exchange};
use crate::response;

/// The conventional CDXJ media type for an entry backed by a `revisit` record.
const REVISIT_MIME: &str = "warc/revisit";

/// The identity of a stored `response` record that later revisits of its payload reference.
#[derive(Clone, Debug)]
pub(super) struct RevisitTarget {
    /// The original record's `WARC-Record-ID`.
    record_id: Uri<String>,
    /// The original capture's target URI.
    target_uri: Uri<String>,
    /// The original record's `WARC-Date`.
    date: WarcDate,
    /// The original record's payload digest, which every revisit of it shares.
    payload_digest: Sha256Digest,
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
    warc_name: &str,
    gzip: bool,
    via: Option<&str>,
    revisit_of: Option<&RevisitTarget>,
) -> Result<(cdxj::Item<'static>, Option<RevisitTarget>), Error> {
    let Exchange {
        key,
        date,
        status,
        payload_digest,
        payload_length: _,
        revalidated,
        captured,
    } = exchange;

    let (records, target_uri) = if let Some(original) = revisit_of {
        let profile = if revalidated.is_some() {
            RevisitProfile::SERVER_NOT_MODIFIED
        } else {
            RevisitProfile::IDENTICAL_PAYLOAD_DIGEST
        };

        revisit_records(captured, date, warcinfo_id, original, profile, via)?
    } else {
        full_records(captured, date, warcinfo_id, payload_digest.as_ref(), via)?
    };

    let mime = if revisit_of.is_some() {
        Some(Cow::Borrowed(REVISIT_MIME))
    } else {
        records
            .response
            .payload()
            .and_then(|payload| payload.identified_payload_type.as_ref())
            .map(|media_type| Cow::Owned(media_type.to_string()))
    };
    // A revisit's payload is the original's, whatever the revisiting response itself carried.
    let digest = revisit_of.map_or(payload_digest, |original| Some(original.payload_digest));
    let target = revisit_of
        .is_none()
        .then_some(payload_digest)
        .flatten()
        .map(|payload_digest| RevisitTarget {
            record_id: records.response.core().record_id.clone(),
            target_uri: target_uri.clone(),
            date,
            payload_digest,
        });
    write_record(writer, records.request, gzip)?;
    let written = write_record(writer, records.response, gzip)?;
    if let Some(metadata) = records.metadata {
        write_record(writer, metadata, gzip)?;
    }

    Ok((
        cdxj::Item {
            key: Cow::Owned(key),
            timestamp: cdxj::Timestamp::with_milliseconds(date.date_time()),
            fields: cdxj::Fields {
                url: Cow::Owned(target_uri.into_string()),
                digest: digest.map(|digest| Cow::Owned(digest.to_string())),
                mime,
                status: Some(status),
                offset: Some(written.offset),
                length: Some(written.length),
                filename: Some(Cow::Owned(warc_name.to_owned())),
                record_digest: Some(stored_digest(&written)),
                extra: ExtraProperties::default(),
            },
        },
        target,
    ))
}

/// Build a capture's request, response, and metadata records, returning its target URI.
fn full_records(
    captured: CapturedExchange,
    date: WarcDate,
    warcinfo_id: &Uri<String>,
    payload_digest: Option<&Sha256Digest>,
    via: Option<&str>,
) -> Result<(CaptureRecords, Uri<String>), Error> {
    let CapturedExchange {
        request,
        response,
        target_uri,
        ip_address,
        date: _,
        fetch_time,
        truncated,
    } = captured;
    let mut event = CaptureEvent::new(target_uri.clone(), date)
        .warcinfo_id(warcinfo_id.clone())
        .ip_address(ip_address)
        .identify_payload_type();

    if let Some(digest) = payload_digest {
        event = event.payload_digest(LabelledDigest::from_digest(
            DigestAlgorithm::Sha256,
            &digest.0,
        ));
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
        fetch_time,
        via,
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
    via: Option<&str>,
) -> Result<(CaptureRecords, Uri<String>), Error> {
    let CapturedExchange {
        request,
        mut response,
        target_uri,
        ip_address,
        date: _,
        fetch_time,
        truncated: _,
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
        .payload_digest(LabelledDigest::from_digest(
            DigestAlgorithm::Sha256,
            &original.payload_digest.0,
        ))
        .warcinfo_id(warcinfo_id.clone())
        .ip_address(ip_address)
        .refers_to(original.record_id.clone())
        .refers_to_target_uri(original.target_uri.clone())
        .refers_to_date(original.date);
    let head = response::head(&response)
        .expect("invariant violation: the recorder stores a well-formed response head");
    if head.body_offset < response.len() {
        response.truncate(head.body_offset);
        revisit = revisit.truncated(TruncatedType::Length);
    }
    let revisit = revisit.body(response)?;

    let metadata = metadata_record(
        date,
        target_uri.clone(),
        revisit.core().record_id.clone(),
        warcinfo_id,
        fetch_time,
        via,
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

fn stored_digest(written: &Written) -> Sha256Digest {
    written
        .digest
        .as_ref()
        .and_then(LabelledDigest::decoded)
        .and_then(|bytes| bytes.try_into().ok())
        .map(Sha256Digest)
        .expect("invariant violation: a digesting writer reports a 32-byte SHA-256 digest")
}
