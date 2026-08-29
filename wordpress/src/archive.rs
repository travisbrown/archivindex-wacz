//! Archiving a site's `WordPress` REST API v2 collections one endpoint at a time.
//!
//! [`ArchiveDriver`] drives an `archivindex-archiver` session. A run captures the API's root
//! resources, discovers custom collections from the type and taxonomy registries among them,
//! probes every supported [`Endpoint`] and then every custom collection with a bare request, and
//! finally pages each exposed collection twice, in that order. The second pass detects records
//! shifted onto earlier pages by concurrent deletions. Its [`Checkpoint`] names the page a
//! stopped run is continued from.

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;

use archivindex_archiver::Error;
use archivindex_archiver::session::{Capture, Driver, Inspection, Request};
use chrono::{DateTime, Utc};
use url::Url;

use crate::endpoint::{Collection, Endpoint, EndpointType, ROOT_ENDPOINTS, Registry};

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

    /// The bare URL of the collection at `wp-json/wp/v2/{endpoint}`.
    fn endpoint_url(&self, endpoint: &str) -> String {
        self.url(&format!("wp-json/wp/v2/{endpoint}"))
    }

    /// The URL of one page of an endpoint's collection, in ascending ID order up to `before`.
    fn page_url(&self, endpoint: &str, before: DateTime<Utc>, page: usize) -> String {
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Checkpoint {
    /// The root resources and endpoint probes that begin an archive are not finished, and are
    /// repeated by a new archive rather than continued.
    Initial,
    /// A run continues an endpoint after the last page captured of it.
    Resume(Resumption),
    /// Every exposed collection completed both paging passes.
    Finished,
}

/// The page a stopped run is continued after.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resumption {
    /// The endpoint to continue.
    pub endpoint: Collection,
    /// The last page of that endpoint captured so far, or zero when it is yet to be probed.
    pub last_page: usize,
    /// The most recently observed page count, if one was available.
    pub total_pages: Option<usize>,
}

/// Progress through one exposed collection whose probe advertised a page count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaginationProgress {
    /// The collection being paged.
    pub collection: Collection,
    /// Pages completed in the current pass.
    pub page: usize,
    /// Page count advertised by the collection's probe.
    pub total_pages: usize,
    /// Whether the collection is being read for its validation pass.
    pub validating: bool,
}

/// Archive a site's collections one endpoint at a time through a session.
///
/// An archive begins with the API root resources and a bare probe of every [`Endpoint`], all
/// requested as seeds. The type and taxonomy registries among the roots advertise the site's
/// custom collections, which are appended after the supported endpoints in registry order (types
/// before taxonomies) and probed via the registry that advertised them. A resumed run begins with
/// the page after its checkpoint, requested via the page before it, and probes only the endpoints
/// still to come. A probe answered with success is paged from page one through the greatest
/// `X-WP-TotalPages` value seen, all pages carrying the run's `before` cutoff; any other answer,
/// such as a 404 for a collection the site lacks, skips the endpoint. Exposed collections are
/// paged one at a time after the last probe, a collection's first page requested via its probe
/// and every later page via the page before it. After reaching a collection's end, the driver
/// re-reads it from page one, via the last page read, with a stable advertised page count; this
/// validation pass captures records shifted earlier by deletions during the first pass.
///
/// An unexpected page response or unreadable registry ends the session with an error, and a
/// failed capture ends the driver's requests; [`checkpoint`](Self::checkpoint) then names the page
/// to continue from.
pub struct ArchiveDriver {
    site: Site,
    before: DateTime<Utc>,
    /// Whether the run began with the root resources and every probe, which cannot be resumed.
    initial: bool,
    /// Index into [`ROOT_ENDPOINTS`] of the next root resource to request.
    next_root: usize,
    /// The supported endpoints followed by the custom collections, in probing order.
    endpoints: Vec<Collection>,
    /// Index into `endpoints` of the next collection to probe.
    next_probe: usize,
    probed: Vec<(Collection, u16)>,
    /// Exposed collections whose probes advertised a page count, in probe order.
    pagination: Vec<(Collection, usize)>,
    /// Endpoints whose probe succeeded, awaiting paging in order.
    pending: VecDeque<Series>,
    current: Option<Series>,
    /// Page count carried by a page-zero resume until its probe, the run's first, is inspected.
    resume_total_pages: Option<usize>,
    /// Whether a capture failed, after which nothing more is requested.
    stopped: bool,
}

