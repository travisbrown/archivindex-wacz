//! Layered WACZ conformance validation.
//!
//! [`WaczReader::validate`] checks an archive against the requirements of the [WACZ 1.1.1
//! specification](https://specs.webrecorder.net/wacz/1.1.1/) and reports findings in layers,
//! keeping structural conformance violations, metadata parse failures, and digest mismatches
//! distinguishable. The layout, manifest, and signature layers always run and read only the ZIP
//! central directory and the two metadata files; the fixity, content, and index layers each read
//! complete members and are selected through [`ValidationOptions`].
//!
//! Duplicate ZIP entry names are reported before name-based validation selects an entry.

use std::collections::HashSet;
use std::io::{Read, Seek};

use archivindex_cdx::cdxj::Item;
use archivindex_cdx::timestamp::Timestamp;
use archivindex_surt::url::Canonicalizer;
use archivindex_warc::record::Record;
use archivindex_warc::record::extension::NoExtension;
use archivindex_warc::record::http::ResponseMetadata;
use archivindex_warc::value::LabelledDigest;

use super::{DigestFile, Error, Fixity, WaczReader};
use crate::digest::Sha256Digest;
use crate::frictionless::{DataPackage, PROFILE, WACZ_VERSION};
use crate::{
    DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, PAGES_PATH, PAGES_PREFIX, cdxj, frictionless,
};

/// The layers of [`WaczReader::validate`] that read complete members.
///
/// The layout, manifest, and signature layers always run because they read only the ZIP central
/// directory and the two metadata files. The default selects none of the expensive layers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValidationOptions {
    /// Check every manifest resource against its declared size and SHA-256 digest, as
    /// [`WaczReader::verify_fixity`] does.
    pub fixity: bool,
    /// Parse every page list, index, and WARC member, reporting the first problem in each.
    pub content: bool,
    /// Check every index entry against its WARC record and verify every `ZipNum` block digest.
    ///
    /// Selecting this layer also runs the content layer, since an index entry that does not
    /// resolve is only meaningful for an index that parses.
    pub index: bool,
}

impl ValidationOptions {
    /// Select every layer.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            fixity: true,
            content: true,
            index: true,
        }
    }
}

/// The findings of [`WaczReader::validate`], one collection per layer.
///
/// The `content` and `index` fields are `None` when their layers were not selected, so an empty
/// vector always means the layer ran and found nothing.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ValidationReport {
    /// Required members that are absent from the ZIP container.
    pub layout: Vec<LayoutProblem>,
    /// Ways the manifest violates the WACZ or Frictionless Data Package specifications, including
    /// members that the manifest does not account for.
    pub manifest: Vec<ManifestProblem>,
    /// Members whose content does not parse, if the content layer ran.
    pub content: Option<Vec<ContentProblem>>,
    /// Index entries and `ZipNum` blocks that do not match their data, if the index layer ran.
    pub index: Option<Vec<IndexProblem>>,
    /// The outcome of checking declared digests and sizes, if the fixity layer ran and the
    /// manifest was present and parseable.
    pub fixity: Option<Fixity>,
    /// What the `datapackage-digest.json` file claims and whether it is internally consistent.
    pub signature: SignatureStatus,
}

impl ValidationReport {
    /// Whether every layer that ran found no problems.
    ///
    /// Layers that were not selected do not affect the result, and neither does the difference
    /// between an absent, unsigned, or unverified (but consistent) signature.
    #[must_use]
    pub fn is_conformant(&self) -> bool {
        self.layout.is_empty()
            && self.manifest.is_empty()
            && !matches!(self.signature, SignatureStatus::Invalid(_))
            && self.content.as_ref().is_none_or(Vec::is_empty)
            && self.index.as_ref().is_none_or(Vec::is_empty)
            && self.fixity.as_ref().is_none_or(Fixity::is_success)
    }
}

