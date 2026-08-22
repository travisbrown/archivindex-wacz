//! Command-line tools for writing WARC captures and packaging WACZ distributions.
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use archivindex_archiver::client::{Archiver, CaptureControl, CaptureEvent};
use archivindex_archiver::config::Config;
use archivindex_archiver::session::{
    Capture, CaptureProcessor, Inspection, Operator, RetryConfig, Session,
};
use archivindex_archiver::wordpress::read::read_comments;
use archivindex_archiver::wordpress::{CommentCaptureProcessor, CommentProgress};
use archivindex_packager::WarcToWacz;
use archivindex_wacz::io::write::IndexFormat;
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
        Command::ArchiveWpComments(options) => archive_wp_comments(options),
        Command::WarcToWacz(options) => warc_to_wacz(&options),
        Command::ReadWpComments(options) => read_wp_comments(options),
    }
}

/// Archive a list of URLs read from standard input.
fn archive(options: ArchiveOptions) -> Result<(), Error> {
    let config = options.config.into_config(options.concurrency);
    let archiver = Archiver::new(config)?;
    let input_error = Rc::new(RefCell::new(None));
    let urls = read_urls(std::io::stdin().lock(), Rc::clone(&input_error));
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
    let input_error = input_error.borrow_mut().take();
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

/// Archive the comments exposed by a site's `WordPress` REST API v2 endpoint.
fn archive_wp_comments(options: ArchiveWpCommentsOptions) -> Result<(), Error> {
    let titles = options.titles;
    let processor =
        CommentCaptureProcessor::new(&options.base_url)?.second_sweep(options.second_sweep);
    let first_url = processor.first_comment_url();
    let comment_progress = Rc::new(RefCell::new(None));
    let processor = ProgressingCommentProcessor {
        processor,
        progress: Rc::clone(&comment_progress),
    };
    let retry_defaults = RetryConfig::default();
    let retry = RetryConfig {
        attempts: options.retry_attempts.unwrap_or(retry_defaults.attempts),
        initial_backoff: options
            .retry_initial_backoff
            .map_or(retry_defaults.initial_backoff, Duration::from_secs),
        max_backoff: options
            .retry_max_backoff
            .map_or(retry_defaults.max_backoff, Duration::from_secs),
    };
    let config = options.config.into_config(None);
    let archiver = Archiver::new(config)?;
    let progress = message_spinner("Downloading comments");
    let event_progress = progress.clone();
    let event_comment_progress = Rc::clone(&comment_progress);
    let operator = Operator {
        name: options.operator,
        email: options.operator_email,
    };
    let mut session = Session::new(
        archiver,
        &options.session_name,
        operator,
        [first_url],
        &options.output,
    )?
    .processor(processor)
    .events(move |event: CaptureEvent<'_>| {
        if matches!(event, CaptureEvent::Written { .. })
            && let Some(snapshot) = *event_comment_progress.borrow()
        {
            event_progress.set_message(snapshot.to_string());
        }
        CaptureControl::Continue
    })
    .retry(retry)
    .request_delay(Duration::from_secs(options.request_delay));

    if titles {
        session = session.titles();
    }

    if let Some(revisit_index) = options.revisit_index {
        session = session.revisit_index(revisit_index);
    }
    if let Some(limit) = options.limit {
        session = session.limit(limit);
    }

    let summary = session.run()?;
    progress.finish_and_clear();

    for failure in &summary.failures {
        log::warn!("Failed to capture {}: {}", failure.url, failure.error);
    }
    if let Some(error) = &summary.fatal_error {
        log::warn!("The session ended early: {error}");
    }

    if let Some(snapshot) = *comment_progress.borrow() {
        if let Some(shortfall) = snapshot.visibility_shortfall() {
            log::warn!(
                "WordPress counted {} comments before visibility filtering but returned {} visible comments ({shortfall} omitted)",
                snapshot.total,
                snapshot.downloaded
            );
        }
        println!("{snapshot} to {}", options.output.display());
    } else {
        println!("Downloaded no comments to {}", options.output.display());
    }

    if summary.is_complete() {
        Ok(())
    } else {
        Err(Error::PartialArchive(options.output))
    }
}

