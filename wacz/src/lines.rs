//! Shared line-oriented reading for CDXJ and JSON Lines files.

use std::io::BufRead;

use crate::LineContext;

const EXCERPT_CHAR_LIMIT: usize = 160;

/// An I/O failure annotated with the line being read.
#[derive(Debug, thiserror::Error)]
#[error("failed to read {context}")]
pub struct Error {
    /// Location of the failed read.
    pub context: LineContext,
    /// Underlying I/O error.
    #[source]
    pub source: std::io::Error,
}

/// A line source that trims line endings, skips blank lines, and tracks line numbers.
pub struct Lines<R> {
    underlying: R,
    /// Scratch buffer reused across lines; returned content is only valid until the next call.
    line: String,
    line_number: usize,
    source: String,
    fused: bool,
}

impl<R: BufRead> Lines<R> {
    /// Create a line source carrying a member path or other source name for diagnostics.
    pub fn with_source(underlying: R, source: impl Into<String>) -> Self {
        Self {
            underlying,
            line: String::new(),
            line_number: 0,
            source: source.into(),
            fused: false,
        }
    }

    /// Read the next non-blank line, returning its one-based line number and its content with any
    /// trailing line ending removed.
    ///
    /// Blank lines (such as a trailing newline at the end of a file) are skipped rather than
    /// returned, but still counted; `None` marks the end of the stream.
    pub fn next_content(&mut self) -> Result<Option<(LineContext, &str)>, Error> {
        if self.fused {
            return Ok(None);
        }

        loop {
            self.line.clear();

            let read = self
                .underlying
                .read_line(&mut self.line)
                .map_err(|source| {
                    self.fused = true;
                    Error {
                        context: LineContext {
                            source: self.source.clone(),
                            line: self.line_number + 1,
                            excerpt: None,
                        },
                        source,
                    }
                })?;
            if read == 0 {
                self.fused = true;
                return Ok(None);
            }

            self.line_number += 1;
            let line_text = self.line.trim_end_matches(['\r', '\n']);

            if !line_text.is_empty() {
                let length = line_text.len();
                let location = context(&self.source, self.line_number, line_text);

                return Ok(Some((location, &self.line[..length])));
            }
        }
    }
}

fn context(source: &str, line: usize, content: &str) -> LineContext {
    let mut chars = content.chars();
    let excerpt = chars.by_ref().take(EXCERPT_CHAR_LIMIT).collect::<String>();
    let excerpt = if chars.next().is_some() {
        format!("{excerpt}…")
    } else {
        excerpt
    };
    LineContext {
        source: source.to_owned(),
        line,
        excerpt: Some(excerpt),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, Read};

    use super::*;

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("failed"))
        }
    }

    impl BufRead for FailingReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Err(std::io::Error::other("failed"))
        }

        fn consume(&mut self, _amount: usize) {}
    }

    #[test]
    fn next_content_skips_blanks_and_counts_lines() -> Result<(), Box<dyn std::error::Error>> {
        let mut lines = Lines::with_source(&b"first\r\n\n \nsecond"[..], "test");

        let (location, line_text) = lines.next_content()?.expect("first line");
        assert_eq!((location.line, line_text), (1, "first"));
        // The blank second line is skipped but counted; the third holds a space.
        let (location, line_text) = lines.next_content()?.expect("third line");
        assert_eq!((location.line, line_text), (3, " "));
        let (location, line_text) = lines.next_content()?.expect("fourth line");
        assert_eq!((location.line, line_text), (4, "second"));
        assert_eq!(lines.next_content()?, None);

        Ok(())
    }

    #[test]
    fn an_io_failure_fuses_the_line_source() {
        let mut lines = Lines::with_source(FailingReader, "broken.cdxj");

        let error = lines.next_content().expect_err("the first read fails");
        assert_eq!(error.context.source, "broken.cdxj");
        assert_eq!(error.context.line, 1);
        assert!(lines.next_content().expect("fused source").is_none());
    }
}
