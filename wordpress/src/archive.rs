//! Archiving a site's supported `WordPress` REST API v2 collections one endpoint at a time.
//!
//! [`ArchiveDriver`] drives an `archivindex-archiver` session. A run captures the API's root
//! resources, probes every supported [`Endpoint`] with a bare request, and then pages each exposed
//! collection twice, in [`Endpoint::ALL`] order. The second pass detects records shifted onto
//! earlier pages by concurrent deletions. Its [`Checkpoint`] names the page a stopped run is
//! continued from.

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;

use archivindex_archiver::Error;
use archivindex_archiver::session::{Capture, Driver, Inspection, Request};
use chrono::{DateTime, Utc};
use url::Url;

use crate::endpoint::{Endpoint, ROOT_ENDPOINTS};

/// A `WordPress` installation named by its host and optional path, such as `example.com/blog`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Site {
    base: String,
    root: Url,
}

/// The reason a base does not name a [`Site`].
#[derive(Debug, thiserror::Error)]
pub enum SiteError {
    /// The base is not a host with an optional path.
    #[error("site base {0:?} is not a host with an optional path: {1}")]
    Url(String, #[source] url::ParseError),
    /// The base carries a query or fragment, which endpoint paths cannot be appended to.
    #[error("site base {0:?} must not have a query or fragment")]
    QueryOrFragment(String),
}

impl Site {
    /// Name a site by a host with an optional path, without a scheme.
    ///
    /// A trailing slash is removed. Requests use HTTPS; a base beginning with `http://` is
    /// accepted for a site without TLS, and is retained in [`base`](Self::base).
    ///
    /// # Errors
    ///
    /// Returns [`SiteError`] when the base is not a host with an optional path.
    pub fn parse(base: &str) -> Result<Self, SiteError> {
        let (scheme, location) = base.strip_prefix("http://").map_or_else(
            || ("https", base.strip_prefix("https://").unwrap_or(base)),
            |location| ("http", location),
        );
        let location = location.trim_end_matches('/');
        let root = Url::parse(&format!("{scheme}://{location}/"))
            .map_err(|source| SiteError::Url(base.to_owned(), source))?;
        if root.query().is_some() || root.fragment().is_some() {
            return Err(SiteError::QueryOrFragment(base.to_owned()));
        }
        let base = if scheme == "http" {
            format!("http://{location}")
        } else {
            location.to_owned()
        };

        Ok(Self { base, root })
    }

    /// The base without its trailing slash, and without a scheme unless it is `http://`.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The installation root, ending in a slash, that API paths are appended to.
    #[must_use]
    pub const fn root(&self) -> &Url {
        &self.root
    }

    /// The name of a session started at `at`: the base and the epoch second, joined by a hyphen.
    ///
    /// A session identifier permits only URI-unreserved characters, so every other character of
    /// the base, including the slashes between path segments, becomes a hyphen.
    #[must_use]
    pub fn session_name(&self, at: DateTime<Utc>) -> String {
        let location = self.base.strip_prefix("http://").unwrap_or(&self.base);
        let name = location
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_' | '~') {
                    character
                } else {
                    '-'
                }
            })
            .collect::<String>();

        format!("{name}-{}", at.timestamp())
    }

    /// A resource's URL from its path relative to the installation root.
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.root)
    }

    /// The bare URL of an endpoint's collection.
    fn endpoint_url(&self, endpoint: Endpoint) -> String {
        self.url(&format!("wp-json/wp/v2/{endpoint}"))
    }

    /// The URL of one page of an endpoint's collection, in ascending ID order up to `before`.
    fn page_url(&self, endpoint: Endpoint, before: DateTime<Utc>, page: usize) -> String {
        format!(
            "{}?{}",
            self.endpoint_url(endpoint),
            crate::paging_query(None, before, page)
        )
    }
}

impl FromStr for Site {
    type Err = SiteError;

    fn from_str(base: &str) -> Result<Self, Self::Err> {
        Self::parse(base)
    }
}

