//! Random-access index and capture reading.

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::ops::{Range, RangeBounds};

#[cfg(test)]
use archivindex_cdx::format::cdxj;
use archivindex_cdx::format::cdxj::{Fields, Item};
use archivindex_cdx::timestamp::Timestamp;
use archivindex_lines::{LineContext, Lines};
use archivindex_surt::Surt;
use archivindex_surt::url::Canonicalizer;
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::parse::raw;
use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;
use flate2::read::GzDecoder;
use zip::CompressionMethod;

use super::{Error, MAX_DECOMPRESSED, MAX_PREALLOCATION, WaczReader};
use crate::digest::Sha256Digest;
use crate::zipnum::{FORMAT, SummaryEntry, SummaryHeader};
use crate::{ARCHIVE_PREFIX, GZIP_EXTENSION, cdxj as cdxj_io};

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

/// A decoded plain CDXJ index, kept by the reader so later lookups do not inflate it again.
pub(super) struct PlainIndex {
    text: String,
    /// The byte range of every non-empty line, without its line ending.
    lines: Vec<Range<usize>>,
}

impl PlainIndex {
    fn new(text: String) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for line in text.split_inclusive('\n') {
            let content = line.strip_suffix('\n').unwrap_or(line);
            let content = content.strip_suffix('\r').unwrap_or(content);
            if !content.is_empty() {
                lines.push(start..start + content.len());
            }
            start += line.len();
        }
        Self { text, lines }
    }

    fn line(&self, range: &Range<usize>) -> &str {
        &self.text[range.clone()]
    }
}

