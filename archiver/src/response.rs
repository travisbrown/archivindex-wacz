//! Reading response fields out of a recorded HTTP message.
//!
//! The recorder stores responses verbatim, so the status code and redirect target used by the
//! archiver are read back from the raw bytes here. Only the header section is examined, and it is
//! always complete: the recorder fails a fetch that ends before the header terminator, truncating
//! only bodies.

/// The recorded response fields used for payload extraction and redirect handling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Head {
    /// The status code from the final status line.
    pub status: u16,
    /// The value of the first `Location` header holding readable text, if any.
    pub location: Option<String>,
    /// The offset at which the message body begins.
    pub body_offset: usize,
}

/// Read the status line, redirect target, and body offset of a recorded response.
///
/// The first `Location` header wins, matching the selection an HTTP client's header map makes.
/// Obsolete line folding is ignored: a folded continuation extends the previous value, and
/// `Location` does not use folding in practice.
///
/// Returns `None` when the message has no HTTP status line or no header terminator; the recorder
/// rejects both before storing a response, so a recorded message always reads back.
pub fn head(message: &[u8]) -> Option<Head> {
    let status_end = find_crlf(message, 0)?;
    let status = status_code(&message[..status_end])?;

    let mut location = None;
    let mut start = status_end + 2;

    loop {
        let end = find_crlf(message, start)?;
        let line = &message[start..end];
        start = end + 2;

        if line.is_empty() {
            break;
        }

        // A line opening with white space is an obs-fold continuation, not a new field.
        if line[0] == b' ' || line[0] == b'\t' {
            continue;
        }

        let Some((name, value)) = split_field(line) else {
            continue;
        };

        if location.is_none() && name.eq_ignore_ascii_case(b"location") {
            location = std::str::from_utf8(value).ok().map(str::to_owned);
        }
    }

    Some(Head {
        status,
        location,
        body_offset: start,
    })
}

/// Return the first readable value of a response header, matched case-insensitively.
pub fn header<'a>(message: &'a [u8], wanted: &str) -> Option<&'a [u8]> {
    let status_end = find_crlf(message, 0)?;
    status_code(&message[..status_end])?;
    let mut start = status_end + 2;

    loop {
        let end = find_crlf(message, start)?;
        let line = &message[start..end];
        start = end + 2;

        if line.is_empty() {
            return None;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            continue;
        }

        let Some((name, value)) = split_field(line) else {
            continue;
        };
        if name.eq_ignore_ascii_case(wanted.as_bytes()) {
            return Some(value);
        }
    }
}

/// The position of the next CRLF at or after `from`, when one exists.
fn find_crlf(message: &[u8], from: usize) -> Option<usize> {
    message
        .get(from..)?
        .windows(2)
        .position(|window| window == b"\r\n")
        .map(|position| from + position)
}

/// The status code of an HTTP status line (RFC 9112: version, space, three digits, space, reason).
fn status_code(line: &[u8]) -> Option<u16> {
    let mut parts = line.splitn(3, |&byte| byte == b' ');
    let version = parts.next()?;
    let code = parts.next()?;

    (version.starts_with(b"HTTP/") && code.len() == 3 && code.iter().all(u8::is_ascii_digit)).then(
        || {
            code.iter()
                .fold(0, |value, &byte| value * 10 + u16::from(byte - b'0'))
        },
    )
}

/// Split a header line at its first colon, trimming white space around the value.
fn split_field(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&byte| byte == b':')?;

    Some((&line[..colon], line[colon + 1..].trim_ascii()))
}

#[cfg(test)]
mod tests {
    use super::{head, header};

    #[test]
    fn head_reads_the_response_fields() {
        let message = b"HTTP/1.1 302 Found\r\n\
                        Content-Type: text/html; charset=utf-8\r\n\
                        LOCATION: /target\r\n\
                        \r\n\
                        body";

        let head = head(message).expect("a readable head");

        assert_eq!(head.status, 302);
        assert_eq!(head.location.as_deref(), Some("/target"));
        assert_eq!(&message[head.body_offset..], b"body");
    }

    #[test]
    fn head_takes_the_first_location_header() {
        let message = b"HTTP/1.1 200 OK\r\n\
                        location: /first\r\n\
                        location: /second\r\n\
                        \r\n";

        let head = head(message).expect("a readable head");

        assert_eq!(head.location.as_deref(), Some("/first"));
    }

    #[test]
    fn head_survives_a_bare_reason_and_missing_headers() {
        let head = head(b"HTTP/1.1 520 \r\n\r\n").expect("a readable head");

        assert_eq!(head.status, 520);
        assert_eq!(head.location, None);
        assert_eq!(head.body_offset, "HTTP/1.1 520 \r\n\r\n".len());
    }

    #[test]
    fn head_ignores_folded_continuation_lines() {
        let message = b"HTTP/1.1 200 OK\r\n\
                        x-note: one\r\n\
                        \tcontinued: not a header\r\n\
                        location: /target\r\n\
                        \r\n";

        assert_eq!(
            head(message).expect("a readable head").location.as_deref(),
            Some("/target")
        );
    }

    #[test]
    fn head_rejects_a_malformed_message() {
        assert_eq!(head(b"not http at all"), None);
        assert_eq!(head(b"HTTP/1.1 20 OK\r\n\r\n"), None);
        assert_eq!(head(b"HTTP/1.1 200 OK\r\nunterminated: yes\r\n"), None);
    }

    #[test]
    fn header_reads_a_named_field_case_insensitively() {
        let message = b"HTTP/1.1 200 OK\r\nX-WP-TotalPages: 17\r\nother: value\r\n\r\n";

        assert_eq!(header(message, "x-wp-totalpages"), Some(&b"17"[..]));
        assert_eq!(header(message, "missing"), None);
    }
}
