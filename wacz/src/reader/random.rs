//! Random-access index and capture reading.

use std::borrow::Cow;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::ops::{Bound, RangeBounds};
use std::path::Path;

use archivindex_warc::io::read::WarcReader;
use archivindex_warc::parse::raw;
use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;
use bounded_static::IntoBoundedStatic as _;
use flate2::read::GzDecoder;
use serde::Deserialize;
use zip::CompressionMethod;

use super::{Error, WaczReader};
use crate::cdxj::{self, Fields, Item, Timestamp};
use crate::digest::Sha256Digest;
use crate::{ARCHIVE_PREFIX, GZIP_EXTENSION};

/// A capture found in a WACZ index, together with the index that described it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capture {
    /// The plain CDXJ index or `ZipNum` summary that produced this result.
    pub index_path: String,
    /// The matching CDXJ item.
    pub item: Item<'static>,
}

/// One independently compressed block described by a `ZipNum` summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZipNumBlock {
    /// The first CDXJ search key in the block.
    pub key: String,
    /// The first CDXJ timestamp in the block.
    pub timestamp: Timestamp,
    /// The path of the `.cdx.gz` data member.
    pub data_path: String,
    /// The byte offset within `data_path`.
    pub offset: u64,
    /// The compressed block length.
    pub length: u64,
    /// The SHA-256 digest of the compressed block.
    pub digest: Sha256Digest,
}

/// A parsed `ZipNum` summary and its block descriptors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZipNumSummary {
    /// The `.idx` member that supplied the summary.
    pub path: String,
    /// The `.cdx.gz` member holding its concatenated gzip blocks.
    pub data_path: String,
    /// Blocks in summary order.
    pub blocks: Vec<ZipNumBlock>,
}

#[derive(Deserialize)]
struct SummaryHeader<'a> {
    #[serde(borrow)]
    format: Cow<'a, str>,
    #[serde(borrow)]
    filename: Cow<'a, str>,
}

#[derive(Deserialize)]
struct SummaryEntry {
    offset: u64,
    length: u64,
    digest: Sha256Digest,
}

impl<R: Read + Seek> WaczReader<R> {
    /// Read an exact range from a stored ZIP member.
    ///
    /// WACZ requires WARC and compressed-index members to use ZIP `STORE`, making their CDXJ byte
    /// offsets seekable. Compressed ZIP members are rejected because their logical offsets do not
    /// map directly onto stored bytes.
    pub fn member_range(&mut self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, Error> {
        let metadata = self.member_metadata(path)?;
        if metadata.compression != CompressionMethod::Stored {
            return Err(Error::CompressedMember(path.to_owned()));
        }

        let size = metadata.size;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| Error::RangeOutOfBounds {
                path: path.to_owned(),
                offset,
                end: u64::MAX,
                size,
            })?;

        if end > size {
            return Err(Error::RangeOutOfBounds {
                path: path.to_owned(),
                offset,
                end,
                size,
            });
        }

        let mut member = self.archive.by_name_seek(path)?;
        member.seek(SeekFrom::Start(offset))?;
        let length = usize::try_from(length).map_err(|_| Error::RangeOutOfBounds {
            path: path.to_owned(),
            offset,
            end,
            size,
        })?;
        let mut bytes = vec![0; length];
        member.read_exact(&mut bytes)?;

        Ok(bytes)
    }

