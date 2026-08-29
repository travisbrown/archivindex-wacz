//! Archiving a site's `WordPress` REST API v2 collections one endpoint at a time.
//!
//! [`ArchiveProcessor`] drives an `archivindex-archiver` crawl session. A run captures the API's
//! root resources, probes every [`Endpoint`] with a bare request, and then pages each exposed
//! collection to its end, in [`Endpoint::ALL`] order. Its [`Checkpoint`] names the page a stopped
//! run is continued from.

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;

use archivindex_archiver::session::{Capture, CaptureProcessor, Inspection};
use chrono::{DateTime, Utc};
use url::Url;

/// The API resources requested before any collection, relative to the installation root.
const ROOTS: [&str; 8] = [
    "wp-json",
    "wp-json/wp/v2",
    "wp-json/wp/v2/types",
    "wp-json/wp/v2/taxonomies",
    "wp-json/wp/v2/block-types",
    "wp-json/wp/v2/block-patterns/categories",
    "wp-json/wp/v2/block-patterns/patterns",
    "wp-json/wp/v2/menu-locations",
];

/// A REST API v2 collection endpoint. The variant order is the order endpoints are archived in.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Endpoint {
    /// The `pages` collection.
    Pages,
    /// The `posts` collection.
    Posts,
    /// The `categories` collection.
    Categories,
    /// The `tags` collection.
    Tags,
    /// The `users` collection.
    Users,
    /// The `comments` collection.
    Comments,
    /// The `media` collection.
    Media,
    /// The `videos` collection, a custom post type not every site exposes.
    Videos,
}

impl Endpoint {
    /// Every endpoint, in the order they are probed and paged.
    pub const ALL: [Self; 8] = [
        Self::Pages,
        Self::Posts,
        Self::Categories,
        Self::Tags,
        Self::Users,
        Self::Comments,
        Self::Media,
        Self::Videos,
    ];

    /// The collection's name, which is the last segment of its endpoint path.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pages => "pages",
            Self::Posts => "posts",
            Self::Categories => "categories",
            Self::Tags => "tags",
            Self::Users => "users",
            Self::Comments => "comments",
            Self::Media => "media",
            Self::Videos => "videos",
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A name that is not the lowercase name of an [`Endpoint`].
#[derive(Debug, thiserror::Error)]
#[error(
    "unknown WordPress endpoint {0:?}; expected one of pages, posts, categories, tags, users, \
     comments, media, videos"
)]
pub struct EndpointParseError(String);

impl FromStr for Endpoint {
    type Err = EndpointParseError;

    /// Parse an endpoint's exact lowercase name.
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|endpoint| endpoint.name() == name)
            .ok_or_else(|| EndpointParseError(name.to_owned()))
    }
}

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
    },
    /// Every exposed collection was paged to its end.
    Finished,
}

/// Archive a site's collections one endpoint at a time through a crawl session.
///
/// The processor's [`seeds`](Self::seeds) and [`extras`](Self::extras) start the session. An
/// archive begins with the API root resources and a bare probe of every [`Endpoint`]; a resumed
/// run begins with the page after its checkpoint, as an extra via the page before it, and probes
/// only the endpoints still to come. A probe answered with success is paged from page one through
/// the greatest `X-WP-TotalPages` value seen, all pages carrying the run's `before` cutoff; any
/// other answer, such as a 404 for a collection the site lacks, skips the endpoint. Each exposed
/// collection's first page is discovered on the last probe, together, so the session's
/// depth-first order pages one collection to its end before the next begins.
///
/// An unexpected page response ends the session with an error, and [`checkpoint`](Self::checkpoint)
/// then names the page to continue from.
pub struct ArchiveProcessor {
    site: Site,
    before: DateTime<Utc>,
    /// Whether the run began with the root resources and every probe, which cannot be resumed.
    initial: bool,
    /// Index into [`Endpoint::ALL`] of the next endpoint to probe.
    next_probe: usize,
    probed: Vec<(Endpoint, u16)>,
    /// Endpoints whose probe succeeded, awaiting paging in order.
    pending: VecDeque<Endpoint>,
    current: Option<Series>,
}

