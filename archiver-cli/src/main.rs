//! Command-line tools for writing WACZ archives and crawl sessions.
#![deny(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, rust_2018_idioms)]
#![allow(clippy::missing_errors_doc)]
#![forbid(unsafe_code)]

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

use archivindex_archiver::client::Archiver;
use archivindex_archiver::config::{Config, IndexFormat};
use archivindex_archiver::conversion::WarcToWacz;
use archivindex_archiver::session::{Operator, RetryConfig, Session};
use archivindex_archiver::wordpress::CommentCaptureProcessor;
use archivindex_archiver::wordpress::read::read_comments;
use cli_helpers::prelude::*;
use indicatif::{ProgressBar, ProgressStyle};

fn main() -> Result<(), Error> {
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
    let urls = read_urls(std::io::stdin().lock())?;
    let config = options.config.into_config(options.concurrency);
    let archiver = Archiver::new(config)?;

    // The archiver pulls URLs from the iterator as it dispatches them for download, so the bar
    // tracks dispatches, running at most the configured concurrency ahead of completions. It is
    // cleared before the result is checked so that an error cannot leak a stuck bar.
    let progress = progress_bar(urls.len() as u64, "Archiving", "URLs");
    let result =
        archiver.archive_to_path(urls.iter().inspect(|_| progress.inc(1)), &options.output);
    progress.finish_and_clear();
    let summary = result?;

    for failure in &summary.failures {
        log::warn!("Failed to capture {}: {}", failure.url, failure.error);
    }

    println!(
        "Archived {} of {} URLs to {}",
        summary.captures.len(),
        urls.len(),
        options.output.display()
    );

    Ok(())
}