    /// Parse a `ZipNum` `.idx` summary without reading its compressed CDXJ data member.
    pub fn zipnum_summary(&mut self, path: &str) -> Result<ZipNumSummary, Error> {
        let bytes = self.member_bytes(path)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Error::InvalidIndexEncoding(path.to_owned()))?;
        parse_summary(path, text)
    }

    /// Read, verify, and decompress one block from a `ZipNum` index.
    pub fn zipnum_block(&mut self, block: &ZipNumBlock) -> Result<Vec<u8>, Error> {
        let compressed = self.member_range(&block.data_path, block.offset, block.length)?;
        verify_digest(&block.data_path, &compressed, block.digest)?;

        let mut bytes = Vec::new();
        GzDecoder::new(compressed.as_slice()).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Find captures of `url` in every index whose timestamps fall within `time_range`.
    ///
    /// Plain indexes are searched using binary search after reading the index member. `ZipNum`
    /// summaries are binary-searched first, and only candidate gzip blocks are fetched. Results
    /// are ordered chronologically, with their source index retained in [`Capture::index_path`].
    pub fn lookup<B: RangeBounds<Timestamp>>(
        &mut self,
        url: &str,
        time_range: B,
    ) -> Result<Vec<Capture>, Error> {
        let paths = self.index_paths().map(str::to_owned).collect::<Vec<_>>();
        let mut referenced_data = Vec::new();
        let mut captures = Vec::new();

        for path in paths.iter().filter(|path| has_extension(path, "idx")) {
            let summary = self.zipnum_summary(path)?;
            referenced_data.push(summary.data_path.clone());
            captures.extend(self.lookup_zipnum(&summary, url, &time_range)?);
        }

        for path in paths.iter().filter(|path| {
            !has_extension(path, "idx") && !referenced_data.iter().any(|data| data == *path)
        }) {
            captures.extend(self.lookup_plain(path, url, &time_range)?);
        }

        captures.sort_by(|left, right| {
            left.item
                .timestamp
                .cmp(&right.item.timestamp)
                .then_with(|| left.item.key.cmp(&right.item.key))
                .then_with(|| left.index_path.cmp(&right.index_path))
        });

        Ok(captures)
    }

    /// Search one plain CDXJ index or `ZipNum` `.idx` summary.
    pub fn lookup_index<B: RangeBounds<Timestamp>>(
        &mut self,
        path: &str,
        url: &str,
        time_range: B,
    ) -> Result<Vec<Capture>, Error> {
        if has_extension(path, "idx") {
            let summary = self.zipnum_summary(path)?;
            self.lookup_zipnum(&summary, url, &time_range)
        } else {
            self.lookup_plain(path, url, &time_range)
        }
    }

    /// Read the stored bytes located by CDXJ fields and verify `recordDigest` when present.
    ///
    /// For `.warc.gz` members, the returned bytes are the complete compressed gzip member. For
    /// uncompressed WARC members, they are the complete serialized WARC record.
    pub fn capture_bytes(&mut self, fields: &Fields<'_>) -> Result<Vec<u8>, Error> {
        let filename = fields
            .filename
            .as_deref()
            .ok_or(Error::MissingCaptureField("filename"))?;
        let offset = fields.offset.ok_or(Error::MissingCaptureField("offset"))?;
        let length = fields.length.ok_or(Error::MissingCaptureField("length"))?;
        let path = archive_path(filename);
        let bytes = self.member_range(&path, offset, length)?;

        if let Some(expected) = fields.record_digest {
            verify_digest(&path, &bytes, expected)?;
        }

        Ok(bytes)
    }

    /// Resolve CDXJ fields to exactly one byte-preserving raw WARC record.
    pub fn read_capture_raw(&mut self, fields: &Fields<'_>) -> Result<raw::Record, Error> {
        let bytes = self.decoded_capture_bytes(fields)?;
        let mut records = WarcReader::new(Cursor::new(bytes)).iter_raw_records();
        let record = records.next().transpose()?;

        match (record, records.next()) {
            (Some(record), None) => Ok(record),
            (None, _) => Err(Error::CaptureRecordCount(0)),
            (Some(_), Some(second)) => {
                second?;
                Err(Error::CaptureRecordCount(2))
            }
        }
    }

    /// Resolve CDXJ fields to exactly one semantically validated WARC record.
    pub fn read_capture(&mut self, fields: &Fields<'_>) -> Result<Record<NoExtension>, Error> {
        let bytes = self.decoded_capture_bytes(fields)?;
        let mut records = WarcReader::new(Cursor::new(bytes)).iter_records::<NoExtension>();
        let record = records.next().transpose()?;

        match (record, records.next()) {
            (Some(record), None) => Ok(record),
            (None, _) => Err(Error::CaptureRecordCount(0)),
            (Some(_), Some(second)) => {
                second?;
                Err(Error::CaptureRecordCount(2))
            }
        }
    }

    fn lookup_plain<B: RangeBounds<Timestamp>>(
        &mut self,
        path: &str,
        url: &str,
        time_range: &B,
    ) -> Result<Vec<Capture>, Error> {
        let bytes = self.decoded_member_bytes(path)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Error::InvalidIndexEncoding(path.to_owned()))?;
        let key = cdxj::search_key(url)?;
        let lines = text
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        let first =
            lines.partition_point(|line| line_key(line).is_some_and(|found| found < key.as_str()));
        let mut captures = Vec::new();

        for line in &lines[first..] {
            if line_key(line) != Some(key.as_str()) {
                break;
            }

            let item = Item::parse(line)?.into_static();
            if in_range(item.timestamp, time_range) {
                captures.push(Capture {
                    index_path: path.to_owned(),
                    item,
                });
            }
        }

        Ok(captures)
    }

    fn lookup_zipnum<B: RangeBounds<Timestamp>>(
        &mut self,
        summary: &ZipNumSummary,
        url: &str,
        time_range: &B,
    ) -> Result<Vec<Capture>, Error> {
        let key = cdxj::search_key(url)?;
        let insertion = summary.blocks.partition_point(|block| block.key < key);
        let first = insertion.saturating_sub(1);
        let mut captures = Vec::new();

        for block in &summary.blocks[first..] {
            if block.key > key {
                break;
            }

            let bytes = self.zipnum_block(block)?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| Error::InvalidIndexEncoding(summary.data_path.clone()))?;

            for line in text.lines().filter(|line| !line.is_empty()) {
                if line_key(line) == Some(key.as_str()) {
                    let item = Item::parse(line)?.into_static();
                    if in_range(item.timestamp, time_range) {
                        captures.push(Capture {
                            index_path: summary.path.clone(),
                            item,
                        });
                    }
                }
            }
        }

        Ok(captures)
    }

    fn decoded_capture_bytes(&mut self, fields: &Fields<'_>) -> Result<Vec<u8>, Error> {
        let filename = fields
            .filename
            .as_deref()
            .ok_or(Error::MissingCaptureField("filename"))?;
        let bytes = self.capture_bytes(fields)?;

        if filename.ends_with(GZIP_EXTENSION) {
            let mut decoded = Vec::new();
            GzDecoder::new(bytes.as_slice()).read_to_end(&mut decoded)?;
            Ok(decoded)
        } else {
            Ok(bytes)
        }
    }
}

