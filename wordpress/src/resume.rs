//! Recovering the continuation point of a collection archive from its WARC records.
//!
//! [`inspect_archive`] links each request, response or revisit, and metadata record into a
//! capture, then replays complete captures through [`ArchiveDriver`]. This uses the same endpoint
//! discovery and checkpoint transitions as a live archive. A broken capture is reported but does
//! not prevent the last durable checkpoint from being returned.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use archivindex_archiver::session::{Capture, Driver};
use archivindex_warc::io::read::WarcReader;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::{FieldsBlock, Record, http, payload};
use chrono::{DateTime, Utc};
use url::Url;

use crate::archive::{ArchiveDriver, Checkpoint, ProbeResult, Resumption, Site, SiteError};
use crate::endpoint::{Collection, Endpoint, EndpointType, ROOT_ENDPOINTS, Registry};

/// The recovered state of one collection-archive WARC.
#[derive(Clone, Debug)]
pub struct ResumeInfo {
    /// The `WordPress` installation whose resources the WARC captures.
    pub site: Site,
    /// The fixed archive cutoff, when at least one paginated request records it.
    pub before: Option<DateTime<Utc>>,
    /// The last checkpoint backed by a complete capture group.
    pub checkpoint: Checkpoint,
    /// Supported and custom collections in their final probing order.
    pub endpoints: Vec<Collection>,
    /// Probe results captured in or restored while replaying this WARC.
    pub probes: Vec<ProbeResult>,
    /// Problems found while linking or replaying records.
    pub warnings: Vec<String>,
}

/// An archive cannot be inspected for continuation.
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
    /// The inferred installation URL is not a valid archive site.
    #[error("cannot infer a WordPress site from {url:?}: {source}")]
    Site {
        /// Request URL used for inference.
        url: String,
        /// Why the inferred site was invalid.
        #[source]
        source: SiteError,
    },
    /// A resumed WARC does not begin at a recognizable collection request.
    #[error("cannot infer the starting archive checkpoint from {0:?}")]
    StartingRequest(String),
}

/// Inspect a plain or gzip-compressed WARC and recover its last durable archive checkpoint.
///
/// A request, its linked response or revisit, and the response's linked metadata record must all
/// be present for the capture to advance the checkpoint. Missing, orphaned, duplicated, truncated,
/// or unreadable capture records are returned as warnings. Record parsing errors remain fatal
/// because the next record boundary cannot be trusted after one.
///
/// # Errors
///
/// Returns [`Error`] when the file cannot be read, is not a semantic WARC, or does not contain
/// enough `WordPress` request information to identify the archive traversal.
pub fn inspect_archive(path: impl AsRef<Path>) -> Result<ResumeInfo, Error> {
    inspect_archive_with_restored_probes(path, &[], None)
}

/// Inspect one continuation WARC using probe results recovered from its archive's initial WARC.
///
/// Restored probes let replay advance directly from the continued collection to later successful
/// collections, including for continuation files produced without redundant bare probes.
/// `before` supplies the initial segment's cutoff when this segment contains no paginated URL
/// from which to recover it.
///
/// # Errors
///
/// Returns [`Error`] under the same conditions as [`inspect_archive`].
pub fn inspect_archive_with_restored_probes(
    path: impl AsRef<Path>,
    probes: &[ProbeResult],
    before: Option<DateTime<Utc>>,
) -> Result<ResumeInfo, Error> {
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
        inspect_reader(reader, path, probes, before)
    } else {
        let reader = WarcReader::from_path(path).map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        inspect_reader(reader, path, probes, before)
    }
}

#[derive(Debug)]
struct CaptureGroup {
    url: String,
    response: Option<StoredResponse>,
    metadata: bool,
    via: Option<String>,
}

#[derive(Debug)]
struct StoredResponse {
    url: String,
    body: Vec<u8>,
    truncated: bool,
}

impl CaptureGroup {
    fn problem(&self) -> Option<String> {
        let mut missing = Vec::new();
        if self.response.is_none() {
            missing.push("response or revisit");
        }
        if !self.metadata {
            missing.push("metadata");
        }
        if !missing.is_empty() {
            return Some(format!(
                "capture of {} is missing {}",
                self.url,
                missing.join(" and ")
            ));
        }
        self.response
            .as_ref()
            .is_some_and(|response| response.truncated)
            .then(|| format!("capture of {} has a truncated response", self.url))
    }
}

