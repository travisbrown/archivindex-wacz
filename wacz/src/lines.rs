//! Shared line-oriented reading for CDXJ and JSON Lines files.

use std::io::BufRead;

/// A line source that trims line endings, skips blank lines, and tracks line numbers.
pub struct Lines<R> {
    underlying: R,
    /// Scratch buffer reused across lines; returned content is only valid until the next call.
    line: String,
    line_number: usize,
}

impl<R: BufRead> Lines<R> {
    /// Create a new line source.
    pub const fn new(underlying: R) -> Self {
        Self {
            underlying,
            line: String::new(),
            line_number: 0,
        }
    }

    /// Read the next non-blank line, returning its one-based line number and its content with any
    /// trailing line ending removed.
    ///
    /// Blank lines (such as a trailing newline at the end of a file) are skipped rather than
    /// returned, but still counted; `None` marks the end of the stream.
    pub fn next_content(&mut self) -> std::io::Result<Option<(usize, &str)>> {
        loop {
            self.line.clear();

            if self.underlying.read_line(&mut self.line)? == 0 {
                return Ok(None);
            }

            self.line_number += 1;
            let content = self.line.trim_end_matches(['\r', '\n']);

            if !content.is_empty() {
                let length = content.len();

                return Ok(Some((self.line_number, &self.line[..length])));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_content_skips_blanks_and_counts_lines() -> Result<(), std::io::Error> {
        let mut lines = Lines::new(&b"first\r\n\n \nsecond"[..]);

        assert_eq!(lines.next_content()?, Some((1, "first")));
        // The blank second line is skipped but counted; the third holds a space.
        assert_eq!(lines.next_content()?, Some((3, " ")));
        assert_eq!(lines.next_content()?, Some((4, "second")));
        assert_eq!(lines.next_content()?, None);

        Ok(())
    }
}
