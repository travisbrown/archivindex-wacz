//! A command-line front end for capturing and reading `WordPress` REST API resources.
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use archivindex_archiver::capture::{CaptureControl, CaptureEvent};
use archivindex_archiver::session::{Capture, CaptureProcessor, Inspection, Session};
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
    let config = load_config(options.config.as_deref())?;
    let archiver = match &options.cookie {
        Some(cookie) => Archiver::new(config)?.cookie_for(&options.base_url, cookie)?,
        None => Archiver::new(config)?,
    };
    let progress = message_spinner("Downloading comments");
    let event_progress = progress.clone();
    let event_comment_progress = Rc::clone(&comment_progress);
    let mut session = Session::new(
        archiver,
        &options.session_name,
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use archivindex_archiver::Config;
    use clap::{CommandFactory, Parser};

    use super::{Command, ConfigFormat, Opts, load_config};

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

        let Command::ArchiveComments(options) = options.command else {
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
            "gzip_warc = true\n\
             [operator]\nname = \"A. Archivist\"\nemail = \"archivist@example.com\"\n\
             [session]\nrequest_delay = \"750ms\"\ntitles = true\n",
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

        let Command::ReadComments(options) = options.command else {
            panic!("expected the reading command");
        };

        assert_eq!(options.warc, PathBuf::from("comments.warc.gz"));
    }
}
