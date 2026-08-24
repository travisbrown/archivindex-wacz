//! Removal of the session identifiers that the Wayback Machine strips from URLs.
//!
//! These reproduce the regular expressions of the Python `surt` library's `IAURLCanonicalizer`,
//! including their quirks: a stripped query parameter leaves its neighbouring `&` behind, and the
//! patterns match wherever they occur rather than only at parameter boundaries.

use std::borrow::Cow;

/// A matcher returning the length of a session identifier at the start of its input.
type Pattern = fn(&[u8]) -> Option<usize>;

/// Remove session identifiers from a query (without its leading `?`).
///
/// Each pattern is applied once, at its rightmost match, in the order `jsessionid`, `phpsessid`,
/// `sid`, `ASPSESSIONID`, `cfid`/`cftoken`. A match must run to the end of the query or up to an
/// `&`, and that `&` is removed with it.
pub fn strip_query(query: &str) -> Cow<'_, str> {
    const PATTERNS: [Pattern; 5] = [
        |rest| session_value(rest, b"jsessionid=", 32, u8::is_ascii_alphanumeric),
        |rest| session_value(rest, b"phpsessid=", 32, u8::is_ascii_alphanumeric),
        |rest| session_value(rest, b"sid=", 32, u8::is_ascii_alphanumeric),
        asp_session,
        coldfusion_session,
    ];

    let mut query = Cow::Borrowed(query);

    for pattern in PATTERNS {
        if let Some((start, end)) = rightmost_query_match(query.as_bytes(), pattern) {
            let mut stripped = String::with_capacity(query.len());
            stripped.push_str(&query[..start]);
            stripped.push_str(&query[end..]);
            query = Cow::Owned(stripped);
        }
    }

    query
}

/// Find the rightmost position where `pattern` matches and is followed by `&` or the end.
///
/// Returns the byte range to remove, including a following `&`.
fn rightmost_query_match(
    query: &[u8],
    pattern: fn(&[u8]) -> Option<usize>,
) -> Option<(usize, usize)> {
    (0..query.len()).rev().find_map(|start| {
        let end = start + pattern(&query[start..])?;

        match query.get(end) {
            None => Some((start, end)),
            Some(b'&') => Some((start, end + 1)),
            Some(_) => None,
        }
    })
}

/// Match `key` (case-insensitively) followed by exactly `len` bytes accepted by `class`.
fn session_value(rest: &[u8], key: &[u8], len: usize, class: fn(&u8) -> bool) -> Option<usize> {
    let value = rest.get(key.len()..key.len() + len)?;

    (rest[..key.len()].eq_ignore_ascii_case(key) && value.iter().all(class))
        .then_some(key.len() + len)
}

/// Match `ASPSESSIONID` plus eight letters, `=`, and 24 letters.
fn asp_session(rest: &[u8]) -> Option<usize> {
    const KEY: &[u8] = b"aspsessionid";
    let suffix = rest.get(KEY.len()..KEY.len() + 8)?;
    let value = rest.get(KEY.len() + 9..KEY.len() + 33)?;

    (rest[..KEY.len()].eq_ignore_ascii_case(KEY)
        && suffix.iter().all(u8::is_ascii_alphabetic)
        && rest[KEY.len() + 8] == b'='
        && value.iter().all(u8::is_ascii_alphabetic))
    .then_some(KEY.len() + 33)
}

/// Match `cfid=<value>&cftoken=<value>` with non-empty values.
fn coldfusion_session(rest: &[u8]) -> Option<usize> {
    let cfid_len = key_and_value(rest, b"cfid=")?;
    let token_len = key_and_value(&rest[cfid_len..], b"&cftoken=")?;

    Some(cfid_len + token_len)
}

/// Match `key` (case-insensitively) followed by one or more bytes other than `&`.
fn key_and_value(rest: &[u8], key: &[u8]) -> Option<usize> {
    let prefix = rest.get(..key.len())?;
    let value_len = rest[key.len()..]
        .iter()
        .take_while(|&&byte| byte != b'&')
        .count();

    (prefix.eq_ignore_ascii_case(key) && value_len > 0).then_some(key.len() + value_len)
}

