//! A command-line front end for capturing and reading `WordPress` REST API resources.
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use archivindex_archiver::capture::{CaptureControl, CaptureEvent};
use archivindex_archiver::session::{Capture, CaptureProcessor, Inspection, Session};
use archivindex_archiver::{Archiver, Config};
use archivindex_cli_support::{CommandOutcome, Verbosity, exit_code};
use archivindex_wordpress::complete::{CommentCompletionSummary, complete_comments};
use archivindex_wordpress::read::{
    CommentCompleteness, check_comment_completeness, find_comment_update_anchor, read_comments,
};
use archivindex_wordpress::{CommentCaptureProcessor, CommentProgress};
use chrono::Utc;
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
        Command::Archive(options) => archive_comments(&options, quiet),
        Command::Check(options) => check_wp_comments(&options, quiet),
        Command::Complete(options) => complete_wp_comments(&options, quiet),
        Command::Read(options) => read_wp_comments(options),
        Command::Update(options) => update_comments(&options, quiet),
    }
}

/// Archive the comments exposed by a site's `WordPress` REST API v2 endpoint.
///
/// Captures that fail or a session that ends early leave a partial archive behind, which is
/// reported through the exit status rather than treated as an error.
fn archive_comments(
    options: &ArchiveCommentsOptions,
    quiet: bool,
) -> Result<CommandOutcome, Error> {
    let mut processor = CommentCaptureProcessor::new(&options.base_url)?;
    if let Some(resume_after) = &options.resume_after {
        processor = processor.resume_after(resume_after)?;
    }
    capture_comment_run(
        processor,
        CommentRunOptions {
            base_url: &options.base_url,
            config: options.config.as_deref(),
            cookie: options.cookie.as_deref(),
            output: &options.output,
            session_name: &options.session_name,
            revisit_index: options.revisit_index.as_deref(),
            limit: options.limit,
            second_sweep: options.second_sweep,
        },
        quiet,
    )
}

/// Capture comments newer than an overlap before the last archived comment.
fn update_comments(options: &UpdateCommentsOptions, quiet: bool) -> Result<CommandOutcome, Error> {
    let anchor = find_comment_update_anchor(&options.input)?;
    let before = Utc::now();
    let overlap = chrono::Duration::from_std(options.overlap)
        .map_err(|_| Error::OverlapOutOfRange(options.overlap))?;
    let after = anchor
        .datetime
        .checked_sub_signed(overlap)
        .ok_or(Error::OverlapOutOfRange(options.overlap))?;
    if after >= before {
        return Err(Error::InvalidUpdateWindow { after, before });
    }
    log::info!(
        "updating {} comments after {} and before {} (anchor from {})",
        anchor.base_url,
        after.to_rfc3339(),
        before.to_rfc3339(),
        if anchor.from_comment {
            "latest comment"
        } else {
            "archived before cutoff"
        }
    );
    let processor = CommentCaptureProcessor::for_window(&anchor.base_url, after, before)?;

    capture_comment_run(
        processor,
        CommentRunOptions {
            base_url: &anchor.base_url,
            config: options.config.as_deref(),
            cookie: options.cookie.as_deref(),
            output: &options.output,
            session_name: &options.session_name,
            revisit_index: options.revisit_index.as_deref(),
            limit: options.limit,
            second_sweep: options.second_sweep,
        },
        quiet,
    )
}

#[derive(Clone, Copy)]
struct CommentRunOptions<'a> {
    base_url: &'a str,
    config: Option<&'a Path>,
    cookie: Option<&'a str>,
    output: &'a Path,
    session_name: &'a str,
    revisit_index: Option<&'a Path>,
    limit: Option<usize>,
    second_sweep: bool,
}

