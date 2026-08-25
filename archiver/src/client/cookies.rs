//! Host-scoped cookies retained across requests.

use std::collections::HashMap;
use std::sync::MutexGuard;

use http::header::HeaderValue;
use url::Url;

use crate::Archiver;

/// The cookies a host has been given, either by the caller or by a recognized challenge.
///
/// This is deliberately not a general cookie store. One `Cookie` field value is kept per host,
/// with neither path scoping nor expiry, which is all a clearance cookie needs: hosts issue them
/// for a whole origin and expect them back on every later request.
#[derive(Debug, Default)]
pub struct CookieJar {
    by_host: HashMap<String, StoredCookie>,
}

/// A `Cookie` field value and whether it may be sent only over HTTPS.
#[derive(Clone, Debug)]
pub struct StoredCookie {
    /// The complete field value, as it is sent.
    pub value: HeaderValue,
    /// Whether the cookie was issued with the `Secure` attribute, or supplied for an HTTPS URL.
    pub secure: bool,
}

impl CookieJar {
    /// The cookie to send with a request for `url`, when one is held for its host.
    #[must_use]
    pub fn get(&self, url: &Url) -> Option<HeaderValue> {
        let cookie = self.by_host.get(url.host_str()?)?;
        (!cookie.secure || url.scheme() == "https").then(|| cookie.value.clone())
    }

    /// Retain a cookie for the host of `url`, replacing any cookie already held for that host.
    pub fn insert(&mut self, url: &Url, cookie: StoredCookie) {
        if let Some(host) = url.host_str() {
            self.by_host.insert(host.to_owned(), cookie);
        }
    }

    /// Retain a supplied value, restricted to HTTPS exactly when `url` is itself served over it.
    pub fn insert_header(&mut self, url: &Url, value: HeaderValue) {
        self.insert(
            url,
            StoredCookie {
                value,
                secure: url.scheme() == "https",
            },
        );
    }
}

impl Archiver {
    /// Borrow the cookie jar shared by this archiver's clones and capture threads.
    ///
    /// A panic while the jar is held cannot leave it inconsistent, since every operation on it
    /// completes before the guard is released, so a poisoned lock is taken as it stands.
    pub(crate) fn cookie_jar(&self) -> MutexGuard<'_, CookieJar> {
        self.cookies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