/// Progress through one exposed collection.
struct Series {
    endpoint: Endpoint,
    /// The last page captured, or zero before the first.
    page: usize,
    /// The greatest page count advertised so far.
    total_pages: Option<usize>,
}

/// What a collection page's response means for the series.
enum PageOutcome {
    /// A further page follows.
    Next,
    /// The page was the collection's last.
    Last,
    /// The page no longer exists, so the collection ended before it.
    Gone,
}

impl Series {
    const fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            page: 0,
            total_pages: None,
        }
    }

    /// Record the response to the page after the last one captured.
    fn record(&mut self, capture: &Capture<'_>) -> Result<PageOutcome, String> {
        let page = self.page + 1;
        // A page can disappear between requests when deletions reduce the page count, which some
        // WordPress endpoints report with this posts-controller error code.
        if capture.status == 400 && page > 1 && crate::is_invalid_page_error(capture.payload) {
            return Ok(PageOutcome::Gone);
        }
        if !matches!(capture.status, 200 | 304) {
            return Err(format!(
                "unexpected WordPress response status {} on {} page {page}",
                capture.status, self.endpoint
            ));
        }
        if let Some(total_pages) = capture
            .header("x-wp-totalpages")
            .and_then(|value| value.parse::<usize>().ok())
        {
            self.total_pages = Some(
                self.total_pages
                    .map_or(total_pages, |known| known.max(total_pages)),
            );
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
        } else {
            PageOutcome::Last
        })
    }
}

impl ArchiveProcessor {
    /// Begin an archive of `site` with the root resources and every probe.
    ///
    /// Every page requested carries `before` as its cutoff, so pass the time the archive started.
    #[must_use]
    pub const fn new(site: Site, before: DateTime<Utc>) -> Self {
        Self {
            site,
            before,
            initial: true,
            next_probe: 0,
            probed: Vec::new(),
            pending: VecDeque::new(),
            current: None,
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
    ) -> Self {
        let mut processor = Self::new(site, before);
        processor.initial = false;
        if last_page == 0 {
            processor.next_probe = endpoint as usize;
        } else {
            processor.next_probe = endpoint as usize + 1;
            processor.current = Some(Series {
                endpoint,
                page: last_page,
                total_pages: None,
            });
        }

        processor
    }

    /// The URLs a session starting this run requests as seeds, in order.
    #[must_use]
    pub fn seeds(&self) -> Vec<String> {
        let roots = self
            .initial
            .then_some(ROOTS)
            .into_iter()
            .flatten()
            .map(|root| self.site.url(root));
        let probes = Endpoint::ALL[self.next_probe..]
            .iter()
            .map(|&endpoint| self.site.endpoint_url(endpoint));

        roots.chain(probes).collect()
    }

    /// The page a resumed session requests first, with the preceding page it is requested via.
    #[must_use]
    pub fn extras(&self) -> Option<(String, String)> {
        let series = self.current.as_ref()?;

        Some((
            self.site
                .page_url(series.endpoint, self.before, series.page + 1),
            self.site
                .page_url(series.endpoint, self.before, series.page),
        ))
    }

    /// Where a run stopped now would be continued from.
    #[must_use]
    pub fn checkpoint(&self) -> Checkpoint {
        if let Some(series) = &self.current {
            return Checkpoint::Resume {
                endpoint: series.endpoint,
                last_page: series.page,
            };
        }
        let next_probe = Endpoint::ALL.get(self.next_probe).copied();
        if self.initial && next_probe.is_some() {
            return Checkpoint::Initial;
        }

        // Endpoints already found exposed are paged only after the remaining probes, so a resumed
        // run must probe them again to reach them.
        self.pending
            .front()
            .copied()
            .or(next_probe)
            .map_or(Checkpoint::Finished, |endpoint| Checkpoint::Resume {
                endpoint,
                last_page: 0,
            })
    }

    /// Every endpoint probed so far with the status of its bare response, in order.
    #[must_use]
    pub fn probed(&self) -> &[(Endpoint, u16)] {
        &self.probed
    }

    /// Record a probe's answer and, after the last probe, discover every exposed first page.
    fn inspect_probe(&mut self, endpoint: Endpoint, status: u16) -> Inspection {
        self.next_probe += 1;
        self.probed.push((endpoint, status));
        if (200..300).contains(&status) || status == 304 {
            self.pending.push_back(endpoint);
        }
        if self.next_probe < Endpoint::ALL.len() {
            return Inspection::default();
        }

        let links = self
            .pending
            .iter()
            .map(|&endpoint| self.site.page_url(endpoint, self.before, 1))
            .collect();
        self.current = self.pending.pop_front().map(Series::new);

        Inspection {
            links,
            ..Inspection::default()
        }
    }
}

impl CaptureProcessor for ArchiveProcessor {
    fn inspect(&mut self, capture: &Capture<'_>) -> Inspection {
        if crate::is_cloudflare_challenge(capture) {
            return Inspection::error(crate::CLOUDFLARE_CHALLENGE);
        }

        if let Some(series) = &mut self.current
            && capture.url
                == self
                    .site
                    .page_url(series.endpoint, self.before, series.page + 1)
        {
            return match series.record(capture) {
                Ok(outcome) => {
                    let title = Some(format!(
                        "{} {} page {} of {}",
                        self.site.base,
                        series.endpoint,
                        series.page,
                        series.total_pages.unwrap_or(series.page)
                    ));
                    match outcome {
                        PageOutcome::Next => Inspection {
                            links: vec![self.site.page_url(
                                series.endpoint,
                                self.before,
                                series.page + 1,
                            )],
                            title,
                            error: None,
                        },
                        PageOutcome::Last | PageOutcome::Gone => {
                            let title = matches!(outcome, PageOutcome::Last)
                                .then_some(title)
                                .flatten();
                            self.current = self.pending.pop_front().map(Series::new);
                            Inspection {
                                title,
                                ..Inspection::default()
                            }
                        }
                    }
                }
                Err(message) => Inspection::error(message),
            };
        }

        if let Some(&endpoint) = Endpoint::ALL.get(self.next_probe)
            && capture.url == self.site.endpoint_url(endpoint)
        {
            return self.inspect_probe(endpoint, capture.status);
        }

        if self.initial && ROOTS.iter().any(|root| capture.url == self.site.url(root)) {
            return Inspection::default();
        }

        Inspection::error(format!("unexpected capture of {}", capture.url))
    }
}

impl fmt::Display for ArchiveProcessor {
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
    use archivindex_archiver::capture::Origin;
    use archivindex_archiver::session::{Capture, CaptureProcessor};
    use chrono::{DateTime, Utc};

