//! CDXJ models and WACZ-specific stream reading.
//!
//! The data model is provided by [`archivindex_cdx::cdxj`]. This module retains the stream reader
//! because line-oriented I/O and source diagnostics are WACZ processing concerns.

use std::io::BufRead;

use archivindex_cdx::cdxj::Item;

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
    Item(#[from] archivindex_cdx::cdxj::Error),
}

/// The underlying failure for a line read by [`IndexReader`].
#[derive(Debug, thiserror::Error)]
pub enum InvalidLineSource {
    /// The line was read successfully but was not a valid CDXJ item.
    #[error(transparent)]
    Item(#[from] archivindex_cdx::cdxj::Error),
    /// The line-oriented input could not be read.
    #[error("failed to read CDXJ input")]
    Io(#[source] std::io::Error),
}

/// A reader that parses nonblank CDXJ lines from a stream.
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

#[cfg(test)]
mod tests {
    use super::*;

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
