//! Plain and `ZipNum` CDXJ index writing.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Seek, Write};
use std::path::PathBuf;

use archivindex_cdx::cdxj;
use archivindex_cdx::timestamp::Timestamp;
use flate2::Compression;
use flate2::write::GzEncoder;

use super::resource::options_for;
use super::{Error, IndexFormat, WaczWriter};
use crate::cdxj::IndexReader;
use crate::digest::Sha256Digest;
use crate::zipnum::{FORMAT, SummaryEntry, SummaryHeader};
use crate::{GZIP_EXTENSION, INDEXES_PREFIX};

impl<W: Write + Seek> WaczWriter<W> {
    /// Write a sorted CDXJ index in the configured plain or `ZipNum` format.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidIndexName`] if the name is not a direct `.cdx` member name, and
    /// [`Error::Io`] if the index cannot be written.
    pub fn add_index<'a, I: IntoIterator<Item = &'a cdxj::ConformingItem<'a>>>(
        &mut self,
        name: &str,
        items: I,
    ) -> Result<(), Error> {
        let mut spool = IndexSpool::new();
        for item in items {
            spool.push(item)?;
        }

        self.add_spooled_index(name, spool)
    }

    /// Add an already sorted, deduplicated CDXJ file without retaining the collection in memory.
    ///
    /// Every line is parsed before any member is written, and the file is rejected if a line sorts
    /// before its predecessor by `(key, timestamp)`; lines sharing a prefix are accepted in the
    /// order given.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidIndexName`] for a name that is not a direct `.cdx` member name,
    /// [`Error::InvalidIndex`] for a line that is not a CDXJ item, [`Error::NonConformingIndex`]
    /// for a line that omits a required CDXJ field, and [`Error::UnsortedIndex`] for the first
    /// line that sorts before the line preceding it.
    pub fn add_sorted_index_file<R: BufRead + Seek>(
        &mut self,
        name: &str,
        mut reader: R,
    ) -> Result<(), Error> {
        if !crate::paths::valid_index_name(name) {
            return Err(Error::InvalidIndexName(name.to_owned()));
        }
        let mut previous: Option<(String, Timestamp)> = None;
        for (index, item) in IndexReader::new(&mut reader).enumerate() {
            let item = item.map_err(Error::InvalidIndex)?;
            cdxj::ConformingFields::try_from(&item.fields).map_err(|source| {
                Error::NonConformingIndex {
                    line: index + 1,
                    source,
                }
            })?;
            // The reader yields owned keys, so `into_owned` moves rather than copies.
            let current = (item.key.into_owned(), item.timestamp);
            if previous.is_some_and(|previous| previous > current) {
                return Err(Error::UnsortedIndex { line: index + 1 });
            }
            previous = Some(current);
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
        let mut summary = BufWriter::new(tempfile::tempfile()?);
        let header = SummaryHeader {
            format: Cow::Borrowed(FORMAT),
            filename: Cow::Borrowed(&data_name),
        };
        let header =
            serde_json::to_string(&header).expect("summary header serialization cannot fail");
        writeln!(summary, "!meta 0 {header}")?;
        let data_options = options_for(&data_path, self.config.zip_compression_level);
        let compression = Compression::new(self.config.gzip_compression_level);
        let lines_per_block = lines_per_block.max(1);
        self.add_member(&data_path, data_options, |writer| {
            let mut offset = 0_u64;
            let mut line = String::new();
            let mut prefix = String::new();
            let mut compressed = Vec::new();
            // Each block is one gzip member encoded into a reused buffer, digested and written
            // from memory; the line buffer is reused across the whole stream.
            while reader.read_line(&mut line)? != 0 {
                let (first_prefix, _) = cdxj::split_prefix(&line)
                    .expect("validated CDXJ lines have a key and timestamp");
                prefix.clear();
                prefix.push_str(first_prefix);
                let mut encoder = GzEncoder::new(&mut compressed, compression);
                let mut lines = 0;
                loop {
                    encoder.write_all(line.as_bytes())?;
                    lines += 1;
                    line.clear();
                    if lines == lines_per_block || reader.read_line(&mut line)? == 0 {
                        break;
                    }
                }
                let block = encoder.finish()?;
                writer.write_all(block)?;
                let length = block.len() as u64;
                let entry = SummaryEntry {
                    offset,
                    length,
                    digest: Sha256Digest::compute(block.as_slice()),
                };
                let entry =
                    serde_json::to_string(&entry).expect("summary entry serialization cannot fail");
                writeln!(summary, "{prefix} {entry}")?;
                offset += length;
                block.clear();
            }
            if offset == 0 {
                let empty = GzEncoder::new(Vec::new(), compression).finish()?;
                writer.write_all(&empty)?;
            }
            Ok(())
        })?;
        self.add_typed_resource(&idx_path, finish_writer(summary)?)
    }
}

/// The number of bytes of lines buffered in memory before they are sorted into a run.
const SORT_RUN_BYTES: usize = 64 << 20;
/// The number of runs merged at once.
const SORT_MERGE_FAN_IN: usize = 64;

/// A disk-backed, incrementally populated CDXJ sorter.
///
/// Lines are sorted and deduplicated in bounded-memory runs with bounded merge fan-in. The
/// completed spool can be passed to [`WaczWriter::add_spooled_index`].
pub struct IndexSpool {
    chunk: Vec<String>,
    chunk_bytes: usize,
    run_bytes: usize,
    fan_in: usize,
    directory: Option<tempfile::TempDir>,
    runs: Vec<Run>,
    created_runs: usize,
}

/// A sorted, deduplicated run file; `level` counts the merges that produced it.
struct Run {
    path: PathBuf,
    level: u32,
}