fn parse_summary(path: &str, text: &str) -> Result<ZipNumSummary, Error> {
    let mut lines = text.lines().filter(|line| !line.is_empty());
    let header_line = lines
        .next()
        .ok_or_else(|| Error::InvalidZipNum(format!("{path}: missing !meta header")))?;
    let (header_key, header_json) = split_prefix(header_line)
        .ok_or_else(|| Error::InvalidZipNum(format!("{path}: malformed !meta header")))?;

    if header_key != "!meta 0" {
        return Err(Error::InvalidZipNum(format!(
            "{path}: expected !meta 0 header"
        )));
    }

    let header: SummaryHeader<'_> = serde_json::from_str(header_json)
        .map_err(|error| Error::InvalidZipNum(format!("{path}: {error}")))?;
    if header.format != "cdxj-gzip-1.0" {
        return Err(Error::InvalidZipNum(format!(
            "{path}: unsupported format {}",
            header.format
        )));
    }

    let data_path = sibling_path(path, &header.filename);
    let mut blocks = Vec::new();

    for (line_number, line) in lines.enumerate() {
        let (prefix, json) = split_prefix(line).ok_or_else(|| {
            Error::InvalidZipNum(format!("{path}: malformed line {}", line_number + 2))
        })?;
        let (key, timestamp) = prefix.rsplit_once(' ').ok_or_else(|| {
            Error::InvalidZipNum(format!(
                "{path}: malformed prefix on line {}",
                line_number + 2
            ))
        })?;
        let entry: SummaryEntry = serde_json::from_str(json).map_err(|error| {
            Error::InvalidZipNum(format!("{path}: line {}: {error}", line_number + 2))
        })?;
        let timestamp = timestamp.parse().map_err(|error| {
            Error::InvalidZipNum(format!("{path}: line {}: {error}", line_number + 2))
        })?;

        blocks.push(ZipNumBlock {
            key: key.to_owned(),
            timestamp,
            data_path: data_path.clone(),
            offset: entry.offset,
            length: entry.length,
            digest: entry.digest,
        });
    }

    if !blocks.windows(2).all(|pair| {
        (pair[0].key.as_str(), pair[0].timestamp) <= (pair[1].key.as_str(), pair[1].timestamp)
    }) {
        return Err(Error::InvalidZipNum(format!(
            "{path}: block prefixes are not sorted"
        )));
    }

    Ok(ZipNumSummary {
        path: path.to_owned(),
        data_path,
        blocks,
    })
}

fn split_prefix(line: &str) -> Option<(&str, &str)> {
    let (json, _) = line.match_indices(' ').nth(1)?;
    Some((&line[..json], &line[json + 1..]))
}

fn sibling_path(path: &str, filename: &str) -> String {
    path.rsplit_once('/').map_or_else(
        || filename.to_owned(),
        |(parent, _)| format!("{parent}/{filename}"),
    )
}

fn line_key(line: &str) -> Option<&str> {
    line.split_once(' ').map(|(key, _)| key)
}

fn has_extension(path: &str, expected: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn in_range<B: RangeBounds<Timestamp>>(timestamp: Timestamp, range: &B) -> bool {
    let after_start = match range.start_bound() {
        Bound::Included(start) => timestamp >= *start,
        Bound::Excluded(start) => timestamp > *start,
        Bound::Unbounded => true,
    };
    let before_end = match range.end_bound() {
        Bound::Included(end) => timestamp <= *end,
        Bound::Excluded(end) => timestamp < *end,
        Bound::Unbounded => true,
    };

    after_start && before_end
}

fn archive_path(filename: &str) -> String {
    if filename.starts_with(ARCHIVE_PREFIX) {
        filename.to_owned()
    } else {
        format!("{ARCHIVE_PREFIX}{filename}")
    }
}

fn verify_digest(path: &str, bytes: &[u8], expected: Sha256Digest) -> Result<(), Error> {
    let actual = Sha256Digest::compute(bytes);
    if actual == expected {
        Ok(())
    } else {
        Err(Error::DigestMismatch {
            path: path.to_owned(),
            expected,
            actual,
        })
    }
}