/// Archive the comments exposed by a site's `WordPress` REST API v2 endpoint.
fn archive_wp_comments(options: ArchiveWpCommentsOptions) -> Result<(), Error> {
    let processor = CommentCaptureProcessor::new(&options.base_url)?;
    let first_url = processor.first_comment_url();
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
    .software(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
    .processor(processor)
    .retry(retry);

    if let Some(revisit_index) = options.revisit_index {
        session = session.revisit_index(revisit_index);
    }
    if let Some(limit) = options.limit {
        session = session.limit(limit);
    }

    let summary = session.run()?;

    for failure in &summary.failures {
        log::warn!("Failed to capture {}: {}", failure.url, failure.error);
    }
    if let Some(error) = &summary.fatal_error {
        log::warn!("The session ended early: {error}");
    }

    let captures = summary.seed_captures.len() + summary.extra_captures.len();

    println!(
        "Archived {captures} WordPress comment batches to {}",
        options.output.display()
    );

    Ok(())
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
        .run()?;

    println!(
        "Converted {} records and {} captures from {} to {}",
        summary.records,
        summary.captures,
        options.warc.display(),
        options.output.display()
    );
    Ok(())
}

/// Read, sort, and deduplicate `WordPress` comments captured in a WACZ file.
fn read_wp_comments(options: ReadWpCommentsOptions) -> Result<(), Error> {
    let result = read_comments(options.wacz)?;
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
fn read_urls<R: BufRead>(reader: R) -> Result<Vec<String>, std::io::Error> {
    let mut urls = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let url = line.trim();

        if !url.is_empty() {
            urls.push(url.to_owned());
        }
    }

    Ok(urls)
}

/// Create a progress bar of `len` steps, labelled with `message` and counting units named `unit`.
fn progress_bar(len: u64, message: &'static str, unit: &str) -> ProgressBar {
    let progress = ProgressBar::new(len);
    progress.set_style(
        ProgressStyle::with_template(&format!(
            "{{msg}} [{{bar:40}}] {{human_pos}}/{{human_len}} {unit} ({{eta}})"
        ))
        .expect("valid progress bar template"),
    );
    progress.set_message(message);
    progress
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CLI argument reading error: {0}")]
    Args(#[from] cli_helpers::Error),
    #[error("invalid WordPress base URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("archiving error: {0}")]
    Archive(#[from] archivindex_archiver::client::Error),
    #[error("WARC conversion error: {0}")]
    Convert(#[from] archivindex_archiver::conversion::Error),
    #[error("WordPress comment reading error: {0}")]
    ReadComments(#[from] archivindex_archiver::wordpress::read::Error),
    #[error("JSON writing error: {0}")]
    Json(#[from] serde_json::Error),
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
    /// Read comments captured from the `WordPress` REST API in a WACZ file.
    ReadWpComments(ReadWpCommentsOptions),
}

/// Options for archiving URLs read from standard input.
#[derive(Debug, clap::Args)]
struct ArchiveOptions {
    #[clap(flatten)]
    config: ConfigOptions,
    /// Path of the WACZ file to write (an existing file is not overwritten).
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
    /// Path of the WACZ file to write (an existing file is not overwritten).
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
    /// Total attempts for a transiently failing URL (defaults to 3; zero is treated as one).
    #[clap(long)]
    retry_attempts: Option<usize>,
    /// Seconds before the first retry (defaults to 1; subsequent delays double).
    #[clap(long)]
    retry_initial_backoff: Option<u64>,
    /// Maximum retry delay in seconds (defaults to 30).
    #[clap(long)]
    retry_max_backoff: Option<u64>,
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
}

/// Options for reading comments from a WACZ file.
#[derive(Debug, clap::Args)]
struct ReadWpCommentsOptions {
    /// Path of the WACZ file to read.
    wacz: PathBuf,
}

/// Capture settings shared by both workflows.
#[derive(Debug, clap::Args)]
struct ConfigOptions {
    /// Store the WARC file uncompressed instead of gzip-compressed.
    #[clap(long)]
    no_gzip: bool,
    /// Write the index as a compressed `ZipNum` pair (`index.cdx.gz` and `index.idx`) instead of a
    /// plain-text index.cdx.
    #[clap(long)]
    compressed_index: bool,
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
            gzip_warc: !self.no_gzip,
            index_format: if self.compressed_index {
                IndexFormat::zipnum()
            } else {
                IndexFormat::Plain
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cli_helpers::prelude::Parser;

    use super::{Command, Opts};

    #[test]
    fn read_urls_trims_and_skips_blank_lines() {
        let input = "https://example.com/\n\n  https://example.org/  \n";

        let urls = super::read_urls(input.as_bytes()).expect("read URLs");

        assert_eq!(urls, ["https://example.com/", "https://example.org/"]);
    }

    #[test]
    fn wordpress_command_reads_required_options_and_limit() {
        let options = Opts::try_parse_from([
            "archivindex-archiver",
            "archive-wp-comments",
            "--base-url",
            "https://example.com/",
            "--output",
            "comments.wacz",
            "--session-name",
            "comments-2026",
            "--operator",
            "A. Archivist",
            "--revisit-index",
            "crawl-state.sqlite3",
            "--limit",
            "12",
        ])
        .expect("valid options");

        let Command::ArchiveWpComments(options) = options.command else {
            panic!("expected the WordPress command");
        };

        assert_eq!(options.base_url, "https://example.com/");
        assert_eq!(options.output, PathBuf::from("comments.wacz"));
        assert_eq!(options.session_name, "comments-2026");
        assert_eq!(options.operator, "A. Archivist");
        assert_eq!(
            options.revisit_index,
            Some(PathBuf::from("crawl-state.sqlite3"))
        );
        assert_eq!(options.limit, Some(12));
    }

    #[test]
    fn read_wordpress_comments_command_takes_a_wacz_path() {
        let options =
            Opts::try_parse_from(["archivindex-archiver", "read-wp-comments", "comments.wacz"])
                .expect("valid options");

        let Command::ReadWpComments(options) = options.command else {
            panic!("expected the WordPress reading command");
        };

        assert_eq!(options.wacz, PathBuf::from("comments.wacz"));
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
        ])
        .expect("valid options");

        let Command::WarcToWacz(options) = options.command else {
            panic!("expected the WARC conversion command");
        };
        assert_eq!(options.warc, PathBuf::from("capture.warc.gz"));
        assert_eq!(options.output, PathBuf::from("capture.wacz"));
        assert!(options.compressed_index);
    }
}
