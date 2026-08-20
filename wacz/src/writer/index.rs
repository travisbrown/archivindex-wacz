//! Plain and `ZipNum` CDXJ index writing.

use std::fmt::Write as _;
use std::io::{Seek, Write};

use flate2::Compression;
use flate2::write::GzEncoder;

use super::resource::options_for;
use super::{Error, IndexFormat, WaczWriter};
use crate::cdxj;
use crate::digest::Sha256Digest;
use crate::{GZIP_EXTENSION, INDEXES_PREFIX};

const ZIPNUM_FORMAT: &str = "cdxj-gzip-1.0";

impl<W: Write + Seek> WaczWriter<W> {
    /// Write a sorted CDXJ index in the configured plain or `ZipNum` format.
    pub fn add_index<'a, I: IntoIterator<Item = &'a cdxj::Item<'a>>>(
        &mut self,
        name: &str,
        items: I,
    ) -> Result<(), Error> {
        let mut rendered = items
            .into_iter()
            .map(|item| format!("{item}\n"))
            .collect::<Vec<_>>();
        rendered.sort_unstable();
        rendered.dedup();

        match self.config.index_format {
            IndexFormat::Plain => {
                let path = format!("{INDEXES_PREFIX}{name}");
                let options = options_for(&path, self.config.zip_compression_level)?;
                self.add_member(&path, options, |writer| {
                    for line in &rendered {
                        writer.write_all(line.as_bytes())?;
                    }
                    Ok(())
                })
            }
            IndexFormat::ZipNum { lines } => self.add_zipnum_index(name, &rendered, lines),
        }
    }

    fn add_zipnum_index(
        &mut self,
        name: &str,
        rendered: &[String],
        lines: usize,
    ) -> Result<(), Error> {
        let data_name = format!("{name}{GZIP_EXTENSION}");
        let idx_name = format!("{}.idx", name.strip_suffix(".cdx").unwrap_or(name));
        let data_path = format!("{INDEXES_PREFIX}{data_name}");
        let idx_path = format!("{INDEXES_PREFIX}{idx_name}");
        let escaped_data_name =
            serde_json::to_string(&data_name).expect("string serialization cannot fail");
        let mut summary = String::new();
        writeln!(
            summary,
            "!meta 0 {{\"format\": \"{ZIPNUM_FORMAT}\", \"filename\": {escaped_data_name}}}"
        )
        .expect("writing to a String cannot fail");

        let data_options = options_for(&data_path, self.config.zip_compression_level)?;
        self.add_member(&data_path, data_options, |writer| {
            let mut offset = 0_u64;
            for block in rendered.chunks(lines.max(1)) {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                for line in block {
                    encoder.write_all(line.as_bytes())?;
                }
                let compressed = encoder.finish()?;
                let length = compressed.len();
                let digest = Sha256Digest::compute(&compressed);
                writeln!(
                    summary,
                    "{} {{\"offset\": {offset}, \"length\": {length}, \"digest\": \"{digest}\"}}",
                    line_prefix(&block[0])
                )
                .expect("writing to a String cannot fail");
                writer.write_all(&compressed)?;
                offset += length as u64;
            }

            if offset == 0 {
                let compressed = GzEncoder::new(Vec::new(), Compression::default()).finish()?;
                writer.write_all(&compressed)?;
            }
            Ok(())
        })?;

        let idx_options = options_for(&idx_path, self.config.zip_compression_level)?;
        self.add_member(&idx_path, idx_options, |writer| {
            writer.write_all(summary.as_bytes())?;
            Ok(())
        })
    }
}

fn line_prefix(line: &str) -> &str {
    line.match_indices(' ')
        .nth(1)
        .map_or_else(|| line.trim_end(), |(index, _)| &line[..index])
}