/// Where a stopped run is continued from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Checkpoint {
    /// The root resources and endpoint probes that begin an archive are not finished, and are
    /// repeated by a new archive rather than continued.
    Initial,
    /// A run continues `endpoint` after `last_page`, which is zero when the endpoint is yet to be
    /// probed.
    Resume {
        /// The endpoint to continue.
        endpoint: Endpoint,
        /// The last page of that endpoint captured so far.
        last_page: usize,
        /// The most recently observed page count, if one was available.
        total_pages: Option<usize>,
    },
    /// Every supported, exposed collection completed both paging passes.
    Finished,
}

/// Archive a site's supported collections one endpoint at a time through a session.
///
/// An archive begins with the API root resources and a bare probe of every [`Endpoint`], all
/// requested as seeds; a resumed run begins with the page after its checkpoint, requested via the
/// page before it, and probes only the endpoints still to come. A probe answered with success is
/// paged from page one through the greatest `X-WP-TotalPages` value seen, all pages carrying the
/// run's `before` cutoff; any other answer, such as a 404 for a collection the site lacks, skips
/// the endpoint. Exposed collections are paged one at a time after the last probe, a collection's
/// first page requested via its probe and every later page via the page before it. After reaching
/// a collection's end, the driver re-reads it from page one, via the last page read, with a
/// stable advertised page count; this validation pass captures records shifted earlier by
/// deletions during the first pass.
///
/// An unexpected page response ends the session with an error, and a failed capture ends the
/// driver's requests; [`checkpoint`](Self::checkpoint) then names the page to continue from.
pub struct ArchiveDriver {
    site: Site,
    before: DateTime<Utc>,
    /// Whether the run began with the root resources and every probe, which cannot be resumed.
    initial: bool,
    /// Index into [`ROOT_ENDPOINTS`] of the next root resource to request.
    next_root: usize,
    /// Index into [`Endpoint::ALL`] of the next endpoint to probe.
    next_probe: usize,
    probed: Vec<(Endpoint, u16)>,
    /// Endpoints whose probe succeeded, awaiting paging in order.
    pending: VecDeque<Series>,
    current: Option<Series>,
    /// Page count carried by a page-zero resume until its probe is inspected.
    resume_total_pages: Option<(Endpoint, usize)>,
    /// Whether a capture failed, after which nothing more is requested.
    stopped: bool,
}

/// Progress through one exposed collection.
struct Series {
    endpoint: Endpoint,
    /// The last page captured, or zero before the first.
    page: usize,
    /// The greatest page count advertised so far.
    total_pages: Option<usize>,
    /// The page count established during the validation pass.
    validation_total_pages: Option<usize>,
    phase: SeriesPhase,
}

/// What a collection page's response means for the series.
enum PageOutcome {
    /// A further page follows.
    Next,
    /// Re-read the collection from its first page to catch shifted records.
    Validate,
    /// The page was the collection's last.
    Last,
}

enum SeriesPhase {
    Primary,
    /// A second pass from page one, whose first page is requested via primary page `after`.
    Validation {
        after: usize,
    },
}

impl Series {
    const fn new(endpoint: Endpoint, total_pages: Option<usize>) -> Self {
        Self {
            endpoint,
            page: 0,
            total_pages,
            validation_total_pages: None,
            phase: SeriesPhase::Primary,
        }
    }

    const fn resume(endpoint: Endpoint, page: usize, total_pages: Option<usize>) -> Self {
        Self {
            endpoint,
            page,
            total_pages,
            validation_total_pages: None,
            phase: SeriesPhase::Primary,
        }
    }

    const fn begin_validation(&mut self, after: usize) {
        self.page = 0;
        self.validation_total_pages = None;
        self.phase = SeriesPhase::Validation { after };
    }

    const fn is_primary(&self) -> bool {
        matches!(self.phase, SeriesPhase::Primary)
    }

    const fn checkpoint(&self) -> Checkpoint {
        Checkpoint::Resume {
            endpoint: self.endpoint,
            // A validation pass is deliberately replayed as a fresh primary pass after a stop.
            last_page: if self.is_primary() { self.page } else { 0 },
            total_pages: self.total_pages,
        }
    }