fn capture_comment_run(
    processor: CommentCaptureProcessor,
    options: CommentRunOptions<'_>,
    quiet: bool,
) -> Result<CommandOutcome, Error> {
    let processor = processor.second_sweep(options.second_sweep);
    let first_url = processor.first_comment_url();
    let comment_progress = Rc::new(RefCell::new(None));
    let processor = ProgressingCommentProcessor {
        processor,
        progress: Rc::clone(&comment_progress),
    };
    let config = load_config(options.config)?;
    let archiver = match options.cookie {
        Some(cookie) => Archiver::new(config)?.cookie_for(options.base_url, cookie)?,
        None => Archiver::new(config)?,
    };
    let progress = message_spinner("Downloading comments");
    let event_progress = progress.clone();
    let event_comment_progress = Rc::clone(&comment_progress);
    let mut session = Session::new(archiver, options.session_name, [first_url], options.output)?
        .processor(processor)
        .events(move |event: CaptureEvent<'_>| {
            if matches!(event, CaptureEvent::Written { .. })
                && let Some(snapshot) = *event_comment_progress.borrow()
            {
                event_progress.set_message(snapshot.to_string());
            }
            CaptureControl::Continue
        });

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

/// Read the configuration file at `path`, or take the default configuration without one.
fn load_config(path: Option<&Path>) -> Result<Config, Error> {
    path.map_or_else(
        || Ok(Config::default()),
        |path| {
            let format = ConfigFormat::of(path)?;
            let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
                path: path.to_owned(),
                source,
            })?;

            format.parse(path, &text)
        },
    )
}

/// A supported configuration document format, recognized by file extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigFormat {
    Toml,
    Json,
}

impl ConfigFormat {
    fn of(path: &Path) -> Result<Self, Error> {
        let extension = path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase);