    use super::{ArchiveProcessor, Checkpoint, Endpoint, Site};

    const BEFORE: &str = "2026-08-20T00:00:00Z";
    const OK: &[u8] = b"HTTP/1.1 200 OK\r\n\r\n";
    const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\n\r\n";
    const FORBIDDEN: &[u8] = b"HTTP/1.1 403 Forbidden\r\n\r\n";
    const ONE_PAGE: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 3\r\nX-WP-TotalPages: 1\r\n\r\n";
    const TWO_PAGES: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 101\r\nX-WP-TotalPages: 2\r\n\r\n";
    const THREE_PAGES: &[u8] = b"HTTP/1.1 200 OK\r\nX-WP-Total: 201\r\nX-WP-TotalPages: 3\r\n\r\n";
    const BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\r\n";
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

    fn inspect(processor: &mut ArchiveProcessor, url: &str, response: &[u8]) -> super::Inspection {
        inspect_payload(processor, url, b"[]", response)
    }

    fn inspect_payload(
        processor: &mut ArchiveProcessor,
        url: &str,
        payload: &[u8],
        response: &[u8],
    ) -> super::Inspection {
        let capture =
            Capture::new(url, url, Origin::Seed, payload, response).expect("a complete response");

        processor.inspect(&capture)
    }

    /// Answer every probe with `responses` in endpoint order, returning the last inspection.
    fn probe_all(processor: &mut ArchiveProcessor, responses: [&[u8]; 8]) -> super::Inspection {
        Endpoint::ALL
            .into_iter()
            .zip(responses)
            .map(|(endpoint, response)| inspect(processor, &endpoint_url(endpoint), response))
            .last()
            .expect("eight probes")
    }

