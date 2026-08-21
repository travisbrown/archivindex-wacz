//! Plain and `ZipNum` CDXJ index writing.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{BufRead, BufReader, Seek, Write};

use flate2::Compression;
use flate2::write::GzEncoder;

use super::resource::options_for;
use super::{Error, IndexFormat, WaczWriter};
use crate::cdxj;
use crate::digest::Sha256Digest;
use crate::zipnum::{self, FORMAT, SummaryEntry, SummaryHeader};
use crate::{GZIP_EXTENSION, INDEXES_PREFIX};

impl<W: Write + Seek> WaczWriter<W> {
    /// Write a sorted CDXJ index in the configured plain or `ZipNum` format.
    pub fn add_index<'a, I: IntoIterator<Item = &'a cdxj::ConformingItem<'a>>>(
        &mut self,
        name: &str,
        items: I,
    ) -> Result<(), Error> {
        if !crate::paths::valid_index_name(name) {
            return Err(Error::InvalidIndexName(name.to_owned()));
        }
        self.write_index(name, items, |item| {
            Ok(crate::cdxj::validate_cdxj_extra(&item.fields.extra)?)
        })
    }

    /// Write an index while explicitly allowing entries that omit normative CDXJ fields.
    pub fn add_index_lenient<'a, I: IntoIterator<Item = &'a cdxj::Item<'a>>>(
        &mut self,
        name: &str,
        items: I,
    ) -> Result<(), Error> {
        if !crate::paths::valid_index_name(name) {
            return Err(Error::InvalidIndexName(name.to_owned()));
        }
        self.write_index(name, items, |_| Ok(()))
    }

    /// Add an already sorted, deduplicated CDXJ file without retaining the collection in memory.
    pub fn add_sorted_index_file<R: BufRead + Seek>(
        &mut self,
        name: &str,
        mut reader: R,
    ) -> Result<(), Error> {
        if !crate::paths::valid_index_name(name) {
            return Err(Error::InvalidIndexName(name.to_owned()));
        }
        for item in cdxj::IndexReader::new(&mut reader) {
            item.map_err(Error::InvalidIndex)?;
        }
        reader.rewind()?;
        match self.config.index_format {
            IndexFormat::Plain => {
                let path = format!("{INDEXES_PREFIX}{name}");
                self.add_typed_resource(&path, reader)
            }
            IndexFormat::ZipNum { lines } => self.add_zipnum_stream(name, reader, lines),
        }
    }

    fn write_index<'a, F: serde::Serialize + 'a, I: IntoIterator<Item = &'a cdxj::Item<'a, F>>>(
        &mut self,
        name: &str,
        items: I,
        validate: impl Fn(&cdxj::Item<'a, F>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut sorter = IndexSpool::new();
        for item in items {
            validate(item)?;
            sorter.push(item)?;
        }
        self.add_sorted_index_file(name, BufReader::new(sorter.finish()?))
    }

    /// Add an index accumulated by an [`IndexSpool`].
    pub fn add_spooled_index(&mut self, name: &str, spool: IndexSpool) -> Result<(), Error> {
        self.add_sorted_index_file(name, BufReader::new(spool.finish()?))
    }

    fn add_zipnum_stream(
        &mut self,
        name: &str,
        mut reader: impl BufRead,
        lines_per_block: usize,
    ) -> Result<(), Error> {
        let data_name = format!("{name}{GZIP_EXTENSION}");
        let idx_name = format!("{}.idx", name.strip_suffix(".cdx").unwrap_or(name));
        let data_path = format!("{INDEXES_PREFIX}{data_name}");
        let idx_path = format!("{INDEXES_PREFIX}{idx_name}");
        let mut summary = tempfile::tempfile()?;
        let header = SummaryHeader {
            format: Cow::Borrowed(FORMAT),
            filename: Cow::Borrowed(&data_name),
        };
        let header = zipnum::to_json(&header).expect("summary header serialization cannot fail");
        writeln!(summary, "!meta 0 {header}")?;
        let data_options = options_for(&data_path, self.config.zip_compression_level)?;
        self.add_member(&data_path, data_options, |writer| {
            let mut offset = 0_u64;
            let mut block = Vec::with_capacity(lines_per_block.max(1));
            loop {
                let mut line = String::new();
                let eof = reader.read_line(&mut line)? == 0;
                if !eof {
                    block.push(line);
                }
                if block.len() == lines_per_block.max(1) || (!block.is_empty() && eof) {
                    let (prefix, _) = cdxj::split_prefix(&block[0])
                        .expect("validated CDXJ lines have a key and timestamp");
                    let mut compressed = tempfile::tempfile()?;
                    {
                        let mut encoder = GzEncoder::new(&mut compressed, Compression::default());
                        for value in &block {
                            encoder.write_all(value.as_bytes())?;
                        }
                        encoder.finish()?;
                    }
                    let length = compressed.stream_position()?;
                    compressed.rewind()?;
                    let (digest, copied) = Sha256Digest::from_reader(&mut compressed)?;
                    compressed.rewind()?;
                    std::io::copy(&mut compressed, writer)?;
                    debug_assert_eq!(length, copied);
                    let entry = SummaryEntry {
                        offset,
                        length,
                        digest,
                    };
                    let entry =
                        zipnum::to_json(&entry).expect("summary entry serialization cannot fail");
                    writeln!(summary, "{prefix} {entry}")?;
                    offset += length;
                    block.clear();
                }
                if eof {
                    if offset == 0 {
                        let compressed =
                            GzEncoder::new(Vec::new(), Compression::default()).finish()?;
                        writer.write_all(&compressed)?;
                    }
                    break;
                }
            }
            Ok(())
        })?;
        summary.rewind()?;
        self.add_typed_resource(&idx_path, summary)
    }
}

