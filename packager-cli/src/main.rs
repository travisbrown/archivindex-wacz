//! A command-line front end for packaging WARC captures as WACZ distributions.
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use archivindex_packager::WarcToWacz;
use archivindex_wacz::io::write::IndexFormat;
use cli_helpers::prelude::*;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::WarcToWacz(options) => warc_to_wacz(&options),
    }
}

/// Convert an existing WARC file into an indexed WACZ package.
fn warc_to_wacz(options: &WarcToWaczOptions) -> Result<(), Error> {
    let index_format = if options.compressed_index {
        IndexFormat::zipnum()
    } else {
        IndexFormat::Plain
    };
    let summary = WarcToWacz::new(&options.warc, &options.output)
        .index_format(index_format)
        .gzip_warc(options.gzip_warc)
        .gzip_compression_level(options.gzip_compression_level)
        .zip_compression_level(options.zip_compression_level)
        .run()?;

    for warning in &summary.warnings {
        log::warn!("{warning}");
    }
    println!(
        "Converted {} records and {} captures from {} to {}",
        summary.records,
        summary.captures,
        options.warc.display(),
        options.output.display()
    );
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("CLI argument reading error: {0}")]
    Args(#[from] cli_helpers::Error),
    #[error("WARC conversion error: {0}")]
    Convert(#[from] archivindex_packager::Error),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-packager", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

/// The packaging workflow to run.
#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Convert an existing WARC file into an indexed WACZ package.
    WarcToWacz(WarcToWaczOptions),
}

/// Options for converting an existing WARC file into a WACZ package.
#[derive(Debug, clap::Args)]
struct WarcToWaczOptions {
    /// Plain or gzip-compressed WARC file to convert.
    warc: PathBuf,
    /// Path of the WACZ file to write (an existing file is not overwritten).
    #[clap(long)]
    output: PathBuf,
    /// Write the index as a compressed `ZipNum` pair instead of plain CDXJ.
    #[clap(long)]
    compressed_index: bool,
    /// Gzip a plain input WARC, compressing each record independently for random access.
    #[clap(long)]
    gzip_warc: bool,
    /// Gzip compression level for packaged WARC records (0-9; defaults to 6).
    #[clap(long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(0..=9))]
    gzip_compression_level: u32,
    /// ZIP DEFLATE level for compressible WACZ members (1-264; defaults to 6).
    #[clap(long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(1..=264))]
    zip_compression_level: u32,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cli_helpers::prelude::Parser;

    use super::{Command, Opts};

    #[test]
    fn warc_conversion_command_reads_paths_and_index_format() {
        let options = Opts::try_parse_from([
            "archivindex-packager",
            "warc-to-wacz",
            "capture.warc.gz",
            "--output",
            "capture.wacz",
            "--compressed-index",
            "--gzip-warc",
            "--gzip-compression-level",
            "9",
            "--zip-compression-level",
            "264",
        ])
        .expect("valid options");

        let Command::WarcToWacz(options) = options.command;
        assert_eq!(options.warc, PathBuf::from("capture.warc.gz"));
        assert_eq!(options.output, PathBuf::from("capture.wacz"));
        assert!(options.compressed_index);
        assert!(options.gzip_warc);
        assert_eq!(options.gzip_compression_level, 9);
        assert_eq!(options.zip_compression_level, 264);
    }

    #[test]
    fn warc_conversion_command_rejects_invalid_gzip_compression_levels() {
        let result = Opts::try_parse_from([
            "archivindex-packager",
            "warc-to-wacz",
            "capture.warc",
            "--output",
            "capture.wacz",
            "--gzip-warc",
            "--gzip-compression-level",
            "10",
        ]);

        assert!(result.is_err());
    }

    #[test]
    fn warc_conversion_command_rejects_invalid_zip_compression_levels() {
        let result = Opts::try_parse_from([
            "archivindex-packager",
            "warc-to-wacz",
            "capture.warc",
            "--output",
            "capture.wacz",
            "--zip-compression-level",
            "265",
        ]);

        assert!(result.is_err());
    }
}