        match extension.as_deref() {
            Some("toml") => Ok(Self::Toml),
            Some("json") => Ok(Self::Json),
            _ => Err(Error::ConfigExtension(path.to_owned())),
        }
    }

    fn parse(self, path: &Path, text: &str) -> Result<Config, Error> {
        match self {
            Self::Toml => toml::from_str(text).map_err(|source| Error::ConfigToml {
                path: path.to_owned(),
                source,
            }),
            Self::Json => serde_json::from_str(text).map_err(|source| Error::ConfigJson {
                path: path.to_owned(),
                source,
            }),
        }
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

/// Check that every page advertised in a comments WARC has a qualifying capture record.
fn check_wp_comments(options: &CheckCommentsOptions, quiet: bool) -> Result<CommandOutcome, Error> {
    let coverage = check_comment_completeness(&options.warc)?;
    let complete = coverage.is_complete();
    let total_changed = coverage.advertised_total_changed();

    if let Some(warning) = page_total_change_warning(&coverage) {
        log::warn!("{warning}");
    }

    if complete {
        if !quiet {
            println!(
                "{} is complete: all {} advertised comment pages were captured",
                options.warc.display(),
                coverage
                    .total_pages
                    .expect("complete coverage has an advertised page count")
            );
        }
    } else {
        match coverage.total_pages {
            None => log::warn!(
                "{} has no qualifying record with a valid X-WP-TotalPages header",
                options.warc.display()
            ),
            Some(total_pages) => {
                let missing_count = coverage
                    .missing_page_count()
                    .expect("an advertised page count has a missing-page count");
                let mut missing = coverage.missing_pages();
                let shown = missing.by_ref().take(20).collect::<Vec<_>>();
                let suffix = (missing_count > shown.len())
                    .then(|| format!(" (and {} more)", missing_count - shown.len()));
                log::warn!(
                    "{} is missing qualifying records for {} of {} advertised pages: {}{}",
                    options.warc.display(),
                    missing_count,
                    total_pages,
                    shown
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    suffix.as_deref().unwrap_or("")
                );
            }
        }

        if !quiet {
            println!("{} is incomplete", options.warc.display());
        }
    }

    Ok(CommandOutcome::from_reported_problems(
        !complete || total_changed,
    ))
}

/// Capture exactly the comment pages missing from an existing WARC.
fn complete_wp_comments(
    options: &CompleteCommentsOptions,
    quiet: bool,
) -> Result<CommandOutcome, Error> {
    let config = load_config(options.config.as_deref())?;
    let archiver = Archiver::new(config)?;
    let progress = message_spinner("Completing comments");
    let summary = complete_comments(&archiver, &options.input, &options.output)?;
    progress.finish_and_clear();

    report_completion_problems(&summary);
    if !quiet {
        if summary.missing_pages.is_empty() {
            println!(
                "{} was already complete; wrote its warcinfo record to {}",
                options.input.display(),
                options.output.display()
            );
        } else {
            println!(
                "Captured {} of {} missing comment pages to {}",
                summary.missing_pages.len() - summary.uncaptured_pages.len(),
                summary.missing_pages.len(),
                options.output.display()
            );
        }
    }

    Ok(CommandOutcome::from_reported_problems(
        !summary.is_complete(),
    ))
}

fn report_completion_problems(summary: &CommentCompletionSummary) {
    if let Some(archive) = &summary.archive {
        for failure in &archive.failures {
            log::warn!("Failed to capture {}: {}", failure.url, failure.error);
        }
        if archive.cancelled {
            log::warn!("comment completion was cancelled before every request was made");
        }
        let partial = archive.partial_captures();
        if partial > 0 {
            log::warn!("{partial} comment page captures were unexpectedly truncated");
        }
    }
    if !summary.uncaptured_pages.is_empty() {
        log::warn!(
            "no qualifying response was captured for comment pages {}",
            summary
                .uncaptured_pages
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// Describe the signed difference at every transition between advertised totals.
fn page_total_change_warning(coverage: &CommentCompleteness) -> Option<String> {
    if !coverage.advertised_total_changed() {
        return None;
    }

    let totals = coverage
        .advertised_page_totals
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(" -> ");
    let differences = coverage
        .advertised_page_totals
        .windows(2)
        .map(|pair| {
            if pair[1] >= pair[0] {
                format!("+{}", pair[1] - pair[0])
            } else {
                format!("-{}", pair[0] - pair[1])
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    Some(format!(
        "X-WP-TotalPages changed over the WARC session ({totals}); successive differences: \
         {differences}"
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
    #[error("invalid archiver configuration: {0}")]
    Config(#[from] archivindex_archiver::ConfigError),
    #[error("cannot read configuration file {}: {source}", path.display())]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration file {} must have a .toml or .json extension", .0.display())]
    ConfigExtension(PathBuf),
    #[error("cannot parse TOML configuration file {}: {source}", path.display())]
    ConfigToml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("cannot parse JSON configuration file {}: {source}", path.display())]
    ConfigJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    UserAgent(#[from] archivindex_archiver::UserAgentError),
    #[error(transparent)]
    SessionId(#[from] archivindex_archiver::session::SessionIdError),
    #[error("WordPress comment reading error: {0}")]
    ReadComments(#[from] archivindex_wordpress::read::Error),
    #[error("WordPress comment completion error: {0}")]
    CompleteComments(#[from] archivindex_wordpress::complete::Error),
    #[error("comment update overlap is out of range: {0:?}")]
    OverlapOutOfRange(Duration),
    #[error("comment update window starts at {after}, which is not before {before}")]
    InvalidUpdateWindow {
        after: chrono::DateTime<Utc>,
        before: chrono::DateTime<Utc>,
    },
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
    #[clap(name = "archive-comments")]
    Archive(ArchiveCommentsOptions),
    /// Check that every advertised comments page has a qualifying response or revisit record.
    #[clap(name = "check-comments")]
    Check(CheckCommentsOptions),
    /// Capture pages missing from a comments WARC into a new WARC.
    #[clap(name = "complete-comments")]
    Complete(CompleteCommentsOptions),
    /// Read comments captured from the `WordPress` REST API in a WARC file.
    #[clap(name = "read-comments")]
    Read(ReadCommentsOptions),
    /// Capture new comments in a window overlapping an existing comments WARC.
    #[clap(name = "update-comments")]
    Update(UpdateCommentsOptions),
}

/// Options for archiving comments from the `WordPress` REST API.
#[derive(Debug, clap::Args)]
struct ArchiveCommentsOptions {
    /// A TOML or JSON archiver configuration file, recognized by its extension; every key is
    /// optional and takes its default when absent.
    #[clap(short, long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    config: Option<PathBuf>,
    /// Base URL of the `WordPress` site.
    #[clap(long)]
    base_url: String,
    /// Path of the WARC file to write (an existing file is not overwritten).
    #[clap(long)]
    output: PathBuf,
    /// URL-safe name identifying the session and its WARC file.
    #[clap(long)]
    session_name: String,
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
}

/// Options for reading comments from a WARC file.
#[derive(Debug, clap::Args)]
struct ReadCommentsOptions {
    /// Path of the plain or gzip-compressed WARC file to read.
    warc: PathBuf,
}

/// Options for checking comments page coverage in a WARC file.
#[derive(Debug, clap::Args)]
struct CheckCommentsOptions {
    /// Path of the plain or gzip-compressed WARC file to check.
    warc: PathBuf,
}

/// Options for capturing pages missing from a comments WARC.
#[derive(Debug, clap::Args)]
struct CompleteCommentsOptions {
    /// A TOML or JSON archiver configuration file, recognized by its extension; every key is
    /// optional and takes its default when absent.
    #[clap(short, long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    config: Option<PathBuf>,
    /// Path of the plain or gzip-compressed WARC file to inspect.
    input: PathBuf,
    /// Path of the completion WARC to write (an existing file is not overwritten).
    output: PathBuf,
}

/// Options for incrementally updating an archived comments collection.
#[derive(Debug, clap::Args)]
struct UpdateCommentsOptions {
    /// A TOML or JSON archiver configuration file, recognized by its extension; every key is
    /// optional and takes its default when absent.
    #[clap(short, long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    config: Option<PathBuf>,
    /// Existing plain or gzip-compressed comments WARC used to choose the update window.
    input: PathBuf,
    /// Path of the WARC file to write (an existing file is not overwritten).
    #[clap(long)]
    output: PathBuf,
    /// URL-safe name identifying the update session and its WARC file.
    #[clap(long)]
    session_name: String,
    /// Begin this far before the latest archived comment datetime.
    #[clap(long, default_value = "1day", value_parser = parse_duration)]
    overlap: Duration,
    /// Persistent payload-revisit and conditional-request state database.
    #[clap(long)]
    revisit_index: Option<PathBuf>,
    /// Stop successfully after capturing this many comment batches.
    #[clap(long)]
    limit: Option<usize>,
    /// Always perform a second complete sweep, even when the first sweep's totals are consistent.
    #[clap(long)]
    second_sweep: bool,
    /// Cookie header obtained from a browser, scoped to the archived site's host.
    #[clap(long)]
    cookie: Option<String>,
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use archivindex_archiver::Config;
    use archivindex_wordpress::read::CommentCompleteness;
    use clap::{CommandFactory, Parser};

    use super::{Command, ConfigFormat, Opts, load_config, page_total_change_warning};

    #[test]
    fn archive_command_reads_workflow_and_config_options() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "archive-comments",
            "--config",
            "capture.toml",
            "--base-url",
            "https://example.com/",
            "--output",
            "comments.warc.gz",
            "--session-name",
            "comments-2026",
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
        ])
        .expect("valid options");

        let Command::Archive(options) = options.command else {
            panic!("expected the archiving command");
        };

        assert_eq!(options.base_url, "https://example.com/");
        assert_eq!(options.output, PathBuf::from("comments.warc.gz"));
        assert_eq!(options.session_name, "comments-2026");
        assert_eq!(options.config, Some(PathBuf::from("capture.toml")));
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
    }

    #[test]
    fn archive_command_does_not_duplicate_configuration_fields() {
        let command = Opts::command();
        let archive = command
            .find_subcommand("archive-comments")
            .expect("the archive-comments command");
        let argument_ids = archive
            .get_arguments()
            .map(|argument| argument.get_id().as_str())
            .collect::<Vec<_>>();

        for removed in [
            "gzip",
            "user_agent",
            "timeout",
            "max_redirects",
            "max_response_length",
            "operator",
            "operator_email",
            "titles",
            "retry_attempts",
            "retry_initial_backoff",
            "retry_max_backoff",
            "request_delay",
        ] {
            assert!(!argument_ids.contains(&removed), "unexpected --{removed}");
        }
    }

    #[test]
    fn no_configuration_file_uses_archiver_defaults() {
        assert_eq!(
            load_config(None).expect("the default configuration"),
            Config::default()
        );
    }

    #[test]
    fn configuration_file_supplies_archiver_and_session_settings() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("capture.toml");
        std::fs::write(
            &path,
            "gzip-warc = true\n\
             [operator]\nname = \"A. Archivist\"\nemail = \"archivist@example.com\"\n\
             [session]\nrequest-delay = \"750ms\"\ntitles = true\n",
        )
        .expect("write the configuration");

        let config = load_config(Some(&path)).expect("read the configuration");

        assert!(config.gzip_warc);
        let operator = config.operator.expect("a configured operator");
        assert_eq!(operator.name, "A. Archivist");
        assert_eq!(operator.email.as_deref(), Some("archivist@example.com"));
        assert_eq!(config.session.request_delay, Duration::from_millis(750));
        assert!(config.session.titles);
    }

    #[test]
    fn configuration_format_is_recognized_by_extension() {
        assert_eq!(
            ConfigFormat::of(Path::new("capture.toml")).ok(),
            Some(ConfigFormat::Toml)
        );
        assert_eq!(
            ConfigFormat::of(Path::new("capture.JSON")).ok(),
            Some(ConfigFormat::Json)
        );
        assert!(ConfigFormat::of(Path::new("capture.yaml")).is_err());
    }

    #[test]
    fn read_command_takes_a_warc_path() {
        let options =
            Opts::try_parse_from(["archivindex-wordpress", "read-comments", "comments.warc.gz"])
                .expect("valid options");

        let Command::Read(options) = options.command else {
            panic!("expected the reading command");
        };

        assert_eq!(options.warc, PathBuf::from("comments.warc.gz"));
    }

    #[test]
    fn check_command_takes_a_warc_path() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "check-comments",
            "comments.warc.gz",
        ])
        .expect("valid options");

        let Command::Check(options) = options.command else {
            panic!("expected the checking command");
        };

        assert_eq!(options.warc, PathBuf::from("comments.warc.gz"));
    }

    #[test]
    fn complete_command_takes_input_and_output_warc_paths() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "complete-comments",
            "comments.warc.gz",
            "completion.warc.gz",
            "--config",
            "capture.toml",
        ])
        .expect("valid options");

        let Command::Complete(options) = options.command else {
            panic!("expected the completion command");
        };

        assert_eq!(options.input, PathBuf::from("comments.warc.gz"));
        assert_eq!(options.output, PathBuf::from("completion.warc.gz"));
        assert_eq!(options.config, Some(PathBuf::from("capture.toml")));
    }

    #[test]
    fn update_command_uses_a_one_day_default_overlap() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "update-comments",
            "historical.warc.gz",
            "--output",
            "update.warc.gz",
            "--session-name",
            "comments-update-2026-08-20",
        ])
        .expect("valid options");

        let Command::Update(options) = options.command else {
            panic!("expected the update command");
        };

        assert_eq!(options.input, PathBuf::from("historical.warc.gz"));
        assert_eq!(options.output, PathBuf::from("update.warc.gz"));
        assert_eq!(options.session_name, "comments-update-2026-08-20");
        assert_eq!(options.overlap, Duration::from_hours(24));
    }

    #[test]
    fn update_command_parses_a_configured_overlap() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "update-comments",
            "historical.warc",
            "--output",
            "update.warc",
            "--session-name",
            "comments-update",
            "--overlap",
            "36hours",
        ])
        .expect("valid options");

        let Command::Update(options) = options.command else {
            panic!("expected the update command");
        };
        assert_eq!(options.overlap, Duration::from_hours(36));
    }

    #[test]
    fn changed_page_totals_report_successive_differences() {
        let coverage = CommentCompleteness {
            total_pages: Some(4),
            advertised_page_totals: vec![2, 4, 3],
            captured_pages: vec![1, 2, 3],
        };

        assert_eq!(
            page_total_change_warning(&coverage).as_deref(),
            Some(
                "X-WP-TotalPages changed over the WARC session (2 -> 4 -> 3); successive \
                 differences: +2, -1"
            )
        );
    }
}
