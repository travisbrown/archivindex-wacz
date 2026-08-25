//! Shared line-oriented reading for CDXJ and JSON Lines files.

use std::io::{self, BufRead, Read};

use crate::LineContext;

const EXCERPT_CHAR_LIMIT: usize = 160;

/// The longest line accepted from a member, excluding its line ending.
///
/// Page entries can carry extracted full text, so the bound is generous; it exists so that a
/// hostile member cannot make the reader buffer an unbounded line.
pub const MAX_LINE_BYTES: usize = 16 << 20;

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
    line: Vec<u8>,
    line_number: usize,
    source: String,
    fused: bool,
}

impl<R: BufRead> Lines<R> {
    /// Create a line source carrying a member path or other source name for diagnostics.
    pub fn with_source(underlying: R, source: impl Into<String>) -> Self {
        Self {
            underlying,
            line: Vec::new(),
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
    ///
    /// # Errors
    ///
    /// Returns an error, and yields nothing further, when the underlying read fails, when a line
    /// is longer than [`MAX_LINE_BYTES`], or when a line is not valid UTF-8.
    pub fn next_content(&mut self) -> Result<Option<(LineContext, &str)>, Error> {
        if self.fused {
            return Ok(None);
        }

        loop {
            self.line.clear();

            // Reading one byte past the limit distinguishes an over-long line from one that is
            // exactly the limit; the stray byte is never returned.
            let read = Read::by_ref(&mut self.underlying)
                .take(MAX_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut self.line)
                .map_err(|source| self.fail(self.line_number + 1, source))?;
            if read == 0 {
                self.fused = true;
                return Ok(None);
            }

            self.line_number += 1;
            let trimmed = self.line.len()
                - self
                    .line
                    .iter()
                    .rev()
                    .take_while(|byte| matches!(byte, b'\r' | b'\n'))
                    .count();

            if trimmed > MAX_LINE_BYTES {
                let source = io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line exceeds {MAX_LINE_BYTES} bytes"),
                );
                return Err(self.fail(self.line_number, source));
            }

            if trimmed > 0 {
                // The arms touch disjoint fields, so the returned borrow of `line` may coexist
                // with fusing the source; a `&mut self` helper could not be called here.
                let line_text = match std::str::from_utf8(&self.line[..trimmed]) {
                    Ok(line_text) => line_text,
                    Err(error) => {
                        self.fused = true;
                        let source = io::Error::new(io::ErrorKind::InvalidData, error);
                        return Err(line_error(&self.source, self.line_number, source));
                    }
                };
                let location = context(&self.source, self.line_number, line_text);

                return Ok(Some((location, line_text)));
            }
        }
    }

    /// Fuse the source and describe a failure on line `line`.
    fn fail(&mut self, line: usize, source: io::Error) -> Error {
        self.fused = true;
        line_error(&self.source, line, source)
    }
}

/// Describe an I/O failure on line `line`, without an excerpt.
fn line_error(source_name: &str, line: usize, source: io::Error) -> Error {
    Error {
        context: LineContext {
            source: source_name.to_owned(),
            line,
            excerpt: None,
        },
        source,
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

    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

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
    fn over_long_and_invalid_lines_are_rejected() {
        let mut input = vec![b'a'; MAX_LINE_BYTES];
        input.extend_from_slice(b"\r\n");
        input.extend_from_slice(&vec![b'b'; MAX_LINE_BYTES + 1]);
        input.push(b'\n');
        let mut lines = Lines::with_source(&input[..], "long.jsonl");

        let (location, line_text) = lines
            .next_content()
            .expect("a line at the limit")
            .expect("l");
        assert_eq!((location.line, line_text.len()), (1, MAX_LINE_BYTES));
        // The first line's `\n` was left behind the limit and counts as a blank second line.
        let error = lines.next_content().expect_err("one byte over the limit");
        assert_eq!(
            (error.context.line, error.source.kind()),
            (3, io::ErrorKind::InvalidData)
        );
        assert!(lines.next_content().expect("fused source").is_none());

        let mut lines = Lines::with_source(&b"\xff\n"[..], "bad.jsonl");
        let error = lines.next_content().expect_err("invalid UTF-8");
        assert_eq!(error.source.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_io_failure_fuses_the_line_source() {
        let mut lines = Lines::with_source(FailingReader, "broken.cdxj");

        let error = lines.next_content().expect_err("the first read fails");
        assert_eq!(error.context.source, "broken.cdxj");
        assert_eq!(error.context.line, 1);
        assert!(lines.next_content().expect("fused source").is_none());
    }

    /// Every non-blank line is returned once, in order, under its own line number, and each
    /// carries an excerpt bounded by a character count rather than a byte count.
    #[test_strategy::proptest]
    fn content_lines_are_returned_with_their_numbers(
        #[strategy(strategies::lines())] input: (Vec<(String, &'static str)>, bool),
    ) {
        let (lines, ends_with_a_line_ending) = input;
        let mut text = String::new();
        for (index, (content, ending)) in lines.iter().enumerate() {
            text.push_str(content);
            if ends_with_a_line_ending || index + 1 < lines.len() {
                text.push_str(ending);
            }
        }

        let mut source = Lines::with_source(text.as_bytes(), "test.jsonl");
        let mut read = Vec::new();
        while let Some((context, content)) = source.next_content().unwrap() {
            let excerpt = context.excerpt.clone().expect("content has an excerpt");
            // The generated alphabet has no ellipsis, so only truncation can add one.
            if let Some(prefix) = excerpt.strip_suffix('\u{2026}') {
                prop_assert_eq!(prefix.chars().count(), EXCERPT_CHAR_LIMIT);
                prop_assert!(content.starts_with(prefix));
                prop_assert!(content.chars().count() > EXCERPT_CHAR_LIMIT);
            } else {
                prop_assert_eq!(&excerpt, content);
                prop_assert!(excerpt.chars().count() <= EXCERPT_CHAR_LIMIT);
            }
            prop_assert_eq!(&context.source, "test.jsonl");
            read.push((context.line, content.to_owned()));
        }

        let expected = lines
            .into_iter()
            .enumerate()
            .filter(|(_, (content, _))| !content.is_empty())
            .map(|(index, (content, _))| (index + 1, content))
            .collect::<Vec<_>>();

        prop_assert_eq!(read, expected);
    }
}
