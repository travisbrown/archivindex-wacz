//! Structural validation of collection archives produced by [`ArchiveDriver`](crate::archive::ArchiveDriver).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use archivindex_warc::io::read::WarcReader;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::header::{RevisitHeader, RevisitProfile};
use archivindex_warc::record::{FieldsBlock, Record, http, payload};
use serde_json::Value;
use url::Url;

use crate::archive::{Site, SiteError};
use crate::endpoint::{Collection, Endpoint, EndpointType, ROOT_ENDPOINTS, Registry};

/// The severity of one archive lint finding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    /// A violation of the capture or pagination protocol.
    Error,
    /// A missing or incorrect advisory HTTP pagination link.
    Warning,
}

/// One problem found in a collection archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    /// Whether the finding is an error or warning.
    pub severity: Severity,
    /// Human-readable description of the problem.
    pub message: String,
}

/// Counts advertised for one successfully probed and paginated endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaginationSummary {
    /// Bare collection endpoint URI.
    pub endpoint: String,
    /// Greatest page count advertised during pagination.
    pub pages: Option<usize>,
    /// Item count advertised by the endpoint probe.
    pub items: Option<usize>,
}

/// Results of linting one `WordPress` collection archive.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LintReport {
    /// Problems in capture order, followed by cross-capture protocol problems.
    pub findings: Vec<Finding>,
    /// Successfully probed endpoints in probe order, with their advertised counts.
    pub pagination: Vec<PaginationSummary>,
    /// Number of required root captures found.
    pub roots: usize,
    /// Number of required known endpoint probes found.
    pub known_probes: usize,
    /// Number of registry-advertised custom endpoint probes found.
    pub custom_probes: usize,
}

impl LintReport {
    /// Whether the archive has no errors or warnings.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// Number of error findings.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Error)
            .count()
    }

    /// Number of warning findings.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.severity == Severity::Warning)
            .count()
    }
}

/// A collection archive could not be read or did not identify a `WordPress` site.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The input file could not be opened or read.
    #[error("cannot read {}: {source}", path.display())]
    Io {
        /// Path that was being read.
        path: PathBuf,
        /// Underlying file error.
        #[source]
        source: std::io::Error,
    },
    /// A WARC record is syntactically or semantically invalid.
    #[error("cannot read a WARC record from {}: {source}", path.display())]
    Warc {
        /// Path that was being read.
        path: PathBuf,
        /// Underlying WARC error.
        #[source]
        source: archivindex_warc::io::read::Error,
    },
    /// No request identifies a `WordPress` installation.
    #[error("{} contains no WordPress REST API requests", .0.display())]
    NoWordPressRequests(PathBuf),
    /// The inferred installation URL is invalid.
    #[error("cannot infer a WordPress site from {url:?}: {source}")]
    Site {
        /// Request URI used for inference.
        url: String,
        /// Why the inferred site was invalid.
        #[source]
        source: SiteError,
    },
}

