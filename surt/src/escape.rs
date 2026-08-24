//! Percent-encoding, path, and host normalization shared by the canonicalization rules.
//!
//! The escaping rules follow the Google Safe Browsing canonicalization that the Python `surt`
//! library applies before its Wayback rules: escapes are decoded until nothing changes, then only
//! the bytes that cannot stand on their own are re-encoded.

use std::borrow::Cow;
use std::fmt::Write;
use std::net::Ipv4Addr;

/// Decode `%XX` escapes until the text no longer changes.
///
/// The result is raw bytes, since escapes can encode text that is not UTF-8.
pub fn unescape_repeatedly(text: &[u8]) -> Vec<u8> {
    let mut current = text.to_vec();

    while let Some(next) = unescape_once(&current) {
        current = next;
    }

    current
}

/// Decode one layer of `%XX` escapes, returning `None` if there was nothing to decode.
fn unescape_once(text: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(text.len());
    let mut decoded = false;
    let mut index = 0;

    while index < text.len() {
        match (text.get(index + 1), text.get(index + 2)) {
            (Some(&high), Some(&low)) if text[index] == b'%' => {
                if let (Some(high), Some(low)) = (hex_value(high), hex_value(low)) {
                    output.push(high << 4 | low);
                    decoded = true;
                    index += 3;
                    continue;
                }
            }
            _ => {}
        }

        output.push(text[index]);
        index += 1;
    }

    decoded.then_some(output)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Whether a byte has to be percent-encoded: controls, space, `#`, `%`, and everything non-ASCII.
const fn needs_escape(byte: u8) -> bool {
    byte <= b' ' || byte == b'#' || byte == b'%' || byte >= 0x7f
}

/// Percent-encode the bytes that cannot appear literally, with lowercase hex digits.
///
/// Existing `%` characters are encoded too, so this is only meaningful after
/// [`unescape_repeatedly`].
pub fn escape_once(text: &[u8]) -> String {
    let mut output = String::with_capacity(text.len());
    escape_once_into(&mut output, text);
    output
}

/// Append the [`escape_once`] encoding of `text` to `output`.
pub fn escape_once_into(output: &mut String, text: &[u8]) {
    for &byte in text {
        if needs_escape(byte) {
            // Writing to a `String` cannot fail, so the `fmt::Result` carries no information.
            let _ = write!(output, "%{byte:02x}");
        } else {
            output.push(char::from(byte));
        }
    }
}

/// Decode escapes and re-encode once, when `enabled`.
pub fn normalize_escapes(text: &str, enabled: bool) -> Cow<'_, str> {
    if enabled && text.bytes().any(|byte| byte == b'%' || needs_escape(byte)) {
        Cow::Owned(escape_once(&unescape_repeatedly(text.as_bytes())))
    } else {
        Cow::Borrowed(text)
    }
}

/// Resolve `.` and `..` segments and, optionally, collapse runs of slashes.
///
/// Dot segments are resolved as in RFC 3986 (a trailing `.` or `..` leaves a trailing slash),
/// except that a `..` with nothing above it is kept, as the Python `surt` library keeps it: `/../a`
/// stays as it is, while `/../../a` is `/a` because the second `..` removes the first. An empty
/// path becomes `/`. When `collapse_slashes` is set, empty segments other than the one
/// that forms a trailing slash are dropped, so `/a//b/` becomes `/a/b/`.
pub fn normalize_path(path: &str, collapse_slashes: bool) -> Cow<'_, str> {
    // `/.` covers `/./`, `/../`, and trailing `/.` and `/..` (and, harmlessly, `/.hidden`).
    if !(path.is_empty() || path.contains("/.") || (collapse_slashes && path.contains("//"))) {
        return Cow::Borrowed(path);
    }

    let mut segments = Vec::new();
    // The path is empty or starts with `/`, so the first split element is always empty.
    let mut iter = path.split('/').skip(1).peekable();

    while let Some(segment) = iter.next() {
        let last = iter.peek().is_none();

        match segment {
            "." => {
                if last {
                    segments.push("");
                }
            }
            ".." => {
                if segments.pop().is_none() {
                    segments.push("..");
                }

                if last {
                    segments.push("");
                }
            }
            "" if collapse_slashes && !last => {}
            _ => segments.push(segment),
        }
    }

    let mut output = String::with_capacity(path.len().max(1));

    for segment in segments {
        output.push('/');
        output.push_str(segment);
    }

    if output.is_empty() {
        output.push('/');
    }

    Cow::Owned(output)
}

