#![cfg_attr(docsrs, feature(doc_cfg))]
//! Command-line behavior shared by this repository's applications.
//!
//! Each of them takes the same verbosity options, logs to standard error at the same levels,
//! exits with the same status for the same kind of outcome, and counts what it worked on the same
//! way.

use std::fmt::Display;

/// The exit code of a command that did what it was asked.
pub const SUCCESS: u8 = 0;

/// The exit code of a command that finished with problems to report about its input.
pub const REPORTED_PROBLEMS: u8 = 1;

/// The exit code of a command that could not do what it was asked.
pub const OPERATIONAL_ERROR: u8 = 2;

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
    use clap::Parser;

    use super::Verbosity;

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
}