    #[test]
    fn endpoint_names_parse_exactly() {
        for endpoint in Endpoint::ALL {
            assert_eq!(endpoint.name().parse::<Endpoint>().ok(), Some(endpoint));
        }
        assert!("Posts".parse::<Endpoint>().is_err());
        assert!("posts/".parse::<Endpoint>().is_err());
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
    fn an_archive_seeds_the_roots_and_every_probe() {
        let processor = ArchiveProcessor::new(site(), before());

        let seeds = processor.seeds();

        assert_eq!(seeds.len(), 11);
        assert_eq!(seeds[0], "https://example.com/blog/wp-json");
        assert_eq!(seeds[1], "https://example.com/blog/wp-json/wp/v2");
        assert_eq!(seeds[2], "https://example.com/blog/wp-json/wp/v2/types");
        assert_eq!(seeds[3], endpoint_url(Endpoint::Pages));
        assert_eq!(seeds[10], endpoint_url(Endpoint::Videos));
        assert_eq!(processor.extras(), None);
        assert_eq!(processor.checkpoint(), Checkpoint::Initial);
        assert_eq!(processor.to_string(), "example.com/blog: probing pages");
    }

    #[test]
    fn exposed_collections_are_paged_in_order_after_the_last_probe() {
        let mut processor = ArchiveProcessor::new(site(), before());
        for root in ["wp-json", "wp-json/wp/v2", "wp-json/wp/v2/types"] {
            let inspection = inspect(
                &mut processor,
                &format!("https://example.com/blog/{root}"),
                OK,
            );
            assert_eq!(inspection, super::Inspection::default());
        }
        assert_eq!(processor.checkpoint(), Checkpoint::Initial);

        let last = probe_all(
            &mut processor,
            [
                OK, NOT_FOUND, OK, FORBIDDEN, NOT_FOUND, OK, NOT_FOUND, NOT_FOUND,
            ],
        );

        assert_eq!(
            last.links,
            [
                page_url(Endpoint::Pages, 1),
                page_url(Endpoint::Categories, 1),
                page_url(Endpoint::Comments, 1)
            ]
        );
        assert_eq!(
            processor.probed(),
            [
                (Endpoint::Pages, 200),
                (Endpoint::Posts, 404),
                (Endpoint::Categories, 200),
                (Endpoint::Tags, 403),
                (Endpoint::Users, 404),
                (Endpoint::Comments, 200),
                (Endpoint::Media, 404),
                (Endpoint::Videos, 404)
            ]
        );
        assert_eq!(
            processor.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Pages,
                last_page: 0
            }
        );

        let first = inspect(&mut processor, &page_url(Endpoint::Pages, 1), TWO_PAGES);
        assert_eq!(first.links, [page_url(Endpoint::Pages, 2)]);
        assert_eq!(
            first.title.as_deref(),
            Some("example.com/blog pages page 1 of 2")
        );
        assert_eq!(processor.to_string(), "example.com/blog: pages page 1 of 2");

        // The greatest advertised page count decides where the collection ends.
        let second = inspect(&mut processor, &page_url(Endpoint::Pages, 2), THREE_PAGES);
        assert_eq!(second.links, [page_url(Endpoint::Pages, 3)]);
        let third = inspect(&mut processor, &page_url(Endpoint::Pages, 3), TWO_PAGES);
        assert_eq!(third.links, Vec::<String>::new());
        assert_eq!(
            third.title.as_deref(),
            Some("example.com/blog pages page 3 of 3")
        );
        assert_eq!(
            processor.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Categories,
                last_page: 0
            }
        );

        let categories = inspect(&mut processor, &page_url(Endpoint::Categories, 1), ONE_PAGE);
        assert_eq!(categories.links, Vec::<String>::new());
        let comments = inspect(&mut processor, &page_url(Endpoint::Comments, 1), ONE_PAGE);
        assert_eq!(comments.error, None);
        assert_eq!(processor.checkpoint(), Checkpoint::Finished);
        assert_eq!(processor.to_string(), "example.com/blog: finished");
    }

