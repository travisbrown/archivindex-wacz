//! A command-line front end for capturing and reading `WordPress` REST API resources.
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use archivindex_archiver::capture::{CaptureControl, CaptureEvent};
use archivindex_archiver::session::{
    Capture, CaptureProcessor, Inspection, Operator, RetryConfig, Session,
};
use archivindex_archiver::{Archiver, Config};
use archivindex_cli_support::{CommandOutcome, Verbosity, exit_code};
use archivindex_wordpress::read::read_comments;
use archivindex_wordpress::{CommentCaptureProcessor, CommentProgress};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};

fn main() -> ExitCode {
    let opts = Opts::parse();
    opts.verbosity.init_logging();

    exit_code(run(opts))
}

fn run(opts: Opts) -> Result<CommandOutcome, Error> {
    let quiet = opts.verbosity.is_quiet();

    match opts.command {
        Command::ArchiveComments(options) => archive_comments(options, quiet),
        Command::ReadComments(options) => read_wp_comments(options),
    }
}

/// Archive the comments exposed by a site's `WordPress` REST API v2 endpoint.
///
/// Captures that fail or a session that ends early leave a partial archive behind, which is
/// reported through the exit status rather than treated as an error.
fn archive_comments(options: ArchiveCommentsOptions, quiet: bool) -> Result<CommandOutcome, Error> {
    let titles = options.titles;
    let mut processor = CommentCaptureProcessor::new(&options.base_url)?;
    if let Some(resume_after) = &options.resume_after {
        processor = processor.resume_after(resume_after)?;
    }
    let processor = processor.second_sweep(options.second_sweep);
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
    let config = options.config.into_config();
    let archiver = match &options.cookie {
        Some(cookie) => Archiver::new(config)?.cookie_for(&options.base_url, cookie)?,
        None => Archiver::new(config)?,
    };
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
        if !quiet {
            println!("{snapshot} to {}", options.output.display());
        }
    } else if !quiet {
        println!("Downloaded no comments to {}", options.output.display());
    }

    if summary.is_complete() {
        Ok(CommandOutcome::Success)
    } else {
        log::warn!(
            "a partial archive was published at {}",
            options.output.display()
        );

        Ok(CommandOutcome::ReportedProblems)
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

/// Read, sort, and deduplicate `WordPress` comments captured in a WARC file.
///
/// Comments captured with conflicting contents are logged as warnings, and the exit status
/// reflects that some were found.
fn read_wp_comments(options: ReadCommentsOptions) -> Result<CommandOutcome, Error> {
    let result = read_comments(options.warc)?;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    for comment in result.comments {
        serde_json::to_writer(&mut output, &comment)?;
        writeln!(output)?;
    }

    for warning in &result.warnings {
        log::warn!(
            "Conflicting objects for WordPress comment {}: {} != {}",
            warning.id,
            warning.first,
            warning.second
        );
    }

    Ok(CommandOutcome::from_reported_problems(
        !result.warnings.is_empty(),
    ))
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
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid WordPress base URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("invalid WordPress resume URI: {0}")]
    Resume(#[from] archivindex_wordpress::CommentResumeError),
    #[error("invalid cookie: {0}")]
    Cookie(#[from] archivindex_archiver::CookieError),
    #[error("archiving error: {0}")]
    Archive(#[from] archivindex_archiver::Error),
    #[error(transparent)]
    UserAgent(#[from] archivindex_archiver::UserAgentError),
    #[error(transparent)]
    SessionId(#[from] archivindex_archiver::session::SessionIdError),
    #[error("WordPress comment reading error: {0}")]
    ReadComments(#[from] archivindex_wordpress::read::Error),
    #[error("JSON writing error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Parser)]
#[clap(name = "archivindex-wordpress", version, author)]
struct Opts {
    #[clap(flatten)]
    verbosity: Verbosity,
    #[clap(subcommand)]
    command: Command,
}

/// The comment-capture workflow to run.
#[derive(Debug, clap::Subcommand)]
// One value of this enum exists per process, so the size difference between its variants costs
// nothing, and boxing a variant would only obscure the derived argument parsing.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Archive comments iteratively through a site's `WordPress` REST API v2 endpoint.
    ArchiveComments(ArchiveCommentsOptions),
    /// Read comments captured from the `WordPress` REST API in a WARC file.
    ReadComments(ReadCommentsOptions),
}

/// Options for archiving comments from the `WordPress` REST API.
#[derive(Debug, clap::Args)]
struct ArchiveCommentsOptions {
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
    /// Last successfully archived comments URI; resume with its snapshot cutoff at the next page.
    #[clap(long)]
    resume_after: Option<String>,
    /// Cookie header obtained from a browser, scoped to the base URL's host.
    ///
    /// The value is sent with every request to that host and recorded in the WARC request records.
    /// Quote values containing semicolons.
    #[clap(long)]
    cookie: Option<String>,
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

/// Options for reading comments from a WARC file.
#[derive(Debug, clap::Args)]
struct ReadCommentsOptions {
    /// Path of the plain or gzip-compressed WARC file to read.
    warc: PathBuf,
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
    /// Build an archiver configuration.
    ///
    /// A comment session walks one paginated sequence, so it never fetches concurrently.
    fn into_config(self) -> Config {
        let defaults = Config::default();

        Config {
            user_agent: self.user_agent.unwrap_or(defaults.user_agent),
            timeout: self.timeout.map_or(defaults.timeout, Duration::from_secs),
            max_redirects: self.max_redirects.unwrap_or(defaults.max_redirects),
            concurrency: defaults.concurrency,
            max_response_length: self.max_response_length,
            gzip_warc: self.gzip,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::{Command, Opts};

    #[test]
    fn archive_command_reads_required_options_and_limit() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "archive-comments",
            "--base-url",
            "https://example.com/",
            "--output",
            "comments.warc.gz",
            "--session-name",
            "comments-2026",
            "--operator",
            "A. Archivist",
            "--revisit-index",
            "comments-state.sqlite3",
            "--limit",
            "12",
            "--second-sweep",
            "--resume-after",
            "https://example.com/wp-json/wp/v2/comments?\
             before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=8&per_page=100",
            "--cookie",
            "cf_clearance=test-clearance; __cf_bm=test-bot-cookie",
            "--request-delay",
            "7",
            "--gzip",
        ])
        .expect("valid options");

        let Command::ArchiveComments(options) = options.command else {
            panic!("expected the archiving command");
        };

        assert_eq!(options.base_url, "https://example.com/");
        assert_eq!(options.output, PathBuf::from("comments.warc.gz"));
        assert_eq!(options.session_name, "comments-2026");
        assert_eq!(options.operator, "A. Archivist");
        assert_eq!(
            options.revisit_index,
            Some(PathBuf::from("comments-state.sqlite3"))
        );
        assert_eq!(options.limit, Some(12));
        assert!(options.second_sweep);
        assert_eq!(
            options.resume_after.as_deref(),
            Some(
                "https://example.com/wp-json/wp/v2/comments?\
                 before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=8&per_page=100"
            )
        );
        assert_eq!(
            options.cookie.as_deref(),
            Some("cf_clearance=test-clearance; __cf_bm=test-bot-cookie")
        );
        assert!(!options.titles);
        assert_eq!(options.request_delay, 7);
        assert!(options.config.gzip);
    }

    #[test]
    fn archive_command_enables_titles_explicitly() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "archive-comments",
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

        let Command::ArchiveComments(options) = options.command else {
            panic!("expected the archiving command");
        };

        assert!(options.titles);
    }

    #[test]
    fn read_command_takes_a_warc_path() {
        let options =
            Opts::try_parse_from(["archivindex-wordpress", "read-comments", "comments.warc.gz"])
                .expect("valid options");

        let Command::ReadComments(options) = options.command else {
            panic!("expected the reading command");
        };

        assert_eq!(options.warc, PathBuf::from("comments.warc.gz"));
    }
}