    /// Record the response to the page after the last one captured.
    fn record(&mut self, capture: &Capture<'_>) -> Result<PageOutcome, String> {
        let page = self.page + 1;
        // A page can disappear between requests when deletions reduce the page count, which some
        // WordPress endpoints report with this posts-controller error code.
        if capture.status == 400 && page > 1 && crate::is_invalid_page_error(capture.payload) {
            return if self.is_primary() {
                Ok(PageOutcome::Validate)
            } else {
                Err(format!(
                    "{} page {page} disappeared during its validation pass",
                    self.endpoint
                ))
            };
        }
        if !matches!(capture.status, 200 | 304) {
            return Err(format!(
                "unexpected WordPress response status {} on {} page {page}",
                capture.status, self.endpoint
            ));
        }
        if let Some(advertised) = capture
            .header("x-wp-totalpages")
            .and_then(|value| value.parse::<usize>().ok())
        {
            if self.is_primary() {
                self.total_pages = Some(
                    self.total_pages
                        .map_or(advertised, |known| known.max(advertised)),
                );
            } else {
                if self
                    .validation_total_pages
                    .is_some_and(|known| known != advertised)
                {
                    return Err(format!(
                        "X-WP-TotalPages changed during the {} validation pass",
                        self.endpoint
                    ));
                }
                self.validation_total_pages = Some(advertised);
                self.total_pages = Some(advertised);
            }
        }
        let Some(total_pages) = self.total_pages else {
            return Err(format!(
                "missing or invalid X-WP-TotalPages on {} page {page}",
                self.endpoint
            ));
        };
        self.page = page;

        Ok(if page < total_pages {
            PageOutcome::Next
        } else if self.is_primary() {
            PageOutcome::Validate
        } else {
            PageOutcome::Last
        })
    }
}

impl ArchiveDriver {
    /// Begin an archive of `site` with the root resources and every probe.
    ///
    /// Every page requested carries `before` as its cutoff, so pass the time the archive started.
    #[must_use]
    pub const fn new(site: Site, before: DateTime<Utc>) -> Self {
        Self {
            site,
            before,
            initial: true,
            next_root: 0,
            next_probe: 0,
            probed: Vec::new(),
            pending: VecDeque::new(),
            current: None,
            resume_total_pages: None,
            stopped: false,
        }
    }

    /// Continue an archive of `site` with the same `before` cutoff from a checkpoint.
    ///
    /// With `last_page` above zero the run begins with `endpoint`'s next page and probes the
    /// endpoints after it; with zero it begins by probing `endpoint` itself.
    #[must_use]
    pub const fn resume(
        site: Site,
        before: DateTime<Utc>,
        endpoint: Endpoint,
        last_page: usize,
        total_pages: Option<usize>,
    ) -> Self {
        let mut driver = Self::new(site, before);
        driver.initial = false;
        driver.next_root = ROOT_ENDPOINTS.len();
        if last_page == 0 {
            driver.next_probe = endpoint as usize;
            driver.resume_total_pages = match total_pages {
                Some(total_pages) => Some((endpoint, total_pages)),
                None => None,
            };
        } else {
            driver.next_probe = endpoint as usize + 1;
            driver.current = Some(Series::resume(endpoint, last_page, total_pages));
        }

        driver
    }

    /// Where a run stopped now would be continued from.
    #[must_use]
    pub fn checkpoint(&self) -> Checkpoint {
        if let Some(series) = &self.current {
            return series.checkpoint();
        }
        let next_probe = Endpoint::ALL.get(self.next_probe).copied();
        if self.initial && next_probe.is_some() {
            return Checkpoint::Initial;
        }

        // Endpoints already found exposed are paged only after the remaining probes, so a resumed
        // run must probe them again to reach them.
        self.pending
            .front()
            .map(Series::checkpoint)
            .or_else(|| {
                next_probe.map(|endpoint| Checkpoint::Resume {
                    endpoint,
                    last_page: 0,
                    total_pages: self
                        .resume_total_pages
                        .filter(|(resume_endpoint, _)| *resume_endpoint == endpoint)
                        .map(|(_, total_pages)| total_pages),
                })
            })
            .unwrap_or(Checkpoint::Finished)
    }

