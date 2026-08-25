//! Recognition of Simply.com's proof-of-work browser challenge.
//!
//! The challenge page declares a token and how many leading zero bits `SHA-256(token:nonce)` must
//! have. The winning nonce is posted back to a verification endpoint, which answers with the
//! clearance cookie the host then expects.

use archivindex_warc::recorder::CapturedExchange;
use http::header::HeaderValue;
use sha2::{Digest, Sha256};
use url::Url;

use super::StoredCookie;

const CHALLENGE_STATUS: u16 = 454;
/// The most leading zero bits solvable in a bounded search: each further bit doubles the work.
const MAX_DIFFICULTY: u32 = 20;
const MAX_ATTEMPTS: u64 = 10_000_000;
/// The only path a challenge may direct its proof of work to.
const VERIFICATION_PATH: &str = "/.sc-verify/";

/// A recognized and solved challenge, ready to submit.
pub struct Challenge {
    token: String,
    timestamp: String,
    nonce: u64,
    verification_url: Url,
}

impl Challenge {
    /// Where the proof of work is submitted.
    pub const fn verification_url(&self) -> &Url {
        &self.verification_url
    }

    /// The form-encoded proof of work.
    pub fn request_body(&self) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("ts", &self.timestamp)
            .append_pair("nonce", &self.nonce.to_string())
            .append_pair("token", &self.token)
            .finish()
    }
}

/// Recognize and solve a bounded Simply.com challenge without executing its JavaScript.
pub fn challenge(captured: &CapturedExchange, request_url: &Url) -> Option<Challenge> {
    // Simply-hosted sites may put another reverse proxy (notably Cloudflare) in front of the
    // challenge, so the response's `Server` field is not a reliable identifier. The distinctive
    // status and the validated challenge protocol below provide the useful recognition signals.
    if captured.response_metadata.status != CHALLENGE_STATUS {
        return None;
    }

    let body = captured.entity_body().ok()?;
    let html = std::str::from_utf8(&body).ok()?;
    let declaration = html.split_once("var T=")?.1;
    let (token, declaration) = parse_js_string(declaration)?;
    let declaration = declaration.strip_prefix(",TS=")?;
    let (timestamp, declaration) = parse_js_string(declaration)?;
    let difficulty = declaration
        .strip_prefix(",D=")?
        .split_once(';')?
        .0
        .parse::<u32>()
        .ok()?;
    if token.len() != 64
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
        || timestamp.is_empty()
        || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
        || difficulty > MAX_DIFFICULTY
    {
        return None;
    }

    let verification_path = html.split_once("x.open(\"POST\",")?.1;
    let (verification_path, _) = parse_js_string(verification_path)?;
    let verification_url = request_url.join(&verification_path).ok()?;
    if verification_url.origin() != request_url.origin()
        || verification_url.path() != VERIFICATION_PATH
        || verification_url.query().is_some()
        || verification_url.fragment().is_some()
        || !verification_url.username().is_empty()
        || verification_url.password().is_some()
    {
        return None;
    }

    Some(Challenge {
        nonce: solve(&token, difficulty)?,
        token,
        timestamp,
        verification_url,
    })
}

/// Read the clearance cookie from a successful verification response.
pub fn clearance_cookie(
    captured: &CapturedExchange,
    verification_url: &Url,
) -> Option<StoredCookie> {
    if captured.response_metadata.status != 200 {
        return None;
    }

    let response =
        serde_json::from_slice::<VerificationResponse>(&captured.entity_body().ok()?).ok()?;
    if !response.ok
        || response.cookie.is_empty()
        || !response
            .cookie
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b';' | b',' | b' ' | b'\t'))
    {
        return None;
    }

    Some(StoredCookie {
        value: HeaderValue::from_str(&format!("sc_clearance={}", response.cookie)).ok()?,
        secure: verification_url.scheme() == "https",
    })
}

/// Search for a nonce whose digest has at least `difficulty` leading zero bits.
///
/// The token is hashed once into a prefix state that each attempt resumes from, so the search
/// allocates nothing.
fn solve(token: &str, difficulty: u32) -> Option<u64> {
    let mut prefix = Sha256::new();
    prefix.update(token.as_bytes());
    prefix.update(b":");
    let mut buffer = [0; 20];

    (0..MAX_ATTEMPTS).find(|nonce| {
        let mut hasher = prefix.clone();
        hasher.update(super::decimal(*nonce, &mut buffer));
        leading_zero_bits(&hasher.finalize()) >= difficulty
    })
}

fn leading_zero_bits(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .map(|byte| byte.leading_zeros())
        .take_while(|bits| *bits == 8)
        .sum::<u32>()
        + bytes
            .iter()
            .find(|byte| **byte != 0)
            .map_or(0, |byte| byte.leading_zeros())
}

/// Parse the restricted double-quoted string literals used by the challenge's generator.
fn parse_js_string(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start();
    let quote = *input.as_bytes().first()?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let end = input.as_bytes()[1..]
        .iter()
        .position(|byte| *byte == quote)?
        + 1;
    let value = &input[1..end];
    value
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b'\\')
        .then(|| (value.to_owned(), &input[end + 1..]))
}

#[derive(serde::Deserialize)]
struct VerificationResponse {
    ok: bool,
    cookie: String,
}

#[cfg(test)]
mod tests {
    use sha2::Digest as _;

    use super::{leading_zero_bits, solve};

    #[test]
    fn proof_of_work_has_the_requested_leading_zero_bits() {
        let token = "021c7f24e8c1ed8c4472a22aa9b441b223a08cb15fa889293574500e190960dc";
        let nonce = solve(token, 12).expect("a bounded solution");
        let digest = sha2::Sha256::digest(format!("{token}:{nonce}"));

        assert!(leading_zero_bits(&digest) >= 12);
    }
}
