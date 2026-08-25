//! Recognition of the interstitial challenges some hosts serve in place of a response.
//!
//! A challenge page is not the representation the crawl asked for, and archiving it instead of the
//! page wastes the capture. Each recognizer here identifies one narrowly specified challenge and
//! derives the cookie its host expects from data the response states outright, never by executing
//! the script that accompanies it: an unrecognized page is captured as the response it is.
//!
//! Every exchange a challenge causes is recorded like any other, so the WARC file documents how
//! the eventual response was obtained.

use archivindex_warc::recorder::CapturedExchange;
use http::header::{CONTENT_TYPE, COOKIE, HeaderValue};
use url::Url;

use super::cookies::StoredCookie;
use super::outcome::Exchange;
use crate::{Archiver, Error};

mod simply;
mod sucuri;
mod varnish_pow;

/// A recognized challenge, and what answering it requires.
pub enum Challenge {
    /// The cookie the host expects, which it stated in the challenge page.
    Cookie(StoredCookie),
    /// A proof of work to submit, which the host answers with a clearance cookie.
    ProofOfWork(simply::Challenge),
}

/// Recognize the challenge a response carries, when it is one this crate reads.
pub fn recognize(captured: &CapturedExchange, url: &Url) -> Option<Challenge> {
    sucuri::challenge_cookie(captured)
        .or_else(|| varnish_pow::challenge_cookie(captured, url))
        .map(Challenge::Cookie)
        .or_else(|| simply::challenge(captured, url).map(Challenge::ProofOfWork))
}

impl Archiver {
    /// Answer a recognized challenge, retaining the cookie its host expects.
    ///
    /// Any exchange needed to obtain that cookie is appended to `exchanges`. Returns whether the
    /// request that met the challenge can now be repeated; it cannot when the host declined to
    /// issue clearance, in which case the challenge response stands as the capture.
    pub(crate) fn answer(
        &self,
        url: &Url,
        challenge: Challenge,
        exchanges: &mut Vec<Exchange>,
    ) -> Result<bool, Error> {
        let cookie = match challenge {
            Challenge::Cookie(cookie) => Some(cookie),
            Challenge::ProofOfWork(challenge) => {
                let (exchange, cookie) = self.submit_proof_of_work(&challenge)?;
                exchanges.push(exchange);
                cookie
            }
        };

        Ok(cookie.is_some_and(|cookie| {
            self.cookie_jar().insert(url, cookie);
            true
        }))
    }

    /// Submit a solved proof of work and retain the complete exchange.
    fn submit_proof_of_work(
        &self,
        challenge: &simply::Challenge,
    ) -> Result<(Exchange, Option<StoredCookie>), Error> {
        let verification_url = challenge.verification_url();
        let target = verification_url
            .as_str()
            .parse::<http::Uri>()
            .map_err(|source| Error::InvalidUri {
                url: verification_url.to_string(),
                source,
            })?;
        let mut headers = self.headers.clone();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        let cookie = self.cookie_jar().get(verification_url);
        if let Some(cookie) = cookie {
            headers.insert(COOKIE, cookie);
        }
        let body = challenge.request_body();
        let captured = self.recorder.fetch(
            &http::Method::POST,
            &target,
            &headers,
            Some(body.as_bytes()),
        )?;
        let cookie = simply::clearance_cookie(&captured, verification_url);

        Ok((Exchange::new(captured, None), cookie))
    }
}

/// Write a decimal integer into a reused buffer, which is wide enough for any `u64`.
fn decimal(value: u64, buffer: &mut [u8; 20]) -> &[u8] {
    let mut index = buffer.len();
    let mut remaining = value;

    loop {
        index -= 1;
        buffer[index] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            return &buffer[index..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::decimal;

    #[test]
    fn decimals_are_rendered_without_allocating() {
        let mut buffer = [0; 20];

        assert_eq!(decimal(0, &mut buffer), b"0");
        assert_eq!(decimal(1, &mut buffer), b"1");
        assert_eq!(decimal(u64::MAX, &mut buffer), b"18446744073709551615");
    }
}