    #[test]
    fn no_exposed_collection_finishes_an_archive() {
        let mut processor = ArchiveProcessor::new(site(), before());

        let last = probe_all(&mut processor, [NOT_FOUND; 8]);

        assert_eq!(last, super::Inspection::default());
        assert_eq!(processor.checkpoint(), Checkpoint::Finished);
    }

    #[test]
    fn a_resumed_run_continues_the_endpoint_and_probes_the_rest() {
        let processor = ArchiveProcessor::resume(site(), before(), Endpoint::Comments, 7);

        assert_eq!(
            processor.seeds(),
            [
                endpoint_url(Endpoint::Media),
                endpoint_url(Endpoint::Videos)
            ]
        );
        assert_eq!(
            processor.extras(),
            Some((
                page_url(Endpoint::Comments, 8),
                page_url(Endpoint::Comments, 7)
            ))
        );
        assert_eq!(
            processor.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Comments,
                last_page: 7
            }
        );

        let mut processor = processor;
        let last = inspect(
            &mut processor,
            &page_url(Endpoint::Comments, 8),
            b"HTTP/1.1 200 OK\r\nX-WP-TotalPages: 8\r\n\r\n",
        );
        assert_eq!(last.links, Vec::<String>::new());
        assert_eq!(
            processor.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Media,
                last_page: 0
            }
        );

        // An endpoint found exposed is paged only after the remaining probes, so a run stopped
        // during those probes resumes by probing it again.
        let media = inspect(&mut processor, &endpoint_url(Endpoint::Media), OK);
        assert_eq!(media, super::Inspection::default());
        assert_eq!(
            processor.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Media,
                last_page: 0
            }
        );
        let videos = inspect(&mut processor, &endpoint_url(Endpoint::Videos), OK);
        assert_eq!(
            videos.links,
            [page_url(Endpoint::Media, 1), page_url(Endpoint::Videos, 1)]
        );
    }

    #[test]
    fn a_resumed_run_at_page_zero_probes_the_endpoint_itself() {
        let processor = ArchiveProcessor::resume(site(), before(), Endpoint::Videos, 0);

        assert_eq!(processor.seeds(), [endpoint_url(Endpoint::Videos)]);
        assert_eq!(processor.extras(), None);
        assert_eq!(
            processor.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Videos,
                last_page: 0
            }
        );
    }

    #[test]
    fn a_vanished_page_ends_the_collection_without_a_title() {
        let mut processor = ArchiveProcessor::resume(site(), before(), Endpoint::Posts, 4);

        let gone = inspect_payload(
            &mut processor,
            &page_url(Endpoint::Posts, 5),
            INVALID_PAGE_ERROR,
            BAD_REQUEST,
        );

        assert_eq!(gone, super::Inspection::default());
        assert_eq!(
            processor.checkpoint(),
            Checkpoint::Resume {
                endpoint: Endpoint::Categories,
                last_page: 0
            }
        );
    }

    #[test]
    fn unexpected_page_responses_stop_the_run_at_the_last_good_page() {
        let mut processor = ArchiveProcessor::resume(site(), before(), Endpoint::Posts, 4);
        let checkpoint = Checkpoint::Resume {
            endpoint: Endpoint::Posts,
            last_page: 4,
        };

        let forbidden = inspect(&mut processor, &page_url(Endpoint::Posts, 5), FORBIDDEN);
        assert_eq!(
            forbidden.error.as_deref(),
            Some("unexpected WordPress response status 403 on posts page 5")
        );
        assert_eq!(processor.checkpoint(), checkpoint);

        let untotalled = inspect(&mut processor, &page_url(Endpoint::Posts, 5), OK);
        assert_eq!(
            untotalled.error.as_deref(),
            Some("missing or invalid X-WP-TotalPages on posts page 5")
        );
        assert_eq!(processor.checkpoint(), checkpoint);

        let unexpected = inspect(&mut processor, &page_url(Endpoint::Posts, 6), TWO_PAGES);
        assert!(unexpected.error.is_some());

        let challenge = inspect(
            &mut processor,
            &page_url(Endpoint::Posts, 5),
            b"HTTP/1.1 403 Forbidden\r\ncf-mitigated: challenge\r\n\r\n",
        );
        assert!(
            challenge
                .error
                .is_some_and(|error| error.contains("interactive browser challenge"))
        );
    }
}
