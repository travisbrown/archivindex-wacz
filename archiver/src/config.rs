//! The default configuration of the archiving client.

use std::time::Duration;

use crate::Config;

impl Config {
    /// The default `User-Agent` header value, identifying this crate and its version.
    pub const DEFAULT_USER_AGENT: &str =
        concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
}

impl Default for Config {
    /// The default configuration: this crate's `User-Agent`, a 30-second timeout, at most ten
    /// redirects per URL, one download at a time, unbounded response sizes, and an uncompressed
    /// WARC file.
    fn default() -> Self {
        Self {
            user_agent: Self::DEFAULT_USER_AGENT.to_owned(),
            timeout: Duration::from_secs(30),
            max_redirects: 10,
            gzip_warc: false,
            concurrency: 1,
            max_response_length: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn warc_is_uncompressed_by_default() {
        assert!(!Config::default().gzip_warc);
    }
}