/// Lowercase text, avoiding an allocation when it is already lowercase.
pub fn lowercase(text: &str) -> Cow<'_, str> {
    if text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(text.to_lowercase())
    } else if text.is_ascii() {
        Cow::Borrowed(text)
    } else {
        // Non-ASCII text may lowercase to something else even without ASCII uppercase.
        let lowered = text.to_lowercase();

        if lowered == text {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(lowered)
        }
    }
}

/// Interpret the numeric host forms that resolve to an IPv4 address.
///
/// A host that is one decimal number is the address's 32-bit value, and dotted forms with one to
/// four decimal or octal parts follow `inet_aton`, so `10.0.258` is `10.0.1.2` and `017.0.0.1`
/// is `15.0.0.1`. Ordinary dotted-quad addresses are returned unchanged.
pub fn numeric_ipv4(host: &str) -> Option<Ipv4Addr> {
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }

    if !host.contains('.') {
        // Python's `int(host) & 0xffffffff`: fold the digits modulo 2^32.
        let value = host.bytes().fold(0u32, |acc, digit| {
            acc.wrapping_mul(10).wrapping_add(u32::from(digit - b'0'))
        });

        return Some(Ipv4Addr::from(value));
    }

    let parts: Vec<u32> = host.split('.').map(inet_aton_part).collect::<Option<_>>()?;
    let (&last, leading) = parts.split_last()?;

    if parts.len() > 4 || leading.iter().any(|&part| part > 0xff) {
        return None;
    }

    let mut value: u32 = 0;

    for &part in leading {
        value = value << 8 | part;
    }

    // `leading` has at most three parts, so the shift is at most 24 bits.
    let remaining_bits = 8 * u32::try_from(4 - leading.len()).ok()?;
    (u64::from(last) < 1 << remaining_bits).then(|| Ipv4Addr::from(value << remaining_bits | last))
}

/// Parse one dotted part as `inet_aton` does: a leading zero means octal.
fn inet_aton_part(part: &str) -> Option<u32> {
    match part.strip_prefix('0') {
        Some(rest) if !rest.is_empty() => u32::from_str_radix(rest, 8).ok(),
        Some(_) => Some(0),
        None if part.is_empty() => None,
        None => part.parse().ok(),
    }
}