fn inspect_reader<R: BufRead>(
    reader: WarcReader<R>,
    path: &Path,
    restored_probes: &[ProbeResult],
    restored_before: Option<DateTime<Utc>>,
) -> Result<ResumeInfo, Error> {
    let (groups, mut warnings) = collect_groups(reader, path)?;
    let site = groups
        .iter()
        .find_map(|group| site_from_request(&group.url))
        .transpose()?
        .ok_or_else(|| Error::NoWordPressRequests(path.to_owned()))?;
    let before = groups.iter().find_map(|group| cutoff(&group.url));
    if let Some(before) = before
        && groups
            .iter()
            .filter_map(|group| cutoff(&group.url))
            .any(|candidate| candidate != before)
    {
        warnings.push(format!(
            "paginated requests contain conflicting before cutoffs; using {}",
            before.to_rfc3339()
        ));
    }
    let custom = custom_collections(&groups, &site, &mut warnings);
    let first = groups
        .iter()
        .find(|group| belongs_to_site(&group.url, &site))
        .ok_or_else(|| Error::NoWordPressRequests(path.to_owned()))?;
    let before_or_epoch = before
        .or(restored_before)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let mut driver = if restored_probes.is_empty() || first.url == format!("{}wp-json", site.root())
    {
        initial_driver(first, &groups, site.clone(), before_or_epoch, &custom)?
    } else {
        let resumption = starting_resumption(
            first,
            &groups,
            &site,
            restored_probes.iter().map(|probe| &probe.collection),
        )?;
        ArchiveDriver::resume_with_probes(
            site.clone(),
            before_or_epoch,
            resumption,
            restored_probes.to_vec(),
        )
    };

    replay(&groups, &mut driver, &mut warnings);
    let checkpoint = driver.checkpoint();
    let probes = if restored_probes.is_empty() {
        driver.probe_results()
    } else {
        restored_probes.to_vec()
    };
    let endpoints = merge_endpoints(driver.endpoints(), &custom);

    Ok(ResumeInfo {
        site,
        before,
        checkpoint,
        endpoints,
        probes,
        warnings,
    })
}

fn collect_groups<R: BufRead>(
    reader: WarcReader<R>,
    path: &Path,
) -> Result<(Vec<CaptureGroup>, Vec<String>), Error> {
    let mut groups = Vec::<CaptureGroup>::new();
    let mut requests = HashMap::<String, usize>::new();
    let mut responses = HashMap::<String, usize>::new();
    let mut warnings = Vec::new();

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
                    warnings.push(format!("duplicate request record ID {id}"));
                }
                groups.push(CaptureGroup {
                    url: header.target_uri.into_string(),
                    response: None,
                    metadata: false,
                    via: None,
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
                    &mut warnings,
                    header.core.record_id.into_string(),
                    header.target_uri.into_string(),
                    request,
                    header.core.truncated.is_some(),
                    body,
                );
            }
            Record::Revisit { header, body } => {
                let request = header
                    .concurrent_to
                    .iter()
                    .find_map(|record_id| requests.get(record_id.as_str()).copied());
                attach_response(
                    &mut groups,
                    &mut responses,
                    &mut warnings,
                    header.core.record_id.into_string(),
                    header.target_uri.into_string(),
                    request,
                    header.core.truncated.is_some(),
                    body,
                );
            }
            Record::Metadata { header, body } => {
                let id = header.core.record_id.into_string();
                let Some(index) = header
                    .concurrent_to
                    .iter()
                    .find_map(|record_id| responses.get(record_id.as_str()).copied())
                else {
                    warnings.push(format!(
                        "metadata record {id} has no linked response or revisit"
                    ));
                    continue;
                };
                let group = &mut groups[index];
                if group.metadata {
                    warnings.push(format!("capture of {} has duplicate metadata", group.url));
                }
                group.metadata = true;
                if let FieldsBlock::Fields(fields) = body {
                    group.via = fields.via().map(str::to_owned);
                }
            }
            Record::Warcinfo { .. }
            | Record::Resource { .. }
            | Record::Conversion { .. }
            | Record::Continuation { .. }
            | Record::Other { .. } => {}
        }
    }

    warnings.extend(groups.iter().filter_map(CaptureGroup::problem));
    Ok((groups, warnings))
}