/// A way the ZIP container's members violate the specification's layout rules.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, thiserror::Error)]
pub enum LayoutProblem {
    /// More than one ZIP central-directory entry has the same file name.
    #[error("duplicate ZIP member name: {0}")]
    DuplicateMember(String),
    /// A member under `archive/` is not a direct WARC path.
    #[error("invalid WARC member path: {0}")]
    InvalidWarcMember(String),
    /// A member under `indexes/` is neither a plain CDXJ index nor part of a complete `ZipNum` pair.
    #[error("invalid index member path or incomplete ZipNum pair: {0}")]
    InvalidIndexMember(String),
    /// A member that must be stored was compressed by the ZIP container.
    #[error("member is ZIP-compressed but must be stored: {0}")]
    RecompressedMember(String),
    /// There is no `datapackage.json` manifest.
    #[error("missing required member: datapackage.json")]
    MissingDataPackage,
    /// There is no `pages/pages.jsonl` page list.
    #[error("missing required member: pages/pages.jsonl")]
    MissingPages,
    /// There are no members under `archive/`.
    #[error("no WARC members under archive/")]
    NoWarcMembers,
    /// There are no members under `indexes/`.
    #[error("no index members under indexes/")]
    NoIndexMembers,
}

/// A way the manifest violates the WACZ or Frictionless Data Package specifications.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, thiserror::Error)]
pub enum ManifestProblem {
    /// The manifest is not valid JSON or does not match the data package schema.
    #[error("unparseable manifest: {0}")]
    Unparseable(String),
    /// The `profile` property is not `data-package`.
    #[error("profile is not data-package: {0}")]
    Profile(String),
    /// The `wacz_version` property is not `1.1.1`.
    #[error("wacz_version is not 1.1.1: {0}")]
    WaczVersion(String),
    /// A date-time property is not written in the RFC 3339 form the specification requires.
    #[error("{property} is not an RFC 3339 date-time: {value}")]
    NonConformingDate {
        /// The property name, as it appears in the manifest.
        property: String,
        /// The value, as it was written.
        value: String,
    },
    /// The manifest lists no resources.
    #[error("empty resource list")]
    NoResources,
    /// The package metadata violates a Data Package constraint.
    #[error("{0}")]
    Constraint(String),
    /// A resource name is empty or uses characters outside lowercase `a-z0-9._-`.
    #[error("invalid resource name: {0}")]
    InvalidResourceName(String),
    /// A resource path is not safely relative.
    #[error("unsafe resource path: {0}")]
    InvalidResourcePath(String),
    /// Two resources share a name.
    #[error("duplicate resource name: {0}")]
    DuplicateResourceName(String),
    /// Two resources share a path.
    #[error("duplicate resource path: {0}")]
    DuplicateResourcePath(String),
    /// A resource names the manifest or digest file, which must not list themselves.
    #[error("resource names a generated metadata file: {0}")]
    ReservedResourcePath(String),
    /// A listed resource is absent from the ZIP container.
    #[error("resource is absent from the archive: {0}")]
    MissingResourceMember(String),
    /// A ZIP member other than the manifest and digest file is not listed as a resource, so
    /// fixity checking cannot cover it.
    #[error("member is not listed as a resource: {0}")]
    UnlistedMember(String),
}

/// What the `datapackage-digest.json` file claims about the manifest.
///
/// Cryptographic verification of a `signedData` envelope is out of scope here:
/// [`Unverified`](Self::Unverified) reports only that a signature is present and internally
/// consistent.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub enum SignatureStatus {
    /// There is no digest file. The specification only recommends one.
    Absent,
    /// The digest file corroborates the manifest but carries no signature.
    Unsigned,
    /// The digest file carries a signature and is internally consistent, but the signature's
    /// cryptographic validity has not been checked.
    Unverified,
    /// The digest file does not parse or is inconsistent with the manifest or its own signature.
    Invalid(Vec<SignatureProblem>),
}

/// An inconsistency in the `datapackage-digest.json` file.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, thiserror::Error)]
pub enum SignatureProblem {
    /// The digest file is not valid JSON or does not match its schema.
    #[error("unparseable digest file: {0}")]
    Unparseable(String),
    /// The digest file names a path other than `datapackage.json`.
    #[error("digest file names a path other than the manifest: {0}")]
    Path(String),
    /// The digest file's hash does not match the manifest bytes.
    #[error(
        "digest file hash does not match the manifest: declared {declared}, computed {computed}"
    )]
    ManifestHash {
        /// The hash declared by the digest file.
        declared: Sha256Digest,
        /// The hash computed from the manifest bytes.
        computed: Sha256Digest,
    },
    /// The signature envelope covers a hash other than the one the digest file declares.
    #[error(
        "signed hash does not match the digest file: digest file declares {declared}, \
         signature covers {signed}"
    )]
    SignedHash {
        /// The hash declared by the digest file.
        declared: Sha256Digest,
        /// The hash covered by the signature envelope.
        signed: Sha256Digest,
    },
}

