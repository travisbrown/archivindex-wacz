#![cfg_attr(docsrs, feature(doc_cfg))]
//! Command-line behavior shared by this repository's applications.
//!
//! Each of them takes the same verbosity options, logs to standard error at the same levels,
//! exits with the same status for the same kind of outcome, and counts what it worked on the same
//! way.

use std::fmt::Display;
use std::process::ExitCode;

/// The process-level outcome of a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CommandOutcome {
    /// The command did what it was asked without reportable input problems.
    Success = 0,
    /// The command finished but found problems worth reporting about its input.
    ReportedProblems = 1,
    /// The command could not do what it was asked.
    OperationalError = 2,
}

impl CommandOutcome {
    /// Select success or reported problems from whether any problems were found.
    #[must_use]
    pub const fn from_reported_problems(found: bool) -> Self {
        if found {
            Self::ReportedProblems
        } else {
            Self::Success
        }
    }
}

impl From<CommandOutcome> for ExitCode {
    fn from(outcome: CommandOutcome) -> Self {
        Self::from(outcome as u8)
    }
}

/// Convert a command result to its process exit code, logging operational errors.
pub fn exit_code<E: Display>(result: Result<CommandOutcome, E>) -> ExitCode {
    match result {
        Ok(outcome) => outcome.into(),
        Err(error) => {
            log::error!("{error}");
            CommandOutcome::OperationalError.into()
        }
    }
}

/// A count and the noun it counts, pluralized by the count.
#[must_use]
pub fn plural<N: Copy + Display + From<u8> + PartialEq>(count: N, noun: &str) -> String {
    let suffix = if count == N::from(1) { "" } else { "s" };
    format!("{count} {noun}{suffix}")
}

/// Logging detail: errors only with `--quiet`, warnings by default, and informational, debug,
/// and trace diagnostics with each repetition of `-v`.
///
/// `--quiet` also suppresses the summary a command prints when it succeeds.
#[derive(Debug, clap::Args)]
pub struct Verbosity {
    /// Log errors only.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,

    /// Log informational diagnostics; repeat for debug and trace.
    #[arg(short, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
}

impl Verbosity {
    /// Whether errors alone are logged and normal program output is suppressed.
    #[must_use]
    pub const fn is_quiet(&self) -> bool {
        self.quiet
    }

    /// Whether informational diagnostics or more are logged.
    #[must_use]
    pub const fn is_verbose(&self) -> bool {
        self.verbose > 0
    }

    /// The most detailed level to log.
    #[must_use]
    pub const fn level(&self) -> log::LevelFilter {
        if self.quiet {
            log::LevelFilter::Error
        } else {
            match self.verbose {
                0 => log::LevelFilter::Warn,
                1 => log::LevelFilter::Info,
                2 => log::LevelFilter::Debug,
                _ => log::LevelFilter::Trace,
            }
        }
    }

    /// Start logging to standard error at the selected level.
    ///
    /// # Panics
    ///
    /// Panics if a logger has already been installed in this process.
    pub fn init_logging(&self) {
        let config = simplelog::ConfigBuilder::new()
            .set_time_level(log::LevelFilter::Off)
            .build();

        simplelog::TermLogger::init(
            self.level(),
            config,
            simplelog::TerminalMode::Stderr,
            simplelog::ColorChoice::Auto,
        )
        .expect("invariant violation: the logger is initialized once");
    }
}

#[cfg(test)]
mod tests {
    use std::process::ExitCode;

    use clap::Parser;

    use super::{CommandOutcome, Verbosity};

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(flatten)]
        verbosity: Verbosity,
    }

    fn verbosity(flags: &[&str]) -> Verbosity {
        let mut args = vec!["test"];
        args.extend_from_slice(flags);

        Cli::try_parse_from(args).expect("valid options").verbosity
    }

    #[test]
    fn selects_the_documented_levels() {
        assert_eq!(verbosity(&[]).level(), log::LevelFilter::Warn);
        assert_eq!(verbosity(&["--quiet"]).level(), log::LevelFilter::Error);
        assert_eq!(verbosity(&["-v"]).level(), log::LevelFilter::Info);
        assert_eq!(verbosity(&["-vv"]).level(), log::LevelFilter::Debug);
        assert_eq!(verbosity(&["-vvv"]).level(), log::LevelFilter::Trace);
        assert_eq!(verbosity(&["-vvvv"]).level(), log::LevelFilter::Trace);
    }

    #[test]
    fn reports_quiet_and_verbose_separately() {
        assert!(verbosity(&["-q"]).is_quiet());
        assert!(!verbosity(&["-q"]).is_verbose());
        assert!(!verbosity(&[]).is_quiet());
        assert!(!verbosity(&[]).is_verbose());
        assert!(verbosity(&["-v"]).is_verbose());
    }

    /// The two options describe opposite intents, so asking for both is an error.
    #[test]
    fn quiet_and_verbose_conflict() {
        assert!(Cli::try_parse_from(["test", "-q", "-v"]).is_err());
    }

    #[test]
    fn plural_agrees_with_its_count() {
        assert_eq!(super::plural(0_usize, "record"), "0 records");
        assert_eq!(super::plural(1_usize, "record"), "1 record");
        assert_eq!(super::plural(2_u64, "byte"), "2 bytes");
    }

    #[test]
    fn command_outcomes_have_the_documented_exit_codes() {
        assert_eq!(ExitCode::from(0), CommandOutcome::Success.into());
        assert_eq!(ExitCode::from(1), CommandOutcome::ReportedProblems.into());
        assert_eq!(ExitCode::from(2), CommandOutcome::OperationalError.into());
    }
}