    /// Every endpoint probed so far with the status of its bare response, in order.
    #[must_use]
    pub fn probed(&self) -> &[(Endpoint, u16)] {
        &self.probed
    }

    /// The request for a series' next page, via the page its position follows from.
    fn page_request(&self, series: &Series) -> Request {
        let via = match (&series.phase, series.page) {
            (SeriesPhase::Primary, 0) => self.site.endpoint_url(series.endpoint),
            (SeriesPhase::Validation { after }, 0) => {
                self.site.page_url(series.endpoint, self.before, *after)
            }
            (_, page) => self.site.page_url(series.endpoint, self.before, page),
        };

        Request::extra(
            self.site
                .page_url(series.endpoint, self.before, series.page + 1),
            via,
        )
    }

    /// Record a probe's answer and, after the last probe, begin paging the exposed collections.
    fn inspect_probe(&mut self, endpoint: Endpoint, status: u16) {
        self.next_probe += 1;
        self.probed.push((endpoint, status));
        // A bare probe uses WordPress's default page size, not the 100-item page size below, so
        // only a count carried from an earlier paged response is applicable here.
        let total_pages = self
            .resume_total_pages
            .take_if(|(resume_endpoint, _)| *resume_endpoint == endpoint)
            .map(|(_, total_pages)| total_pages);
        if (200..300).contains(&status) || status == 304 {
            self.pending.push_back(Series::new(endpoint, total_pages));
        }
        if self.next_probe == Endpoint::ALL.len() {
            self.current = self.pending.pop_front();
        }
    }

    /// Record a collection page and move the series past it.
    fn inspect_page(&mut self, capture: &Capture<'_>) -> Inspection {
        let series = self
            .current
            .as_mut()
            .expect("a page is inspected only while a series is current");
        let requested = series.page + 1;
        match series.record(capture) {
            Ok(outcome) => {
                let title = matches!(capture.status, 200 | 304).then(|| {
                    format!(
                        "{} {} page {} of {}",
                        self.site.base,
                        series.endpoint,
                        series.page,
                        series.total_pages.unwrap_or(series.page)
                    )
                });
                match outcome {
                    PageOutcome::Next => {}
                    PageOutcome::Validate => series.begin_validation(requested),
                    PageOutcome::Last => self.current = self.pending.pop_front(),
                }
                Inspection { title, error: None }
            }
            Err(message) => Inspection::error(message),
        }
    }
}

impl Driver for ArchiveDriver {
    fn next(&mut self) -> Option<Request> {
        if self.stopped {
            return None;
        }
        if let Some(series) = &self.current {
            return Some(self.page_request(series));
        }
        if let Some(root) = ROOT_ENDPOINTS.get(self.next_root) {
            return Some(Request::seed(self.site.url(root)));
        }

        Endpoint::ALL
            .get(self.next_probe)
            .map(|&endpoint| Request::seed(self.site.endpoint_url(endpoint)))
    }

    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        if crate::is_cloudflare_challenge(capture) {
            return Inspection::error(crate::CLOUDFLARE_CHALLENGE);
        }

        if let Some(series) = &self.current
            && capture.url
                == self
                    .site
                    .page_url(series.endpoint, self.before, series.page + 1)
        {
            return self.inspect_page(capture);
        }

        if let Some(root) = ROOT_ENDPOINTS.get(self.next_root)
            && capture.url == self.site.url(root)
        {
            self.next_root += 1;
            return Inspection::default();
        }

        if let Some(&endpoint) = Endpoint::ALL.get(self.next_probe)
            && capture.url == self.site.endpoint_url(endpoint)
        {
            self.inspect_probe(endpoint, capture.status);
            return Inspection::default();
        }

        Inspection::error(format!("unexpected capture of {}", capture.url))
    }

    /// Every later request depends on the failed one, so the run stops at its checkpoint.
    fn failed(&mut self, _url: &str, _error: &Error) {
        self.stopped = true;
    }
}