/// A member whose content does not parse as its location requires.
///
/// Only the first problem in each member is reported, keeping the report bounded and
/// deterministic for members with cascading parse failures.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ContentProblem {
    /// The member path.
    pub path: String,
    /// The format rule the member violated.
    pub kind: ContentKind,
    /// A description of the first failure, when the kind does not describe it completely.
    pub message: Option<String>,
}

/// The kind of content expected at a WACZ member path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub enum ContentKind {
    /// A page list.
    Pages,
    /// A CDXJ index.
    Index,
    /// The JSON fields of a CDXJ index entry.
    IndexFields,
    /// Sorted CDXJ index lines.
    IndexOrder,
    /// A `ZipNum` summary.
    ZipNum,
    /// A WARC file.
    Warc,
}

impl std::fmt::Display for ContentProblem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: ", self.path)?;
        if let Some(message) = &self.message {
            message.fmt(formatter)
        } else {
            formatter.write_str("lines are not sorted by search key and timestamp")
        }
    }
}

impl std::error::Error for ContentProblem {}

impl ContentProblem {
    fn new(path: &str, kind: ContentKind, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            kind,
            message: Some(message.into()),
        }
    }

    fn index_order(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            kind: ContentKind::IndexOrder,
            message: None,
        }
    }
}

/// An index entry or `ZipNum` block that does not resolve to the data it describes.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, thiserror::Error)]
pub enum IndexProblem {
    /// An index entry does not locate or describe exactly one WARC record.
    #[error("{index_path}: capture {key} {timestamp}: {message}")]
    Capture {
        /// The index or summary member the entry came from.
        index_path: String,
        /// The entry's search key.
        key: String,
        /// The entry's rendered timestamp.
        timestamp: String,
        /// A description of the failure.
        message: String,
    },
    /// A `ZipNum` block cannot be read, does not match its declared digest, or does not
    /// decompress.
    #[error("{summary_path}: block at offset {offset}: {message}")]
    Block {
        /// The `.idx` summary member describing the block.
        summary_path: String,
        /// The block's byte offset within its data member.
        offset: u64,
        /// A description of the failure.
        message: String,
    },
}

impl<R: Read + Seek> WaczReader<R> {
    /// Check the WACZ against the specification's structural requirements, in layers.
    ///
    /// The layout, manifest, and signature layers always run; `options` selects the fixity,
    /// content, and index layers, which read complete members. All findings are reported in the
    /// result rather than treated as errors; an error is returned only when the underlying stream
    /// or ZIP container cannot be read.
    ///
    /// When the manifest is missing or unparseable, the layout or manifest layer reports it and
    /// the fixity layer is skipped even if selected, since there are no declared digests to
    /// check.
    pub fn validate(&mut self, options: ValidationOptions) -> Result<ValidationReport, Error> {
        let mut layout = self.layout_problems();
        layout.extend(self.compression_problems());

        let manifest_bytes = match self.member_bytes(DATA_PACKAGE_PATH) {
            Ok(bytes) => Some(bytes),
            Err(Error::MissingMember(_)) => None,
            Err(error) => return Err(error),
        };

        let mut manifest = Vec::new();
        // The manifest is read through the compatibility stage so that a date written in a
        // non-conforming form is reported as exactly that, rather than collapsing every other
        // manifest check into a single parse failure.
        let package = manifest_bytes.as_deref().and_then(|bytes| {
            match frictionless::compat::parse_data_package(bytes) {
                Ok((package, non_conforming)) => {
                    manifest.extend(non_conforming.into_iter().map(|date| {
                        ManifestProblem::NonConformingDate {
                            property: date.property.to_owned(),
                            value: date.value,
                        }
                    }));
                    Some(package)
                }
                Err(error) => {
                    manifest.push(ManifestProblem::Unparseable(error.to_string()));
                    None
                }
            }
        });

        if let Some(package) = &package {
            self.manifest_problems(package, &mut manifest);
        }

        let digest_file = self.digest_file()?;
        let signature = Self::signature_status(manifest_bytes.as_deref(), &digest_file);

        let fixity = if options.fixity {
            match (package.as_ref(), manifest_bytes.as_deref()) {
                (Some(package), Some(manifest_bytes)) => {
                    Some(self.verify_fixity_for(package, manifest_bytes, &digest_file)?)
                }
                _ => None,
            }
        } else {
            None
        };

        let content = if options.content || options.index {
            Some(self.content_problems())
        } else {
            None
        };

        let index = if options.index {
            Some(self.capture_problems())
        } else {
            None
        };

        Ok(ValidationReport {
            layout,
            manifest,
            content,
            index,
            fixity,
            signature,
        })
    }

