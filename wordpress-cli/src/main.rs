//! A command-line front end for capturing and reading `WordPress` REST API resources.
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use archivindex_archiver::capture::{CaptureControl, CaptureEvent};
use archivindex_archiver::session::{
    Capture, Driver, Inspection, Request, Session, SessionSummary,
};
use archivindex_archiver::{Archiver, Config};
use archivindex_cli_support::{
    CommandOutcome, Verbosity, exit_code, interrupt_flag, load_config, spinner,
};
use archivindex_wordpress::archive::{ArchiveDriver, Checkpoint, Resumption, Site};
use archivindex_wordpress::complete::{CommentCompletionSummary, complete_comments_with_delay};
use archivindex_wordpress::endpoint::{Collection, Registry};
use archivindex_wordpress::lint::{Severity, lint_archive};
use archivindex_wordpress::read::{
    CommentCompleteness, CommentUpdateAnchor, check_comment_collections,
    find_comment_update_anchors, read_comments,
};
use archivindex_wordpress::resume::inspect_archive;
use archivindex_wordpress::{CommentDriver, CommentProgress};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

fn main() -> ExitCode {
    let opts = Opts::parse();
    opts.verbosity.init_logging();

    exit_code(run(opts))
}

fn run(opts: Opts) -> Result<CommandOutcome, Error> {
    let quiet = opts.verbosity.is_quiet();

    match opts.command {
        Command::Archive(options) => archive_site(&options, quiet),
        Command::Check(options) => check_wp_comments(&options, quiet),
        Command::Complete(options) => complete_wp_comments(&options, quiet),
        Command::Lint(options) => lint_wp_archive(&options, quiet),
        Command::Read(options) => read_wp_comments(options),
        Command::ResumeArchive(options) => resume_archive(&options, quiet),
        Command::ResumeInfo(options) => resume_info(&options, quiet),
        Command::Update(options) => update_comments(&options, quiet),
    }
}

/// Validate the capture graph and collection pagination protocol of an archive WARC.
fn lint_wp_archive(options: &LintOptions, quiet: bool) -> Result<CommandOutcome, Error> {
    let report = lint_archive(&options.warc)?;
    for finding in &report.findings {
        match finding.severity {
            Severity::Error => log::error!("{}", finding.message),
            Severity::Warning => log::warn!("{}", finding.message),
        }
    }

    if !quiet {
        for pagination in &report.pagination {
            let pages = pagination
                .pages
                .map_or_else(|| "unknown".to_owned(), |pages| pages.to_string());
            let items = pagination
                .items
                .map_or_else(|| "unknown".to_owned(), |items| items.to_string());
            println!("{}: {pages} pages, {items} items", pagination.endpoint);
        }
        println!(
            "{}: {} roots, {} known probes, {} custom probes, {} paginated endpoints; {} errors, {} warnings",
            options.warc.display(),
            report.roots,
            report.known_probes,
            report.custom_probes,
            report.pagination.len(),
            report.error_count(),
            report.warning_count(),
        );
    }

    Ok(CommandOutcome::from_reported_problems(!report.is_clean()))
}

/// Archive every supported collection a site exposes, beginning with the API roots and probes.
fn archive_site(options: &ArchiveRunOptions, quiet: bool) -> Result<CommandOutcome, Error> {
    let before = Utc::now();

    run_archive(
        ArchiveDriver::new(options.base.clone(), before),
        options,
        before,
        quiet,
    )
}

/// Continue an archive from the checkpoint an earlier run reported.
fn resume_archive(options: &ResumeArchiveOptions, quiet: bool) -> Result<CommandOutcome, Error> {
    let custom = options.custom_collections();
    let endpoint = Collection::find(&options.endpoint, &custom)
        .ok_or_else(|| Error::UnknownEndpoint(options.endpoint.clone()))?;
    let resumption = Resumption {
        endpoint,
        last_page: options.last_page,
        total_pages: options.total_pages,
    };
    let driver =
        ArchiveDriver::resume(options.run.base.clone(), options.before, resumption, custom);

    run_archive(driver, &options.run, options.before, quiet)
}

/// Recover and print the command continuing a collection archive WARC.
fn resume_info(options: &ResumeInfoOptions, quiet: bool) -> Result<CommandOutcome, Error> {
    let info = inspect_archive(&options.warc)?;
    for warning in &info.warnings {
        log::warn!("{warning}");
    }

    match info.checkpoint {
        Checkpoint::Finished => {
            if !quiet {
                println!("{} is complete", options.warc.display());
            }
            if info.warnings.is_empty() {
                Ok(CommandOutcome::Success)
            } else {
                Ok(CommandOutcome::ReportedProblems)
            }
        }
        Checkpoint::Resume(resumption) => {
            let before = info
                .before
                .ok_or_else(|| Error::MissingResumeCutoff(options.warc.clone()))?;
            let parent = options
                .warc
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let run = ArchiveRunOptions {
                config: None,
                base: info.site,
                output: parent.to_owned(),
                session_name: None,
                revisit_index: None,
                limit: None,
                cookie: None,
            };
            println!(
                "{}",
                resume_command(&run, before, &resumption, &info.endpoints)
            );
            Ok(CommandOutcome::ReportedProblems)
        }
        Checkpoint::Initial => Err(Error::InitialArchiveCannotResume(options.warc.clone())),
    }
}

/// Run an archiving session to a new plain WARC in the output directory.
///
/// A run that stops after the initial requests reports the command that continues it; one that
/// stops during them is an error, since there is nothing to continue.
fn run_archive(
    driver: ArchiveDriver,
    options: &ArchiveRunOptions,
    before: DateTime<Utc>,
    quiet: bool,
) -> Result<CommandOutcome, Error> {
    let session_name = options
        .session_name
        .clone()
        .unwrap_or_else(|| options.base.session_name(Utc::now()));
    std::fs::create_dir_all(&options.output)?;
    let output = options.output.join(format!("{session_name}.warc"));
    // The directory accumulates one plain WARC per run, to be merged later.
    let mut config: Config = load_config(options.config.as_deref())?;
    config.gzip_warc = false;
    let mut archiver = Archiver::new(config)?;
    if let Some(cookie) = &options.cookie {
        archiver = archiver.cookie_for(options.base.root().as_str(), cookie)?;
    }

    let state = Rc::new(RefCell::new(ArchiveRunState::new(driver)));
    let progress = Rc::new(RefCell::new(ArchiveProgress::new(options.base.base())));
    let event_progress = Rc::clone(&progress);
    let event_state = Rc::clone(&state);
    // An interrupt ends the session cleanly, so its captures are published and the checkpoint
    // reported instead of abandoning a partial file.
    let interrupted = interrupt_flag();
    let mut session = Session::new(
        archiver,
        &session_name,
        SharedArchiveDriver(Rc::clone(&state)),
        &output,
    )?
    .events(move |event: CaptureEvent<'_>| {
        match event {
            CaptureEvent::Written { url } => {
                let mut state = event_state.borrow_mut();
                state.written(url);
                event_progress.borrow_mut().update(&state.driver);
                if interrupted.load(Ordering::Relaxed) {
                    CaptureControl::Cancel
                } else {
                    CaptureControl::Continue
                }
            }
            // Once a response has been captured, let inspection and recording finish before
            // honoring an interrupt so the reported checkpoint is durable.
            CaptureEvent::Captured { .. } => CaptureControl::Continue,
            CaptureEvent::Started { .. }
            | CaptureEvent::Retrying { .. }
            | CaptureEvent::Failed { .. } => {
                if interrupted.load(Ordering::Relaxed) {
                    CaptureControl::Cancel
                } else {
                    CaptureControl::Continue
                }
            }
        }
    });

    if let Some(revisit_index) = &options.revisit_index {
        session = session.revisit_index(revisit_index);
    }
    if let Some(limit) = options.limit {
        session = session.limit(limit);
    }

    let summary = session.run()?;
    progress.borrow().finish();

    let state = state.borrow();
    report_archive_problems(&summary, &state.driver, options);
    let checkpoint = state.checkpoint_for_summary(&summary);

    match checkpoint {
        Checkpoint::Finished if summary.is_complete() => {
            if !quiet {
                println!(
                    "Archived {} captures from {} to {}",
                    summary.seed_captures.len() + summary.extra_captures.len(),
                    options.base.base(),
                    output.display()
                );
            }
            Ok(CommandOutcome::Success)
        }
        Checkpoint::Finished => {
            log::warn!(
                "the archive session reported problems after its final driver checkpoint; \
                 start a new archive to guarantee completeness"
            );
            Ok(CommandOutcome::ReportedProblems)
        }
        Checkpoint::Resume(resumption) => {
            log::warn!("a partial archive was published at {}", output.display());
            println!(
                "Continue the archive with: {}",
                resume_command(options, before, &resumption, state.driver.endpoints())
            );
            Ok(CommandOutcome::ReportedProblems)
        }
        Checkpoint::Initial => Err(Error::InitialRequestsIncomplete(output)),
    }
}