/// Lint a plain or gzip-compressed `WordPress` collection WARC.
///
/// The archive must begin with all API roots and known probes, followed by registry-advertised
/// custom probes. Every successful probe must have one correctly linked pagination traversal.
///
/// # Errors
///
/// Returns [`Error`] when the file cannot be read as a WARC or no `WordPress` request identifies the
/// installation. Protocol violations are returned as findings rather than errors.
pub fn lint_archive(path: impl AsRef<Path>) -> Result<LintReport, Error> {
    let path = path.as_ref();
    let gzip = crate::read::is_gzip_file(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    if gzip {
        let reader = WarcReader::from_path_gzip(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        lint_reader(reader, path)
    } else {
        let reader = WarcReader::from_path(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        lint_reader(reader, path)
    }
}

#[derive(Debug)]
struct CaptureGroup {
    url: String,
    response: Option<StoredResponse>,
    metadata: Option<StoredMetadata>,
}

#[derive(Debug)]
struct StoredResponse {
    url: String,
    body: Vec<u8>,
    truncation: Option<String>,
    revisit: Option<StoredRevisit>,
}

#[derive(Debug)]
struct StoredRevisit {
    profile: StoredRevisitProfile,
    original: Option<usize>,
    identified_json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredRevisitProfile {
    IdenticalPayloadDigest,
    ServerNotModified,
    Other,
}

#[derive(Debug)]
struct StoredMetadata {
    url: Option<String>,
    via: Option<String>,
    fields: bool,
}

#[derive(Clone, Debug)]
struct Probe {
    collection: Collection,
    url: String,
    status: Option<u16>,
    items: Option<usize>,
}

#[derive(Clone, Debug)]
struct PageCapture {
    group: usize,
    page: Option<usize>,
    valid_query: bool,
}

fn lint_reader<R: BufRead>(reader: WarcReader<R>, path: &Path) -> Result<LintReport, Error> {
    let (groups, mut report) = collect_groups(reader, path)?;
    let site = groups
        .iter()
        .find_map(|group| site_from_request(&group.url))
        .transpose()?
        .ok_or_else(|| Error::NoWordPressRequests(path.to_owned()))?;
    let mut checked_shapes = HashSet::new();

    let customs = discover_customs(&groups, &site, &mut report);
    let expected_initial = expected_initial(&site, &customs);

    let mut previous = None;
    let mut probes = Vec::new();
    for (position, (url, collection)) in expected_initial.iter().enumerate() {
        let candidates = initial_capture_candidates(&groups, url);
        let group = candidates.first().copied();
        let label = collection.as_ref().map_or_else(
            || format!("initial root {url}"),
            |collection| format!("{} probe", collection.name()),
        );
        if candidates.is_empty() {
            error(&mut report, format!("missing required {label}"));
        } else {
            if candidates.len() > 1 {
                error(
                    &mut report,
                    format!("{label} is captured {} times", candidates.len()),
                );
            }
            if let Some(previous) = previous
                && group.is_some_and(|group| group < previous)
            {
                error(
                    &mut report,
                    format!("{label} is out of initial capture order"),
                );
            }
            if let Some(group) = group {
                previous = Some(group);
                check_capture_shape(&groups, group, &mut checked_shapes, &mut report);
                let expected_via = collection
                    .as_ref()
                    .and_then(Collection::registry)
                    .map(|registry| format!("{}{}", site.root(), registry.path()));
                check_via(&groups[group], expected_via.as_deref(), &label, &mut report);
            }
            if position < ROOT_ENDPOINTS.len() {
                report.roots += 1;
            } else if position < ROOT_ENDPOINTS.len() + Endpoint::ALL.len() {
                report.known_probes += 1;
            } else {
                report.custom_probes += 1;
            }
        }

        if let Some(collection) = collection {
            let response = group.and_then(|group| response_metadata(&groups[group]));
            let status = response.as_ref().map(|metadata| metadata.status);
            let items = response
                .as_ref()
                .and_then(|metadata| numeric_header(metadata, "x-wp-total"));
            if status.is_some_and(is_success) && items.is_none() {
                error(
                    &mut report,
                    format!(
                        "successful {} probe has missing or invalid X-WP-Total",
                        collection.name()
                    ),
                );
            }
            if let (Some(group), Some(metadata)) = (group, response.as_ref())
                && is_success(metadata.status)
            {
                check_probe_response(&groups, group, metadata, collection.name(), &mut report);
            }
            probes.push(Probe {
                collection: collection.clone(),
                url: url.clone(),
                status,
                items,
            });
        }
    }

    check_unadvertised_custom_probes(&groups, &site, &customs, &mut report);
    lint_pagination(&groups, &probes, &mut checked_shapes, &mut report);
    Ok(report)
}

fn expected_initial(site: &Site, customs: &[Collection]) -> Vec<(String, Option<Collection>)> {
    let mut expected = ROOT_ENDPOINTS
        .map(|root| (format!("{}{root}", site.root()), None))
        .to_vec();
    expected.extend(Endpoint::ALL.map(|endpoint| {
        (
            endpoint_url(site, endpoint.name()),
            Some(Collection::Known(endpoint)),
        )
    }));
    expected.extend(
        customs
            .iter()
            .cloned()
            .map(|collection| (endpoint_url(site, collection.name()), Some(collection))),
    );
    expected
}

fn initial_capture_candidates(groups: &[CaptureGroup], url: &str) -> Vec<usize> {
    groups
        .iter()
        .enumerate()
        .filter(|(_, group)| group.url == url && !is_server_challenge(group))
        .map(|(index, _)| index)
        .collect()
}

fn collect_groups<R: BufRead>(
    reader: WarcReader<R>,
    path: &Path,
) -> Result<(Vec<CaptureGroup>, LintReport), Error> {
    let mut groups = Vec::<CaptureGroup>::new();
    let mut requests = HashMap::<String, usize>::new();
    let mut responses = HashMap::<String, usize>::new();
    let mut report = LintReport::default();

    for record in reader.iter_records::<NoExtension>().records() {
        let record = record.map_err(|source| Error::Warc {
            path: path.to_owned(),
            source,
        })?;
        match record {
            Record::Request { header, .. } => {
                let id = header.core.record_id.into_string();
                let index = groups.len();
                if requests.insert(id.clone(), index).is_some() {
                    error(&mut report, format!("duplicate request record ID {id}"));
                }
                groups.push(CaptureGroup {
                    url: header.target_uri.into_string(),
                    response: None,
                    metadata: None,
                });
            }
            Record::Response { header, body } => {
                let request = header
                    .concurrent_to
                    .iter()
                    .find_map(|record_id| requests.get(record_id.as_str()).copied());
                attach_response(
                    &mut groups,
                    &mut responses,
                    &mut report,
                    header.core.record_id.into_string(),
                    header.target_uri.into_string(),
                    request,
                    header
                        .core
                        .truncated
                        .as_ref()
                        .map(|reason| reason.as_str().to_owned()),
                    None,
                    body,
                );
            }
            Record::Revisit { header, body } => collect_revisit(
                header,
                body,
                &requests,
                &mut responses,
                &mut groups,
                &mut report,
            ),
            Record::Metadata { header, body } => {
                let id = header.core.record_id.into_string();
                let Some(index) = header
                    .concurrent_to
                    .iter()
                    .find_map(|record_id| responses.get(record_id.as_str()).copied())
                else {
                    error(
                        &mut report,
                        format!("metadata record {id} has no linked response or revisit"),
                    );
                    continue;
                };
                if groups[index].metadata.is_some() {
                    error(
                        &mut report,
                        format!("capture of {} has duplicate metadata", groups[index].url),
                    );
                    continue;
                }
                let (via, fields) = match body {
                    FieldsBlock::Fields(fields) => (fields.via().map(str::to_owned), true),
                    FieldsBlock::Raw(_) => (None, false),
                };
                let url = header.target_uri.map(|url| format!("{url}"));
                groups[index].metadata = Some(StoredMetadata { url, via, fields });
            }
            Record::Warcinfo { .. }
            | Record::Resource { .. }
            | Record::Conversion { .. }
            | Record::Continuation { .. }
            | Record::Other { .. } => {}
        }
    }
    Ok((groups, report))
}

fn collect_revisit(
    header: RevisitHeader,
    body: Vec<u8>,
    requests: &HashMap<String, usize>,
    responses: &mut HashMap<String, usize>,
    groups: &mut [CaptureGroup],
    report: &mut LintReport,
) {
    let request = header
        .concurrent_to
        .iter()
        .find_map(|record_id| requests.get(record_id.as_str()).copied());
    let original = header
        .refers_to
        .as_ref()
        .and_then(|record_id| responses.get(record_id.as_str()).copied());
    let profile = match header.profile {
        RevisitProfile::IdenticalPayloadDigest(_) => StoredRevisitProfile::IdenticalPayloadDigest,
        RevisitProfile::ServerNotModified(_) => StoredRevisitProfile::ServerNotModified,
        RevisitProfile::Other(_) => StoredRevisitProfile::Other,
    };
    let identified_json = header
        .payload
        .identified_payload_type
        .as_ref()
        .is_some_and(|media_type| media_type.is("application", "json"));
    attach_response(
        groups,
        responses,
        report,
        header.core.record_id.into_string(),
        header.target_uri.into_string(),
        request,
        header
            .core
            .truncated
            .as_ref()
            .map(|reason| reason.as_str().to_owned()),
        Some(StoredRevisit {
            profile,
            original,
            identified_json,
        }),
        body,
    );
}

#[allow(clippy::too_many_arguments)]
fn attach_response(
    groups: &mut [CaptureGroup],
    responses: &mut HashMap<String, usize>,
    report: &mut LintReport,
    id: String,
    url: String,
    request: Option<usize>,
    truncation: Option<String>,
    revisit: Option<StoredRevisit>,
    body: Vec<u8>,
) {
    let Some(index) = request else {
        error(
            report,
            format!("response or revisit record {id} has no linked request"),
        );
        return;
    };
    if groups[index].response.is_some() {
        error(
            report,
            format!("capture of {} has duplicate responses", groups[index].url),
        );
        return;
    }
    responses.insert(id, index);
    groups[index].response = Some(StoredResponse {
        url,
        body,
        truncation,
        revisit,
    });
}

fn site_from_request(request: &str) -> Option<Result<Site, Error>> {
    let mut url = Url::parse(request).ok()?;
    let marker = url.path().find("/wp-json")?;
    let path = match &url.path()[..marker] {
        "" => "/".to_owned(),
        path => path.to_owned(),
    };
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    let inferred = url.as_str().trim_end_matches('/');
    Some(Site::parse(inferred).map_err(|source| Error::Site {
        url: request.to_owned(),
        source,
    }))
}

fn discover_customs(
    groups: &[CaptureGroup],
    site: &Site,
    report: &mut LintReport,
) -> Vec<Collection> {
    let mut custom = Vec::new();
    for registry in Registry::ALL {
        let url = format!("{}{}", site.root(), registry.path());
        let Some(group) = groups
            .iter()
            .find(|group| group.url == url && !is_server_challenge(group))
        else {
            continue;
        };
        let Some(response) = &group.response else {
            continue;
        };
        let Some(metadata) = http::ResponseMetadata::parse(&response.body) else {
            continue;
        };
        if !(200..300).contains(&metadata.status) {
            continue;
        }
        let entity = match payload::entity_body(&response.body) {
            Ok(entity) => entity,
            Err(source) => {
                error(
                    report,
                    format!("cannot read {url} response payload: {source}"),
                );
                continue;
            }
        };
        let entries = match EndpointType::parse_registry(&entity) {
            Ok(entries) => entries,
            Err(source) => {
                error(
                    report,
                    format!("unreadable {url} registry response: {source}"),
                );
                continue;
            }
        };
        for name in EndpointType::custom_endpoints(&entries) {
            if !custom.iter().any(|item: &Collection| item.name() == name) {
                custom.push(Collection::Custom {
                    name: name.to_owned(),
                    registry,
                });
            }
        }
    }
    custom
}

fn check_unadvertised_custom_probes(
    groups: &[CaptureGroup],
    site: &Site,
    custom: &[Collection],
    report: &mut LintReport,
) {
    let known = Endpoint::ALL.map(Endpoint::name);
    let prefix = format!("{}wp-json/wp/v2/", site.root());
    for group in groups {
        let Ok(url) = Url::parse(&group.url) else {
            continue;
        };
        let Some(name) = url.path().strip_prefix(
            Url::parse(&prefix)
                .expect("a site root makes a valid endpoint prefix")
                .path(),
        ) else {
            continue;
        };
        if url.query().is_none()
            && !name.is_empty()
            && !name.contains('/')
            && !known.contains(&name)
            && !ROOT_ENDPOINTS
                .iter()
                .any(|root| root.strip_prefix("wp-json/wp/v2/") == Some(name))
            && !custom.iter().any(|collection| collection.name() == name)
        {
            error(
                report,
                format!("custom endpoint probe {name:?} was not advertised by a registry"),
            );
        }
    }
}

fn check_capture_shape(
    groups: &[CaptureGroup],
    index: usize,
    checked: &mut HashSet<usize>,
    report: &mut LintReport,
) {
    if !checked.insert(index) {
        return;
    }
    let group = &groups[index];
    let Some(response) = &group.response else {
        error(
            report,
            format!("capture of {} is missing a response or revisit", group.url),
        );
        return;
    };
    if response.url != group.url {
        error(
            report,
            format!(
                "capture request URI {} does not match response URI {}",
                group.url, response.url
            ),
        );
    }
    if let Some(reason) = response
        .truncation
        .as_deref()
        .filter(|reason| !intentional_revisit_truncation(response, reason))
    {
        error(
            report,
            format!(
                "capture of {} has a response truncated because of {reason}",
                group.url
            ),
        );
    }
    if http::ResponseMetadata::parse(&response.body).is_none() {
        error(
            report,
            format!("capture of {} has an invalid HTTP response", group.url),
        );
    }
    let Some(metadata) = &group.metadata else {
        error(
            report,
            format!("capture of {} is missing metadata", group.url),
        );
        return;
    };
    if metadata.url.as_deref() != Some(response.url.as_str()) {
        error(
            report,
            format!(
                "capture response URI {} does not match metadata URI {}",
                response.url,
                metadata.url.as_deref().unwrap_or("<missing>")
            ),
        );
    }
    if !metadata.fields {
        error(
            report,
            format!(
                "capture of {} metadata is not application/warc-fields",
                group.url
            ),
        );
    }
}

fn check_via(group: &CaptureGroup, expected: Option<&str>, label: &str, report: &mut LintReport) {
    let actual = group
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.via.as_deref());
    if actual != expected {
        error(
            report,
            format!(
                "{label} has via {}, expected {}",
                display_optional(actual),
                display_optional(expected)
            ),
        );
    }
}

fn lint_pagination(
    groups: &[CaptureGroup],
    probes: &[Probe],
    checked_shapes: &mut HashSet<usize>,
    report: &mut LintReport,
) {
    let (mut pages, encountered) = collect_page_captures(groups, probes, report);

    let successful = probes
        .iter()
        .filter(|probe| probe.status.is_some_and(is_success))
        .map(|probe| probe.collection.name().to_owned())
        .collect::<Vec<_>>();
    let encountered_unique = encountered
        .iter()
        .filter(|name| pages.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if encountered_unique
        != successful
            .iter()
            .filter(|name| pages.contains_key(*name))
            .cloned()
            .collect::<Vec<_>>()
    {
        error(
            report,
            format!(
                "pagination endpoints are out of probe order: found {}, expected {}",
                encountered_unique.join(", "),
                successful.join(", ")
            ),
        );
    }
    let mut seen = HashSet::new();
    for name in &encountered {
        if !seen.insert(name) {
            error(
                report,
                format!("pagination series for {name} is interrupted by another endpoint"),
            );
        }
    }

    for probe in probes {
        let name = probe.collection.name();
        let captures = pages.remove(name).unwrap_or_default();
        let success = probe.status.is_some_and(is_success);
        if !success {
            if !captures.is_empty() {
                error(
                    report,
                    format!(
                        "unsuccessful {name} probe has a pagination series of {} captures",
                        captures.len()
                    ),
                );
            }
            continue;
        }
        if captures.is_empty() {
            error(
                report,
                format!("successful {name} probe has no pagination series"),
            );
            report.pagination.push(PaginationSummary {
                endpoint: probe.url.clone(),
                pages: None,
                items: probe.items,
            });
            continue;
        }
        let total_pages = lint_series(groups, probe, &captures, checked_shapes, report);
        report.pagination.push(PaginationSummary {
            endpoint: probe.url.clone(),
            pages: total_pages,
            items: probe.items,
        });
    }
}

fn collect_page_captures(
    groups: &[CaptureGroup],
    probes: &[Probe],
    report: &mut LintReport,
) -> (BTreeMap<String, Vec<PageCapture>>, Vec<String>) {
    let mut pages = BTreeMap::<String, Vec<PageCapture>>::new();
    let mut cutoff = None;
    let endpoints = probes
        .iter()
        .map(|probe| (probe.url.as_str(), probe.collection.name()))
        .collect::<Vec<_>>();
    let mut encountered = Vec::<String>::new();
    for (index, group) in groups.iter().enumerate() {
        let Some((_, name)) = endpoints
            .iter()
            .find(|(probe_url, _)| group.url.starts_with(&format!("{probe_url}?")))
        else {
            continue;
        };
        let (page, valid_query, before) = page_query(&group.url);
        if let Some(before) = before {
            match &cutoff {
                None => cutoff = Some(before),
                Some(expected) if expected != &before => error(
                    report,
                    format!(
                        "pagination request {} has a conflicting before cutoff",
                        group.url
                    ),
                ),
                Some(_) => {}
            }
        }
        if encountered.last().is_none_or(|previous| previous != *name) {
            encountered.push((*name).to_owned());
        }
        pages
            .entry((*name).to_owned())
            .or_default()
            .push(PageCapture {
                group: index,
                page,
                valid_query,
            });
    }
    (pages, encountered)
}

fn page_query(url: &str) -> (Option<usize>, bool, Option<String>) {
    let Ok(url) = Url::parse(url) else {
        return (None, false, None);
    };
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for (name, value) in url.query_pairs() {
        values
            .entry(name.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    let one = |name: &str| {
        values
            .get(name)
            .and_then(|values| (values.len() == 1).then(|| values[0].as_str()))
    };
    let page = one("page").and_then(|value| value.parse::<usize>().ok());
    let before = one("before").map(str::to_owned);
    let valid = values.len() == 5
        && page.is_some_and(|page| page > 0)
        && before
            .as_deref()
            .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
        && one("orderby") == Some("id")
        && one("order") == Some("asc")
        && one("per_page")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|per_page| (1..=100).contains(&per_page))
        && url.fragment().is_none();
    (page, valid, before)
}

fn lint_series(
    groups: &[CaptureGroup],
    probe: &Probe,
    captures: &[PageCapture],
    checked_shapes: &mut HashSet<usize>,
    report: &mut LintReport,
) -> Option<usize> {
    let name = probe.collection.name();
    let split = captures
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, capture)| (capture.page == Some(1)).then_some(index));
    let (pagination, legacy_validation) =
        split.map_or((captures, &[][..]), |index| captures.split_at(index));
    let total_pages = pagination
        .iter()
        .filter_map(|capture| response_metadata(&groups[capture.group]))
        .filter_map(|metadata| numeric_header(&metadata, "x-wp-totalpages"))
        .max();

    lint_pass(
        groups,
        name,
        pagination,
        &probe.url,
        total_pages,
        "pagination",
        checked_shapes,
        report,
    );
    if !legacy_validation.is_empty() {
        if total_pages.is_some_and(|pages| pages > 1) {
            let via = pagination.last().map_or(probe.url.as_str(), |capture| {
                groups[capture.group].url.as_str()
            });
            lint_pass(
                groups,
                name,
                legacy_validation,
                via,
                total_pages,
                "legacy validation",
                checked_shapes,
                report,
            );
        } else {
            error(
                report,
                format!("{name} pagination series has an unnecessary second pass"),
            );
        }
    }
    total_pages
}

#[allow(clippy::too_many_arguments)]
fn lint_pass(
    groups: &[CaptureGroup],
    name: &str,
    captures: &[PageCapture],
    first_via: &str,
    expected_total: Option<usize>,
    pass: &str,
    checked_shapes: &mut HashSet<usize>,
    report: &mut LintReport,
) {
    for (position, capture) in captures.iter().enumerate() {
        let expected_via = if position == 0 {
            first_via
        } else {
            &groups[captures[position - 1].group].url
        };
        lint_page_capture(
            groups,
            capture,
            position,
            expected_via,
            name,
            pass,
            checked_shapes,
            report,
        );
    }

    if let Some(total) = expected_total
        && captures.len() != total.max(1)
    {
        error(
            report,
            format!(
                "{name} {pass} pass has {} captures, expected {} for {total} advertised pages",
                captures.len(),
                total.max(1)
            ),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn lint_page_capture(
    groups: &[CaptureGroup],
    capture: &PageCapture,
    position: usize,
    expected_via: &str,
    name: &str,
    pass: &str,
    checked_shapes: &mut HashSet<usize>,
    report: &mut LintReport,
) {
    let group = &groups[capture.group];
    check_capture_shape(groups, capture.group, checked_shapes, report);
    if !capture.valid_query {
        error(
            report,
            format!(
                "{name} {pass} pagination URI has the wrong shape: {}",
                group.url
            ),
        );
    }
    let expected_page = position + 1;
    if capture.page != Some(expected_page) {
        error(
            report,
            format!(
                "{name} {pass} pagination capture {} is page {}, expected page {expected_page}",
                position + 1,
                capture
                    .page
                    .map_or_else(|| "?".to_owned(), |page| page.to_string())
            ),
        );
    }
    check_via(
        group,
        Some(expected_via),
        &format!("{name} {pass} page {expected_page}"),
        report,
    );

    let Some(metadata) = response_metadata(group) else {
        return;
    };
    if !matches!(metadata.status, 200 | 304) {
        error(
            report,
            format!(
                "{name} {pass} page {expected_page} has unexpected HTTP status {}",
                metadata.status
            ),
        );
    }
    let advertised = numeric_header(&metadata, "x-wp-totalpages");
    if advertised.is_none() {
        error(
            report,
            format!("{name} {pass} page {expected_page} has missing or invalid X-WP-TotalPages"),
        );
    }
    check_json_array(
        groups,
        capture.group,
        &metadata,
        name,
        pass,
        expected_page,
        report,
    );
    check_header_links(
        group,
        &metadata,
        expected_page,
        advertised,
        name,
        pass,
        report,
    );
}

fn check_json_array(
    groups: &[CaptureGroup],
    group: usize,
    metadata: &http::ResponseMetadata,
    name: &str,
    pass: &str,
    page: usize,
    report: &mut LintReport,
) {
    if metadata.status != 200 {
        return;
    }
    let valid = match resolved_payload(groups, group) {
        ResolvedPayload::Block(body) => payload::entity_body(body)
            .ok()
            .and_then(|entity| serde_json::from_slice::<Value>(&entity).ok())
            .is_some_and(|value| value.is_array()),
        ResolvedPayload::IdentifiedJson => true,
        ResolvedPayload::Missing => false,
    };
    if !valid {
        error(
            report,
            format!("{name} {pass} page {page} response is not a JSON array"),
        );
    }
}

fn check_probe_response(
    groups: &[CaptureGroup],
    group: usize,
    metadata: &http::ResponseMetadata,
    name: &str,
    report: &mut LintReport,
) {
    if numeric_header(metadata, "x-wp-totalpages").is_none() {
        error(
            report,
            format!("successful {name} probe has missing or invalid X-WP-TotalPages"),
        );
    }
    if metadata.status == 200 {
        let valid = match resolved_payload(groups, group) {
            ResolvedPayload::Block(body) => payload::entity_body(body)
                .ok()
                .and_then(|entity| serde_json::from_slice::<Value>(&entity).ok())
                .is_some_and(|value| value.is_array()),
            ResolvedPayload::IdentifiedJson => true,
            ResolvedPayload::Missing => false,
        };
        if !valid {
            error(
                report,
                format!("successful {name} probe response is not a JSON array"),
            );
        }
    }
}

const fn intentional_revisit_truncation(response: &StoredResponse, reason: &str) -> bool {
    reason.eq_ignore_ascii_case("length")
        && matches!(
            &response.revisit,
            Some(StoredRevisit {
                profile: StoredRevisitProfile::IdenticalPayloadDigest
                    | StoredRevisitProfile::ServerNotModified,
                ..
            })
        )
}

enum ResolvedPayload<'a> {
    Block(&'a [u8]),
    IdentifiedJson,
    Missing,
}

fn resolved_payload(groups: &[CaptureGroup], mut group: usize) -> ResolvedPayload<'_> {
    let mut visited = HashSet::new();
    while visited.insert(group) {
        let Some(response) = groups.get(group).and_then(|group| group.response.as_ref()) else {
            return ResolvedPayload::Missing;
        };
        let Some(revisit) = &response.revisit else {
            return ResolvedPayload::Block(&response.body);
        };
        if let Some(original) = revisit.original {
            group = original;
        } else if revisit.identified_json {
            return ResolvedPayload::IdentifiedJson;
        } else {
            return ResolvedPayload::Missing;
        }
    }
    ResolvedPayload::Missing
}

fn check_header_links(
    group: &CaptureGroup,
    metadata: &http::ResponseMetadata,
    page: usize,
    total: Option<usize>,
    name: &str,
    pass: &str,
    report: &mut LintReport,
) {
    let Some(total) = total else {
        return;
    };
    for (relation, expected_page) in [
        ("prev", (page > 1).then_some(page - 1)),
        ("next", (page < total).then_some(page + 1)),
    ] {
        let Some(expected_page) = expected_page else {
            continue;
        };
        let found = metadata
            .headers("link")
            .filter_map(|value| std::str::from_utf8(value).ok())
            .flat_map(parse_links)
            .any(|(target, relations)| {
                relations.iter().any(|value| value == relation)
                    && same_page_uri(&target, &group.url, expected_page)
            });
        if !found {
            warning(
                report,
                format!(
                    "{name} {pass} page {page} has no expected {relation} Link to page {expected_page}"
                ),
            );
        }
    }
}

fn parse_links(value: &str) -> impl Iterator<Item = (String, Vec<String>)> + '_ {
    value.split(',').filter_map(|part| {
        let part = part.trim();
        let end = part.find('>')?;
        let target = part.get(1..end)?.to_owned();
        let relations = part[end + 1..]
            .split(';')
            .filter_map(|parameter| parameter.trim().split_once('='))
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("rel").then(|| {
                    value
                        .trim_matches('"')
                        .split_ascii_whitespace()
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        Some((target, relations))
    })
}

fn same_page_uri(candidate: &str, current: &str, expected_page: usize) -> bool {
    let (Ok(candidate), Ok(current)) = (Url::parse(candidate), Url::parse(current)) else {
        return false;
    };
    if candidate.scheme() != current.scheme()
        || candidate.host_str() != current.host_str()
        || candidate.port_or_known_default() != current.port_or_known_default()
        || candidate.path() != current.path()
    {
        return false;
    }
    let query = |url: &Url| {
        url.query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<BTreeMap<_, _>>()
    };
    let mut expected = query(&current);
    expected.insert("page".to_owned(), expected_page.to_string());
    query(&candidate) == expected
}

fn response_metadata(group: &CaptureGroup) -> Option<http::ResponseMetadata> {
    group
        .response
        .as_ref()
        .and_then(|response| http::ResponseMetadata::parse(&response.body))
}

fn is_server_challenge(group: &CaptureGroup) -> bool {
    let Some(response) = &group.response else {
        return false;
    };
    let Some(metadata) = http::ResponseMetadata::parse(&response.body) else {
        return false;
    };
    if metadata.status == 403
        && metadata
            .header("cf-mitigated")
            .is_some_and(|value| value.eq_ignore_ascii_case(b"challenge"))
    {
        return true;
    }
    if metadata.status != 454 {
        return false;
    }
    payload::entity_body(&response.body).is_ok_and(|entity| {
        contains_bytes(&entity, b"sc-challenge") && contains_bytes(&entity, b"/.sc-verify/")
    })
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn numeric_header(metadata: &http::ResponseMetadata, name: &str) -> Option<usize> {
    metadata
        .header(name)
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.trim().parse().ok())
}

const fn is_success(status: u16) -> bool {
    (status >= 200 && status < 300) || status == 304
}

fn endpoint_url(site: &Site, name: &str) -> String {
    format!("{}wp-json/wp/v2/{name}", site.root())
}

fn display_optional(value: Option<&str>) -> String {
    value.map_or_else(|| "no value".to_owned(), |value| format!("{value:?}"))
}

fn error(report: &mut LintReport, message: String) {
    report.findings.push(Finding {
        severity: Severity::Error,
        message,
    });
}

fn warning(report: &mut LintReport, message: String) {
    report.findings.push(Finding {
        severity: Severity::Warning,
        message,
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        CaptureGroup, LintReport, PageCapture, Probe, StoredMetadata, StoredResponse,
        StoredRevisit, StoredRevisitProfile, expected_initial, initial_capture_candidates,
        is_server_challenge, lint_series, page_query, same_page_uri,
    };
    use crate::archive::Site;
    use crate::endpoint::{Endpoint, ROOT_ENDPOINTS};

    const BEFORE: &str = "2026-08-20T00:00:00Z";

    fn endpoint() -> String {
        "https://example.com/wp-json/wp/v2/posts".to_owned()
    }

    fn page(number: usize) -> String {
        format!(
            "{}?before={BEFORE}&orderby=id&order=asc&page={number}&per_page=100",
            endpoint()
        )
    }

    fn capture(number: usize, via: &str, total: usize) -> CaptureGroup {
        let url = page(number);
        let links = [
            (number > 1).then(|| format!("<{}>; rel=\"prev\"", page(number - 1))),
            (number < total).then(|| format!("<{}>; rel=\"next\"", page(number + 1))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let link = (!links.is_empty()).then(|| format!("Link: {links}\r\n"));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             X-WP-TotalPages: {total}\r\n{}Content-Length: 2\r\n\r\n[]",
            link.as_deref().unwrap_or("")
        );
        CaptureGroup {
            url: url.clone(),
            response: Some(StoredResponse {
                url: url.clone(),
                body: response.into_bytes(),
                truncation: None,
                revisit: None,
            }),
            metadata: Some(StoredMetadata {
                url: Some(url),
                via: Some(via.to_owned()),
                fields: true,
            }),
        }
    }

    fn root_capture(status: u16, body: &str) -> CaptureGroup {
        let url = "https://example.com/wp-json".to_owned();
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        CaptureGroup {
            url: url.clone(),
            response: Some(StoredResponse {
                url,
                body: response.into_bytes(),
                truncation: None,
                revisit: None,
            }),
            metadata: None,
        }
    }

    fn revisit(number: usize, via: &str, total: usize, original: usize) -> CaptureGroup {
        let mut group = capture(number, via, total);
        let response = group.response.as_mut().expect("a stored response");
        let body_offset = response
            .body
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
            .expect("an HTTP response head");
        response.body.truncate(body_offset);
        response.truncation = Some("length".to_owned());
        response.revisit = Some(StoredRevisit {
            profile: StoredRevisitProfile::IdenticalPayloadDigest,
            original: Some(original),
            identified_json: true,
        });
        group
    }

    fn pages(groups: &[CaptureGroup]) -> Vec<PageCapture> {
        groups
            .iter()
            .enumerate()
            .map(|(group, capture)| {
                let (page, valid_query, _) = page_query(&capture.url);
                PageCapture {
                    group,
                    page,
                    valid_query,
                }
            })
            .collect()
    }

    fn probe() -> Probe {
        Probe {
            collection: Endpoint::Posts.into(),
            url: endpoint(),
            status: Some(200),
            items: Some(101),
        }
    }

    #[test]
    fn initial_capture_order_uses_the_supported_endpoint_order() {
        let site = Site::parse("example.com").expect("a site");
        let initial = expected_initial(&site, &[]);
        let endpoints = initial[ROOT_ENDPOINTS.len()..]
            .iter()
            .map(|(_, collection)| {
                collection
                    .as_ref()
                    .expect("an endpoint follows the roots")
                    .name()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            endpoints,
            [
                "pages",
                "posts",
                "media",
                "comments",
                "users",
                "categories",
                "tags",
                "navigation",
            ]
        );
    }

    #[test]
    fn simply_browser_challenges_do_not_count_as_initial_captures() {
        let challenge = root_capture(
            454,
            r#"<script sc-challenge>fetch('/.sc-verify/')</script>"#,
        );
        assert!(is_server_challenge(&challenge));

        let groups = [challenge, root_capture(200, "{}")];
        assert_eq!(
            initial_capture_candidates(&groups, "https://example.com/wp-json"),
            [1]
        );
    }

    #[test]
    fn ordinary_error_responses_still_count_as_initial_captures() {
        let groups = [
            root_capture(454, "ordinary server error"),
            root_capture(200, "{}"),
        ];
        assert_eq!(
            initial_capture_candidates(&groups, "https://example.com/wp-json"),
            [0, 1]
        );
    }

    #[test]
    fn paging_query_requires_the_archive_shape() {
        assert_eq!(
            page_query(&page(2)),
            (Some(2), true, Some(BEFORE.to_owned()))
        );
        assert!(!page_query(&format!("{}?page=2", endpoint())).1);
        assert!(!page_query(&page(0)).1);
        assert!(!page_query(&format!("{}&extra=yes", page(2))).1);
    }

    #[test]
    fn link_targets_are_compared_by_query_values() {
        let encoded = page(2).replace(BEFORE, "2026-08-20T00%3A00%3A00Z");
        assert!(same_page_uri(&encoded, &page(1), 2));
        assert!(!same_page_uri(&page(3), &page(1), 2));
    }

    #[test]
    fn a_multi_page_series_requires_one_complete_pagination_pass() {
        let probe = probe();
        let mut groups = vec![capture(1, &probe.url, 2)];
        groups.push(capture(2, &groups[0].url, 2));
        let captures = pages(&groups);
        let mut report = LintReport::default();

        assert_eq!(
            lint_series(&groups, &probe, &captures, &mut HashSet::new(), &mut report,),
            Some(2)
        );
        assert!(report.is_clean(), "{:?}", report.findings);

        let mut repeated_groups = vec![capture(1, &probe.url, 2)];
        repeated_groups.push(capture(2, &repeated_groups[0].url, 2));
        repeated_groups.push(revisit(1, &repeated_groups[1].url, 2, 0));
        repeated_groups.push(revisit(2, &repeated_groups[2].url, 2, 1));
        let repeated_captures = pages(&repeated_groups);
        let mut legacy = LintReport::default();
        lint_series(
            &repeated_groups,
            &probe,
            &repeated_captures,
            &mut HashSet::new(),
            &mut legacy,
        );
        assert!(legacy.is_clean(), "{:?}", legacy.findings);
    }

    #[test]
    fn an_empty_collection_still_has_page_one() {
        let mut probe = probe();
        probe.items = Some(0);
        let groups = vec![capture(1, &probe.url, 0)];
        let captures = pages(&groups);
        let mut report = LintReport::default();

        assert_eq!(
            lint_series(&groups, &probe, &captures, &mut HashSet::new(), &mut report,),
            Some(0)
        );
        assert!(report.is_clean(), "{:?}", report.findings);

        let mut groups = groups;
        groups.push(capture(1, &groups[0].url, 0));
        let captures = pages(&groups);
        let mut repeated = LintReport::default();
        lint_series(
            &groups,
            &probe,
            &captures,
            &mut HashSet::new(),
            &mut repeated,
        );
        assert!(repeated.findings.iter().any(|finding| {
            finding.message == "posts pagination series has an unnecessary second pass"
        }));
    }
}