const SORT_CHUNK_LINES: usize = 4096;
const SORT_MERGE_FAN_IN: usize = 64;

/// A disk-backed, incrementally populated CDXJ sorter.
///
/// Lines are sorted and deduplicated in bounded-memory runs. The completed spool can be passed to
/// [`WaczWriter::add_spooled_index`] after another streaming member has released the writer.
pub struct IndexSpool {
    chunk: Vec<String>,
    runs: Vec<std::fs::File>,
}

impl IndexSpool {
    /// Create an empty index spool.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chunk: Vec::with_capacity(SORT_CHUNK_LINES),
            runs: Vec::new(),
        }
    }

    /// Add one CDXJ item.
    pub fn push<F: serde::Serialize>(
        &mut self,
        item: &cdxj::Item<'_, F>,
    ) -> Result<(), std::io::Error> {
        self.push_line(format!("{item}\n"))
    }

    fn push_line(&mut self, line: String) -> Result<(), std::io::Error> {
        self.chunk.push(line);
        if self.chunk.len() == SORT_CHUNK_LINES {
            self.flush_run()?;
        }
        Ok(())
    }

    fn flush_run(&mut self) -> Result<(), std::io::Error> {
        self.chunk.sort_unstable();
        self.chunk.dedup();
        let mut run = tempfile::tempfile()?;
        for line in self.chunk.drain(..) {
            run.write_all(line.as_bytes())?;
        }
        run.rewind()?;
        self.runs.push(run);
        Ok(())
    }

    fn finish(mut self) -> Result<std::fs::File, std::io::Error> {
        if !self.chunk.is_empty() || self.runs.is_empty() {
            self.flush_run()?;
        }
        let mut runs = self.runs;
        while runs.len() > SORT_MERGE_FAN_IN {
            let mut source = runs.into_iter();
            let mut merged = Vec::new();
            loop {
                let group = source.by_ref().take(SORT_MERGE_FAN_IN).collect::<Vec<_>>();
                if group.is_empty() {
                    break;
                }
                merged.push(merge_runs(group)?);
            }
            runs = merged;
        }
        merge_runs(runs)
    }
}

impl Default for IndexSpool {
    fn default() -> Self {
        Self::new()
    }
}

fn merge_runs(runs: Vec<std::fs::File>) -> Result<std::fs::File, std::io::Error> {
    let mut readers = runs.into_iter().map(BufReader::new).collect::<Vec<_>>();
    let mut heap = BinaryHeap::new();
    for (index, reader) in readers.iter_mut().enumerate() {
        let mut line = String::new();
        if reader.read_line(&mut line)? != 0 {
            heap.push(Reverse((line, index)));
        }
    }
    let mut output = tempfile::tempfile()?;
    let mut previous = String::new();
    while let Some(Reverse((line, index))) = heap.pop() {
        if line != previous {
            output.write_all(line.as_bytes())?;
            previous.clone_from(&line);
        }
        let mut next = String::new();
        if readers[index].read_line(&mut next)? != 0 {
            heap.push(Reverse((next, index)));
        }
    }
    output.rewind()?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    #[test]
    fn external_sort_crosses_run_boundaries_and_deduplicates() {
        let mut sorter = IndexSpool::new();
        for value in (0..5000).rev() {
            sorter.push_line(format!("{value:04}\n")).unwrap();
        }
        sorter.push_line("2500\n".to_owned()).unwrap();
        let mut output = String::new();
        sorter
            .finish()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        let lines = output.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 5000);
        assert_eq!(lines.first(), Some(&"0000"));
        assert_eq!(lines.last(), Some(&"4999"));
        assert!(lines.is_sorted());
    }
}