/// Encode a non-ASCII host label as an IDNA A-label (`xn--` plus RFC 3492 Punycode).
///
/// Returns `None` when the label is not a domain name label (it contains a control, space, or
/// other character that would need escaping) or cannot be represented (an arithmetic overflow,
/// which only pathological labels reach).
// The single-letter names are RFC 3492's, kept so the encoder can be checked against it.
#[allow(clippy::many_single_char_names)]
pub fn idna_label(label: &str) -> Option<String> {
    const BASE: u32 = 36;
    const T_MIN: u32 = 1;
    const T_MAX: u32 = 26;
    const INITIAL_BIAS: u32 = 72;
    const INITIAL_N: u32 = 128;

    if label
        .bytes()
        .any(|byte| byte.is_ascii() && needs_escape(byte))
    {
        return None;
    }

    let input: Vec<u32> = label.chars().map(u32::from).collect();
    let mut output = String::from("xn--");
    output.extend(label.chars().filter(char::is_ascii));

    let basic_len = u32::try_from(output.len() - 4).ok()?;

    if basic_len > 0 {
        output.push('-');
    }

    let mut n = INITIAL_N;
    let mut delta = 0u32;
    let mut bias = INITIAL_BIAS;
    let mut handled = basic_len;
    let total = u32::try_from(input.len()).ok()?;

    while handled < total {
        let m = input.iter().copied().filter(|&c| c >= n).min()?;
        delta = delta.checked_add((m - n).checked_mul(handled + 1)?)?;
        n = m;

        for &c in &input {
            if c < n {
                delta = delta.checked_add(1)?;
            }

            if c == n {
                let mut q = delta;
                let mut k = BASE;

                loop {
                    let t = k.saturating_sub(bias).clamp(T_MIN, T_MAX);

                    if q < t {
                        break;
                    }

                    output.push(punycode_digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }

                output.push(punycode_digit(q));
                bias = punycode_adapt(delta, handled + 1, handled == basic_len);
                delta = 0;
                handled += 1;
            }
        }

        delta += 1;
        n += 1;
    }

    Some(output)
}

fn punycode_digit(value: u32) -> char {
    // `value` is always below 36 by construction, so the narrowing cannot fail.
    let value = u8::try_from(value).expect("Punycode digits are below 36");

    char::from(if value < 26 {
        b'a' + value
    } else {
        b'0' + value - 26
    })
}

const fn punycode_adapt(delta: u32, num_points: u32, first_time: bool) -> u32 {
    const BASE: u32 = 36;
    const T_MIN: u32 = 1;
    const T_MAX: u32 = 26;
    const SKEW: u32 = 38;
    const DAMP: u32 = 700;

    let mut delta = if first_time { delta / DAMP } else { delta / 2 };
    delta += delta / num_points;
    let mut k = 0;

    while delta > ((BASE - T_MIN) * T_MAX) / 2 {
        delta /= BASE - T_MIN;
        k += BASE;
    }

    k + (BASE - T_MIN + 1) * delta / (delta + SKEW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_until_fixed_point() {
        assert_eq!(unescape_repeatedly(b"/%25%32%35"), b"/%");
        assert_eq!(unescape_repeatedly(b"a%2520b"), b"a b");
        assert_eq!(unescape_repeatedly(b"a%zzb%2"), b"a%zzb%2");
    }

    #[test]
    fn escapes_only_what_it_must() {
        assert_eq!(
            escape_once(b"!\"$&'()*+,-./:;<=>?@[\\]^_`{|}~aZ09"),
            "!\"$&'()*+,-./:;<=>?@[\\]^_`{|}~aZ09"
        );
        assert_eq!(escape_once(b" #%\x7f\x01\xc5\x82"), "%20%23%25%7f%01%c5%82");
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_path("", true), "/");
        assert_eq!(normalize_path("/", true), "/");
        assert_eq!(normalize_path("/blah/..", true), "/");
        assert_eq!(normalize_path("/a/b/../c/./d", true), "/a/c/d");
        assert_eq!(normalize_path("/a//b/", true), "/a/b/");
        assert_eq!(normalize_path("/a//b/", false), "/a//b/");
        assert_eq!(normalize_path("/a/b/..", false), "/a/");
        assert_eq!(normalize_path("/../a", true), "/../a");
        assert_eq!(normalize_path("/../../a", true), "/a");
        assert_eq!(normalize_path("/..", true), "/../");
        assert_eq!(normalize_path("/a/.", true), "/a/");
    }

    #[test]
    fn parses_numeric_ipv4_forms() {
        assert_eq!(
            numeric_ipv4("3279880203"),
            Some(Ipv4Addr::new(195, 127, 0, 11))
        );
        assert_eq!(numeric_ipv4("10.0.258"), Some(Ipv4Addr::new(10, 0, 1, 2)));
        assert_eq!(numeric_ipv4("017.0.0.1"), Some(Ipv4Addr::new(15, 0, 0, 1)));
        assert_eq!(
            numeric_ipv4("168.188.99.26"),
            Some(Ipv4Addr::new(168, 188, 99, 26))
        );
        assert_eq!(numeric_ipv4("1.2.3.256"), None);
        assert_eq!(numeric_ipv4("990.991.992.993"), None);
        assert_eq!(numeric_ipv4("1.2.3.08"), None);
        assert_eq!(numeric_ipv4("1.2.3.4.5"), None);
        assert_eq!(numeric_ipv4("www.example.com"), None);
        assert_eq!(numeric_ipv4(""), None);
    }

    #[test]
    fn encodes_idna_labels() {
        assert_eq!(idna_label("bücher").as_deref(), Some("xn--bcher-kva"));
        assert_eq!(idna_label("☃").as_deref(), Some("xn--n3h"));
        assert_eq!(idna_label("münchen").as_deref(), Some("xn--mnchen-3ya"));
    }

    #[test]
    fn lowercases_without_copying_lowercase_text() {
        assert!(matches!(lowercase("already"), Cow::Borrowed(_)));
        assert_eq!(lowercase("MiXed"), "mixed");
        assert_eq!(lowercase("Ü"), "ü");
    }
}
