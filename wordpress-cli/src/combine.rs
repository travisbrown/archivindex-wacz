//! Combining the continuation segments of one site's collection archive.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufWriter, Read};
use std::path::{Path, PathBuf};

use archivindex_warc::io::read::WarcReader;
use archivindex_warc::io::write::{Compression, WarcWriter};

/// Options for combining a site's archive segments.
#[derive(Debug, clap::Args)]
pub struct CombineOptions {
    /// Directory containing the archive and resume-run WARC files.
    #[clap(long, value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    pub(super) input: PathBuf,
    /// Domain prefix used by the archive session names, such as `example.com`.
    #[clap(long, value_name = "DOMAIN")]
    pub(super) domain: String,
    /// Gzip-compressed WARC to write; its name must end in `.warc.gz` and it must not exist.
    #[clap(short, long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    pub(super) output: PathBuf,
}

/// What a successful combination wrote.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CombineSummary {
    pub(super) files: usize,
    pub(super) records: usize,
}

/// The ways archive segments can fail to combine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot read combine input directory {}: {source}", path.display())]
    InputDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("combine input directory {} contains no direct WARC files for domain {domain:?}", input.display())]
    NoInputs { input: PathBuf, domain: String },
    #[error("combined output {} must end in .warc.gz", .0.display())]
    OutputExtension(PathBuf),
    #[error("cannot create combined output {}: {source}", path.display())]
    OutputCreate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot inspect archive segment {}: {source}", path.display())]
    InspectInput {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read archive segment {}: {source}", path.display())]
    ReadInput {
        path: PathBuf,
        #[source]
        source: archivindex_warc::io::read::Error,
    },
    #[error("cannot write combined output {}: {source}", path.display())]
    WriteOutput {
        path: PathBuf,
        #[source]
        source: archivindex_warc::io::write::Error,
    },
    #[error("cannot flush combined output {}: {source}", path.display())]
    FlushOutput {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Combine domain-prefixed WARC files in filename order into a record-at-a-time gzip WARC.
pub fn combine_archives(options: &CombineOptions) -> Result<CombineSummary, Error> {
    if !has_warc_gzip_extension(&options.output) {
        return Err(Error::OutputExtension(options.output.clone()));
    }

    let inputs = domain_inputs(&options.input, &options.domain, &options.output)?;
    if inputs.is_empty() {
        return Err(Error::NoInputs {
            input: options.input.clone(),
            domain: options.domain.clone(),
        });
    }

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.output)
        .map_err(|source| Error::OutputCreate {
            path: options.output.clone(),
            source,
        })?;
    let result = write_inputs(&inputs, file, &options.output);
    if result.is_err()
        && let Err(error) = std::fs::remove_file(&options.output)
    {
        log::warn!(
            "could not remove partial combined output {}: {error}",
            options.output.display()
        );
    }

    result.map(|records| CombineSummary {
        files: inputs.len(),
        records,
    })
}

fn domain_inputs(input: &Path, domain: &str, output: &Path) -> Result<Vec<PathBuf>, Error> {
    let entries = std::fs::read_dir(input).map_err(|source| Error::InputDirectory {
        path: input.to_owned(),
        source,
    })?;
    let prefix = format!("{domain}-");
    let mut paths = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|source| Error::InputDirectory {
            path: input.to_owned(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if path != output && path.is_file() && name.starts_with(&prefix) && has_warc_extension(name)
        {
            paths.push(path);
        }
    }

    paths.sort();
    Ok(paths)
}

fn write_inputs(inputs: &[PathBuf], file: File, output: &Path) -> Result<usize, Error> {
    let mut writer = WarcWriter::new(BufWriter::new(file)).with_compression(Compression::gzip());
    let mut records = 0;

    for input in inputs {
        let gzip = is_gzip_file(input).map_err(|source| Error::InspectInput {
            path: input.clone(),
            source,
        })?;
        records += if gzip {
            let reader =
                WarcReader::from_path_gzip(input).map_err(|source| Error::InspectInput {
                    path: input.clone(),
                    source,
                })?;
            copy_records(reader, &mut writer, input, output)?
        } else {
            let reader = WarcReader::from_path(input).map_err(|source| Error::InspectInput {
                path: input.clone(),
                source,
            })?;
            copy_records(reader, &mut writer, input, output)?
        };
    }

    writer.flush().map_err(|source| Error::FlushOutput {
        path: output.to_owned(),
        source,
    })?;
    Ok(records)
}

fn copy_records<R: BufRead>(
    reader: WarcReader<R>,
    writer: &mut WarcWriter<BufWriter<File>>,
    input: &Path,
    output: &Path,
) -> Result<usize, Error> {
    let mut count = 0;
    for record in reader.iter_raw_records().records() {
        let record = record.map_err(|source| Error::ReadInput {
            path: input.to_owned(),
            source,
        })?;
        writer.write(&record).map_err(|source| Error::WriteOutput {
            path: output.to_owned(),
            source,
        })?;
        count += 1;
    }

    Ok(count)
}

fn is_gzip_file(path: &Path) -> Result<bool, std::io::Error> {
    let mut file = File::open(path)?;
    let mut magic = [0; 2];
    Ok(file.read(&mut magic)? == magic.len() && magic == [0x1f, 0x8b])
}

fn has_warc_extension(name: &str) -> bool {
    let path = Path::new(name);
    path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("warc")
            || extension.eq_ignore_ascii_case("gz")
                && path
                    .file_stem()
                    .and_then(|stem| Path::new(stem).extension())
                    .is_some_and(|inner| inner.eq_ignore_ascii_case("warc"))
    })
}

fn has_warc_gzip_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gz"))
        && path
            .file_stem()
            .and_then(|stem| Path::new(stem).extension())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("warc"))
}