/// Probe spinner followed by one progress bar per exposed collection with a reported page count.
struct ArchiveProgress {
    multi: MultiProgress,
    probing: ProgressBar,
    pagination: BTreeMap<String, ProgressBar>,
    initialized: bool,
}

impl ArchiveProgress {
    fn new(base: &str) -> Self {
        let multi = MultiProgress::new();
        let probing = multi.add(spinner(format!("Archiving {base}"), None));

        Self {
            multi,
            probing,
            pagination: BTreeMap::new(),
            initialized: false,
        }
    }

    fn update(&mut self, driver: &ArchiveDriver) {
        if !driver.probes_finished() {
            self.probing.set_message(driver.to_string());
            return;
        }
        let progress = driver.pagination_progress();
        if !self.initialized {
            self.probing.finish_and_clear();
            let style = ProgressStyle::with_template(
                "{msg:24} [{bar:40.cyan/blue}] {pos:>3}/{len:3} pages",
            )
            .expect("invariant violation: the archive progress template is well formed")
            .progress_chars("=>-");
            for endpoint in &progress {
                let bar = self.multi.add(ProgressBar::new(
                    u64::try_from(endpoint.total_pages).unwrap_or(u64::MAX),
                ));
                bar.set_style(style.clone());
                self.pagination
                    .insert(endpoint.collection.name().to_owned(), bar);
            }
            self.initialized = true;
        }

        for endpoint in progress {
            if let Some(bar) = self.pagination.get(endpoint.collection.name()) {
                let message = if endpoint.validating {
                    format!("{} (validation)", endpoint.collection)
                } else {
                    endpoint.collection.to_string()
                };
                bar.set_message(message);
                bar.set_position(u64::try_from(endpoint.page).unwrap_or(u64::MAX));
            }
        }
    }

    fn finish(&self) {
        self.probing.finish_and_clear();
        for bar in self.pagination.values() {
            bar.finish_and_clear();
        }
    }
}

fn report_archive_problems(
    summary: &SessionSummary,
    driver: &ArchiveDriver,
    options: &ArchiveRunOptions,
) {
    for failure in &summary.failures {
        log::warn!("Failed to capture {}: {}", failure.url, failure.error);
    }
    if let Some(error) = &summary.fatal_error {
        log::warn!("The session ended early: {error}");
    }
    if summary.cancelled {
        log::warn!("the session was cancelled before all requested captures were completed");
    }
    if summary.partial_captures() > 0 {
        log::warn!(
            "{} capture(s) were unexpectedly truncated",
            summary.partial_captures()
        );
    }
    for (endpoint, status) in driver.probed() {
        let status = *status;
        if status == 404 {
            log::info!("{} does not expose {endpoint}", options.base.base());
        } else if !(200..300).contains(&status) && status != 304 {
            log::warn!(
                "{} answered the {endpoint} probe with status {status}; the endpoint was skipped",
                options.base.base()
            );
        }
    }
}

/// The command continuing an archive from `resumption` with this run's settings.
///
/// The custom collections among `endpoints` are repeated with the flag for their registry, in
/// order, since the resumed run does not read the registries again. A cookie is not repeated,
/// since it is a secret; the session name is left to default to a new timestamp.
fn resume_command(
    options: &ArchiveRunOptions,
    before: DateTime<Utc>,
    resumption: &Resumption,
    endpoints: &[Collection],
) -> String {
    let mut command = format!(
        "archivindex-wordpress resume-archive --output {} --base {} --endpoint {} \
         --last-page {} --before {}",
        shell_word(&options.output.to_string_lossy()),
        shell_word(options.base.base()),
        shell_word(resumption.endpoint.name()),
        resumption.last_page,
        shell_word(&before.to_rfc3339_opts(SecondsFormat::Secs, true))
    );
    if let Some(total_pages) = resumption.total_pages {
        // Writing to a `String` cannot fail, so the `fmt::Result` carries nothing.
        let _ = write!(command, " --total-pages {total_pages}");
    }
    for endpoint in endpoints {
        if let Some(registry) = endpoint.registry() {
            let flag = match registry {
                Registry::Types => "--custom",
                Registry::Taxonomies => "--custom-taxonomy",
            };
            let _ = write!(command, " {flag} {}", shell_word(endpoint.name()));
        }
    }
    let optional = [("--config", &options.config)];
    for (flag, path) in optional {
        if let Some(path) = path {
            let _ = write!(command, " {flag} {}", shell_word(&path.to_string_lossy()));
        }
    }
    // An unknown page count plus conditional 304 responses cannot establish where a collection
    // ends. Carry the revisit index only when the checkpoint also carries the needed count.
    if resumption.total_pages.is_some()
        && let Some(path) = &options.revisit_index
    {
        let _ = write!(
            command,
            " --revisit-index {}",
            shell_word(&path.to_string_lossy())
        );
    }

    command
}

/// Quote one command-line argument for a POSIX-compatible shell.
fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// Driver progress paired with the latest checkpoint known to have reached the WARC.
struct ArchiveRunState {
    driver: ArchiveDriver,
    durable: Checkpoint,
    pending: Option<(String, Checkpoint)>,
    transitions: Vec<(String, Checkpoint)>,
}

impl ArchiveRunState {
    fn new(driver: ArchiveDriver) -> Self {
        let durable = driver.checkpoint();
        Self {
            driver,
            durable,
            pending: None,
            transitions: Vec::new(),
        }
    }

    fn written(&mut self, url: &str) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(pending_url, _)| pending_url == url)
        {
            let (_, checkpoint) = self.pending.take().expect("checked pending transition");
            self.durable = checkpoint;
        }
    }

    /// Roll back before the first unexpectedly partial response; otherwise use durable progress.
    fn checkpoint_for_summary(&self, summary: &SessionSummary) -> Checkpoint {
        let partial_urls = summary
            .seed_captures
            .iter()
            .chain(&summary.extra_captures)
            .filter(|capture| capture.is_partial())
            .map(|capture| capture.url.as_str())
            .collect::<HashSet<_>>();

        self.transitions
            .iter()
            .find(|(url, _)| partial_urls.contains(url.as_str()))
            .map_or_else(|| self.durable.clone(), |(_, before)| before.clone())
    }
}

/// The session's driver, shared with the event sink that commits written progress.
struct SharedArchiveDriver(Rc<RefCell<ArchiveRunState>>);

