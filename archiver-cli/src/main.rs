//! A command-line front end for archiving URLs into WARC files.
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use archivindex_archiver::capture::{CaptureControl, CaptureEvent};
use archivindex_archiver::{Archiver, Config};
use cli_helpers::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            error.exit_code()
        }
    }
}

fn run() -> Result<(), Error> {
    let opts: Opts = Opts::parse();
    opts.verbose.init_logging()?;

    match opts.command {
        Command::Archive(options) => archive(options),
    }
}

/// Archive a list of URLs read from standard input.
fn archive(options: ArchiveOptions) -> Result<(), Error> {
    let config = options.config.into_config(options.concurrency);
    let archiver = Archiver::new(config)?;
    let mut input_error = None;
    let urls = read_urls(std::io::stdin().lock(), &mut input_error);
    let progress = progress_spinner("Archiving", "URLs");
    let mut events = |event: CaptureEvent<'_>| {
        if matches!(event, CaptureEvent::Written { .. }) {
            progress.inc(1);
        }
        CaptureControl::Continue
    };
    let result = archiver.archive_to_path_with_events(urls, &options.output, &mut events);
    progress.finish_and_clear();
    let summary = result?;
    if let Some(error) = &input_error {
        log::warn!("Stopped reading input early: {error}");
    }

    for failure in &summary.failures {
        log::warn!("Failed to capture {}: {}", failure.url, failure.error);
    }

    println!(
        "Archived {} of {} URLs to {}",
        summary.captures.len(),
        summary.captures.len() + summary.failures.len(),
        options.output.display()
    );

    if summary.is_complete() && input_error.is_none() {
        Ok(())
    } else {
        Err(Error::PartialArchive(options.output))
    }
}

/// Read one URL per line, trimming surrounding whitespace and skipping blank lines.
///
/// A read failure ends iteration and is stored in `error`.
fn read_urls<'a, R: BufRead + 'a>(
    reader: R,
    error: &'a mut Option<std::io::Error>,
) -> impl Iterator<Item = String> + 'a {
    reader
        .lines()
        .map_while(move |line| match line {
            Ok(line) => {
                let url = line.trim();
                Some((!url.is_empty()).then(|| url.to_owned()))
            }
            Err(source) => {
                *error = Some(source);
                None
            }
        })
        .flatten()
}

fn progress_spinner(message: &'static str, unit: &str) -> ProgressBar {
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template(&format!("{{msg}} {{human_pos}} {unit} {{spinner}}"))
            .expect("valid progress spinner template"),
    );
    progress.set_message(message);
    progress
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("a partial archive was published at {}", .0.display())]
    PartialArchive(PathBuf),
    #[error("CLI argument reading error: {0}")]
    Args(#[from] cli_helpers::Error),
    #[error("archiving error: {0}")]
    Archive(#[from] archivindex_archiver::Error),
    #[error(transparent)]
    UserAgent(#[from] archivindex_archiver::UserAgentError),
    #[error(transparent)]
    SessionId(#[from] archivindex_archiver::session::SessionIdError),
}

impl Error {
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::PartialArchive(_) => ExitCode::from(2),
            _ => ExitCode::FAILURE,
        }
    }
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-archiver", version, author)]
struct Opts {
    #[clap(flatten)]
    verbose: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

/// The archiving workflow to run.
#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Archive URLs read one per line from standard input.
    Archive(ArchiveOptions),
}

/// Options for archiving URLs read from standard input.
#[derive(Debug, clap::Args)]
struct ArchiveOptions {
    #[clap(flatten)]
    config: ConfigOptions,
    /// Path of the WARC file to write (an existing file is not overwritten).
    #[clap(long)]
    output: PathBuf,
    /// The number of URLs downloaded concurrently (defaults to 1).
    #[clap(long)]
    concurrency: Option<usize>,
}

/// Capture settings for the archiving workflow.
#[derive(Debug, clap::Args)]
struct ConfigOptions {
    /// Compress each WARC record as an independent gzip member.
    #[clap(long)]
    gzip: bool,
    /// The User-Agent header value sent with every request (defaults to the archiver's own).
    #[clap(long)]
    user_agent: Option<String>,
    /// The timeout in seconds for each request (defaults to 30).
    #[clap(long)]
    timeout: Option<u64>,
    /// The maximum number of redirects followed for each URL (defaults to 10).
    #[clap(long)]
    max_redirects: Option<usize>,
    /// The maximum number of response bytes stored for one fetch (unbounded when unset; a response
    /// reaching the limit is archived truncated rather than failed).
    #[clap(long)]
    max_response_length: Option<u64>,
}

impl ConfigOptions {
    /// Build an archiver configuration, optionally overriding its concurrency.
    fn into_config(self, concurrency: Option<usize>) -> Config {
        let defaults = Config::default();

        Config {
            user_agent: self.user_agent.unwrap_or(defaults.user_agent),
            timeout: self.timeout.map_or(defaults.timeout, Duration::from_secs),
            max_redirects: self.max_redirects.unwrap_or(defaults.max_redirects),
            concurrency: concurrency.unwrap_or(defaults.concurrency),
            max_response_length: self.max_response_length,
            gzip_warc: self.gzip,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;

    use cli_helpers::prelude::Parser;

    use super::{Command, Error, Opts};

    #[test]
    fn partial_archives_have_a_distinct_exit_status() {
        assert_eq!(
            Error::PartialArchive("partial.warc".into()).exit_code(),
            ExitCode::from(2)
        );
        assert_eq!(
            Error::Archive(archivindex_archiver::Error::MissingHost(
                "mailto:a@b".to_owned()
            ))
            .exit_code(),
            ExitCode::FAILURE
        );
    }

    #[test]
    fn read_urls_trims_and_skips_blank_lines() {
        let input = "https://example.com/\n\n  https://example.org/  \n";

        let mut error = None;
        let urls = super::read_urls(input.as_bytes(), &mut error).collect::<Vec<_>>();

        assert_eq!(urls, ["https://example.com/", "https://example.org/"]);
        assert!(error.is_none());
    }

    #[test]
    fn archive_defaults_to_an_uncompressed_warc() {
        let options = Opts::try_parse_from([
            "archivindex-archiver",
            "archive",
            "--output",
            "capture.warc",
        ])
        .expect("valid options");

        let Command::Archive(options) = options.command;

        assert!(!options.config.gzip);
    }
}