    /// Check that the members the specification requires are present.
    fn layout_problems(&self) -> Vec<LayoutProblem> {
        let mut problems = self
            .duplicate_members
            .iter()
            .cloned()
            .map(LayoutProblem::DuplicateMember)
            .collect::<Vec<_>>();

        if self.archive.index_for_name(DATA_PACKAGE_PATH).is_none() {
            problems.push(LayoutProblem::MissingDataPackage);
        }
        if self.archive.index_for_name(PAGES_PATH).is_none() {
            problems.push(LayoutProblem::MissingPages);
        }
        let paths = self.member_paths().collect::<HashSet<_>>();
        let mut has_warc = false;
        let mut has_index = false;
        for path in &paths {
            if path.starts_with(crate::ARCHIVE_PREFIX) {
                if crate::paths::is_warc(path) {
                    has_warc = true;
                } else {
                    problems.push(LayoutProblem::InvalidWarcMember((*path).to_owned()));
                }
            }
            if path.starts_with(crate::INDEXES_PREFIX) {
                if crate::paths::is_cdxj_index(path) {
                    has_index = true;
                } else if !crate::paths::is_zipnum_summary(path)
                    || !crate::paths::zipnum_partner(path)
                        .is_some_and(|partner| paths.contains(partner.as_str()))
                {
                    // A `ZipNum` summary holds no CDXJ data itself, so it neither satisfies the
                    // requirement for an index nor stands on its own without its block file.
                    problems.push(LayoutProblem::InvalidIndexMember((*path).to_owned()));
                }
            }
        }
        if !has_warc {
            problems.push(LayoutProblem::NoWarcMembers);
        }
        if !has_index {
            problems.push(LayoutProblem::NoIndexMembers);
        }

        problems
    }

    /// Members that the specification requires to be stored but that the container compressed.
    ///
    /// Unlike the other layout checks this one reads local file headers, so it is separated from
    /// the name-based checks in [`Self::layout_problems`].
    fn compression_problems(&mut self) -> Vec<LayoutProblem> {
        let mut problems = Vec::new();

        // Reading by index gives each member's name and compression method together, which a
        // shared borrow of the central directory cannot provide.
        for index in 0..self.archive.len() {
            // A member whose local header cannot be read is reported by the content layer.
            let Ok(member) = self.archive.by_index(index) else {
                continue;
            };
            let name = member.name();
            if !name.ends_with('/')
                && crate::paths::requires_stored(name)
                && member.compression() != zip::CompressionMethod::Stored
            {
                problems.push(LayoutProblem::RecompressedMember(name.to_owned()));
            }
        }

        problems
    }