/// Remove ASP.NET cookieless session identifiers from a path.
///
/// Strips a `/(S(<24 alphanumerics>))/` or `/(<24 alphanumerics>)/` segment (the first form may
/// carry several parenthesized parts) when the rest of the path leads to an `.aspx` resource.
pub fn strip_path(path: &str) -> Cow<'_, str> {
    let mut path = Cow::Borrowed(path);

    for pattern in [tagged_session_segment, bare_session_segment] {
        if let Some((start, end)) = rightmost_path_match(path.as_bytes(), pattern) {
            let mut stripped = String::with_capacity(path.len());
            stripped.push_str(&path[..start]);
            stripped.push_str(&path[end..]);
            path = Cow::Owned(stripped);
        }
    }

    path
}

/// Find the rightmost segment start (just after a `/`) where `pattern` matches a session segment
/// ending in `/`, with an `.aspx` resource somewhere after it.
fn rightmost_path_match(
    path: &[u8],
    pattern: fn(&[u8]) -> Option<usize>,
) -> Option<(usize, usize)> {
    (1..=path.len())
        .rev()
        .filter(|&start| path[start - 1] == b'/')
        .find_map(|start| {
            let end = start + pattern(&path[start..])?;

            leads_to_aspx(&path[end..]).then_some((start, end))
        })
}

/// Whether the remainder of a path matches `[^?]+\.aspx.*` (case-insensitively).
fn leads_to_aspx(rest: &[u8]) -> bool {
    rest.len() > 5
        && rest[1..]
            .windows(5)
            .any(|window| window.eq_ignore_ascii_case(b".aspx"))
}

/// Match `(` then one or more `<letter>(<24 alphanumerics>)` groups, then `)/`.
fn tagged_session_segment(rest: &[u8]) -> Option<usize> {
    let mut index = usize::from(*rest.first()? == b'(');

    if index == 0 {
        return None;
    }

    let mut groups = 0;

    while let Some(tag) = rest.get(index) {
        if !tag.is_ascii_alphabetic() || rest.get(index + 1) != Some(&b'(') {
            break;
        }

        let value = rest.get(index + 2..index + 26)?;

        if !value.iter().all(u8::is_ascii_alphanumeric) || rest.get(index + 26) != Some(&b')') {
            return None;
        }

        index += 27;
        groups += 1;
    }

    (groups > 0 && rest.get(index..index + 2) == Some(b")/")).then_some(index + 2)
}

/// Match `(<24 alphanumerics>)/`.
fn bare_session_segment(rest: &[u8]) -> Option<usize> {
    let value = rest.get(1..25)?;

    (rest[0] == b'('
        && value.iter().all(u8::is_ascii_alphanumeric)
        && rest.get(25..27) == Some(b")/"))
    .then_some(27)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_32: &str = "0123456789abcdefghijklemopqrstuv";

    #[test]
    fn strips_query_session_ids() {
        assert_eq!(strip_query(&format!("jsessionid={SESSION_32}")), "");
        assert_eq!(
            strip_query(&format!("PHPSESSID={SESSION_32}&action=profile")),
            "action=profile"
        );
        assert_eq!(
            strip_query(&format!("one=two&jsessionid={SESSION_32}")),
            "one=two&"
        );
        assert_eq!(strip_query(&format!("sid={SESSION_32}&a=1")), "a=1");
        assert_eq!(
            strip_query("ASPSESSIONIDAQBSSABC=ABCDEFGHIJKLMNOPQRSTUVWX&x=y"),
            "x=y"
        );
        assert_eq!(strip_query("a=1&cfid=12345&cftoken=abc-987&b=2"), "a=1&b=2");
        assert_eq!(strip_query("cfid=12345&cftoken=abc-987"), "");
        assert_eq!(strip_query("sid=short&a=1"), "sid=short&a=1");
        assert_eq!(
            strip_query(&format!("jsessionid={SESSION_32}x&a=1")),
            format!("jsessionid={SESSION_32}x&a=1")
        );
    }

    #[test]
    fn strips_path_session_ids() {
        assert_eq!(
            strip_path("/(S(4hqa0555fwsecu455xqckv45))/mileg.aspx"),
            "/mileg.aspx"
        );
        assert_eq!(
            strip_path("/(4hqa0555fwsecu455xqckv45)/mileg.aspx"),
            "/mileg.aspx"
        );
        assert_eq!(
            strip_path("/(S(4hqa0555fwsecu455xqckv45)A(abcdefghijklmnopqrstuvwx))/x/mileg.aspx"),
            "/x/mileg.aspx"
        );
        assert_eq!(
            strip_path("/(4hqa0555fwsecu455xqckv45)/mileg.html"),
            "/(4hqa0555fwsecu455xqckv45)/mileg.html"
        );
        assert_eq!(strip_path("/plain/path.aspx"), "/plain/path.aspx");
    }
}