impl fmt::Display for ArchiveDriver {
    /// The run's position: the last page captured, the endpoint probed next, or its end.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: ", self.site.base)?;
        if let Some(series) = &self.current {
            write!(formatter, "{} page {}", series.endpoint, series.page)?;
            if let Some(total_pages) = series.total_pages {
                write!(formatter, " of {total_pages}")?;
            }
            Ok(())
        } else if let Some(endpoint) = Endpoint::ALL.get(self.next_probe) {
            write!(formatter, "probing {endpoint}")
        } else {
            formatter.write_str("finished")
        }
    }
}

#[cfg(test)]
mod tests {
    use archivindex_archiver::Error;
    use archivindex_archiver::session::{Capture, Driver, Inspection, Request};
    use chrono::{DateTime, Utc};

    use super::{ArchiveDriver, Checkpoint, Site};
    use crate::endpoint::{Endpoint, ROOT_ENDPOINTS};

    const BEFORE: &str = "2026-08-20T00:00:00Z";
    const OK: &[u8] = b"HTTP/1.1 200 OK\r\n\r\n";
    const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\n\r\n";
    const FORBIDDEN: &[u8] = b"HTTP/1.1 403 Forbidden\r\n\r\n";
    const ONE_PAGE: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 3\r\nX-WP-TotalPages: 1\r\n\r\n";
    const TWO_PAGES: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 101\r\nX-WP-TotalPages: 2\r\n\r\n";
    const THREE_PAGES: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 201\r\nX-WP-TotalPages: 3\r\n\r\n";
    const EIGHT_PAGES: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-TotalPages: 8\r\n\r\n";
    const BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\r\n";
    const NOT_MODIFIED: &[u8] = b"HTTP/1.1 304 Not Modified\r\n\r\n";
    const INVALID_PAGE_ERROR: &[u8] =
        br#"{"code": "rest_post_invalid_page_number", "message": "", "data": {"status": 400}}"#;

