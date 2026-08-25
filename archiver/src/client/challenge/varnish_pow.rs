//! Recognition of the Varnish hexadecimal-prefix proof-of-work challenge.
//!
//! The challenge page declares a nonce, an issuance timestamp, and how many leading hexadecimal
//! digits of `SHA-256(nonce || issued_at || candidate)` must equal a given digit. The host expects
//! the winning candidate back in a cookie, alongside the trace cookie it set on the challenge.

use archivindex_warc::recorder::CapturedExchange;
use http::header::HeaderValue;
use sha2::{Digest, Sha256};
use url::Url;

use super::StoredCookie;

const CHALLENGE_STATUS: u16 = 202;
/// The most leading digits solvable in a bounded search: a sixth would need 16 times the work.
const MAX_DIFFICULTY: usize = 5;
const MAX_ATTEMPTS: u64 = 10_000_000;

/// Solve a recognized challenge and return the trace and bypass cookies for the reload.
pub fn challenge_cookie(captured: &CapturedExchange, request_url: &Url) -> Option<StoredCookie> {
    if captured.response_metadata.status != CHALLENGE_STATUS
        || captured
            .response_metadata
            .header("server")
            .is_none_or(|value| !value.eq_ignore_ascii_case(b"Varnish"))
    {
        return None;
    }

    let body = captured.entity_body().ok()?;
    let html = std::str::from_utf8(&body).ok()?;
    let data = html.split_once("window.POW_CHALLENGE_DATA={")?.1;
    let nonce = field(data, "challenge_nonce")?;
    let hmac = field(data, "challenge_hmac")?;
    let difficulty = field(data, "difficulty")?.parse::<usize>().ok()?;
    let difficulty_char = field(data, "difficulty_char")?;
    let issued_at = field(data, "issued_at")?;
    let cookie_domain = field(data, "cookie_domain")?;

    if nonce.len() != 32
        || !is_lower_hex(&nonce)
        || hmac.is_empty()
        || !is_lower_hex(&hmac)
        || difficulty == 0
        || difficulty > MAX_DIFFICULTY
        || difficulty_char.len() != 1
        || !is_lower_hex(&difficulty_char)
        || issued_at.is_empty()
        || !issued_at.bytes().all(|byte| byte.is_ascii_digit())
        || !domain_matches(request_url.host_str()?, &cookie_domain)
    {
        return None;
    }

    let (candidate, digest) = solve(
        &nonce,
        &issued_at,
        difficulty_char.as_bytes()[0],
        difficulty,
    )?;
    let trace = captured.response_metadata.header("set-cookie")?;
    let trace = std::str::from_utf8(trace).ok()?.split(';').next()?.trim();
    let trace_value = trace.strip_prefix("pow_trace=")?;
    let (trace_nonce, trace_issued_at) = trace_value.split_once('|')?;
    // A renewed challenge may retain the previous trace identity while issuing a fresh nonce for
    // the proof-of-work bypass. This is also what the site's browser script does: it accepts the
    // response's `pow_trace` cookie and builds `pow_bypass` from the HTML challenge. Validate both
    // independently, tying them together by their issuance timestamp.
    if trace_nonce.len() != 32
        || !is_lower_hex(trace_nonce)
        || trace_issued_at != issued_at
        || trace_value.matches('|').count() != 1
    {
        return None;
    }

    let cookie = format!("{trace}; pow_bypass={nonce}|{issued_at}|{candidate}|{digest}|{hmac}");
    Some(StoredCookie {
        value: HeaderValue::from_str(&cookie).ok()?,
        secure: request_url.scheme() == "https",
    })
}

/// The value of a quoted field in the challenge's JavaScript object literal.
fn field(data: &str, name: &str) -> Option<String> {
    let value = data.split_once(&format!("{name}:"))?.1.trim_start();
    let quote = *value.as_bytes().first()?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let end = value.as_bytes()[1..]
        .iter()
        .position(|byte| *byte == quote)?
        + 1;
    let value = &value[1..end];
    value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'\\' | b'\'' | b'"'))
        .then(|| value.to_owned())
}

/// Search for a candidate whose digest begins with `difficulty` copies of `difficulty_char`.
///
/// The nonce and timestamp are hashed once into a prefix state that each attempt resumes from,
/// and a candidate's digits are compared against the raw digest, so the search allocates nothing.
fn solve(
    nonce: &str,
    issued_at: &str,
    difficulty_char: u8,
    difficulty: usize,
) -> Option<(u64, String)> {
    // A single hexadecimal digit, so its value fits in a nibble.
    let nibble = u8::try_from(char::from(difficulty_char).to_digit(16)?).ok()?;
    let mut prefix = Sha256::new();
    prefix.update(nonce.as_bytes());
    prefix.update(issued_at.as_bytes());
    let mut buffer = [0; 20];

    (1..=MAX_ATTEMPTS).find_map(|candidate| {
        let mut hasher = prefix.clone();
        hasher.update(super::decimal(candidate, &mut buffer));
        let digest = hasher.finalize();
        starts_with_nibble(&digest, nibble, difficulty).then(|| (candidate, hex(&digest)))
    })
}

/// Whether a digest's first `count` hexadecimal digits all equal `nibble`.
fn starts_with_nibble(digest: &[u8], nibble: u8, count: usize) -> bool {
    (0..count).all(|index| {
        let byte = digest[index / 2];
        let digit = if index % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0f
        };
        digit == nibble
    })
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Whether a host is covered by a cookie domain, which may name the host or a parent of it.
fn domain_matches(host: &str, domain: &str) -> bool {
    let domain = domain.strip_prefix('.').unwrap_or(domain);
    host.eq_ignore_ascii_case(domain)
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::{hex, solve};

    #[test]
    fn solution_has_the_requested_hexadecimal_prefix() {
        let (candidate, digest) = solve("83462578e314e3b20855f1cb32d30a09", "1787485140", b'b', 2)
            .expect("a bounded solution");

        assert!(candidate > 0);
        assert!(digest.starts_with("bb"));
        assert_eq!(digest.len(), 64);
        assert_eq!(hex(&[0, 15, 16, 255]), "000f10ff");
    }
}