/// Progress through one exposed collection.
struct Series {
    endpoint: Collection,
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
    const fn new(endpoint: Collection, total_pages: Option<usize>) -> Self {
        Self {
            endpoint,
            page: 0,
            total_pages,
            validation_total_pages: None,
            phase: SeriesPhase::Primary,
        }
    }

    const fn resume(endpoint: Collection, page: usize, total_pages: Option<usize>) -> Self {
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

    fn checkpoint(&self) -> Checkpoint {
        Checkpoint::Resume(Resumption {
            endpoint: self.endpoint.clone(),
            // A validation pass is deliberately replayed as a fresh primary pass after a stop.
            last_page: if self.is_primary() { self.page } else { 0 },
            total_pages: self.total_pages,
        })
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
    pub fn new(site: Site, before: DateTime<Utc>) -> Self {
        Self {
            site,
            before,
            initial: true,
            next_root: 0,
            endpoints: Endpoint::ALL.map(Collection::Known).to_vec(),
            next_probe: 0,
            probed: Vec::new(),
            pagination: Vec::new(),
            pending: VecDeque::new(),
            current: None,
            resume_total_pages: None,
            stopped: false,
        }
    }

    /// Continue an archive of `site` with the same `before` cutoff from a checkpoint.
    ///
    /// `custom` lists the custom collections the earlier run discovered, in its order, since the
    /// registries are not read again. With `last_page` above zero the run begins with the
    /// endpoint's next page and probes the endpoints after it; with zero it begins by probing the
    /// endpoint itself. A custom endpoint absent from `custom` is archived after them.
    #[must_use]
    pub fn resume(
        site: Site,
        before: DateTime<Utc>,
        resumption: Resumption,
        custom: Vec<Collection>,
    ) -> Self {
        let Resumption {
            endpoint,
            last_page,
            total_pages,
        } = resumption;
        let mut driver = Self::new(site, before);
        driver.initial = false;
        driver.next_root = ROOT_ENDPOINTS.len();
        driver.endpoints.extend(custom);
        let index = driver
            .endpoints
            .iter()
            .position(|collection| collection.name() == endpoint.name())
            .unwrap_or_else(|| {
                driver.endpoints.push(endpoint.clone());
                driver.endpoints.len() - 1
            });
        if last_page == 0 {
            driver.next_probe = index;
            driver.resume_total_pages = total_pages;
        } else {
            driver.next_probe = index + 1;
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
        let next_probe = self.endpoints.get(self.next_probe);
        if self.initial && next_probe.is_some() {
            return Checkpoint::Initial;
        }

        // Endpoints already found exposed are paged only after the remaining probes, so a resumed
        // run must probe them again to reach them.
        self.pending
            .front()
            .map(Series::checkpoint)
            .or_else(|| {
                next_probe.map(|endpoint| {
                    Checkpoint::Resume(Resumption {
                        endpoint: endpoint.clone(),
                        last_page: 0,
                        total_pages: self.resume_total_pages,
                    })
                })
            })
            .unwrap_or(Checkpoint::Finished)
    }

    /// The supported endpoints followed by the custom collections discovered so far, in the
    /// order they are probed.
    #[must_use]
    pub fn endpoints(&self) -> &[Collection] {
        &self.endpoints
    }

    /// Every endpoint probed so far with the status of its bare response, in order.
    #[must_use]
    pub fn probed(&self) -> &[(Collection, u16)] {
        &self.probed
    }

    /// Whether every collection probe for this run has been inspected.
    #[must_use]
    pub const fn probes_finished(&self) -> bool {
        self.next_root == ROOT_ENDPOINTS.len() && self.next_probe == self.endpoints.len()
    }

    /// Progress for exposed collections whose probes advertised `X-WP-TotalPages`.
    ///
    /// The position resets to zero when a collection begins its validation pass. A collection no
    /// longer current or pending is reported at its advertised total.
    #[must_use]
    pub fn pagination_progress(&self) -> Vec<PaginationProgress> {
        self.pagination
            .iter()
            .map(|(collection, total_pages)| {
                let series = self
                    .current
                    .iter()
                    .chain(&self.pending)
                    .find(|series| series.endpoint.name() == collection.name());
                let (page, validating) = series.map_or((*total_pages, false), |series| {
                    (series.page.min(*total_pages), !series.is_primary())
                });

                PaginationProgress {
                    collection: collection.clone(),
                    page,
                    total_pages: *total_pages,
                    validating,
                }
            })
            .collect()
    }

    /// The request for a series' next page, via the page its position follows from.
    fn page_request(&self, series: &Series) -> Request {
        let endpoint = series.endpoint.name();
        let via = match (&series.phase, series.page) {
            (SeriesPhase::Primary, 0) => self.site.endpoint_url(endpoint),
            (SeriesPhase::Validation { after }, 0) => {
                self.site.page_url(endpoint, self.before, *after)
            }
            (_, page) => self.site.page_url(endpoint, self.before, page),
        };

        Request::extra(
            self.site.page_url(endpoint, self.before, series.page + 1),
            via,
        )
    }

    /// The bare probe of the next collection: a seed, or an extra via the advertising registry.
    fn probe_request(&self, collection: &Collection) -> Request {
        let url = self.site.endpoint_url(collection.name());

        collection.registry().map_or_else(
            || Request::seed(&url),
            |registry| Request::extra(&url, self.site.url(registry.path())),
        )
    }

    /// Record a root resource and append the custom collections a registry advertises.
    ///
    /// A registry answered conditionally (`304 Not Modified`) arrives without a payload and so
    /// advertises nothing; only a fresh success is read.
    fn inspect_root(&mut self, root: &str, capture: &Capture<'_>) -> Inspection {
        self.next_root += 1;
        let Some(registry) = Registry::ALL
            .into_iter()
            .find(|registry| registry.path() == root)
        else {
            return Inspection::default();
        };
        if !(200..300).contains(&capture.status) {
            return Inspection::default();
        }
        let entries = match EndpointType::parse_registry(capture.payload) {
            Ok(entries) => entries,
            Err(error) => return Inspection::error(format!("unreadable {root} response: {error}")),
        };
        for name in EndpointType::custom_endpoints(&entries) {
            if !self
                .endpoints
                .iter()
                .any(|collection| collection.name() == name)
            {
                self.endpoints.push(Collection::Custom {
                    name: name.to_owned(),
                    registry,
                });
            }
        }

        Inspection::default()
    }

    /// Record a probe's answer and, after the last probe, begin paging the exposed collections.
    fn inspect_probe(&mut self, endpoint: Collection, capture: &Capture<'_>) {
        self.next_probe += 1;
        // A bare probe uses WordPress's default page size, not the 100-item page size below. Its
        // advertised count is retained for UI progress only; only a count carried from an earlier
        // paged response can drive the resumed series itself.
        let total_pages = self.resume_total_pages.take();
        if (200..300).contains(&capture.status) || capture.status == 304 {
            if let Some(advertised) = capture
                .header("x-wp-totalpages")
                .and_then(|value| value.parse::<usize>().ok())
                .or(total_pages)
            {
                self.pagination.push((endpoint.clone(), advertised));
            }
            self.pending
                .push_back(Series::new(endpoint.clone(), total_pages));
        }
        self.probed.push((endpoint, capture.status));
        if self.next_probe == self.endpoints.len() {
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
                match outcome {
                    PageOutcome::Next => {}
                    PageOutcome::Validate => series.begin_validation(requested),
                    PageOutcome::Last => self.current = self.pending.pop_front(),
                }
                Inspection::default()
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

        self.endpoints
            .get(self.next_probe)
            .map(|collection| self.probe_request(collection))
    }

    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        if crate::is_cloudflare_challenge(capture) {
            return Inspection::error(crate::CLOUDFLARE_CHALLENGE);
        }

        if let Some(series) = &self.current
            && capture.url
                == self
                    .site
                    .page_url(series.endpoint.name(), self.before, series.page + 1)
        {
            return self.inspect_page(capture);
        }

        if let Some(root) = ROOT_ENDPOINTS.get(self.next_root)
            && capture.url == self.site.url(root)
        {
            return self.inspect_root(root, capture);
        }

        if let Some(endpoint) = self.endpoints.get(self.next_probe)
            && capture.url == self.site.endpoint_url(endpoint.name())
        {
            let endpoint = endpoint.clone();
            self.inspect_probe(endpoint, capture);
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
        } else if let Some(endpoint) = self.endpoints.get(self.next_probe) {
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

    use super::{ArchiveDriver, Checkpoint, PaginationProgress, Resumption, Site};
    use crate::endpoint::{Collection, Endpoint, ROOT_ENDPOINTS, Registry};

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
    const TYPES_URL: &str = "https://example.com/blog/wp-json/wp/v2/types";
    const TAXONOMIES_URL: &str = "https://example.com/blog/wp-json/wp/v2/taxonomies";

    fn before() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(BEFORE)
            .map(|date| date.with_timezone(&Utc))
            .expect("a test timestamp")
    }

    fn site() -> Site {
        Site::parse("example.com/blog").expect("a site")
    }

    fn endpoint_url(endpoint: &str) -> String {
        format!("https://example.com/blog/wp-json/wp/v2/{endpoint}")
    }

    fn page_url(endpoint: &str, page: usize) -> String {
        format!(
            "{}?before={BEFORE}&orderby=id&order=asc&page={page}&per_page=100",
            endpoint_url(endpoint)
        )
    }

    /// The request for `page` of an endpoint, via the page before it or `via` for page one.
    fn page_request(endpoint: &str, page: usize, via: &str) -> Request {
        let via = if page > 1 {
            page_url(endpoint, page - 1)
        } else {
            via.to_owned()
        };

        Request::extra(page_url(endpoint, page), via)
    }

    fn custom(name: &str, registry: Registry) -> Collection {
        Collection::Custom {
            name: name.to_owned(),
            registry,
        }
    }

    fn resumption(
        endpoint: impl Into<Collection>,
        last_page: usize,
        total_pages: Option<usize>,
    ) -> Resumption {
        Resumption {
            endpoint: endpoint.into(),
            last_page,
            total_pages,
        }
    }

    fn resume(
        endpoint: impl Into<Collection>,
        last_page: usize,
        total_pages: Option<usize>,
    ) -> Checkpoint {
        Checkpoint::Resume(resumption(endpoint, last_page, total_pages))
    }

    /// A registry response listing `entries` as `wp/v2` types or taxonomies.
    fn registry(entries: &[&str]) -> Vec<u8> {
        let entries: Vec<String> = entries
            .iter()
            .map(|entry| {
                format!(
                    r#""{entry}": {{"name": "", "description": "", "hierarchical": false,
                        "slug": "{entry}", "rest_base": "{entry}", "rest_namespace": "wp/v2",
                        "_links": {{"wp:items": [{{"href": "https://example.com/x"}}]}}}}"#
                )
            })
            .collect();

        format!("{{{}}}", entries.join(", ")).into_bytes()
    }

    fn inspect(driver: &mut ArchiveDriver, url: &str, response: &[u8]) -> Inspection {
        inspect_payload(driver, url, b"{}", response)
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
        capture_roots_with(driver, b"{}", b"{}");
    }

    /// Like [`capture_roots`], answering the type and taxonomy registries with `types` and
    /// `taxonomies`.
    fn capture_roots_with(driver: &mut ArchiveDriver, types: &[u8], taxonomies: &[u8]) {
        for root in ROOT_ENDPOINTS {
            let url = format!("https://example.com/blog/{root}");
            let payload = match url.as_str() {
                TYPES_URL => types,
                TAXONOMIES_URL => taxonomies,
                _ => b"{}",
            };
            assert_eq!(driver.next(), Some(Request::seed(&url)));
            assert_eq!(
                inspect_payload(driver, &url, payload, OK),
                Inspection::default()
            );
        }
    }

    /// Answer every supported endpoint's probe with `responses` in endpoint order, checking that
    /// each is a seed.
    fn probe_all(driver: &mut ArchiveDriver, responses: [&[u8]; 8]) {
        for (endpoint, response) in Endpoint::ALL.into_iter().zip(responses) {
            let url = endpoint_url(endpoint.name());
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
        assert_eq!(driver.next(), Some(Request::seed(endpoint_url("pages"))));
        probe_all(&mut driver, [NOT_FOUND; 8]);
        assert_eq!(driver.next(), None);
        assert_eq!(driver.endpoints(), Endpoint::ALL.map(Collection::Known));
    }

    #[test]
    fn exposed_collections_are_paged_in_order_after_the_last_probe() {
        let mut driver = ArchiveDriver::new(site(), before());
        capture_roots(&mut driver);

        probe_all(
            &mut driver,
            [
                OK, NOT_FOUND, OK, FORBIDDEN, NOT_FOUND, OK, NOT_FOUND, NOT_FOUND,
            ],
        );

        let pages_probe = endpoint_url("pages");
        assert_eq!(driver.next(), Some(page_request("pages", 1, &pages_probe)));
        assert_eq!(
            driver.probed(),
            [
                (Endpoint::Pages.into(), 200),
                (Endpoint::Posts.into(), 404),
                (Endpoint::Categories.into(), 200),
                (Endpoint::Tags.into(), 403),
                (Endpoint::Users.into(), 404),
                (Endpoint::Comments.into(), 200),
                (Endpoint::Media.into(), 404),
                (Endpoint::Navigation.into(), 404),
            ]
        );
        assert_eq!(driver.checkpoint(), resume(Endpoint::Pages, 0, None));

        // Pages are captured without titles.
        let first = inspect(&mut driver, &page_url("pages", 1), TWO_PAGES);
        assert_eq!(first, Inspection::default());
        assert_eq!(driver.next(), Some(page_request("pages", 2, "")));
        assert_eq!(driver.to_string(), "example.com/blog: pages page 1 of 2");

        // The greatest advertised page count decides where the collection ends.
        let _ = inspect(&mut driver, &page_url("pages", 2), THREE_PAGES);
        assert_eq!(driver.next(), Some(page_request("pages", 3, "")));
        let third = inspect(&mut driver, &page_url("pages", 3), TWO_PAGES);
        assert_eq!(third, Inspection::default());
        // The validation pass begins via the last page read, which prompted it.
        assert_eq!(
            driver.next(),
            Some(page_request("pages", 1, &page_url("pages", 3)))
        );
        assert_eq!(driver.checkpoint(), resume(Endpoint::Pages, 0, Some(3)));

        for page in 1..=3 {
            let _ = inspect(&mut driver, &page_url("pages", page), THREE_PAGES);
            if page < 3 {
                assert_eq!(driver.next(), Some(page_request("pages", page + 1, "")));
            }
        }

        // A one-page collection is validated via that page, then the next collection begins.
        for endpoint in ["categories", "comments"] {
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
    fn pagination_progress_uses_probe_totals_and_tracks_each_pass() {
        let mut driver = ArchiveDriver::new(site(), before());
        capture_roots(&mut driver);
        probe_all(
            &mut driver,
            [
                TWO_PAGES, NOT_FOUND, NOT_FOUND, NOT_FOUND, NOT_FOUND, ONE_PAGE, NOT_FOUND,
                NOT_FOUND,
            ],
        );

        assert!(driver.probes_finished());
        assert_eq!(
            driver.pagination_progress(),
            [
                PaginationProgress {
                    collection: Endpoint::Pages.into(),
                    page: 0,
                    total_pages: 2,
                    validating: false,
                },
                PaginationProgress {
                    collection: Endpoint::Comments.into(),
                    page: 0,
                    total_pages: 1,
                    validating: false,
                },
            ]
        );

        let _ = inspect(&mut driver, &page_url("pages", 1), TWO_PAGES);
        assert_eq!(driver.pagination_progress()[0].page, 1);
        let _ = inspect(&mut driver, &page_url("pages", 2), TWO_PAGES);
        assert_eq!(
            driver.pagination_progress()[0],
            PaginationProgress {
                collection: Endpoint::Pages.into(),
                page: 0,
                total_pages: 2,
                validating: true,
            }
        );

        for page in 1..=2 {
            let _ = inspect(&mut driver, &page_url("pages", page), TWO_PAGES);
        }
        assert_eq!(driver.pagination_progress()[0].page, 2);
        assert_eq!(driver.pagination_progress()[1].page, 0);
    }

    #[test]
    fn registries_advertise_custom_endpoints_probed_via_them() {
        let mut driver = ArchiveDriver::new(site(), before());
        // Supported and excluded entries are skipped; repeats keep their first registry.
        capture_roots_with(
            &mut driver,
            &registry(&["posts", "videos", "templates", "product"]),
            &registry(&["categories", "videos", "series"]),
        );
        let videos = custom("videos", Registry::Types);
        let product = custom("product", Registry::Types);
        let series = custom("series", Registry::Taxonomies);
        assert_eq!(driver.endpoints()[8..], [videos.clone(), product, series]);

        probe_all(&mut driver, [NOT_FOUND; 8]);
        assert_eq!(driver.to_string(), "example.com/blog: probing videos");
        let videos_probe = endpoint_url("videos");
        assert_eq!(
            driver.next(),
            Some(Request::extra(&videos_probe, TYPES_URL))
        );
        assert_eq!(driver.checkpoint(), Checkpoint::Initial);
        let _ = inspect(&mut driver, &videos_probe, OK);
        assert_eq!(
            driver.next(),
            Some(Request::extra(endpoint_url("product"), TYPES_URL))
        );
        let _ = inspect(&mut driver, &endpoint_url("product"), NOT_FOUND);
        assert_eq!(
            driver.next(),
            Some(Request::extra(endpoint_url("series"), TAXONOMIES_URL))
        );
        let _ = inspect(&mut driver, &endpoint_url("series"), FORBIDDEN);

        assert_eq!(
            driver.next(),
            Some(page_request("videos", 1, &videos_probe))
        );
        assert_eq!(driver.checkpoint(), resume(videos, 0, None));
        assert_eq!(driver.probed()[8], (custom("videos", Registry::Types), 200));
    }

    #[test]
    fn an_unreadable_registry_stops_the_run() {
        let mut driver = ArchiveDriver::new(site(), before());
        for root in &ROOT_ENDPOINTS[..2] {
            let url = format!("https://example.com/blog/{root}");
            let _ = inspect(&mut driver, &url, OK);
        }

        let unreadable = inspect_payload(&mut driver, TYPES_URL, b"[]", OK);

        assert!(
            unreadable
                .error
                .is_some_and(|error| error.starts_with("unreadable wp-json/wp/v2/types response"))
        );
        assert_eq!(driver.checkpoint(), Checkpoint::Initial);
    }

    #[test]
    fn a_conditional_registry_response_advertises_nothing() {
        let mut driver = ArchiveDriver::new(site(), before());
        for root in &ROOT_ENDPOINTS[..2] {
            let url = format!("https://example.com/blog/{root}");
            let _ = inspect(&mut driver, &url, OK);
        }

        let unchanged = inspect_payload(&mut driver, TYPES_URL, b"", NOT_MODIFIED);

        assert_eq!(unchanged, Inspection::default());
        assert_eq!(driver.endpoints().len(), Endpoint::ALL.len());
    }

    #[test]
    fn no_exposed_collection_finishes_an_archive() {
        let mut driver = ArchiveDriver::new(site(), before());
        capture_roots(&mut driver);

        probe_all(&mut driver, [NOT_FOUND; 8]);

        assert_eq!(driver.next(), None);
        assert_eq!(driver.checkpoint(), Checkpoint::Finished);
    }

    #[test]
    fn a_resumed_run_continues_the_endpoint_and_probes_the_rest() {
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(Endpoint::Comments, 7, Some(8)),
            Vec::new(),
        );

        assert_eq!(driver.next(), Some(page_request("comments", 8, "")));
        assert_eq!(driver.checkpoint(), resume(Endpoint::Comments, 7, Some(8)));

        let _ = inspect(&mut driver, &page_url("comments", 8), EIGHT_PAGES);
        assert_eq!(
            driver.next(),
            Some(page_request("comments", 1, &page_url("comments", 8)))
        );
        assert_eq!(driver.checkpoint(), resume(Endpoint::Comments, 0, Some(8)));
        for page in 1..=8 {
            let _ = inspect(&mut driver, &page_url("comments", page), EIGHT_PAGES);
        }

        // An endpoint found exposed is paged only after the remaining probes, so a run stopped
        // during those probes resumes by probing it again.
        let media_probe = endpoint_url("media");
        assert_eq!(driver.next(), Some(Request::seed(&media_probe)));
        let _ = inspect(&mut driver, &media_probe, OK);
        assert_eq!(driver.checkpoint(), resume(Endpoint::Media, 0, None));
        assert_eq!(
            driver.next(),
            Some(Request::seed(endpoint_url("navigation")))
        );
        let _ = inspect(&mut driver, &endpoint_url("navigation"), NOT_FOUND);
        assert_eq!(driver.next(), Some(page_request("media", 1, &media_probe)));
    }

    #[test]
    fn a_resumed_run_at_page_zero_probes_the_endpoint_itself() {
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(Endpoint::Media, 0, None),
            Vec::new(),
        );

        assert_eq!(driver.next(), Some(Request::seed(endpoint_url("media"))));
        assert_eq!(driver.checkpoint(), resume(Endpoint::Media, 0, None));
    }

    #[test]
    fn a_resumed_run_probes_the_listed_custom_endpoints_after_the_supported_ones() {
        let videos = custom("videos", Registry::Types);
        let series = custom("series", Registry::Taxonomies);
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(Endpoint::Navigation, 0, None),
            vec![videos.clone(), series.clone()],
        );

        assert_eq!(
            driver.next(),
            Some(Request::seed(endpoint_url("navigation")))
        );
        let _ = inspect(&mut driver, &endpoint_url("navigation"), NOT_FOUND);
        assert_eq!(
            driver.next(),
            Some(Request::extra(endpoint_url("videos"), TYPES_URL))
        );
        assert_eq!(driver.checkpoint(), resume(videos, 0, None));
        let _ = inspect(&mut driver, &endpoint_url("videos"), NOT_FOUND);
        assert_eq!(
            driver.next(),
            Some(Request::extra(endpoint_url("series"), TAXONOMIES_URL))
        );
        let _ = inspect(&mut driver, &endpoint_url("series"), OK);
        assert_eq!(
            driver.next(),
            Some(page_request("series", 1, &endpoint_url("series")))
        );
        assert_eq!(driver.checkpoint(), resume(series, 0, None));
    }

    #[test]
    fn a_resumed_custom_endpoint_continues_from_its_page() {
        let videos = custom("videos", Registry::Types);
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(videos.clone(), 1, Some(2)),
            vec![videos.clone()],
        );

        assert_eq!(driver.next(), Some(page_request("videos", 2, "")));
        assert_eq!(driver.checkpoint(), resume(videos.clone(), 1, Some(2)));
        assert_eq!(driver.to_string(), "example.com/blog: videos page 1 of 2");

        // An endpoint missing from the list is archived after the listed ones.
        let unlisted = ArchiveDriver::resume(
            site(),
            before(),
            resumption(videos.clone(), 0, None),
            vec![custom("series", Registry::Taxonomies)],
        );
        assert_eq!(unlisted.endpoints()[9], videos);
        assert_eq!(unlisted.checkpoint(), resume(videos, 0, None));
    }

    #[test]
    fn a_carried_page_count_makes_not_modified_responses_resumable() {
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(Endpoint::Media, 0, Some(2)),
            Vec::new(),
        );
        let media_probe = endpoint_url("media");

        let _ = inspect(&mut driver, &media_probe, NOT_MODIFIED);
        assert_eq!(
            driver.next(),
            Some(Request::seed(endpoint_url("navigation")))
        );
        assert_eq!(driver.checkpoint(), resume(Endpoint::Media, 0, Some(2)));
        let _ = inspect(&mut driver, &endpoint_url("navigation"), NOT_FOUND);
        assert_eq!(driver.next(), Some(page_request("media", 1, &media_probe)));

        let _ = inspect(&mut driver, &page_url("media", 1), NOT_MODIFIED);
        assert_eq!(driver.next(), Some(page_request("media", 2, "")));
        assert_eq!(driver.checkpoint(), resume(Endpoint::Media, 1, Some(2)));
    }