impl Driver for SharedArchiveDriver {
    fn next(&mut self) -> Option<Request> {
        self.0.borrow_mut().driver.next()
    }

    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        let mut state = self.0.borrow_mut();
        let before = state.driver.checkpoint();
        let inspection = state.driver.inspect(capture);
        let after = state.driver.checkpoint();
        state.transitions.push((capture.url.to_owned(), before));
        state.pending = Some((capture.url.to_owned(), after));
        inspection
    }

    fn failed(&mut self, url: &str, error: &archivindex_archiver::Error) {
        self.0.borrow_mut().driver.failed(url, error);
    }
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
            let driver = CommentDriver::for_window(&update.anchor.base_url, after, before)?;

            Ok(CommentRun {
                site_url: update.anchor.base_url,
                driver,
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

    let mut updates_by_site: BTreeMap<String, CommentUpdateInput> = BTreeMap::new();
    for path in paths {
        let anchors = find_comment_update_anchors(&path).map_err(|source| Error::UpdateAnchor {
            path: path.clone(),
            source: Box::new(source),
        })?;
        for anchor in anchors {
            let replace = updates_by_site
                .get(&anchor.base_url)
                .is_none_or(|current| update_anchor_is_newer(&anchor, &current.anchor));
            if replace {
                updates_by_site.insert(
                    anchor.base_url.clone(),
                    CommentUpdateInput {
                        path: path.clone(),
                        anchor,
                    },
                );
            }
        }
    }
    let mut updates = updates_by_site.into_values().collect::<Vec<_>>();
    updates.sort_by(|left, right| {
        update_domain(&left.anchor)
            .cmp(&update_domain(&right.anchor))
            .then_with(|| left.anchor.base_url.cmp(&right.anchor.base_url))
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(updates)
}

/// Prefer actual comment datetimes over URL cutoffs, then retain the greatest datetime.
fn update_anchor_is_newer(candidate: &CommentUpdateAnchor, current: &CommentUpdateAnchor) -> bool {
    (candidate.from_comment && !current.from_comment)
        || (candidate.from_comment == current.from_comment && candidate.datetime > current.datetime)
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
    /// Names the site in progress messages and scopes the cookie to its host.
    site_url: String,
    driver: CommentDriver,
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
    let (site_urls, drivers): (Vec<_>, Vec<_>) = runs
        .into_iter()
        .map(|run| (run.site_url, run.driver.second_sweep(options.second_sweep)))
        .unzip();
    let comment_progress = Rc::new(RefCell::new(CommentRunProgress {
        site_urls: site_urls.clone(),
        snapshots: vec![None; site_urls.len()],
        latest: None,
    }));
    let driver = ProgressingCommentDriver {
        drivers,
        active: None,
        progress: Rc::clone(&comment_progress),
    };
    let config = load_config_for_output(options.config, options.output)?;
    let mut archiver = Archiver::new(config)?;
    if let Some(cookie) = options.cookie {
        for site_url in &site_urls {
            archiver = archiver.cookie_for(site_url, cookie)?;
        }
    }
    let progress = spinner("Downloading comments", None);
    let event_progress = progress.clone();
    let event_comment_progress = Rc::clone(&comment_progress);
    // An interrupt ends the session cleanly, so its captures are published and the pages it had
    // yet to request are reported instead of abandoning a partial file.
    let interrupted = interrupt_flag();
    let mut session = Session::new(archiver, options.session_name, driver, options.output)?.events(
        move |event: CaptureEvent<'_>| {
            if interrupted.load(Ordering::Relaxed) {
                return CaptureControl::Cancel;
            }
            if matches!(event, CaptureEvent::Written { .. })
                && let Some(message) = event_comment_progress.borrow().latest_message()
            {
                event_progress.set_message(message);
            }
            CaptureControl::Continue
        },
    );

    if let Some(revisit_index) = options.revisit_index {
        session = session.revisit_index(revisit_index);
    }
    if let Some(limit) = options.limit {
        session = session.limit(limit);
    }

    let summary = session.run()?;
    progress.finish_and_clear();

    Ok(report_comment_run(
        &summary,
        &comment_progress.borrow(),
        options.output,
        quiet,
    ))
}

/// Report a finished session's failures and per-site progress.
fn report_comment_run(
    summary: &SessionSummary,
    comment_progress: &CommentRunProgress,
    output: &Path,
    quiet: bool,
) -> CommandOutcome {
    for failure in &summary.failures {
        log::warn!("Failed to capture {}: {}", failure.url, failure.error);
    }
    if let Some(error) = &summary.fatal_error {
        log::warn!("The session ended early: {error}");
    }
    if summary.cancelled {
        log::warn!("The session was interrupted");
    }

    for (site_url, snapshot) in comment_progress.iter() {
        if let Some(snapshot) = snapshot {
            if let Some(shortfall) = snapshot.visibility_shortfall() {
                log::warn!(
                    "WordPress counted {} comments for {} before visibility filtering but returned {} visible comments ({shortfall} omitted)",
                    snapshot.total,
                    site_url,
                    snapshot.downloaded
                );
            }
            if !quiet {
                println!("{site_url}: {snapshot} to {}", output.display());
            }
        } else if !quiet {
            println!(
                "Downloaded no comments from {site_url} to {}",
                output.display()
            );
        }
    }

    if summary.is_complete() {
        CommandOutcome::Success
    } else {
        log::warn!("a partial archive was published at {}", output.display());

        CommandOutcome::ReportedProblems
    }
}

/// Load the archiver settings, making the output filename authoritative for WARC compression.
fn load_config_for_output(config: Option<&Path>, output: &Path) -> Result<Config, Error> {
    let mut config: Config = load_config(config)?;
    config.gzip_warc = output
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gz"));

    Ok(config)
}

/// Drive each site's comment traversal in turn, reporting progress as its pages are inspected.
struct ProgressingCommentDriver {
    drivers: Vec<CommentDriver>,
    /// The index of the driver whose request is outstanding.
    active: Option<usize>,
    progress: Rc<RefCell<CommentRunProgress>>,
}

struct CommentRunProgress {
    site_urls: Vec<String>,
    snapshots: Vec<Option<CommentProgress>>,
    latest: Option<usize>,
}

impl CommentRunProgress {
    fn latest_message(&self) -> Option<String> {
        let index = self.latest?;
        Some(format!(
            "{}: {}",
            self.site_urls[index], self.snapshots[index]?
        ))
    }

    fn iter(&self) -> impl Iterator<Item = (&str, Option<CommentProgress>)> + '_ {
        self.site_urls
            .iter()
            .map(String::as_str)
            .zip(self.snapshots.iter().copied())
    }
}

impl Driver for ProgressingCommentDriver {
    fn next(&mut self) -> Option<Request> {
        let (index, request) = self
            .drivers
            .iter_mut()
            .enumerate()
            .find_map(|(index, driver)| driver.next().map(|request| (index, request)))?;
        self.active = Some(index);

        Some(request)
    }

    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        let Some(index) = self.active.take() else {
            return Inspection::error(format!(
                "captured an unrequested WordPress comments URL: {}",
                capture.url
            ));
        };
        let driver = &mut self.drivers[index];
        let inspection = driver.inspect(capture);
        let mut progress = self.progress.borrow_mut();
        progress.snapshots[index] = driver.progress();
        progress.latest = Some(index);
        inspection
    }

    fn failed(&mut self, url: &str, error: &archivindex_archiver::Error) {
        if let Some(index) = self.active.take() {
            self.drivers[index].failed(url, error);
        }
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
    let collections = check_comment_collections(&options.warc)?;
    if collections.is_empty() {
        log::warn!(
            "{} has no qualifying WordPress comments capture",
            options.warc.display()
        );
        if !quiet {
            println!("{} is incomplete", options.warc.display());
        }
        return Ok(CommandOutcome::ReportedProblems);
    }

    let mut reported_problems = false;
    for collection in collections {
        let coverage = collection.coverage;
        let complete = coverage.is_complete();
        let total_changed = coverage.advertised_total_changed();
        reported_problems |= !complete || total_changed;

        if let Some(warning) = page_total_change_warning(&coverage) {
            log::warn!("{}: {warning}", collection.endpoint);
        }
        if complete {
            if !quiet {
                println!(
                    "{} is complete for {}: all {} advertised comment pages were captured",
                    options.warc.display(),
                    collection.endpoint,
                    coverage
                        .total_pages
                        .expect("complete coverage has an advertised page count")
                );
            }
            continue;
        }

        match coverage.total_pages {
            None => log::warn!(
                "{} has no qualifying record with a valid X-WP-TotalPages header for {}",
                options.warc.display(),
                collection.endpoint
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
                    "{} is missing qualifying records for {} of {} advertised pages for {}: {}{}",
                    options.warc.display(),
                    missing_count,
                    total_pages,
                    collection.endpoint,
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
            println!(
                "{} is incomplete for {}",
                options.warc.display(),
                collection.endpoint
            );
        }
    }

    Ok(CommandOutcome::from_reported_problems(reported_problems))
}

/// Capture exactly the comment pages missing from an existing WARC.
fn complete_wp_comments(
    options: &CompleteCommentsOptions,
    quiet: bool,
) -> Result<CommandOutcome, Error> {
    let config: Config = load_config(options.config.as_deref())?;
    let request_delay = config.session.request_delay;
    let archiver = Archiver::new(config)?;
    let progress = spinner("Completing comments", None);
    let summary =
        complete_comments_with_delay(&archiver, &options.input, &options.output, request_delay)?;
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

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid WordPress base URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("invalid cookie: {0}")]
    Cookie(#[from] archivindex_archiver::CookieError),
    #[error("archiving error: {0}")]
    Archive(#[from] archivindex_archiver::Error),
    #[error("invalid archiver configuration: {0}")]
    Config(#[from] archivindex_archiver::ConfigError),
    #[error(transparent)]
    ConfigFile(#[from] archivindex_cli_support::ConfigError),
    #[error(transparent)]
    UserAgent(#[from] archivindex_archiver::UserAgentError),
    #[error(transparent)]
    SessionId(#[from] archivindex_archiver::session::SessionIdError),
    #[error("WordPress comment reading error: {0}")]
    ReadComments(#[from] archivindex_wordpress::read::Error),
    #[error("WordPress comment completion error: {0}")]
    CompleteComments(#[from] archivindex_wordpress::complete::Error),
    #[error("WordPress archive resume inspection error: {0}")]
    ResumeInfo(#[from] archivindex_wordpress::resume::Error),
    #[error("WordPress archive lint error: {0}")]
    Lint(#[from] archivindex_wordpress::lint::Error),
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
    #[error(
        "the session stopped before its initial requests were finished, so a new archive must \
         start over; a partial archive was published at {}",
        .0.display()
    )]
    InitialRequestsIncomplete(PathBuf),
    #[error(
        "the archive in {} stopped before its initial requests finished and cannot be resumed; \
         start a new archive instead",
        .0.display()
    )]
    InitialArchiveCannotResume(PathBuf),
    #[error(
        "cannot recover the original before cutoff from {}; no paginated request was recorded",
        .0.display()
    )]
    MissingResumeCutoff(PathBuf),
    #[error(
        "unknown endpoint {0:?}; pass a supported endpoint name or one listed with --custom or \
         --custom-taxonomy"
    )]
    UnknownEndpoint(String),
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

/// The workflow to run.
#[derive(Debug, clap::Subcommand)]
// One value of this enum exists per process, so the size difference between its variants costs
// nothing, and boxing a variant would only obscure the derived argument parsing.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Archive every supported collection a site exposes through its `WordPress` REST API v2.
    #[clap(name = "archive")]
    Archive(ArchiveRunOptions),
    /// Check that every advertised comments page has a qualifying response or revisit record.
    #[clap(name = "check-comments")]
    Check(CheckCommentsOptions),
    /// Capture pages missing from a comments WARC into a new WARC.
    #[clap(name = "complete-comments")]
    Complete(CompleteCommentsOptions),
    /// Validate a collection archive's initial captures and pagination series.
    #[clap(name = "lint")]
    Lint(LintOptions),
    /// Read comments captured from the `WordPress` REST API in a WARC file.
    #[clap(name = "read-comments")]
    Read(ReadCommentsOptions),
    /// Continue an archive from the checkpoint an earlier run reported.
    #[clap(name = "resume-archive")]
    ResumeArchive(ResumeArchiveOptions),
    /// Print the command continuing an incomplete collection-archive WARC.
    #[clap(name = "resume-info")]
    ResumeInfo(ResumeInfoOptions),
    /// Capture new comments in a window overlapping an existing comments WARC.
    #[clap(name = "update-comments")]
    Update(UpdateCommentsOptions),
}

/// Options shared by archiving a site and resuming its archive.
#[derive(Debug, clap::Args)]
struct ArchiveRunOptions {
    /// A TOML or JSON archiver configuration file, recognized by its extension; every key is
    /// optional and takes its default when absent.
    #[clap(short, long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    config: Option<PathBuf>,
    /// The site's host with an optional path and no scheme, such as `example.com` or
    /// `example.com/blog`; a trailing slash is ignored, and HTTPS is used unless the base begins
    /// with `http://`.
    #[clap(long, value_name = "BASE")]
    base: Site,
    /// Directory the session's plain WARC file, named after the session, is written to; it is
    /// created when missing (an existing file is not overwritten).
    #[clap(long, value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    output: PathBuf,
    /// URL-safe name of the session and its WARC file; defaults to the base, with hyphens for its
    /// slashes, followed by a hyphen and the current epoch second.
    #[clap(long)]
    session_name: Option<String>,
    /// Persistent payload-revisit and conditional-request state database.
    #[clap(long)]
    revisit_index: Option<PathBuf>,
    /// Stop after this many captures, reporting the command that continues the archive.
    #[clap(long)]
    limit: Option<usize>,
    /// Cookie header obtained from a browser, scoped to the site's host.
    ///
    /// The value is sent with every request to that host and recorded in the WARC request records.
    /// Quote values containing semicolons.
    #[clap(long)]
    cookie: Option<String>,
}

/// Options for continuing an archive from its checkpoint.
#[derive(Debug, clap::Args)]
struct ResumeArchiveOptions {
    #[clap(flatten)]
    run: ArchiveRunOptions,
    /// The endpoint to continue: a supported endpoint's lowercase name, or a custom collection
    /// also listed with `--custom` or `--custom-taxonomy`.
    #[clap(long, value_name = "ENDPOINT")]
    endpoint: String,
    /// The last page of that endpoint the earlier run captured, or 0 to probe the endpoint again.
    #[clap(long, value_name = "PAGE")]
    last_page: usize,
    /// The most recently advertised page count, when the earlier run reported one.
    #[clap(long, value_name = "PAGES")]
    total_pages: Option<usize>,
    /// The cutoff the archive started with, as reported by the earlier run.
    #[clap(long, value_name = "TIMESTAMP")]
    before: DateTime<Utc>,
    /// A custom collection the earlier run discovered in the site's post types, repeated in the
    /// reported order; the resumed run does not read the registries again.
    #[clap(long, value_name = "ENDPOINT")]
    custom: Vec<String>,
    /// A custom collection the earlier run discovered in the site's taxonomies, repeated in the
    /// reported order.
    #[clap(long, value_name = "ENDPOINT")]
    custom_taxonomy: Vec<String>,
}

/// Options for recovering continuation information from an archive WARC.
#[derive(Debug, clap::Args)]
struct ResumeInfoOptions {
    /// Path of the plain or gzip-compressed WARC file to inspect.
    #[clap(value_hint = clap::ValueHint::FilePath)]
    warc: PathBuf,
}

impl ResumeArchiveOptions {
    /// The custom collections in probing order: those from the post types, then the taxonomies.
    fn custom_collections(&self) -> Vec<Collection> {
        let types = self.custom.iter().zip(std::iter::repeat(Registry::Types));
        let taxonomies = self
            .custom_taxonomy
            .iter()
            .zip(std::iter::repeat(Registry::Taxonomies));

        types
            .chain(taxonomies)
            .map(|(name, registry)| Collection::Custom {
                name: name.clone(),
                registry,
            })
            .collect()
    }
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

/// Options for linting a `WordPress` collection archive.
#[derive(Debug, clap::Args)]
struct LintOptions {
    /// Path of the plain or gzip-compressed WARC file to lint.
    #[clap(value_hint = clap::ValueHint::FilePath)]
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
    /// Path of the completion WARC to write; a `.gz` suffix enables gzip compression (an existing
    /// file is not overwritten).
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
    /// Path of the WARC file to write; a `.gz` suffix enables gzip compression (an existing file
    /// is not overwritten).
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
    use std::cell::RefCell;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::thread;
    use std::time::Duration;

    use archivindex_archiver::session::{Capture, Driver, Request};
    use archivindex_cli_support::{CommandOutcome, load_config};
    use archivindex_test_support::http::{dead_port, response, serve_with};
    use archivindex_warc::io::read::WarcReader;
    use archivindex_warc::io::write::WarcWriter;
    use archivindex_warc::record::extension::NoExtension;
    use archivindex_warc::record::{FieldsBlock, Record};
    use archivindex_wordpress::CommentDriver;
    use archivindex_wordpress::archive::{ArchiveDriver, Checkpoint, Resumption, Site};
    use archivindex_wordpress::endpoint::{Collection, Endpoint, Registry};
    use archivindex_wordpress::lint::{Severity, lint_archive};
    use archivindex_wordpress::read::{CommentCompleteness, check_comment_collections};
    use archivindex_wordpress::resume::inspect_archive;
    use chrono::{DateTime, Utc};
    use clap::{CommandFactory, Parser};
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::{
        ArchiveRunOptions, ArchiveRunState, CheckCommentsOptions, Command, CommentRun,
        CommentRunOptions, Config, Error, Opts, ResumeInfoOptions, SharedArchiveDriver,
        capture_comment_run, check_wp_comments, comment_update_inputs, load_config_for_output,
        page_total_change_warning, resume_archive, resume_command, resume_info, run_archive,
    };

    const BEFORE: &str = "2026-08-20T00:00:00Z";

    fn before() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(BEFORE)
            .map(|date| date.with_timezone(&Utc))
            .expect("a test timestamp")
    }

    fn resumption(
        endpoint: impl Into<Collection>,
        last_page: usize,
        total_pages: Option<usize>,
    ) -> Resumption {
        Resumption {
            endpoint: endpoint.into(),
            last_page,
            total_pages,
        }
    }

    fn custom(name: &str, registry: Registry) -> Collection {
        Collection::Custom {
            name: name.to_owned(),
            registry,
        }
    }

    /// A registry response with one `wp/v2` entry whose collection is at `rest_base`.
    fn registry(rest_base: &str) -> String {
        format!(
            r#"{{"{rest_base}": {{"name": "", "description": "", "hierarchical": false,
                "slug": "{rest_base}", "rest_base": "{rest_base}", "rest_namespace": "wp/v2",
                "_links": {{"wp:items": [{{"href": "https://example.com/x"}}]}}}}}}"#
        )
    }

    /// Serve `requests` of a site exposing two pages of `pages`, one of `comments`, and one of
    /// the custom `videos` type its type registry advertises.
    ///
    /// The taxonomy registry advertises `series`, which is not exposed; every other probe is
    /// answered with 404, and each note is the request's path.
    fn serve_site(requests: usize) -> std::io::Result<(u16, thread::JoinHandle<Vec<String>>)> {
        serve_with(requests, |request| {
            let target = request.path();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let json = [("content-type", "application/json")];
            let reply = match path.strip_prefix("/wp-json/wp/v2/") {
                Some("types") => response("200 OK", &json, &registry("videos")),
                Some("taxonomies") => response("200 OK", &json, &registry("series")),
                Some("pages") if query.is_empty() => response(
                    "200 OK",
                    &[
                        ("content-type", "application/json"),
                        ("x-wp-total", "101"),
                        ("x-wp-totalpages", "2"),
                    ],
                    "[]",
                ),
                Some("comments" | "videos") if query.is_empty() => response(
                    "200 OK",
                    &[
                        ("content-type", "application/json"),
                        ("x-wp-total", "3"),
                        ("x-wp-totalpages", "1"),
                    ],
                    "[]",
                ),
                Some("pages") => response(
                    "200 OK",
                    &[
                        ("content-type", "application/json"),
                        ("x-wp-totalpages", "2"),
                    ],
                    "[]",
                ),
                Some("comments" | "videos") => response(
                    "200 OK",
                    &[
                        ("content-type", "application/json"),
                        ("x-wp-totalpages", "1"),
                    ],
                    "[]",
                ),
                Some(_) => response("404 Not Found", &json, "{}"),
                None => response("200 OK", &json, "{}"),
            };
            (reply, target.to_owned())
        })
    }

    fn archive_options(port: u16, output: &Path, session_name: &str) -> ArchiveRunOptions {
        ArchiveRunOptions {
            config: None,
            base: Site::parse(&format!("http://127.0.0.1:{port}")).expect("a site"),
            output: output.to_owned(),
            session_name: Some(session_name.to_owned()),
            revisit_index: None,
            limit: None,
            cookie: None,
        }
    }

    fn write_update_warc(
        path: &Path,
        base_url: &str,
        comment_datetime: &str,
        gzip: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let body = format!(r#"[{{"id":1,"date_gmt":"{comment_datetime}"}}]"#);
        write_update_warc_batches(
            path,
            &[(base_url, "2026-08-20T00:00:00Z", body.as_str())],
            gzip,
        )
    }

    fn write_update_warc_batches(
        path: &Path,
        batches: &[(&str, &str, &str)],
        gzip: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut bytes = Vec::new();
        let mut writer = WarcWriter::new(&mut bytes);
        for (base_url, before, body) in batches {
            let url = format!("{base_url}wp-json/wp/v2/comments?before={before}&page=1");
            let message = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\n\r\n{body}",
                body.len()
            );
            let record: Record = Record::response(&url, Utc::now())?.body(message.into_bytes())?;
            writer.write(&record.into_raw()?)?;
        }
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

    /// A metadata record's target URL and `via`.
    type MetadataVia = (String, Option<String>);

    fn assert_archive_lint(warc: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let lint = lint_archive(warc)?;
        assert_eq!(
            (lint.roots, lint.known_probes, lint.custom_probes),
            (8, 8, 2)
        );
        assert_eq!(lint.error_count(), 0, "{:?}", lint.findings);
        assert_eq!(lint.warning_count(), 4);
        assert!(
            lint.findings
                .iter()
                .all(|finding| finding.severity == Severity::Warning)
        );
        assert_eq!(
            lint.pagination
                .iter()
                .map(|summary| (
                    summary.endpoint.rsplit('/').next(),
                    summary.pages,
                    summary.items
                ))
                .collect::<Vec<_>>(),
            [
                (Some("pages"), Some(2), Some(101)),
                (Some("comments"), Some(1), Some(3)),
                (Some("videos"), Some(1), Some(3)),
            ]
        );

        let gzip = output.join("site-archive.warc.gz");
        let mut encoder = GzEncoder::new(std::fs::File::create(&gzip)?, Compression::default());
        encoder.write_all(&std::fs::read(warc)?)?;
        encoder.finish()?;
        assert_eq!(lint_archive(gzip)?, lint);
        Ok(())
    }

    /// Serve `requests` of a two-page comments collection on a local port.
    fn serve_comment_pages(
        requests: usize,
    ) -> std::io::Result<(u16, thread::JoinHandle<Vec<String>>)> {
        serve_with(requests, |request| {
            let target = request.path();
            let page = url::Url::parse(&format!("http://localhost{target}"))
                .expect("a request URL")
                .query_pairs()
                .find_map(|(name, value)| (name == "page").then(|| value.into_owned()))
                .expect("a page parameter");
            let body = format!(r#"[{{"id":{page},"date_gmt":"2026-08-20T00:00:0{page}"}}]"#);
            let headers = [
                ("content-type", "application/json"),
                ("x-wp-total", "2"),
                ("x-wp-totalpages", "2"),
            ];
            (response("200 OK", &headers, &body), target.to_owned())
        })
    }

    #[test]
    fn archive_command_reads_workflow_and_config_options() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "archive",
            "--config",
            "capture.toml",
            "--base",
            "example.com/blog/",
            "--output",
            "archives",
            "--session-name",
            "blog-2026",
            "--revisit-index",
            "state.sqlite3",
            "--limit",
            "12",
            "--cookie",
            "cf_clearance=test-clearance; __cf_bm=test-bot-cookie",
        ])
        .expect("valid options");

        let Command::Archive(options) = options.command else {
            panic!("expected the archiving command");
        };

        assert_eq!(options.base.base(), "example.com/blog");
        assert_eq!(options.output, PathBuf::from("archives"));
        assert_eq!(options.session_name.as_deref(), Some("blog-2026"));
        assert_eq!(options.config, Some(PathBuf::from("capture.toml")));
        assert_eq!(options.revisit_index, Some(PathBuf::from("state.sqlite3")));
        assert_eq!(options.limit, Some(12));
        assert_eq!(
            options.cookie.as_deref(),
            Some("cf_clearance=test-clearance; __cf_bm=test-bot-cookie")
        );

        let defaults = Opts::try_parse_from([
            "archivindex-wordpress",
            "archive",
            "--base",
            "example.com",
            "--output",
            "archives",
        ])
        .expect("valid options");
        let Command::Archive(defaults) = defaults.command else {
            panic!("expected the archiving command");
        };
        assert_eq!(defaults.session_name, None);
        assert!(
            defaults
                .base
                .session_name(before())
                .starts_with("example.com-")
        );
    }

    #[test]
    fn lint_command_accepts_a_warc_path() {
        let options =
            Opts::try_parse_from(["archivindex-wordpress", "lint", "archives/site.warc.gz"])
                .expect("valid options");
        let Command::Lint(options) = options.command else {
            panic!("expected the lint command");
        };

        assert_eq!(options.warc, PathBuf::from("archives/site.warc.gz"));
    }

    #[test]
    fn resume_command_requires_the_checkpoint() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "resume-archive",
            "--output",
            "archives",
            "--base",
            "example.com",
            "--endpoint",
            "comments",
            "--last-page",
            "7",
            "--before",
            BEFORE,
            "--custom-taxonomy",
            "series",
            "--custom",
            "videos",
            "--custom",
            "clips",
        ])
        .expect("valid options");

        let Command::ResumeArchive(options) = options.command else {
            panic!("expected the resuming command");
        };

        assert_eq!(options.run.base.base(), "example.com");
        assert_eq!(options.run.output, PathBuf::from("archives"));
        assert_eq!(options.run.session_name, None);
        assert_eq!(options.endpoint, "comments");
        assert_eq!(options.last_page, 7);
        assert_eq!(options.total_pages, None);
        assert_eq!(options.before, before());
        // Custom collections from the post types precede those from the taxonomies.
        assert_eq!(
            options.custom_collections(),
            [
                custom("videos", Registry::Types),
                custom("clips", Registry::Types),
                custom("series", Registry::Taxonomies),
            ]
        );

        let arguments = [
            "archivindex-wordpress",
            "resume-archive",
            "--output",
            "archives",
            "--base",
            "example.com",
            "--endpoint",
            "comments",
            "--last-page",
            "7",
            "--before",
            BEFORE,
        ];
        // Each checkpoint argument is required.
        let rejected = [
            arguments.map(|argument| if argument == "7" { "-1" } else { argument }),
            arguments.map(|argument| {
                if argument == BEFORE {
                    "yesterday"
                } else {
                    argument
                }
            }),
        ];
        for arguments in rejected {
            assert!(Opts::try_parse_from(arguments).is_err(), "{arguments:?}");
        }
        for skipped in [6, 8, 10] {
            let mut incomplete = arguments.to_vec();
            incomplete.drain(skipped..skipped + 2);
            assert!(Opts::try_parse_from(incomplete).is_err());
        }

        // Endpoint names are exact, and a custom endpoint must be listed, before anything runs.
        for (endpoint, custom) in [("Comments", "comments"), ("videos", "clips")] {
            let mut unknown = arguments.to_vec();
            unknown[7] = endpoint;
            unknown.extend(["--custom", custom]);
            let Command::ResumeArchive(options) = Opts::try_parse_from(unknown)
                .expect("valid options")
                .command
            else {
                panic!("expected the resuming command");
            };
            assert!(matches!(
                resume_archive(&options, true),
                Err(Error::UnknownEndpoint(name)) if name == endpoint
            ));
        }
    }

    #[test]
    fn resume_info_command_takes_a_warc_path() {
        let options = Opts::try_parse_from([
            "archivindex-wordpress",
            "resume-info",
            "archives/site.warc.gz",
        ])
        .expect("valid options");

        let Command::ResumeInfo(options) = options.command else {
            panic!("expected the resume information command");
        };
        assert_eq!(options.warc, PathBuf::from("archives/site.warc.gz"));
    }

    #[test]
    fn a_checkpoint_is_printed_as_a_resume_command() {
        let mut options = ArchiveRunOptions {
            config: None,
            base: Site::parse("example.com/blog").expect("a site"),
            output: PathBuf::from("archives"),
            session_name: Some("ignored".to_owned()),
            revisit_index: None,
            limit: Some(3),
            cookie: Some("secret=yes".to_owned()),
        };

        assert_eq!(
            resume_command(
                &options,
                before(),
                &resumption(Endpoint::Comments, 7, None),
                &Endpoint::ALL.map(Collection::Known)
            ),
            "archivindex-wordpress resume-archive --output archives --base example.com/blog \
             --endpoint comments --last-page 7 --before 2026-08-20T00:00:00Z"
        );

        // Custom collections are repeated in order, with the flag of their registry.
        options.config = Some(PathBuf::from("capture.toml"));
        options.revisit_index = Some(PathBuf::from("state.sqlite3"));
        let videos = custom("videos", Registry::Types);
        let endpoints = [
            Endpoint::Media.into(),
            videos.clone(),
            custom("series", Registry::Taxonomies),
            custom("it's", Registry::Types),
        ];
        assert_eq!(
            resume_command(
                &options,
                before(),
                &resumption(Endpoint::Media, 0, Some(12)),
                &endpoints
            ),
            "archivindex-wordpress resume-archive --output archives --base example.com/blog \
             --endpoint media --last-page 0 --before 2026-08-20T00:00:00Z --total-pages 12 \
             --custom videos --custom-taxonomy series --custom 'it'\"'\"'s' \
             --config capture.toml --revisit-index state.sqlite3"
        );

        options.output = PathBuf::from("archive output/it's here");
        options.config = Some(PathBuf::from("capture files/site's.toml"));
        assert_eq!(
            resume_command(&options, before(), &resumption(videos, 3, None), &[]),
            "archivindex-wordpress resume-archive --output 'archive output/it'\"'\"'s here' \
             --base example.com/blog --endpoint videos --last-page 3 \
             --before 2026-08-20T00:00:00Z \
             --config 'capture files/site'\"'\"'s.toml'"
        );
    }

    #[test]
    fn archive_progress_becomes_durable_only_after_the_written_event() {
        let driver = ArchiveDriver::resume(
            Site::parse("example.com").expect("a site"),
            before(),
            resumption(Endpoint::Comments, 1, Some(2)),
            Vec::new(),
        );
        let state = Rc::new(RefCell::new(ArchiveRunState::new(driver)));
        let mut shared = SharedArchiveDriver(Rc::clone(&state));
        let url = format!(
            "https://example.com/wp-json/wp/v2/comments?before={BEFORE}&orderby=id&order=asc\
             &page=2&per_page=100"
        );
        let response = b"HTTP/1.1 200 OK\r\nX-WP-TotalPages: 2\r\n\r\n";
        let capture = Capture::new(&url, &url, b"[]", response).expect("a complete response");

        let inspection = shared.inspect(&capture);

        assert_eq!(
            state.borrow().durable,
            Checkpoint::Resume(resumption(Endpoint::Comments, 1, Some(2)))
        );
        assert_eq!(inspection.error, None);
        assert_eq!(
            shared.next(),
            Some(Request::extra(url.replace("&page=2&", "&page=1&"), &url))
        );

        state.borrow_mut().written(&url);

        assert_eq!(
            state.borrow().durable,
            Checkpoint::Resume(resumption(Endpoint::Comments, 0, Some(2)))
        );
    }

    #[test]
    fn archive_command_does_not_duplicate_configuration_fields() {
        let command = Opts::command();
        let archive = command
            .find_subcommand("archive")
            .expect("the archive command");
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
            "retry_attempts",
            "retry_initial_backoff",
            "retry_max_backoff",
            "request_delay",
        ] {
            assert!(!argument_ids.contains(&removed), "unexpected --{removed}");
        }
    }

    #[test]
    fn configuration_file_supplies_archiver_and_session_settings() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("capture.toml");
        std::fs::write(
            &path,
            "gzip-warc = true\n\
             [operator]\nname = \"A. Archivist\"\nemail = \"archivist@example.com\"\n\
             [session]\nrequest-delay = \"750ms\"\n",
        )
        .expect("write the configuration");

        let config = load_config::<Config>(Some(&path)).expect("read the configuration");

        assert!(config.gzip_warc);
        let operator = config.operator.expect("a configured operator");
        assert_eq!(operator.name, "A. Archivist");
        assert_eq!(operator.email.as_deref(), Some("archivist@example.com"));
        assert_eq!(config.session.request_delay, Duration::from_millis(750));
    }

    #[test]
    fn output_filename_controls_capture_compression() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("capture.toml");
        std::fs::write(&path, "gzip-warc = true\n").expect("write the configuration");

        assert!(
            load_config_for_output(Some(&path), Path::new("comments.warc.gz"))
                .expect("load gzip output settings")
                .gzip_warc
        );
        assert!(
            !load_config_for_output(Some(&path), Path::new("comments.warc"))
                .expect("load plain output settings")
                .gzip_warc
        );
        assert!(
            load_config_for_output(None, Path::new("comments.warc.GZ"))
                .expect("load case-insensitive gzip output settings")
                .gzip_warc
        );
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
    fn update_directory_merges_prior_multi_site_updates_by_site()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write_update_warc(
            &directory.path().join("alpha-history.warc"),
            "https://alpha.example/",
            "2026-08-18T00:00:00",
            false,
        )?;
        write_update_warc(
            &directory.path().join("zeta-history.warc"),
            "https://zeta.example/",
            "2026-08-17T00:00:00",
            false,
        )?;
        write_update_warc_batches(
            &directory.path().join("first-update.warc"),
            &[
                (
                    "https://beta.example/",
                    "2026-08-22T00:00:00Z",
                    r#"[{"id":1,"date_gmt":"2026-08-21T00:00:00"}]"#,
                ),
                (
                    "https://alpha.example/",
                    "2026-08-22T00:00:00Z",
                    r#"[{"id":2,"date_gmt":"2026-08-20T00:00:00"}]"#,
                ),
            ],
            false,
        )?;
        // A later empty run must not displace the latest actual comment for this site.
        write_update_warc_batches(
            &directory.path().join("empty-update.warc"),
            &[("https://alpha.example/", "2026-08-25T00:00:00Z", "[]")],
            false,
        )?;

        let updates = comment_update_inputs(directory.path())?;

        assert_eq!(
            updates
                .iter()
                .map(|update| update.anchor.base_url.as_str())
                .collect::<Vec<_>>(),
            [
                "https://alpha.example/",
                "https://beta.example/",
                "https://zeta.example/"
            ]
        );
        assert_eq!(
            updates[0].anchor.datetime.to_rfc3339(),
            "2026-08-20T00:00:00+00:00"
        );
        assert!(updates[0].anchor.from_comment);
        assert_eq!(
            updates[0].path.file_name().and_then(|name| name.to_str()),
            Some("first-update.warc")
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
        let (first_port, first_server) = serve_comment_pages(2)?;
        let (second_port, second_server) = serve_comment_pages(2)?;
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
                    site_url: base_url.clone(),
                    driver: CommentDriver::for_window(base_url, after, before)?,
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

        let metadata = metadata_vias(&output)?;
        for base_url in base_urls {
            let driver = CommentDriver::for_window(&base_url, after, before)?;
            let first = driver.first_comment_url();
            let second = first.replace("&page=1&", "&page=2&");
            assert!(metadata.contains(&(first.clone(), None)));
            assert!(metadata.contains(&(second, Some(first))));
        }
        let collections = check_comment_collections(&output)?;
        assert_eq!(collections.len(), 2);
        assert!(collections.iter().all(|collection| {
            collection.coverage.total_pages == Some(2)
                && collection.coverage.captured_pages == [1, 2]
                && collection.coverage.is_complete()
        }));
        assert_eq!(
            check_wp_comments(&CheckCommentsOptions { warc: output }, true,)?,
            archivindex_cli_support::CommandOutcome::Success
        );

        Ok(())
    }

    #[test]
    fn an_archive_pages_each_exposed_collection_after_the_probes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (port, server) = serve_site(24)?;
        let directory = tempfile::tempdir()?;
        let output = directory.path().join("archives");
        let options = archive_options(port, &output, "site-archive");
        let root = format!("http://127.0.0.1:{port}/");
        let page = |endpoint: &str, page: usize| {
            format!(
                "{root}wp-json/wp/v2/{endpoint}?before={BEFORE}&orderby=id&order=asc&page={page}\
                 &per_page=100"
            )
        };

        let outcome = run_archive(
            ArchiveDriver::new(options.base.clone(), before()),
            &options,
            before(),
            true,
        )?;

        assert_eq!(outcome, CommandOutcome::Success);
        let mut expected = vec![
            "/wp-json".to_owned(),
            "/wp-json/wp/v2".to_owned(),
            "/wp-json/wp/v2/types".to_owned(),
            "/wp-json/wp/v2/taxonomies".to_owned(),
            "/wp-json/wp/v2/block-types".to_owned(),
            "/wp-json/wp/v2/block-patterns/categories".to_owned(),
            "/wp-json/wp/v2/block-patterns/patterns".to_owned(),
            "/wp-json/wp/v2/menu-locations".to_owned(),
        ];
        expected.extend(
            Endpoint::ALL
                .iter()
                .map(|endpoint| format!("/wp-json/wp/v2/{endpoint}")),
        );
        // The custom collections are probed after the supported endpoints, in registry order.
        expected.extend(["/wp-json/wp/v2/videos", "/wp-json/wp/v2/series"].map(str::to_owned));
        expected.extend(
            [
                ("pages", 1),
                ("pages", 2),
                ("pages", 1),
                ("pages", 2),
                ("comments", 1),
                ("videos", 1),
            ]
            .map(|(endpoint, number)| page(endpoint, number)[root.len() - 1..].to_owned()),
        );
        assert_eq!(server.join().expect("the local server"), expected);

        let warc = output.join("site-archive.warc");
        assert!(std::fs::read(&warc)?.starts_with(b"WARC/"));
        let resume = inspect_archive(&warc)?;
        assert_eq!(resume.checkpoint, Checkpoint::Finished);
        assert!(resume.warnings.is_empty());
        assert_eq!(resume.before, Some(before()));
        assert_eq!(
            resume.endpoints[8..],
            [
                custom("videos", Registry::Types),
                custom("series", Registry::Taxonomies),
            ]
        );
        let vias = metadata_vias(&warc)?;
        let seeds = vias.iter().filter(|(_, via)| via.is_none()).count();
        assert_eq!(seeds, 16);
        assert_eq!(
            vias[16..],
            [
                (
                    format!("{root}wp-json/wp/v2/videos"),
                    Some(format!("{root}wp-json/wp/v2/types"))
                ),
                (
                    format!("{root}wp-json/wp/v2/series"),
                    Some(format!("{root}wp-json/wp/v2/taxonomies"))
                ),
                (page("pages", 1), Some(format!("{root}wp-json/wp/v2/pages"))),
                (page("pages", 2), Some(page("pages", 1))),
                (page("pages", 1), Some(page("pages", 2))),
                (page("pages", 2), Some(page("pages", 1))),
                (
                    page("comments", 1),
                    Some(format!("{root}wp-json/wp/v2/comments"))
                ),
                (
                    page("videos", 1),
                    Some(format!("{root}wp-json/wp/v2/videos"))
                ),
            ]
        );

        assert_archive_lint(&warc, &output)?;

        Ok(())
    }

    #[test]
    fn a_resumed_archive_continues_the_endpoint_via_its_last_page()
    -> Result<(), Box<dyn std::error::Error>> {
        let (port, server) = serve_site(6)?;
        let directory = tempfile::tempdir()?;
        let options = archive_options(port, directory.path(), "site-resumed");
        let root = format!("http://127.0.0.1:{port}/wp-json/wp/v2/");
        let page = |endpoint: &str, page: usize| {
            format!(
                "{root}{endpoint}?before={BEFORE}&orderby=id&order=asc&page={page}&per_page=100"
            )
        };

        let outcome = run_archive(
            ArchiveDriver::resume(
                options.base.clone(),
                before(),
                resumption(Endpoint::Comments, 1, Some(2)),
                vec![custom("videos", Registry::Types)],
            ),
            &options,
            before(),
            true,
        )?;

        assert_eq!(outcome, CommandOutcome::Success);
        let requests = server.join().expect("the local server");
        assert_eq!(requests.len(), 6);
        assert_eq!(
            requests[2..5],
            [
                "/wp-json/wp/v2/media",
                "/wp-json/wp/v2/navigation",
                "/wp-json/wp/v2/videos"
            ]
        );
        assert_eq!(
            metadata_vias(&directory.path().join("site-resumed.warc"))?,
            [
                (page("comments", 2), Some(page("comments", 1))),
                (page("comments", 1), Some(page("comments", 2))),
                (format!("{root}media"), None),
                (format!("{root}navigation"), None),
                (format!("{root}videos"), Some(format!("{root}types"))),
                (page("videos", 1), Some(format!("{root}videos"))),
            ]
        );
        let resume = inspect_archive(directory.path().join("site-resumed.warc"))?;
        assert_eq!(resume.checkpoint, Checkpoint::Finished);
        assert!(resume.warnings.is_empty());
        assert_eq!(resume.before, Some(before()));
        assert!(
            resume
                .endpoints
                .contains(&custom("videos", Registry::Types))
        );

        Ok(())
    }

    #[test]
    fn a_limited_archive_reports_problems_at_its_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let (port, server) = serve_site(19)?;
        let directory = tempfile::tempdir()?;
        let mut options = archive_options(port, directory.path(), "site-limited");
        options.limit = Some(19);

        let outcome = run_archive(
            ArchiveDriver::new(options.base.clone(), before()),
            &options,
            before(),
            true,
        )?;

        assert_eq!(outcome, CommandOutcome::ReportedProblems);
        assert_eq!(server.join().expect("the local server").len(), 19);

        let warc = directory.path().join("site-limited.warc");
        let resume = inspect_archive(&warc)?;
        assert_eq!(
            resume.checkpoint,
            Checkpoint::Resume(resumption(Endpoint::Pages, 1, Some(2)))
        );
        assert_eq!(resume.before, Some(before()));
        assert!(resume.warnings.is_empty());
        assert!(
            resume
                .endpoints
                .contains(&custom("videos", Registry::Types))
        );
        assert!(
            resume
                .endpoints
                .contains(&custom("series", Registry::Taxonomies))
        );

        assert_eq!(
            resume_info(&ResumeInfoOptions { warc }, true)?,
            CommandOutcome::ReportedProblems
        );

        Ok(())
    }

    #[test]
    fn resume_info_warns_and_rolls_back_an_incomplete_capture()
    -> Result<(), Box<dyn std::error::Error>> {
        let (port, server) = serve_site(19)?;
        let directory = tempfile::tempdir()?;
        let mut options = archive_options(port, directory.path(), "site-damaged");
        options.limit = Some(19);
        let _ = run_archive(
            ArchiveDriver::new(options.base.clone(), before()),
            &options,
            before(),
            true,
        )?;
        assert_eq!(server.join().expect("the local server").len(), 19);

        let source = directory.path().join("site-damaged.warc");
        let damaged = directory.path().join("request-only.warc");
        let mut records = WarcReader::from_path(&source)?
            .iter_records::<NoExtension>()
            .records()
            .collect::<Result<Vec<_>, _>>()?;
        let last_request = records
            .iter()
            .rposition(|record| matches!(record, Record::Request { .. }))
            .expect("a request record");
        records.truncate(last_request + 1);
        let mut bytes = Vec::new();
        let mut writer = WarcWriter::new(&mut bytes);
        for record in records {
            writer.write(&record.into_raw()?)?;
        }
        writer.flush()?;
        std::fs::write(&damaged, bytes)?;

        let resume = inspect_archive(&damaged)?;
        assert_eq!(
            resume.checkpoint,
            Checkpoint::Resume(resumption(Endpoint::Pages, 0, None))
        );
        assert_eq!(resume.before, Some(before()));
        assert!(resume.warnings.iter().any(|warning| {
            warning.contains("missing response or revisit and metadata")
                && warning.contains("pages")
        }));
        assert!(
            resume
                .endpoints
                .contains(&custom("videos", Registry::Types))
        );
        assert!(
            resume
                .endpoints
                .contains(&custom("series", Registry::Taxonomies))
        );

        Ok(())
    }

    #[test]
    fn a_failure_during_the_initial_requests_is_fatal() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("capture.toml");
        std::fs::write(&config, "[session.retry]\nattempts = 1\n")?;
        let mut options = archive_options(dead_port()?, directory.path(), "site-unreachable");
        options.config = Some(config);

        let result = run_archive(
            ArchiveDriver::new(options.base.clone(), before()),
            &options,
            before(),
            true,
        );

        assert!(matches!(
            result,
            Err(Error::InitialRequestsIncomplete(output))
                if output == directory.path().join("site-unreachable.warc")
        ));

        Ok(())
    }

    /// Every metadata record's target and `via`, in WARC order.
    fn metadata_vias(warc: &Path) -> Result<Vec<MetadataVia>, Box<dyn std::error::Error>> {
        let mut metadata = Vec::new();
        let gzip = std::fs::read(warc)?
            .get(..2)
            .is_some_and(|magic| magic == [0x1f, 0x8b]);
        let reader = if gzip {
            WarcReader::from_path_gzip(warc)?
        } else {
            WarcReader::from_path(warc)?
        };
        for record in reader.iter_records::<NoExtension>().records() {
            let Record::Metadata { header, body } = record? else {
                continue;
            };
            let Some(target) = header.target_uri else {
                continue;
            };
            let FieldsBlock::Fields(fields) = body else {
                continue;
            };
            metadata.push((target.into_string(), fields.via().map(str::to_owned)));
        }

        Ok(metadata)
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