pub(super) struct IndexPartition {
    pub(super) summaries: Vec<(String, Result<ZipNumSummary, Error>)>,
    pub(super) plain: Vec<String>,
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
        let end = offset.saturating_add(length);
        let length = usize::try_from(length).map_err(|_| Error::RangeOutOfBounds {
            path: path.to_owned(),
            offset,
            end,
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
        // The declared size the range was checked against is untrusted input, so the buffer grows
        // to what is actually read instead of being sized up front.
        let mut bytes = Vec::with_capacity(length.min(MAX_PREALLOCATION));
        member.take(end - offset).read_to_end(&mut bytes)?;

        if bytes.len() != length {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }

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

        decompress(&block.data_path, &compressed, MAX_DECOMPRESSED)
    }

    /// Find captures of `url` within `time_range` across all indexes.
    ///
    /// Searches the Wayback key this crate writes and, when it differs (typically by a trailing
    /// slash), the `warcio.js` key of Browsertrix and `wabac.js`, so either family is found.
    ///
    /// Plain indexes are decoded once per reader, kept in memory, and binary-searched. `ZipNum`
    /// summaries are binary-searched first, and only candidate gzip blocks are fetched. Results are
    /// ordered chronologically, with their source index retained in [`Capture::index_path`].
    /// Indexes must be sorted; use content validation to check their order.
    pub fn lookup<B: RangeBounds<Timestamp>>(
        &mut self,
        url: &str,
        time_range: B,
    ) -> Result<Vec<Capture>, Error> {
        let keys = lookup_keys(url)?;
        let partition = self.partition_indexes();
        let mut captures = Vec::new();

        for (_, summary) in partition.summaries {
            let summary = summary?;
            captures.extend(self.lookup_zipnum(&summary, &keys, &time_range)?);
        }

        for path in partition.plain {
            captures.extend(self.lookup_plain(&path, &keys, &time_range)?);
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
        let keys = lookup_keys(url)?;
        self.lookup_index_key(path, &keys, &time_range)
    }

    fn lookup_index_key<B: RangeBounds<Timestamp>>(
        &mut self,
        path: &str,
        keys: &[Surt<'static>],
        time_range: &B,
    ) -> Result<Vec<Capture>, Error> {
        if crate::paths::is_zipnum_summary(path) {
            let summary = self.zipnum_summary(path)?;
            self.lookup_zipnum(&summary, keys, time_range)
        } else {
            self.lookup_plain(path, keys, time_range)
        }
    }

    /// Read the stored bytes located by CDXJ fields and verify `recordDigest` when present.
    ///
    /// Returns the indexed byte range without checking its WARC framing. For `.warc.gz` members,
    /// gzip encoding is retained. Use [`Self::read_capture_raw`] to require exactly one raw record.
    pub fn capture_bytes(&mut self, fields: &Fields<'_>) -> Result<Vec<u8>, Error> {
        self.capture_bytes_with_path(fields).map(|(_, bytes)| bytes)
    }

    fn capture_bytes_with_path(&mut self, fields: &Fields<'_>) -> Result<(String, Vec<u8>), Error> {
        let filename = fields
            .filename
            .as_deref()
            .ok_or(Error::MissingCaptureField("filename"))?;
        let offset = fields.offset.ok_or(Error::MissingCaptureField("offset"))?;
        let length = fields.length.ok_or(Error::MissingCaptureField("length"))?;
        let path = archive_path(filename);
        let bytes = self.member_range(&path, offset, length)?;

        if let Some(value) = &fields.record_digest {
            let expected = value.parse().map_err(|source| Error::InvalidRecordDigest {
                value: value.to_string(),
                source,
            })?;
            verify_digest(&path, &bytes, expected)?;
        }

        Ok((path, bytes))
    }

    /// Resolve CDXJ fields to exactly one byte-preserving raw WARC record.
    pub fn read_capture_raw(&mut self, fields: &Fields<'_>) -> Result<raw::Record, Error> {
        let bytes = self.decoded_capture_bytes(fields)?;
        single(
            WarcReader::new(Cursor::new(bytes))
                .iter_raw_records()
                .records(),
        )
    }

    /// Resolve CDXJ fields to exactly one semantically validated WARC record.
    pub fn read_capture(&mut self, fields: &Fields<'_>) -> Result<Record<NoExtension>, Error> {
        let bytes = self.decoded_capture_bytes(fields)?;
        single(
            WarcReader::new(Cursor::new(bytes))
                .iter_records::<NoExtension>()
                .records(),
        )
    }

    fn lookup_plain<B: RangeBounds<Timestamp>>(
        &mut self,
        path: &str,
        keys: &[Surt<'static>],
        time_range: &B,
    ) -> Result<Vec<Capture>, Error> {
        let index = self.plain_index(path)?;
        let mut captures = Vec::new();

        for key in keys {
            let key = key.as_str();
            let first = index.lines.partition_point(|range| {
                line_key(index.line(range)).is_some_and(|found| found < key)
            });

            for line in index.lines[first..].iter().map(|range| index.line(range)) {
                if line_key(line) != Some(key) {
                    break;
                }

                let item = Item::parse(line)
                    .map_err(cdxj_io::Error::from)?
                    .into_owned();
                if time_range.contains(&item.timestamp) {
                    captures.push(Capture {
                        index_path: path.to_owned(),
                        item,
                    });
                }
            }
        }

        Ok(captures)
    }

    /// The decoded lines of a plain index, read from its member on first use.
    fn plain_index(&mut self, path: &str) -> Result<&PlainIndex, Error> {
        if !self.plain_indexes.contains_key(path) {
            let bytes = self.decoded_member_bytes(path)?;
            let text = String::from_utf8(bytes)
                .map_err(|_| Error::InvalidIndexEncoding(path.to_owned()))?;
            self.plain_indexes
                .insert(path.to_owned(), PlainIndex::new(text));
        }
        Ok(self
            .plain_indexes
            .get(path)
            .expect("plain index was cached above"))
    }

    fn lookup_zipnum<B: RangeBounds<Timestamp>>(
        &mut self,
        summary: &ZipNumSummary,
        keys: &[Surt<'static>],
        time_range: &B,
    ) -> Result<Vec<Capture>, Error> {
        // Every block that may hold one of the keys, read once even when the keys share blocks.
        let mut candidates = Vec::new();

        for key in keys {
            let key = key.as_str();
            let first = summary
                .blocks
                .partition_point(|block| block.key.as_str() < key)
                .saturating_sub(1);

            candidates.extend(
                (first..summary.blocks.len())
                    .take_while(|&index| summary.blocks[index].key.as_str() <= key),
            );
        }

        candidates.sort_unstable();
        candidates.dedup();

        let mut captures = Vec::new();

        for block in candidates.into_iter().map(|index| &summary.blocks[index]) {
            let bytes = self.zipnum_block(block)?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| Error::InvalidIndexEncoding(summary.data_path.clone()))?;

            for line in text.lines().filter(|line| !line.is_empty()) {
                if line_key(line).is_some_and(|found| keys.iter().any(|key| key.as_str() == found))
                {
                    let item = Item::parse(line)
                        .map_err(cdxj_io::Error::from)?
                        .into_owned();
                    if time_range.contains(&item.timestamp) {
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
        let (path, bytes) = self.capture_bytes_with_path(fields)?;

        if path.ends_with(GZIP_EXTENSION) {
            decompress(&path, &bytes, MAX_DECOMPRESSED)
        } else {
            Ok(bytes)
        }
    }

    pub(super) fn partition_indexes(&mut self) -> IndexPartition {
        let paths = self.index_paths().map(str::to_owned).collect::<Vec<_>>();
        let mut referenced_data = Vec::new();
        let mut summaries = Vec::new();

        for path in paths
            .iter()
            .filter(|path| crate::paths::is_zipnum_summary(path))
        {
            let summary = self.zipnum_summary(path);
            if let Ok(summary) = &summary {
                referenced_data.push(summary.data_path.clone());
            }
            summaries.push((path.clone(), summary));
        }

        let plain = paths
            .into_iter()
            .filter(|path| {
                !crate::paths::is_zipnum_summary(path) && !referenced_data.contains(path)
            })
            .collect();

        IndexPartition { summaries, plain }
    }
}

fn single<T>(
    mut records: impl Iterator<Item = Result<T, archivindex_warc::io::read::Error>>,
) -> Result<T, Error> {
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

fn parse_summary(path: &str, text: &str) -> Result<ZipNumSummary, Error> {
    let mut lines = Lines::with_source(text.as_bytes(), path);
    let (header_context, header_line) =
        lines
            .next_content()
            .map_err(zipnum_io_error)?
            .ok_or_else(|| {
                zipnum_error(
                    LineContext {
                        source: path.to_owned(),
                        line: 1,
                        excerpt: None,
                    },
                    "missing !meta header",
                )
            })?;
    let (header_key, header_json) = crate::cdxj::split_prefix(header_line)
        .ok_or_else(|| zipnum_error(header_context.into_owned(), "malformed !meta header"))?;

    if header_key != "!meta 0" {
        return Err(zipnum_error(
            header_context.into_owned(),
            "expected !meta 0 header",
        ));
    }

    let header: SummaryHeader<'_> = serde_json::from_str(header_json)
        .map_err(|error| zipnum_error(header_context.into_owned(), error.to_string()))?;
    if header.format != FORMAT {
        return Err(zipnum_error(
            header_context.into_owned(),
            format!("unsupported format {}", header.format),
        ));
    }

    let data_path = sibling_path(path, &header.filename);
    let mut blocks: Vec<ZipNumBlock> = Vec::new();

    while let Some((context, line)) = lines.next_content().map_err(zipnum_io_error)? {
        let (prefix, json) = crate::cdxj::split_prefix(line)
            .ok_or_else(|| zipnum_error(context.into_owned(), "malformed line"))?;
        let (key, timestamp) = prefix
            .rsplit_once(' ')
            .ok_or_else(|| zipnum_error(context.into_owned(), "malformed prefix"))?;
        let entry: SummaryEntry = serde_json::from_str(json)
            .map_err(|error| zipnum_error(context.into_owned(), error.to_string()))?;
        let timestamp = timestamp
            .parse::<Timestamp>()
            .map_err(|error| zipnum_error(context.into_owned(), error.to_string()))?;

        let block = ZipNumBlock {
            key: key.to_owned(),
            timestamp,
            data_path: data_path.clone(),
            offset: entry.offset,
            length: entry.length,
            digest: entry.digest,
        };
        if blocks.last().is_some_and(|previous| {
            (previous.key.as_str(), previous.timestamp) > (block.key.as_str(), block.timestamp)
        }) {
            return Err(zipnum_error(
                context.into_owned(),
                "block prefixes are not sorted",
            ));
        }
        blocks.push(block);
    }

    Ok(ZipNumSummary {
        path: path.to_owned(),
        data_path,
        blocks,
    })
}

fn zipnum_error(context: LineContext, message: impl Into<String>) -> Error {
    Error::InvalidZipNum {
        context,
        message: message.into(),
    }
}

fn zipnum_io_error(error: archivindex_lines::Error) -> Error {
    zipnum_error(error.context, error.source.to_string())
}

fn sibling_path(path: &str, filename: &str) -> String {
    path.rsplit_once('/').map_or_else(
        || filename.to_owned(),
        |(parent, _)| format!("{parent}/{filename}"),
    )
}

/// The keys a URL may be indexed under: the Wayback Machine's and, when it differs, that of
/// `warcio.js`, so that indexes written by either family of tools are searched.
fn lookup_keys(url: &str) -> Result<Vec<Surt<'static>>, archivindex_surt::url::Error> {
    let wayback = Surt::from_url(url)?;
    let warcio = Canonicalizer::WARCIO.surt(url)?;
    let mut keys = vec![wayback];

    if warcio != keys[0] {
        keys.push(warcio);
    }

    Ok(keys)
}

fn line_key(line: &str) -> Option<&str> {
    line.split_once(' ').map(|(key, _)| key)
}

fn archive_path(filename: &str) -> String {
    if filename.starts_with(ARCHIVE_PREFIX) {
        filename.to_owned()
    } else {
        format!("{ARCHIVE_PREFIX}{filename}")
    }
}

/// Expand a gzip member, refusing to produce more than `limit` bytes.
fn decompress(path: &str, compressed: &[u8], limit: u64) -> Result<Vec<u8>, Error> {
    let mut decoded = Vec::new();
    // Reading one byte past the ceiling tells a member that exactly fits apart from one that was
    // cut short at it, which would otherwise be indistinguishable from valid content.
    let read = GzDecoder::new(compressed)
        .take(limit + 1)
        .read_to_end(&mut decoded)?;

    if read > usize::try_from(limit).unwrap_or(usize::MAX) {
        return Err(Error::DecompressedTooLarge {
            path: path.to_owned(),
            limit,
        });
    }

    Ok(decoded)
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

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn plain_indexes_are_decoded_once_per_reader() {
        let item = cdxj::Item {
            key: Cow::Borrowed("com,example)/"),
            timestamp: archivindex_cdx::timestamp::Timestamp::new(
                Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).single().unwrap(),
            ),
            fields: cdxj::ConformingFields::new(
                "https://example.com/",
                "sha256:00",
                "text/html",
                200,
                0,
                10,
                "data.warc",
            ),
        };
        let mut writer = crate::io::write::WaczWriter::new(Cursor::new(Vec::new()));
        writer.add_index("index.cdx", [&item]).unwrap();
        writer.add_warc("fixture.warc", &[][..]).unwrap();
        writer
            .add_pages(&crate::pages::PageListHeader::default(), [])
            .unwrap();
        let wacz = writer
            .finish(crate::frictionless::DataPackageBuilder::default())
            .unwrap()
            .into_inner();
        let mut reader = WaczReader::new(Cursor::new(wacz)).unwrap();

        let first = reader.lookup("https://example.com/", ..).unwrap();
        let second = reader.lookup("https://example.com/", ..).unwrap();

        assert_eq!(first.len(), 1);
        assert_eq!(first, second);
        assert_eq!(reader.plain_indexes.len(), 1);
    }

    #[test]
    fn decompression_stops_at_its_ceiling() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &vec![0; 4096]).unwrap();
        let compressed = encoder.finish().unwrap();

        let within = decompress("indexes/index.cdx.gz", &compressed, 4096).unwrap();
        let past = decompress("indexes/index.cdx.gz", &compressed, 4095)
            .expect_err("the member expands past the ceiling");

        assert_eq!(within.len(), 4096);
        assert!(matches!(
            past,
            Error::DecompressedTooLarge { path, limit }
                if path == "indexes/index.cdx.gz" && limit == 4095
        ));
    }

    #[test]
    fn zipnum_errors_carry_line_context() {
        let error = parse_summary(
            "indexes/index.idx",
            "!meta 0 {\"format\":\"cdxj-gzip-1.0\",\"filename\":\"index.cdx.gz\"}\ninvalid\n",
        )
        .expect_err("the second line is malformed");

        assert!(matches!(
            error,
            Error::InvalidZipNum { context, .. }
                if context.source == "indexes/index.idx"
                    && context.line == 2
                    && context.excerpt.as_deref() == Some("invalid")
        ));
    }
}
