//! Capture outcomes, lifecycle events, and cancellation.
//!
//! One-shot archiving and crawl sessions both report their work in these types.

use chrono::{DateTime, Utc};

use crate::Error;

/// The outcome of an archiving run.
#[derive(Debug, Default)]
pub struct ArchiveSummary {
    /// URLs archived successfully, in request order.
    pub captures: Vec<CaptureSummary>,
    /// URLs that could not be captured.
    pub failures: Vec<Failure>,
    /// Whether an event sink requested a clean stop before all input was dispatched.
    pub cancelled: bool,
}

impl ArchiveSummary {
    /// Whether every URL was captured.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failures.is_empty() && !self.cancelled
    }
}

/// The outcome of capturing one URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureSummary {
    /// The requested URL.
    pub url: String,
    /// When capture of the final response began.
    pub date: DateTime<Utc>,
    /// The final response status.
    pub status: u16,
    /// The decoded entity-body length.
    pub size: u64,
    /// The number of redirects followed.
    pub redirects: usize,
}

/// A URL that could not be captured.
#[derive(Debug)]
pub struct Failure {
    /// The requested URL.
    pub url: String,
    /// The capture failure.
    pub error: Error,
}

/// A live capture lifecycle notification.
#[derive(Clone, Copy, Debug)]
pub enum CaptureEvent<'a> {
    /// A URL capture attempt is starting.
    Started {
        /// Requested URL.
        url: &'a str,
        /// One-based attempt number.
        attempt: usize,
    },
    /// A transient failure will be retried after a delay.
    Retrying {
        /// Requested URL.
        url: &'a str,
        /// One-based number of the upcoming attempt.
        attempt: usize,
        /// Delay before that attempt.
        delay: std::time::Duration,
    },
    /// A URL produced a final HTTP response.
    Captured {
        /// Requested URL.
        url: &'a str,
        /// Final HTTP status.
        status: u16,
    },
    /// A URL could not be captured.
    Failed {
        /// Requested URL.
        url: &'a str,
        /// Final capture error.
        error: &'a Error,
    },
    /// The URL's records were written to the pending collection.
    Written {
        /// Requested URL.
        url: &'a str,
    },
}

/// Decision returned by a [`CaptureEventSink`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureControl {
    /// Continue capturing.
    Continue,
    /// Stop dispatching work and finalize what has already completed.
    Cancel,
}

/// An observer that reports progress or requests clean cancellation.
pub trait CaptureEventSink {
    /// Observe one event and decide whether capture should continue.
    fn event(&mut self, event: CaptureEvent<'_>) -> CaptureControl;

    /// Report that a URL capture attempt is starting, returning whether the sink asked to stop.
    fn started(&mut self, url: &str, attempt: usize) -> bool {
        self.event(CaptureEvent::Started { url, attempt }) == CaptureControl::Cancel
    }
}

impl<F> CaptureEventSink for F
where
    F: for<'a> FnMut(CaptureEvent<'a>) -> CaptureControl,
{
    fn event(&mut self, event: CaptureEvent<'_>) -> CaptureControl {
        self(event)
    }
}
