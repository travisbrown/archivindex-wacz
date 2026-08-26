//! CDXJ models and WACZ-specific stream reading.
//!
//! The data model is provided by [`archivindex_cdx::cdxj`]. This module retains the stream reader
//! because line-oriented I/O and source diagnostics are WACZ processing concerns.

use std::io::BufRead;

use archivindex_cdx::cdxj::{Error as ParseError, Item};

use crate::LineContext;
use crate::lines::Lines;

/// A CDXJ stream cannot be read or contains an invalid line.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A line-oriented stream failed or contained an invalid CDXJ item.
    #[error("invalid CDXJ item at {context}")]
    InvalidLine {
        /// Bounded source context.
        context: LineContext,
        /// Parsing or I/O failure.
        #[source]
        source: InvalidLineSource,
    },
    /// A standalone CDXJ item is invalid.
    #[error(transparent)]
    Item(#[from] ParseError),
}

/// The underlying failure for a line read by [`IndexReader`].
#[derive(Debug, thiserror::Error)]
pub enum InvalidLineSource {
    /// The line was read successfully but was not a valid CDXJ item.
    #[error(transparent)]
    Item(#[from] ParseError),
    /// The line-oriented input could not be read.
    #[error("failed to read CDXJ input")]
    Io(#[source] std::io::Error),
}

/// A reader that parses CDXJ lines from a stream.
///
/// Blank lines are skipped unless the reader is built with [`Self::rejecting_blank_lines`], which
/// enforces the rule that every line of a CDXJ file is a record.
pub struct IndexReader<R> {
    lines: Lines<R>,
}

impl<R: BufRead> IndexReader<R> {
    /// Create a reader whose diagnostics name the input `<stream>`.
    #[must_use]
    pub fn new(reader: R) -> Self {
        Self::with_source(reader, "<stream>")
    }

    /// Create a reader with a source name for diagnostics.
    #[must_use]
    pub fn with_source(reader: R, source: impl Into<String>) -> Self {
        Self {
            lines: Lines::with_source(reader, source),
        }
    }

    /// Report a blank line as an invalid line instead of skipping it.
    #[must_use]
    pub fn rejecting_blank_lines(self) -> Self {
        Self {
            lines: self.lines.rejecting_blank_lines(),
        }
    }
}

impl<R: BufRead> Iterator for IndexReader<R> {
    type Item = Result<Item<'static>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.lines.next_content() {
            Ok(Some((context, line))) => Some(Item::parse(line).map(Item::into_owned).map_err(
                |source| Error::InvalidLine {
                    context,
                    source: InvalidLineSource::Item(source),
                },
            )),
            Ok(None) => None,
            Err(error) => Some(Err(Error::InvalidLine {
                context: error.context,
                source: InvalidLineSource::Io(error.source),
            })),
        }
    }
}

/// Split a CDXJ or `ZipNum` summary line into its two-field prefix and JSON object.
pub(crate) fn split_prefix(line: &str) -> Option<(&str, &str)> {
    let (json, _) = line.match_indices(' ').nth(1)?;
    Some((&line[..json], &line[json + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader skips blank lines by default and reports them when built strictly.
    #[test]
    fn blank_lines_are_skipped_or_reported() {
        let input = "\ncom,example)/ 20201007212236 {\"url\":\"https://example.com/\"}\n";

        let mut items = IndexReader::with_source(input.as_bytes(), "indexes/example.cdx");
        assert!(items.next().expect("one content line").is_ok());
        assert!(items.next().is_none());

        let error = IndexReader::with_source(input.as_bytes(), "indexes/example.cdx")
            .rejecting_blank_lines()
            .next()
            .expect("the blank first line")
            .expect_err("blank lines are not records");
        let Error::InvalidLine { context, source } = error else {
            panic!("unexpected error")
        };
        assert_eq!(context.line, 1);
        assert!(matches!(source, InvalidLineSource::Io(_)));
    }

    #[test]
    fn reports_source_and_line() {
        let input = "\ninvalid\n";
        let error = IndexReader::with_source(input.as_bytes(), "indexes/example.cdxj")
            .next()
            .expect("one content line")
            .expect_err("line is invalid");
        let Error::InvalidLine { context, .. } = error else {
            panic!("unexpected error")
        };
        assert_eq!(context.source, "indexes/example.cdxj");
        assert_eq!(context.line, 2);
    }
}
