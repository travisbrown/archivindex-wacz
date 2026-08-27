//! A command-line front end for capturing and reading `WordPress` REST API resources.
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::cell::RefCell;
use std::collections::HashSet;
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
    CommentCompleteness, CommentUpdateAnchor, check_comment_completeness,
    find_comment_update_anchor, read_comments,
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
        vec![CommentRun {
            base_url: options.base_url.clone(),
            processor,
        }],
        CommentRunOptions {
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
    let updates = comment_update_inputs(&options.input)?;
    let before = Utc::now();
    let overlap = chrono::Duration::from_std(options.overlap)
        .map_err(|_| Error::OverlapOutOfRange(options.overlap))?;
    let runs = updates
        .into_iter()
        .map(|update| {
            let after = update
                .anchor
                .datetime
                .checked_sub_signed(overlap)
                .ok_or(Error::OverlapOutOfRange(options.overlap))?;
            if after >= before {
                return Err(Error::InvalidUpdateWindow { after, before });
            }
            log::info!(
                "updating {} comments from {} after {} and before {} (anchor from {})",
                update.anchor.base_url,
                update.path.display(),
                after.to_rfc3339(),
                before.to_rfc3339(),
                if update.anchor.from_comment {
                    "latest comment"
                } else {
                    "archived before cutoff"
                }
            );
            let processor =
                CommentCaptureProcessor::for_window(&update.anchor.base_url, after, before)?;

            Ok(CommentRun {
                base_url: update.anchor.base_url,
                processor,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    capture_comment_run(
        runs,
        CommentRunOptions {
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

struct CommentUpdateInput {
    path: PathBuf,
    anchor: CommentUpdateAnchor,
}

/// Read one update WARC, or every directly contained WARC when `input` is a directory.
fn comment_update_inputs(input: &Path) -> Result<Vec<CommentUpdateInput>, Error> {
    let metadata = std::fs::metadata(input).map_err(|source| Error::UpdateInputRead {
        path: input.to_owned(),
        source,
    })?;
    let mut paths = if metadata.is_dir() {
        let mut paths = Vec::new();
        let entries = std::fs::read_dir(input).map_err(|source| Error::UpdateInputRead {
            path: input.to_owned(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| Error::UpdateInputRead {
                path: input.to_owned(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| Error::UpdateInputRead {
                path: entry.path(),
                source,
            })?;
            if file_type.is_file() && is_warc_file_name(&entry.file_name()) {
                paths.push(entry.path());
            }
        }
        if paths.is_empty() {
            return Err(Error::NoUpdateWarcs(input.to_owned()));
        }
        paths
    } else {
        vec![input.to_owned()]
    };
    // Make the input order deterministic before the semantic domain sort below resolves it.
    paths.sort();

    let mut updates = paths
        .into_iter()
        .map(|path| {
            let anchor =
                find_comment_update_anchor(&path).map_err(|source| Error::UpdateAnchor {
                    path: path.clone(),
                    source: Box::new(source),
                })?;
            Ok(CommentUpdateInput { path, anchor })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    updates.sort_by(|left, right| {
        update_domain(&left.anchor)
            .cmp(&update_domain(&right.anchor))
            .then_with(|| left.anchor.base_url.cmp(&right.anchor.base_url))
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(updates)
}

fn is_warc_file_name(name: &OsStr) -> bool {
    let path = Path::new(name);
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("warc"))
        || (path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gz"))
            && path
                .file_stem()
                .and_then(|stem| Path::new(stem).extension())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("warc")))
}

fn update_domain(anchor: &CommentUpdateAnchor) -> String {
    url::Url::parse(&anchor.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_else(|| anchor.base_url.to_ascii_lowercase())
}

struct CommentRun {
    base_url: String,
    processor: CommentCaptureProcessor,
}

#[derive(Clone, Copy)]
struct CommentRunOptions<'a> {
    config: Option<&'a Path>,
    cookie: Option<&'a str>,
    output: &'a Path,
    session_name: &'a str,
    revisit_index: Option<&'a Path>,
    limit: Option<usize>,
    second_sweep: bool,
}

fn capture_comment_run(
    runs: Vec<CommentRun>,
    options: CommentRunOptions<'_>,
    quiet: bool,
) -> Result<CommandOutcome, Error> {
    let mut first_urls = Vec::with_capacity(runs.len());
    let mut seen_first_urls = HashSet::with_capacity(runs.len());
    let mut scheduled = Vec::with_capacity(runs.len());
    let base_urls = runs
        .into_iter()
        .map(|run| {
            let processor = run.processor.second_sweep(options.second_sweep);
            let first_url = processor.first_comment_url();
            if !seen_first_urls.insert(first_url.clone()) {
                return Err(Error::DuplicateUpdateUrl(first_url));
            }
            first_urls.push(first_url.clone());
            scheduled.push(ScheduledCommentProcessor {
                processor,
                next_url: Some(first_url),
            });

            Ok(run.base_url)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let comment_progress = Rc::new(RefCell::new(CommentRunProgress {
        base_urls: base_urls.clone(),
        snapshots: vec![None; base_urls.len()],
        latest: None,
    }));
    let processor = ProgressingCommentProcessor {
        scheduled,
        progress: Rc::clone(&comment_progress),
    };
    let config = load_config(options.config)?;
    let mut archiver = Archiver::new(config)?;
    if let Some(cookie) = options.cookie {
        for base_url in &base_urls {
            archiver = archiver.cookie_for(base_url, cookie)?;
        }
    }
    let progress = message_spinner("Downloading comments");
    let event_progress = progress.clone();
    let event_comment_progress = Rc::clone(&comment_progress);
    // Every domain's page one is a seed, so it has no `via`. Pages each processor schedules are
    // recaptures and therefore point back to that domain's preceding page.
    let mut session = Session::new(archiver, options.session_name, first_urls, options.output)?
        .processor(processor)
        .events(move |event: CaptureEvent<'_>| {
            if matches!(event, CaptureEvent::Written { .. })
                && let Some(message) = event_comment_progress.borrow().latest_message()
            {
                event_progress.set_message(message);
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

    for (base_url, snapshot) in comment_progress.borrow().iter() {
        if let Some(snapshot) = snapshot {
            if let Some(shortfall) = snapshot.visibility_shortfall() {
                log::warn!(
                    "WordPress counted {} comments for {} before visibility filtering but returned {} visible comments ({shortfall} omitted)",
                    snapshot.total,
                    base_url,
                    snapshot.downloaded
                );
            }
            if !quiet {
                println!("{base_url}: {snapshot} to {}", options.output.display());
            }
        } else if !quiet {
            println!(
                "Downloaded no comments from {base_url} to {}",
                options.output.display()
            );
        }
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
    scheduled: Vec<ScheduledCommentProcessor>,
    progress: Rc<RefCell<CommentRunProgress>>,
}

struct ScheduledCommentProcessor {
    processor: CommentCaptureProcessor,
    next_url: Option<String>,
}

struct CommentRunProgress {
    base_urls: Vec<String>,
    snapshots: Vec<Option<CommentProgress>>,
    latest: Option<usize>,
}

impl CommentRunProgress {
    fn latest_message(&self) -> Option<String> {
        let index = self.latest?;
        Some(format!(
            "{}: {}",
            self.base_urls[index], self.snapshots[index]?
        ))
    }

    fn iter(&self) -> impl Iterator<Item = (&str, Option<CommentProgress>)> + '_ {
        self.base_urls
            .iter()
            .map(String::as_str)
            .zip(self.snapshots.iter().copied())
    }
}

impl CaptureProcessor for ProgressingCommentProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        let Some(index) = self
            .scheduled
            .iter()
            .position(|scheduled| scheduled.next_url.as_deref() == Some(capture.url))
        else {
            return Inspection::error(format!(
                "captured an unscheduled WordPress comments URL: {}",
                capture.url
            ));
        };
        let scheduled = &mut self.scheduled[index];
        let inspection = scheduled.processor.inspect(capture);
        scheduled.next_url = inspection.recaptures.first().cloned();
        let mut progress = self.progress.borrow_mut();
        progress.snapshots[index] = scheduled.processor.progress();
        progress.latest = Some(index);
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
    #[error("cannot read comment update input {}: {source}", path.display())]
    UpdateInputRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("comment update directory {} contains no direct .warc or .warc.gz files", .0.display())]
    NoUpdateWarcs(PathBuf),
    #[error("cannot derive a comment update from {}: {source}", path.display())]
    UpdateAnchor {
        path: PathBuf,
        #[source]
        source: Box<archivindex_wordpress::read::Error>,
    },
    #[error("more than one comment update produced the same first-page URL: {0}")]
    DuplicateUpdateUrl(String),
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
    /// Existing comments WARC, or a directory whose direct .warc and .warc.gz files are updated.
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
    /// Cookie header obtained from a browser, scoped to every archived site's host.
    #[clap(long)]
    cookie: Option<String>,
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;

    use archivindex_archiver::Config;
    use archivindex_warc::io::read::WarcReader;
    use archivindex_warc::io::write::WarcWriter;
    use archivindex_warc::record::extension::NoExtension;
    use archivindex_warc::record::{FieldsBlock, Record};
    use archivindex_wordpress::CommentCaptureProcessor;
    use archivindex_wordpress::read::CommentCompleteness;
    use chrono::Utc;
    use clap::{CommandFactory, Parser};
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::{
        Command, CommentRun, CommentRunOptions, ConfigFormat, Error, Opts, capture_comment_run,
        comment_update_inputs, load_config, page_total_change_warning,
    };

    fn write_update_warc(
        path: &Path,
        base_url: &str,
        comment_datetime: &str,
        gzip: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let url = format!("{base_url}wp-json/wp/v2/comments?before=2026-08-20T00:00:00Z&page=1");
        let body = format!(r#"[{{"id":1,"date_gmt":"{comment_datetime}"}}]"#);
        let message = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        );
        let record: Record = Record::response(&url, Utc::now())?.body(message.into_bytes())?;
        let mut bytes = Vec::new();
        let mut writer = WarcWriter::new(&mut bytes);
        writer.write(&record.into_raw()?)?;
        writer.flush()?;
        if gzip {
            let mut encoder = GzEncoder::new(std::fs::File::create(path)?, Compression::default());
            encoder.write_all(&bytes)?;
            encoder.finish()?;
        } else {
            std::fs::write(path, bytes)?;
        }

        Ok(())
    }

    fn serve_comment_pages() -> std::io::Result<(u16, thread::JoinHandle<Vec<String>>)> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("a local test connection");
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).expect("a request read");
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                let head = String::from_utf8_lossy(&request);
                let target = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .expect("a request target")
                    .to_owned();
                let page = url::Url::parse(&format!("http://localhost{target}"))
                    .expect("a request URL")
                    .query_pairs()
                    .find_map(|(name, value)| (name == "page").then(|| value.into_owned()))
                    .expect("a page parameter");
                let body = format!(r#"[{{"id":{page},"date_gmt":"2026-08-20T00:00:0{page}"}}]"#);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     x-wp-total: 2\r\nx-wp-totalpages: 2\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("a response write");
                requests.push(target);
            }
            requests
        });

        Ok((port, server))
    }

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
    fn update_directory_reads_only_direct_warcs_in_domain_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested)?;
        write_update_warc(
            &directory.path().join("zeta.warc.gz"),
            "https://zeta.example/",
            "2026-08-18T00:00:00",
            true,
        )?;
        write_update_warc(
            &directory.path().join("alpha.warc"),
            "https://alpha.example/blog/",
            "2026-08-19T00:00:00",
            false,
        )?;
        write_update_warc(
            &directory.path().join("ignored.data"),
            "https://ignored.example/",
            "2026-08-19T00:00:00",
            false,
        )?;
        write_update_warc(
            &nested.join("nested.warc"),
            "https://nested.example/",
            "2026-08-19T00:00:00",
            false,
        )?;

        let updates = comment_update_inputs(directory.path())?;

        assert_eq!(
            updates
                .iter()
                .map(|update| update.anchor.base_url.as_str())
                .collect::<Vec<_>>(),
            ["https://alpha.example/blog/", "https://zeta.example/"]
        );
        assert_eq!(
            updates
                .iter()
                .filter_map(|update| update.path.file_name().and_then(|name| name.to_str()))
                .collect::<Vec<_>>(),
            ["alpha.warc", "zeta.warc.gz"]
        );

        Ok(())
    }

    #[test]
    fn update_directory_requires_a_direct_warc() {
        let directory = tempfile::tempdir().expect("a temporary directory");

        assert!(matches!(
            comment_update_inputs(directory.path()),
            Err(Error::NoUpdateWarcs(path)) if path == directory.path()
        ));
    }

    #[test]
    fn multi_domain_update_starts_each_via_chain_at_its_own_first_page()
    -> Result<(), Box<dyn std::error::Error>> {
        let (first_port, first_server) = serve_comment_pages()?;
        let (second_port, second_server) = serve_comment_pages()?;
        let mut base_urls = [
            format!("http://127.0.0.1:{first_port}/"),
            format!("http://127.0.0.1:{second_port}/"),
        ];
        base_urls.sort();
        let before =
            chrono::DateTime::parse_from_rfc3339("2026-08-21T00:00:00Z")?.with_timezone(&Utc);
        let after =
            chrono::DateTime::parse_from_rfc3339("2026-08-19T00:00:00Z")?.with_timezone(&Utc);
        let runs = base_urls
            .iter()
            .map(|base_url| {
                Ok(CommentRun {
                    base_url: base_url.clone(),
                    processor: CommentCaptureProcessor::for_window(base_url, after, before)?,
                })
            })
            .collect::<Result<Vec<_>, url::ParseError>>()?;
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("updates.warc");

        let outcome = capture_comment_run(
            runs,
            CommentRunOptions {
                config: None,
                cookie: None,
                output: &output,
                session_name: "multi-domain-update",
                revisit_index: None,
                limit: None,
                second_sweep: false,
            },
            true,
        )?;
        assert_eq!(outcome, archivindex_cli_support::CommandOutcome::Success);
        first_server.join().expect("the first local server");
        second_server.join().expect("the second local server");

        let mut metadata = Vec::new();
        for record in WarcReader::from_path(&output)?.iter_records::<NoExtension>() {
            let Record::Metadata { header, body } = record? else {
                continue;
            };
            let Some(target) = header.target_uri else {
                continue;
            };
            let target = target.into_string();
            let FieldsBlock::Fields(fields) = body else {
                continue;
            };
            metadata.push((target, fields.via().map(str::to_owned)));
        }
        for base_url in base_urls {
            let processor = CommentCaptureProcessor::for_window(&base_url, after, before)?;
            let first = processor.first_comment_url();
            let second = first.replace("&page=1&", "&page=2&");
            assert!(metadata.contains(&(first.clone(), None)));
            assert!(metadata.contains(&(second, Some(first))));
        }

        Ok(())
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