    /// Check the parsed manifest's declarations against the specification and the ZIP contents.
    fn manifest_problems(&self, package: &DataPackage<'_>, problems: &mut Vec<ManifestProblem>) {
        problems.extend(
            package
                .constraint_errors()
                .iter()
                .map(|error| ManifestProblem::Constraint(error.to_string())),
        );
        if package.profile != PROFILE {
            problems.push(ManifestProblem::Profile(package.profile.to_string()));
        }
        if package.wacz_version != WACZ_VERSION {
            problems.push(ManifestProblem::WaczVersion(
                package.wacz_version.to_string(),
            ));
        }
        if package.resources.is_empty() {
            problems.push(ManifestProblem::NoResources);
        }

        let mut names = HashSet::with_capacity(package.resources.len());
        let mut paths = HashSet::with_capacity(package.resources.len());

        for resource in &package.resources {
            if !crate::paths::valid_name(&resource.name) {
                problems.push(ManifestProblem::InvalidResourceName(
                    resource.name.to_string(),
                ));
            }
            if !names.insert(resource.name.as_ref()) {
                problems.push(ManifestProblem::DuplicateResourceName(
                    resource.name.to_string(),
                ));
            }
            if !crate::paths::is_safe(&resource.path) {
                problems.push(ManifestProblem::InvalidResourcePath(
                    resource.path.to_string(),
                ));
            }
            if !paths.insert(resource.path.as_ref()) {
                problems.push(ManifestProblem::DuplicateResourcePath(
                    resource.path.to_string(),
                ));
            }
            if resource.path == DATA_PACKAGE_PATH || resource.path == DATA_PACKAGE_DIGEST_PATH {
                problems.push(ManifestProblem::ReservedResourcePath(
                    resource.path.to_string(),
                ));
            } else if self.archive.index_for_name(&resource.path).is_none() {
                problems.push(ManifestProblem::MissingResourceMember(
                    resource.path.to_string(),
                ));
            }
        }

        for path in self.member_paths() {
            if path != DATA_PACKAGE_PATH
                && path != DATA_PACKAGE_DIGEST_PATH
                && !paths.contains(path)
            {
                problems.push(ManifestProblem::UnlistedMember(path.to_owned()));
            }
        }
    }

    /// Check the digest file's internal consistency and its claim about the manifest bytes.
    fn signature_status(
        manifest_bytes: Option<&[u8]>,
        digest_file: &DigestFile,
    ) -> SignatureStatus {
        let digest = match digest_file {
            DigestFile::Absent => return SignatureStatus::Absent,
            DigestFile::Unparseable(error) => {
                return SignatureStatus::Invalid(vec![SignatureProblem::Unparseable(
                    error.to_string(),
                )]);
            }
            DigestFile::Parsed(digest) => digest,
        };

        let mut problems = Vec::new();

        if digest.path != DATA_PACKAGE_PATH {
            problems.push(SignatureProblem::Path(digest.path.to_string()));
        }

        if let Some(manifest) = manifest_bytes {
            let computed = Sha256Digest::compute(manifest);
            if digest.hash != computed {
                problems.push(SignatureProblem::ManifestHash {
                    declared: digest.hash,
                    computed,
                });
            }
        }

        let signed = digest.signed_data.as_ref().map(|data| *data.hash());
        if let Some(signed) = signed
            && signed != digest.hash
        {
            problems.push(SignatureProblem::SignedHash {
                declared: digest.hash,
                signed,
            });
        }

        if !problems.is_empty() {
            SignatureStatus::Invalid(problems)
        } else if signed.is_some() {
            SignatureStatus::Unverified
        } else {
            SignatureStatus::Unsigned
        }
    }

    /// Parse every page list, index, and WARC member, collecting the first problem in each.
    ///
    /// Failures to read a member at all (I/O or ZIP-level errors) are reported as that member's
    /// problem rather than aborting validation.
    fn content_problems(&mut self) -> Vec<ContentProblem> {
        let mut problems = Vec::new();

        let page_paths = self
            .paths_under(PAGES_PREFIX)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for path in page_paths {
            problems.extend(self.page_list_problem(&path));
        }
        let index_paths = self.index_paths().map(str::to_owned).collect::<Vec<_>>();
        for path in index_paths {
            problems.extend(self.index_content_problem(&path));
        }

        let warc_paths = self.warc_paths().map(str::to_owned).collect::<Vec<_>>();
        for path in warc_paths {
            problems.extend(self.warc_problem(&path));
        }

        problems
    }

    /// The first failure parsing a member as a page list, if any.
    fn page_list_problem(&mut self, path: &str) -> Option<ContentProblem> {
        let message = match self.page_list(path) {
            Ok(mut pages) => pages.find_map(Result::err).map(|error| error.to_string()),
            Err(error) => Some(error.to_string()),
        };

        message.map(|message| ContentProblem::new(path, ContentKind::Pages, message))
    }