    fn before() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(BEFORE)
            .map(|date| date.with_timezone(&Utc))
            .expect("a test timestamp")
    }

    fn site() -> Site {
        Site::parse("example.com/blog").expect("a site")
    }

    fn endpoint_url(endpoint: Endpoint) -> String {
        format!("https://example.com/blog/wp-json/wp/v2/{endpoint}")
    }

    fn page_url(endpoint: Endpoint, page: usize) -> String {
        format!(
            "{}?before={BEFORE}&orderby=id&order=asc&page={page}&per_page=100",
            endpoint_url(endpoint)
        )
    }

    /// The request for `page` of an endpoint, via the page before it or `via` for page one.
    fn page_request(endpoint: Endpoint, page: usize, via: &str) -> Request {
        let via = if page > 1 {
            page_url(endpoint, page - 1)
        } else {
            via.to_owned()
        };

        Request::extra(page_url(endpoint, page), via)
    }

    fn inspect(driver: &mut ArchiveDriver, url: &str, response: &[u8]) -> Inspection {
        inspect_payload(driver, url, b"[]", response)
    }

    fn inspect_payload(
        driver: &mut ArchiveDriver,
        url: &str,
        payload: &[u8],
        response: &[u8],
    ) -> Inspection {
        let capture = Capture::new(url, url, payload, response).expect("a complete response");

        driver.inspect(&capture)
    }

    /// Request and answer every root resource, checking that each is requested as a seed.
    fn capture_roots(driver: &mut ArchiveDriver) {
        for root in ROOT_ENDPOINTS {
            let url = format!("https://example.com/blog/{root}");
            assert_eq!(driver.next(), Some(Request::seed(&url)));
            assert_eq!(inspect(driver, &url, OK), Inspection::default());
        }
    }

    /// Answer every probe with `responses` in endpoint order, checking that each is a seed.
    fn probe_all(driver: &mut ArchiveDriver, responses: [&[u8]; 7]) {
        for (endpoint, response) in Endpoint::ALL.into_iter().zip(responses) {
            let url = endpoint_url(endpoint);
            assert_eq!(driver.next(), Some(Request::seed(&url)));
            assert_eq!(inspect(driver, &url, response), Inspection::default());
        }
    }

    #[test]
    fn a_site_is_a_host_with_an_optional_path() {
        let site = Site::parse("thefederalist.com/en/").expect("a site");
        assert_eq!(site.base(), "thefederalist.com/en");
        assert_eq!(site.root().as_str(), "https://thefederalist.com/en/");
        assert_eq!(
            site.session_name(before()),
            "thefederalist.com-en-1787184000"
        );

        let bare = Site::parse("thefederalist.com").expect("a site");
        assert_eq!(bare.root().as_str(), "https://thefederalist.com/");
        assert_eq!(bare.session_name(before()), "thefederalist.com-1787184000");

        let insecure = Site::parse("http://127.0.0.1:8080/").expect("a site");
        assert_eq!(insecure.base(), "http://127.0.0.1:8080");
        assert_eq!(insecure.root().as_str(), "http://127.0.0.1:8080/");
        assert_eq!(insecure.session_name(before()), "127.0.0.1-8080-1787184000");

        assert_eq!(
            Site::parse("https://example.com/").expect("a site"),
            Site::parse("example.com").expect("a site")
        );
        assert!(Site::parse("").is_err());
        assert!(Site::parse("example.com/?page=1").is_err());
        assert!(Site::parse("example.com/#top").is_err());
    }

    #[test]
    fn an_archive_requests_the_roots_and_every_probe_as_seeds() {
        let mut driver = ArchiveDriver::new(site(), before());

        assert_eq!(
            driver.next(),
            Some(Request::seed("https://example.com/blog/wp-json"))
        );
        assert_eq!(driver.checkpoint(), Checkpoint::Initial);
        assert_eq!(driver.to_string(), "example.com/blog: probing pages");

        // Each request is repeated until its capture is inspected.
        assert_eq!(driver.next(), driver.next());
        capture_roots(&mut driver);
        assert_eq!(driver.checkpoint(), Checkpoint::Initial);
        assert_eq!(
            driver.next(),
            Some(Request::seed(endpoint_url(Endpoint::Pages)))
        );
        probe_all(&mut driver, [NOT_FOUND; 7]);
        assert_eq!(driver.next(), None);
    }

    #[test]
    fn exposed_collections_are_paged_in_order_after_the_last_probe() {
        let mut driver = ArchiveDriver::new(site(), before());
        capture_roots(&mut driver);

        probe_all(
            &mut driver,
            [OK, NOT_FOUND, OK, FORBIDDEN, NOT_FOUND, OK, NOT_FOUND],
        );

        let pages_probe = endpoint_url(Endpoint::Pages);
        assert_eq!(
            driver.next(),
            Some(page_request(Endpoint::Pages, 1, &pages_probe))
        );
        assert_eq!(
            driver.probed(),
            [
                (Endpoint::Pages, 200),
                (Endpoint::Posts, 404),
                (Endpoint::Categories, 200),
                (Endpoint::Tags, 403),
                (Endpoint::Users, 404),
                (Endpoint::Comments, 200),
                (Endpoint::Media, 404)
            ]
        );
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Pages,
                last_page: 0,
                total_pages: None,
            }
        );

        let first = inspect(&mut driver, &page_url(Endpoint::Pages, 1), TWO_PAGES);
        assert_eq!(
            first.title.as_deref(),
            Some("example.com/blog pages page 1 of 2")
        );
        assert_eq!(driver.next(), Some(page_request(Endpoint::Pages, 2, "")));
        assert_eq!(driver.to_string(), "example.com/blog: pages page 1 of 2");

        // The greatest advertised page count decides where the collection ends.
        let _ = inspect(&mut driver, &page_url(Endpoint::Pages, 2), THREE_PAGES);
        assert_eq!(driver.next(), Some(page_request(Endpoint::Pages, 3, "")));
        let third = inspect(&mut driver, &page_url(Endpoint::Pages, 3), TWO_PAGES);
        assert_eq!(
            third.title.as_deref(),
            Some("example.com/blog pages page 3 of 3")
        );
        // The validation pass begins via the last page read, which prompted it.
        assert_eq!(
            driver.next(),
            Some(page_request(
                Endpoint::Pages,
                1,
                &page_url(Endpoint::Pages, 3)
            ))
        );
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Pages,
                last_page: 0,
                total_pages: Some(3),
            }
        );

        for page in 1..=3 {
            let _ = inspect(&mut driver, &page_url(Endpoint::Pages, page), THREE_PAGES);
            if page < 3 {
                assert_eq!(
                    driver.next(),
                    Some(page_request(Endpoint::Pages, page + 1, ""))
                );
            }
        }

        // A one-page collection is validated via that page, then the next collection begins.
        for endpoint in [Endpoint::Categories, Endpoint::Comments] {
            assert_eq!(
                driver.next(),
                Some(page_request(endpoint, 1, &endpoint_url(endpoint)))
            );
            let _ = inspect(&mut driver, &page_url(endpoint, 1), ONE_PAGE);
            assert_eq!(
                driver.next(),
                Some(page_request(endpoint, 1, &page_url(endpoint, 1)))
            );
            let validation = inspect(&mut driver, &page_url(endpoint, 1), ONE_PAGE);
            assert_eq!(validation.error, None);
        }
        assert_eq!(driver.next(), None);
        assert_eq!(driver.checkpoint(), Checkpoint::Finished);
        assert_eq!(driver.to_string(), "example.com/blog: finished");
    }

    #[test]
    fn no_exposed_collection_finishes_an_archive() {
        let mut driver = ArchiveDriver::new(site(), before());
        capture_roots(&mut driver);

        probe_all(&mut driver, [NOT_FOUND; 7]);

        assert_eq!(driver.next(), None);
        assert_eq!(driver.checkpoint(), Checkpoint::Finished);
    }

    #[test]
    fn a_resumed_run_continues_the_endpoint_and_probes_the_rest() {
        let mut driver = ArchiveDriver::resume(site(), before(), Endpoint::Comments, 7, Some(8));

        assert_eq!(driver.next(), Some(page_request(Endpoint::Comments, 8, "")));
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Comments,
                last_page: 7,
                total_pages: Some(8),
            }
        );

        let _ = inspect(&mut driver, &page_url(Endpoint::Comments, 8), EIGHT_PAGES);
        assert_eq!(
            driver.next(),
            Some(page_request(
                Endpoint::Comments,
                1,
                &page_url(Endpoint::Comments, 8)
            ))
        );
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Comments,
                last_page: 0,
                total_pages: Some(8),
            }
        );
        for page in 1..=8 {
            let _ = inspect(
                &mut driver,
                &page_url(Endpoint::Comments, page),
                EIGHT_PAGES,
            );
        }

        // An endpoint found exposed is paged only after the remaining probes, so a run stopped
        // during those probes resumes by probing it again.
        let media_probe = endpoint_url(Endpoint::Media);
        assert_eq!(driver.next(), Some(Request::seed(&media_probe)));
        let _ = inspect(&mut driver, &media_probe, OK);
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Media,
                last_page: 0,
                total_pages: None,
            }
        );
        assert_eq!(
            driver.next(),
            Some(page_request(Endpoint::Media, 1, &media_probe))
        );
    }

    #[test]
    fn a_resumed_run_at_page_zero_probes_the_endpoint_itself() {
        let mut driver = ArchiveDriver::resume(site(), before(), Endpoint::Media, 0, None);

        assert_eq!(
            driver.next(),
            Some(Request::seed(endpoint_url(Endpoint::Media)))
        );
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Media,
                last_page: 0,
                total_pages: None,
            }
        );
    }

    #[test]
    fn a_carried_page_count_makes_not_modified_responses_resumable() {
        let mut driver = ArchiveDriver::resume(site(), before(), Endpoint::Media, 0, Some(2));
        let media_probe = endpoint_url(Endpoint::Media);

        let _ = inspect(&mut driver, &media_probe, NOT_MODIFIED);
        assert_eq!(
            driver.next(),
            Some(page_request(Endpoint::Media, 1, &media_probe))
        );
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Media,
                last_page: 0,
                total_pages: Some(2),
            }
        );

        let _ = inspect(&mut driver, &page_url(Endpoint::Media, 1), NOT_MODIFIED);
        assert_eq!(driver.next(), Some(page_request(Endpoint::Media, 2, "")));
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Media,
                last_page: 1,
                total_pages: Some(2),
            }
        );
    }

    #[test]
    fn a_page_count_change_during_validation_stops_the_run() {
        let mut driver = ArchiveDriver::resume(site(), before(), Endpoint::Posts, 1, Some(2));
        let _ = inspect(&mut driver, &page_url(Endpoint::Posts, 2), TWO_PAGES);
        let _ = inspect(&mut driver, &page_url(Endpoint::Posts, 1), TWO_PAGES);

        let changed = inspect(&mut driver, &page_url(Endpoint::Posts, 2), ONE_PAGE);

        assert_eq!(
            changed.error.as_deref(),
            Some("X-WP-TotalPages changed during the posts validation pass")
        );
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Posts,
                last_page: 0,
                total_pages: Some(2),
            }
        );
    }

    #[test]
    fn a_vanished_page_restarts_the_collection_for_validation() {
        let mut driver = ArchiveDriver::resume(site(), before(), Endpoint::Posts, 4, None);

        let gone = inspect_payload(
            &mut driver,
            &page_url(Endpoint::Posts, 5),
            INVALID_PAGE_ERROR,
            BAD_REQUEST,
        );

        assert_eq!(gone.title, None);
        assert_eq!(
            driver.next(),
            Some(page_request(
                Endpoint::Posts,
                1,
                &page_url(Endpoint::Posts, 5)
            ))
        );
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Posts,
                last_page: 0,
                total_pages: None,
            }
        );
        let validation = inspect(&mut driver, &page_url(Endpoint::Posts, 1), ONE_PAGE);
        assert_eq!(validation.error, None);
        assert_eq!(
            driver.next(),
            Some(Request::seed(endpoint_url(Endpoint::Categories)))
        );
    }

    #[test]
    fn unexpected_page_responses_stop_the_run_at_the_last_good_page() {
        let mut driver = ArchiveDriver::resume(site(), before(), Endpoint::Posts, 4, None);
        let checkpoint = Checkpoint::Resume {
            endpoint: Endpoint::Posts,
            last_page: 4,
            total_pages: None,
        };

        let forbidden = inspect(&mut driver, &page_url(Endpoint::Posts, 5), FORBIDDEN);
        assert_eq!(
            forbidden.error.as_deref(),
            Some("unexpected WordPress response status 403 on posts page 5")
        );
        assert_eq!(driver.checkpoint(), checkpoint);

        let untotalled = inspect(&mut driver, &page_url(Endpoint::Posts, 5), OK);
        assert_eq!(
            untotalled.error.as_deref(),
            Some("missing or invalid X-WP-TotalPages on posts page 5")
        );
        assert_eq!(driver.checkpoint(), checkpoint);

        let unexpected = inspect(&mut driver, &page_url(Endpoint::Posts, 6), TWO_PAGES);
        assert!(unexpected.error.is_some());

        let challenge = inspect(
            &mut driver,
            &page_url(Endpoint::Posts, 5),
            b"HTTP/1.1 403 Forbidden\r\ncf-mitigated: challenge\r\n\r\n",
        );
        assert!(
            challenge
                .error
                .is_some_and(|error| error.contains("interactive browser challenge"))
        );
    }

    #[test]
    fn a_failed_capture_ends_the_requests_at_the_checkpoint() {
        let mut driver = ArchiveDriver::resume(site(), before(), Endpoint::Posts, 4, None);
        let url = page_url(Endpoint::Posts, 5);

        driver.failed(&url, &Error::MissingHost(url.clone()));

        assert_eq!(driver.next(), None);
        assert_eq!(
            driver.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Posts,
                last_page: 4,
                total_pages: None,
            }
        );
    }
}