impl IndexSpool {
    /// Create an empty index spool.
    ///
    /// No temporary storage is used until the buffered lines exceed the run size.
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(SORT_RUN_BYTES, SORT_MERGE_FAN_IN)
    }

    fn with_limits(run_bytes: usize, fan_in: usize) -> Self {
        Self {
            chunk: Vec::new(),
            chunk_bytes: 0,
            run_bytes,
            fan_in: fan_in.max(2),
            directory: None,
            runs: Vec::new(),
            created_runs: 0,
        }
    }

    /// Add one CDXJ item.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if a run cannot be written.
    pub fn push(&mut self, item: &cdxj::ConformingItem<'_>) -> Result<(), Error> {
        Ok(self.push_line(format!("{item}\n"))?)
    }

    fn push_line(&mut self, line: String) -> Result<(), std::io::Error> {
        self.chunk_bytes += line.len();
        self.chunk.push(line);
        if self.chunk_bytes >= self.run_bytes {
            self.flush_run()?;
        }
        Ok(())
    }

    /// Sort buffered lines into a run and merge complete groups of same-level runs.
    fn flush_run(&mut self) -> Result<(), std::io::Error> {
        let path = self.run_path()?;
        write_sorted(&mut self.chunk, File::create(&path)?)?;
        self.chunk_bytes = 0;
        self.runs.push(Run { path, level: 0 });
        while self.runs.len() >= self.fan_in && self.newest_share_level() {
            self.merge_newest()?;
        }
        Ok(())
    }

    fn newest_share_level(&self) -> bool {
        let newest = &self.runs[self.runs.len() - self.fan_in..];
        newest.iter().all(|run| run.level == newest[0].level)
    }

    /// Merge the newest `fan_in` runs into one run of the next level, removing their files.
    fn merge_newest(&mut self) -> Result<(), std::io::Error> {
        let path = self.run_path()?;
        let group = self.runs.split_off(self.runs.len() - self.fan_in);
        merge_runs(&group, File::create(&path)?)?;
        self.runs.push(Run {
            path,
            level: group[0].level + 1,
        });
        Ok(())
    }

    fn run_path(&mut self) -> Result<PathBuf, std::io::Error> {
        let directory = match self.directory.as_ref() {
            Some(directory) => directory,
            None => self.directory.insert(tempfile::tempdir()?),
        };
        let path = directory.path().join(format!("run-{}", self.created_runs));
        self.created_runs += 1;
        Ok(path)
    }

    fn finish(mut self) -> Result<File, std::io::Error> {
        if self.runs.is_empty() {
            return write_sorted(&mut self.chunk, tempfile::tempfile()?);
        }
        if !self.chunk.is_empty() {
            self.flush_run()?;
        }
        while self.runs.len() > self.fan_in {
            self.merge_newest()?;
        }
        merge_runs(&self.runs, tempfile::tempfile()?)
    }
}

impl Default for IndexSpool {
    fn default() -> Self {
        Self::new()
    }
}

/// Sort and deduplicate `chunk` into `sink`, leaving the chunk empty and the sink rewound.
fn write_sorted(chunk: &mut Vec<String>, sink: File) -> Result<File, std::io::Error> {
    chunk.sort_unstable();
    chunk.dedup();
    let mut writer = BufWriter::new(sink);
    for line in chunk.drain(..) {
        writer.write_all(line.as_bytes())?;
    }
    finish_writer(writer)
}

/// Merge sorted runs into `sink`, dropping repeated lines, then remove the run files.
fn merge_runs(runs: &[Run], sink: File) -> Result<File, std::io::Error> {
    let mut readers = runs
        .iter()
        .map(|run| File::open(&run.path).map(BufReader::new))
        .collect::<Result<Vec<_>, _>>()?;
    let mut heap = BinaryHeap::with_capacity(readers.len());
    for (index, reader) in readers.iter_mut().enumerate() {
        let mut line = String::new();
        if reader.read_line(&mut line)? != 0 {
            heap.push(Reverse((line, index)));
        }
    }
    let mut output = BufWriter::new(sink);
    let mut previous = String::new();
    // Each popped line's buffer is reused for the next line of the same run; an emitted line
    // swaps buffers with `previous` instead of being copied.
    while let Some(Reverse((mut line, index))) = heap.pop() {
        if line != previous {
            output.write_all(line.as_bytes())?;
            std::mem::swap(&mut previous, &mut line);
        }
        line.clear();
        if readers[index].read_line(&mut line)? != 0 {
            heap.push(Reverse((line, index)));
        }
    }
    drop(readers);
    for run in runs {
        std::fs::remove_file(&run.path)?;
    }
    finish_writer(output)
}

fn finish_writer(writer: BufWriter<File>) -> Result<File, std::io::Error> {
    let mut file = writer
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?;
    file.rewind()?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn sorted_output(mut sorter: IndexSpool) -> Vec<String> {
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
        output.lines().map(str::to_owned).collect()
    }

    #[test]
    fn external_sort_merges_runs_across_levels_and_deduplicates() {
        // Runs of about a dozen lines and a fan-in of two force merges at many levels.
        let lines = sorted_output(IndexSpool::with_limits(64, 2));

        assert_eq!(lines.len(), 5000);
        assert_eq!(lines.first().map(String::as_str), Some("0000"));
        assert_eq!(lines.last().map(String::as_str), Some("4999"));
        assert!(lines.is_sorted());
    }

    #[test]
    fn in_memory_sort_never_creates_a_run_directory() {
        let mut sorter = IndexSpool::new();
        sorter.push_line("b\n".to_owned()).unwrap();
        sorter.push_line("a\n".to_owned()).unwrap();

        assert!(sorter.directory.is_none());
        let mut output = String::new();
        sorter
            .finish()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        assert_eq!(output, "a\nb\n");
    }
}