struct ProgressingCommentProcessor {
    processor: CommentCaptureProcessor,
    progress: Rc<RefCell<Option<CommentProgress>>>,
}

impl CaptureProcessor for ProgressingCommentProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        let inspection = self.processor.inspect(capture);
        *self.progress.borrow_mut() = self.processor.progress();
        inspection
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

/// Read, sort, and deduplicate `WordPress` comments captured in a WARC file.
fn read_wp_comments(options: ReadWpCommentsOptions) -> Result<(), Error> {
    let result = read_comments(options.warc)?;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    for comment in result.comments {
        serde_json::to_writer(&mut output, &comment)?;
        writeln!(output)?;
    }

    for warning in result.warnings {
        log::warn!(
            "Conflicting objects for WordPress comment {}: {} != {}",
            warning.id,
            warning.first,
            warning.second
        );
    }

    Ok(())
}

/// Read one URL per line, trimming surrounding whitespace and skipping blank lines.
fn read_urls<R: BufRead>(
    reader: R,
    error: Rc<RefCell<Option<std::io::Error>>>,
) -> impl Iterator<Item = String> {
    reader
        .lines()
        .map_while(move |line| match line {
            Ok(line) => {
                let url = line.trim();
                Some((!url.is_empty()).then(|| url.to_owned()))
            }
            Err(source) => {
                *error.borrow_mut() = Some(source);
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

fn message_spinner(message: &'static str) -> ProgressBar {
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{msg} {spinner}").expect("valid progress spinner template"),
    );
    progress.set_message(message);
    progress
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("a partial archive was published at {}", .0.display())]
    PartialArchive(PathBuf),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CLI argument reading error: {0}")]
    Args(#[from] cli_helpers::Error),
    #[error("invalid WordPress base URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("archiving error: {0}")]
    Archive(#[from] archivindex_archiver::client::Error),
    #[error("WARC conversion error: {0}")]
    Convert(#[from] archivindex_packager::Error),
    #[error("WordPress comment reading error: {0}")]
    ReadComments(#[from] archivindex_archiver::wordpress::read::Error),
    #[error("JSON writing error: {0}")]
    Json(#[from] serde_json::Error),
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
    /// Archive comments iteratively through a site's `WordPress` REST API v2 endpoint.
    ArchiveWpComments(ArchiveWpCommentsOptions),
    /// Convert an existing WARC file into an indexed WACZ package.
    WarcToWacz(WarcToWaczOptions),
    /// Read comments captured from the `WordPress` REST API in a WARC file.
    ReadWpComments(ReadWpCommentsOptions),
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

/// Options for archiving comments from the `WordPress` REST API.
#[derive(Debug, clap::Args)]
struct ArchiveWpCommentsOptions {
    #[clap(flatten)]
    config: ConfigOptions,
    /// Base URL of the `WordPress` site.
    #[clap(long)]
    base_url: String,
    /// Path of the WARC file to write (an existing file is not overwritten).
    #[clap(long)]
    output: PathBuf,
    /// URL-safe name identifying the session and its WARC file.
    #[clap(long)]
    session_name: String,
    /// Name of the operator running the crawl, recorded in `warcinfo`.
    #[clap(long)]
    operator: String,
    /// Email address of the operator running the crawl, recorded in `warcinfo`.
    #[clap(long)]
    operator_email: Option<String>,
    /// Persistent payload-revisit and conditional-request state database.
    #[clap(long)]
    revisit_index: Option<PathBuf>,
    /// Stop successfully after capturing this many comment batches.
    #[clap(long)]
    limit: Option<usize>,
    /// Always perform a second complete sweep, even when the first sweep's totals are consistent.
    #[clap(long)]
    second_sweep: bool,
    /// Record titles in the session's `warcinfo` and per-capture metadata records.
    #[clap(long)]
    titles: bool,
    /// Total attempts for a transiently failing URL (defaults to 3; zero is treated as one).
    #[clap(long)]
    retry_attempts: Option<usize>,
    /// Seconds before the first retry (defaults to 1; subsequent delays double).
    #[clap(long)]
    retry_initial_backoff: Option<u64>,
    /// Maximum retry delay in seconds (defaults to 30).
    #[clap(long)]
    retry_max_backoff: Option<u64>,
    /// Seconds to wait between successive comment-batch requests (defaults to 0).
    #[clap(long, default_value_t = 0)]
    request_delay: u64,
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

/// Options for reading comments from a WARC file.
#[derive(Debug, clap::Args)]
struct ReadWpCommentsOptions {
    /// Path of the plain or gzip-compressed WARC file to read.
    warc: PathBuf,
}

/// Capture settings shared by both workflows.
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
    use std::path::PathBuf;
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
            Error::Io(std::io::Error::other("failed")).exit_code(),
            ExitCode::FAILURE
        );
    }

    #[test]
    fn read_urls_trims_and_skips_blank_lines() {
        let input = "https://example.com/\n\n  https://example.org/  \n";

        let error = std::rc::Rc::new(std::cell::RefCell::new(None));
        let urls =
            super::read_urls(input.as_bytes(), std::rc::Rc::clone(&error)).collect::<Vec<_>>();

        assert_eq!(urls, ["https://example.com/", "https://example.org/"]);
        assert!(error.borrow().is_none());
    }

    #[test]
    fn wordpress_command_reads_required_options_and_limit() {
        let options = Opts::try_parse_from([
            "archivindex-archiver",
            "archive-wp-comments",
            "--base-url",
            "https://example.com/",
            "--output",
            "comments.warc.gz",
            "--session-name",
            "comments-2026",
            "--operator",
            "A. Archivist",
            "--revisit-index",
            "crawl-state.sqlite3",
            "--limit",
            "12",
            "--second-sweep",
            "--request-delay",
            "7",
            "--gzip",
        ])
        .expect("valid options");

        let Command::ArchiveWpComments(options) = options.command else {
            panic!("expected the WordPress command");
        };

        assert_eq!(options.base_url, "https://example.com/");
        assert_eq!(options.output, PathBuf::from("comments.warc.gz"));
        assert_eq!(options.session_name, "comments-2026");
        assert_eq!(options.operator, "A. Archivist");
        assert_eq!(
            options.revisit_index,
            Some(PathBuf::from("crawl-state.sqlite3"))
        );
        assert_eq!(options.limit, Some(12));
        assert!(options.second_sweep);
        assert!(!options.titles);
        assert_eq!(options.request_delay, 7);
        assert!(options.config.gzip);
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

        let Command::Archive(options) = options.command else {
            panic!("expected the archive command");
        };

        assert!(!options.config.gzip);
    }

    #[test]
    fn wordpress_command_enables_titles_explicitly() {
        let options = Opts::try_parse_from([
            "archivindex-archiver",
            "archive-wp-comments",
            "--base-url",
            "https://example.com/",
            "--output",
            "comments.warc.gz",
            "--session-name",
            "comments-2026",
            "--operator",
            "A. Archivist",
            "--titles",
        ])
        .expect("valid options");

        let Command::ArchiveWpComments(options) = options.command else {
            panic!("expected the WordPress command");
        };

        assert!(options.titles);
    }

    #[test]
    fn read_wordpress_comments_command_takes_a_warc_path() {
        let options = Opts::try_parse_from([
            "archivindex-archiver",
            "read-wp-comments",
            "comments.warc.gz",
        ])
        .expect("valid options");

        let Command::ReadWpComments(options) = options.command else {
            panic!("expected the WordPress reading command");
        };

        assert_eq!(options.warc, PathBuf::from("comments.warc.gz"));
    }

    #[test]
    fn warc_conversion_command_reads_paths_and_index_format() {
        let options = Opts::try_parse_from([
            "archivindex-archiver",
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

        let Command::WarcToWacz(options) = options.command else {
            panic!("expected the WARC conversion command");
        };
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
            "archivindex-archiver",
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
            "archivindex-archiver",
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