#[allow(clippy::too_many_arguments)]
fn attach_response(
    groups: &mut [CaptureGroup],
    responses: &mut HashMap<String, usize>,
    warnings: &mut Vec<String>,
    id: String,
    url: String,
    request: Option<usize>,
    truncated: bool,
    body: Vec<u8>,
) {
    let Some(index) = request else {
        warnings.push(format!(
            "response or revisit record {id} has no linked request"
        ));
        return;
    };
    let group = &mut groups[index];
    if group.response.is_some() {
        warnings.push(format!("capture of {} has duplicate responses", group.url));
        return;
    }
    responses.insert(id, index);
    group.response = Some(StoredResponse {
        url,
        body,
        truncated,
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

fn belongs_to_site(request: &str, site: &Site) -> bool {
    request.starts_with(site.root().as_str())
        && request[site.root().as_str().len()..].starts_with("wp-json")
}

fn cutoff(request: &str) -> Option<DateTime<Utc>> {
    Url::parse(request)
        .ok()?
        .query_pairs()
        .find_map(|(name, value)| (name == "before").then_some(value))
        .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn custom_collections(
    groups: &[CaptureGroup],
    site: &Site,
    warnings: &mut Vec<String>,
) -> Vec<Collection> {
    let mut custom = Vec::new();
    for registry in Registry::ALL {
        let url = format!("{}{}", site.root(), registry.path());
        for group in groups.iter().filter(|group| group.url == url) {
            let Some(response) = group.response.as_ref() else {
                continue;
            };
            let Some(metadata) = http::ResponseMetadata::parse(&response.body) else {
                continue;
            };
            if !(200..300).contains(&metadata.status) {
                continue;
            }
            let Ok(entity) = payload::entity_body(&response.body) else {
                continue;
            };
            let Ok(entries) = EndpointType::parse_registry(&entity) else {
                continue;
            };
            for name in EndpointType::custom_endpoints(&entries) {
                push_custom(&mut custom, name, registry);
            }
        }
    }

    for group in groups {
        let Some(name) = collection_name(&group.url, site) else {
            continue;
        };
        if name.parse::<Endpoint>().is_ok()
            || ROOT_ENDPOINTS
                .iter()
                .any(|root| root.strip_prefix("wp-json/wp/v2/") == Some(name.as_str()))
        {
            continue;
        }
        let registry = group
            .via
            .as_deref()
            .and_then(|via| Registry::ALL.into_iter().find(|registry| via.ends_with(registry.path())))
            .unwrap_or_else(|| {
                if !custom.iter().any(|endpoint| endpoint.name() == name) {
                    warnings.push(format!(
                        "could not determine the registry for custom endpoint {name:?}; assuming types"
                    ));
                }
                Registry::Types
            });
        push_custom(&mut custom, &name, registry);
    }

    custom
}

fn push_custom(custom: &mut Vec<Collection>, name: &str, registry: Registry) {
    if !custom.iter().any(|endpoint| endpoint.name() == name) {
        custom.push(Collection::Custom {
            name: name.to_owned(),
            registry,
        });
    }
}

fn collection_name(request: &str, site: &Site) -> Option<String> {
    let url = Url::parse(request).ok()?;
    let prefix = format!("{}wp-json/wp/v2/", site.root().path());
    url.path()
        .strip_prefix(&prefix)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn initial_driver(
    first: &CaptureGroup,
    groups: &[CaptureGroup],
    site: Site,
    before: DateTime<Utc>,
    custom: &[Collection],
) -> Result<ArchiveDriver, Error> {
    if first.url == format!("{}wp-json", site.root()) {
        return Ok(ArchiveDriver::new(site, before));
    }
    let resumption = starting_resumption(first, groups, &site, custom.iter())?;
    Ok(ArchiveDriver::resume(
        site,
        before,
        resumption,
        custom.to_vec(),
    ))
}

fn starting_resumption<'a>(
    first: &CaptureGroup,
    groups: &[CaptureGroup],
    site: &Site,
    endpoints: impl IntoIterator<Item = &'a Collection>,
) -> Result<Resumption, Error> {
    let name = collection_name(&first.url, site)
        .ok_or_else(|| Error::StartingRequest(first.url.clone()))?;
    let endpoint = endpoints
        .into_iter()
        .find(|endpoint| endpoint.name() == name)
        .cloned()
        .or_else(|| name.parse::<Endpoint>().ok().map(Collection::Known))
        .ok_or_else(|| Error::StartingRequest(first.url.clone()))?;
    let page = Url::parse(&first.url)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find_map(|(key, value)| (key == "page").then_some(value))
                .and_then(|value| value.parse::<usize>().ok())
        })
        .unwrap_or(0);
    let total_pages = groups
        .iter()
        .filter(|group| collection_name(&group.url, site).as_deref() == Some(name.as_str()))
        .filter_map(|group| group.response.as_ref())
        .filter_map(|response| http::ResponseMetadata::parse(&response.body))
        .filter_map(|metadata| {
            metadata
                .header("x-wp-totalpages")
                .and_then(|value| std::str::from_utf8(value).ok())
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .max();
    Ok(Resumption {
        endpoint,
        last_page: page.saturating_sub(1),
        total_pages,
    })
}

fn replay(groups: &[CaptureGroup], driver: &mut ArchiveDriver, warnings: &mut Vec<String>) {
    let mut next_group = 0;
    while let Some(request) = driver.next() {
        let Some(index) = groups[next_group..]
            .iter()
            .position(|group| group.url == request.url)
            .map(|index| next_group + index)
        else {
            break;
        };
        let mut attempt = index;
        let terminal = loop {
            let Some(chain) = redirect_chain(groups, attempt, &request.url, warnings) else {
                return;
            };
            let (after, terminal) = match chain {
                Chain::Complete { after, terminal } => (after, terminal),
                Chain::Retry { next } => {
                    attempt = next;
                    continue;
                }
            };
            next_group = after;
            let Some(response) = terminal.response.as_ref() else {
                return;
            };
            let Some(metadata) = http::ResponseMetadata::parse(&response.body) else {
                warnings.push(format!(
                    "capture of {} has an invalid HTTP response",
                    terminal.url
                ));
                return;
            };
            if is_retryable(metadata.status)
                && groups
                    .get(next_group)
                    .is_some_and(|group| group.url == request.url)
            {
                attempt = next_group;
                continue;
            }
            break terminal;
        };

        let Some(response) = terminal.response.as_ref() else {
            break;
        };
        let payload = match payload::entity_body(&response.body) {
            Ok(payload) => payload,
            Err(error) => {
                warnings.push(format!(
                    "cannot read the payload captured from {}: {error}",
                    terminal.url
                ));
                break;
            }
        };
        let Some(capture) = Capture::new(&request.url, &response.url, &payload, &response.body)
        else {
            warnings.push(format!(
                "capture of {} has an invalid HTTP response",
                terminal.url
            ));
            break;
        };
        let inspection = driver.inspect(&capture);
        if let Some(error) = inspection.error {
            warnings.push(format!(
                "archive traversal stopped after {}: {error}",
                request.url
            ));
            break;
        }
    }
}

enum Chain<'a> {
    Complete {
        after: usize,
        terminal: &'a CaptureGroup,
    },
    Retry {
        next: usize,
    },
}

/// Follow the complete WARC capture groups belonging to one redirect chain.
fn redirect_chain<'a>(
    groups: &'a [CaptureGroup],
    start: usize,
    requested: &str,
    warnings: &mut Vec<String>,
) -> Option<Chain<'a>> {
    let mut index = start;
    loop {
        let group = groups.get(index)?;
        if !group.metadata {
            return None;
        }
        let response = group.response.as_ref()?;
        if response.truncated {
            let next = index + 1;
            return groups
                .get(next)
                .is_some_and(|group| group.url == requested)
                .then_some(Chain::Retry { next });
        }
        let Some(metadata) = http::ResponseMetadata::parse(&response.body) else {
            warnings.push(format!(
                "capture of {} has an invalid HTTP response",
                group.url
            ));
            return None;
        };
        if !(300..400).contains(&metadata.status) || metadata.status == 304 {
            return Some(Chain::Complete {
                after: index + 1,
                terminal: group,
            });
        }
        let Some(location) = metadata
            .header("location")
            .and_then(|value| std::str::from_utf8(value).ok())
        else {
            warnings.push(format!(
                "redirect captured from {} has no readable Location header",
                group.url
            ));
            return None;
        };
        let Some(target) = Url::parse(&response.url)
            .ok()
            .and_then(|base| base.join(location).ok())
        else {
            warnings.push(format!(
                "redirect captured from {} has an invalid Location header {location:?}",
                group.url
            ));
            return None;
        };
        index += 1;
        if groups
            .get(index)
            .is_none_or(|next| next.url != target.as_str())
        {
            warnings.push(format!(
                "redirect captured from {} is missing its request to {}",
                group.url, target
            ));
            return None;
        }
    }
}

const fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn merge_endpoints(discovered: &[Collection], inferred: &[Collection]) -> Vec<Collection> {
    let mut endpoints = discovered.to_vec();
    for endpoint in inferred {
        if !endpoints
            .iter()
            .any(|existing| existing.name() == endpoint.name())
        {
            endpoints.push(endpoint.clone());
        }
    }
    endpoints
}