    #[test]
    fn a_page_count_change_during_validation_stops_the_run() {
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(Endpoint::Posts, 1, Some(2)),
            Vec::new(),
        );
        let _ = inspect(&mut driver, &page_url("posts", 2), TWO_PAGES);
        let _ = inspect(&mut driver, &page_url("posts", 1), TWO_PAGES);

        let changed = inspect(&mut driver, &page_url("posts", 2), ONE_PAGE);

        assert_eq!(
            changed.error.as_deref(),
            Some("X-WP-TotalPages changed during the posts validation pass")
        );
        assert_eq!(driver.checkpoint(), resume(Endpoint::Posts, 0, Some(2)));
    }

    #[test]
    fn a_vanished_page_restarts_the_collection_for_validation() {
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(Endpoint::Posts, 4, None),
            Vec::new(),
        );

        let gone = inspect_payload(
            &mut driver,
            &page_url("posts", 5),
            INVALID_PAGE_ERROR,
            BAD_REQUEST,
        );

        assert_eq!(gone, Inspection::default());
        assert_eq!(
            driver.next(),
            Some(page_request("posts", 1, &page_url("posts", 5)))
        );
        assert_eq!(driver.checkpoint(), resume(Endpoint::Posts, 0, None));
        let validation = inspect(&mut driver, &page_url("posts", 1), ONE_PAGE);
        assert_eq!(validation.error, None);
        assert_eq!(
            driver.next(),
            Some(Request::seed(endpoint_url("categories")))
        );
    }

    #[test]
    fn unexpected_page_responses_stop_the_run_at_the_last_good_page() {
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(Endpoint::Posts, 4, None),
            Vec::new(),
        );
        let checkpoint = resume(Endpoint::Posts, 4, None);

        let forbidden = inspect(&mut driver, &page_url("posts", 5), FORBIDDEN);
        assert_eq!(
            forbidden.error.as_deref(),
            Some("unexpected WordPress response status 403 on posts page 5")
        );
        assert_eq!(driver.checkpoint(), checkpoint);

        let untotalled = inspect(&mut driver, &page_url("posts", 5), OK);
        assert_eq!(
            untotalled.error.as_deref(),
            Some("missing or invalid X-WP-TotalPages on posts page 5")
        );
        assert_eq!(driver.checkpoint(), checkpoint);

        let unexpected = inspect(&mut driver, &page_url("posts", 6), TWO_PAGES);
        assert!(unexpected.error.is_some());

        let challenge = inspect(
            &mut driver,
            &page_url("posts", 5),
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
        let mut driver = ArchiveDriver::resume(
            site(),
            before(),
            resumption(Endpoint::Posts, 4, None),
            Vec::new(),
        );
        let url = page_url("posts", 5);

        driver.failed(&url, &Error::MissingHost(url.clone()));

        assert_eq!(driver.next(), None);
        assert_eq!(driver.checkpoint(), resume(Endpoint::Posts, 4, None));
    }
}