    /// The first failure parsing a member as a `ZipNum` summary or a sorted CDXJ index, if any.
    fn index_content_problem(&mut self, path: &str) -> Option<ContentProblem> {
        if crate::paths::is_zipnum_summary(path) {
            return self
                .zipnum_summary(path)
                .err()
                .map(|error| ContentProblem::new(path, ContentKind::ZipNum, error.to_string()));
        }

        // The validator reads at the strictest level: a blank line is not a CDXJ record.
        let items = match self
            .index(path)
            .map(cdxj::IndexReader::rejecting_blank_lines)
        {
            Ok(items) => items,
            Err(error) => {
                return Some(ContentProblem::new(
                    path,
                    ContentKind::Index,
                    error.to_string(),
                ));
            }
        };

        let mut previous: Option<(String, Timestamp)> = None;
        for (index, item) in items.enumerate() {
            match item {
                Ok(item) => {
                    if let Err(error) =
                        archivindex_cdx::cdxj::ConformingFields::try_from(&item.fields)
                    {
                        return Some(ContentProblem::new(
                            path,
                            ContentKind::IndexFields,
                            format!("line {}: {error}", index + 1),
                        ));
                    }
                    let current = (item.key.into_owned(), item.timestamp);
                    if previous.is_some_and(|previous| previous > current) {
                        return Some(ContentProblem::index_order(path));
                    }
                    previous = Some(current);
                }
                Err(error) => {
                    return Some(ContentProblem::new(
                        path,
                        ContentKind::Index,
                        error.to_string(),
                    ));
                }
            }
        }

        None
    }

    /// The first failure parsing a member as a WARC file, if any.
    ///
    /// Records are parsed structurally (framing and required headers), not semantically.
    fn warc_problem(&mut self, path: &str) -> Option<ContentProblem> {
        let message = match self.warc(path) {
            Ok(records) => records
                .iter_raw_records()
                .find_map(Result::err)
                .map(|error| error.to_string()),
            Err(error) => Some(error.to_string()),
        };

        message.map(|message| ContentProblem::new(path, ContentKind::Warc, message))
    }

    /// Check every index entry against its WARC record and verify every `ZipNum` block digest.
    ///
    /// Members and lines that do not parse are skipped here; the content layer, which the index
    /// layer implies, reports them.
    fn capture_problems(&mut self) -> Vec<IndexProblem> {
        let mut problems = Vec::new();
        let partition = self.partition_indexes();

        for (path, summary) in partition.summaries {
            let Ok(summary) = summary else {
                continue;
            };

            for block in &summary.blocks {
                match self.zipnum_block(block) {
                    Ok(bytes) => self.resolve_captures(&path, &bytes, &mut problems),
                    Err(error) => problems.push(IndexProblem::Block {
                        summary_path: path.clone(),
                        offset: block.offset,
                        message: error.to_string(),
                    }),
                }
            }
        }

        for path in partition.plain {
            if let Ok(bytes) = self.decoded_member_bytes(&path) {
                self.resolve_captures(&path, &bytes, &mut problems);
            }
        }

        problems
    }

    /// Check each parseable CDXJ line against the WARC record it locates.
    fn resolve_captures(
        &mut self,
        index_path: &str,
        bytes: &[u8],
        problems: &mut Vec<IndexProblem>,
    ) {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return;
        };

        for line in text.lines().filter(|line| !line.is_empty()) {
            let Ok(item) = Item::parse(line) else {
                continue;
            };

            let result = self
                .read_capture(&item.fields)
                .map_err(|error| error.to_string())
                .and_then(|record| capture_matches(&item, &record));

            if let Err(message) = result {
                problems.push(IndexProblem::Capture {
                    index_path: index_path.to_owned(),
                    key: item.key.into_owned(),
                    timestamp: item.timestamp.to_string(),
                    message,
                });
            }
        }
    }
}

/// Check the searchable and descriptive fields of a CDXJ item against its record.
fn capture_matches(item: &Item<'_>, record: &Record<NoExtension>) -> Result<(), String> {
    capture_identity_matches(item, record)?;
    capture_http_metadata_matches(item, record)?;
    capture_digest_matches(item, record)
}

/// Check the URL, search key, and capture time.
fn capture_identity_matches(item: &Item<'_>, record: &Record<NoExtension>) -> Result<(), String> {
    let target_uri = record.target_uri().ok_or_else(|| {
        format!(
            "located WARC {} record has no target URI",
            record.type_name()
        )
    })?;
    if item.fields.url != target_uri.as_str() {
        return Err(format!(
            "url `{}` does not match WARC-Target-URI `{target_uri}`",
            item.fields.url
        ));
    }

    let wayback = Canonicalizer::WAYBACK
        .surt(&item.fields.url)
        .map_err(|error| format!("url cannot be canonicalized: {error}"))?;
    let warcio = Canonicalizer::WARCIO
        .surt(&item.fields.url)
        .map_err(|error| format!("url cannot be canonicalized: {error}"))?;
    if item.key != wayback.as_str() && item.key != warcio.as_str() {
        return Err(format!(
            "key `{}` is not a recognized canonical key for `{}`",
            item.key, item.fields.url
        ));
    }

    let date = record.core().date.date_time();
    let timestamp = if item.timestamp.has_milliseconds() {
        Timestamp::with_milliseconds(date)
    } else {
        Timestamp::new(date)
    };
    if item.timestamp != timestamp {
        return Err(format!(
            "timestamp {} does not match WARC-Date at the same precision ({timestamp})",
            item.timestamp
        ));
    }

    Ok(())
}

/// Check the status and payload type against the captured HTTP response.
fn capture_http_metadata_matches(
    item: &Item<'_>,
    record: &Record<NoExtension>,
) -> Result<(), String> {
    let (message, revisit) = match record {
        Record::Response { body, .. } => (body.as_slice(), false),
        Record::Revisit { body, .. } => (body.as_slice(), true),
        _ => {
            return Err(format!(
                "located WARC record has type `{}`, not `response` or `revisit`",
                record.type_name()
            ));
        }
    };
    let response = ResponseMetadata::parse(message).ok_or_else(|| {
        "located WARC record does not contain a parseable HTTP response".to_owned()
    })?;

    if let Some(status) = item.fields.status
        && status != response.status
    {
        return Err(format!(
            "status {status} does not match HTTP status {}",
            response.status
        ));
    }

    if let Some(mime) = item.fields.mime.as_deref() {
        let actual = if revisit {
            "warc/revisit".to_owned()
        } else {
            response
                .header("content-type")
                .and_then(|value| std::str::from_utf8(value).ok())
                .and_then(|value| value.split(';').next())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    record
                        .payload()
                        .and_then(|payload| payload.identified_payload_type.as_ref())
                        .map(|media_type| {
                            format!("{}/{}", media_type.type_name(), media_type.subtype())
                        })
                })
                .unwrap_or_else(|| "unk".to_owned())
        };
        if !mime.eq_ignore_ascii_case(&actual) {
            return Err(format!(
                "mime `{mime}` does not match captured payload type `{actual}`"
            ));
        }
    }

    Ok(())
}

/// Check the payload digest, computing it when the WARC header omits it.
fn capture_digest_matches(item: &Item<'_>, record: &Record<NoExtension>) -> Result<(), String> {
    if let Some(digest) = item.fields.digest.as_deref() {
        let expected = LabelledDigest::parse(digest.as_bytes())
            .map_err(|error| format!("digest `{digest}` is invalid: {error}"))?;
        let declared = record
            .payload()
            .and_then(|payload| payload.payload_digest.as_ref());
        if let Some(actual) = declared {
            if !expected.matches(actual) {
                return Err(format!(
                    "digest `{expected}` does not match WARC-Payload-Digest `{actual}`"
                ));
            }
        } else {
            let algorithm = expected.algorithm().ok_or_else(|| {
                format!(
                    "digest `{expected}` cannot be checked because the WARC record has no \
                     WARC-Payload-Digest and its algorithm is unsupported"
                )
            })?;
            let payload = record
                .payload_bytes()
                .map_err(|error| format!("cannot extract WARC payload: {error}"))?
                .ok_or_else(|| {
                    "digest cannot be checked because the WARC record has no local payload or \
                     WARC-Payload-Digest"
                        .to_owned()
                })?;
            let actual = LabelledDigest::compute(algorithm, &payload).ok_or_else(|| {
                format!(
                    "digest algorithm `{}` is unsupported",
                    expected.algorithm_as_read()
                )
            })?;
            if !expected.matches(&actual) {
                return Err(format!(
                    "digest `{expected}` does not match computed payload digest `{actual}`"
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options_select_no_expensive_layers() {
        assert_eq!(
            ValidationOptions::default(),
            ValidationOptions {
                fixity: false,
                content: false,
                index: false,
            }
        );
        assert_eq!(
            ValidationOptions::all(),
            ValidationOptions {
                fixity: true,
                content: true,
                index: true,
            }
        );
    }
}
