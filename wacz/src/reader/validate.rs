//! Layered WACZ conformance validation.
//!
//! [`WaczReader::validate`] checks an archive against the requirements of the [WACZ 1.1.1
//! specification](https://specs.webrecorder.net/wacz/1.1.1/) and reports findings in layers,
//! keeping structural conformance violations, metadata parse failures, and digest mismatches
//! distinguishable. The layout, manifest, and signature layers always run and read only the ZIP
//! central directory and the two metadata files; the fixity, content, and index layers each read
//! complete members and are selected through [`ValidationOptions`].
//!
//! The ZIP layer indexes members by name, so an archive holding duplicate entry names cannot be
//! detected here; the entry appearing last in the central directory is the one observed.

use std::collections::HashSet;
use std::io::{Read, Seek};

use crate::cdxj::{Item, Timestamp};
use crate::digest::Sha256Digest;
use crate::frictionless::{DataPackage, DataPackageDigest, PROFILE, WACZ_VERSION};
use crate::{DATA_PACKAGE_DIGEST_PATH, DATA_PACKAGE_PATH, PAGES_PATH, PAGES_PREFIX};

use super::random::has_extension;
use super::{Error, Fixity, WaczReader};

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
    /// Resolve every index entry to its WARC record and verify every `ZipNum` block digest.
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
    /// Index entries and `ZipNum` blocks that do not resolve, if the index layer ran.
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

/// A required member that is absent from the ZIP container.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, thiserror::Error)]
pub enum LayoutProblem {
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
    /// The manifest lists no resources.
    #[error("empty resource list")]
    NoResources,
    /// A resource name is empty or uses characters outside lowercase `a-z0-9._-`.
    #[error("invalid resource name: {0}")]
    InvalidResourceName(String),
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
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, thiserror::Error)]
pub enum ContentProblem {
    /// A member under `pages/` is not a valid page list.
    #[error("{path}: {message}")]
    Pages {
        /// The member path.
        path: String,
        /// A description of the first failure.
        message: String,
    },
    /// A member under `indexes/` is not valid CDXJ.
    #[error("{path}: {message}")]
    Index {
        /// The member path.
        path: String,
        /// A description of the first failure.
        message: String,
    },
    /// A CDXJ index's lines are not sorted by search key and timestamp.
    #[error("{path}: lines are not sorted by search key and timestamp")]
    IndexOrder {
        /// The member path.
        path: String,
    },
    /// A `.idx` member is not a valid `ZipNum` summary.
    #[error("{path}: {message}")]
    ZipNum {
        /// The member path.
        path: String,
        /// A description of the first failure.
        message: String,
    },
    /// A member under `archive/` is not a well-formed WARC file.
    #[error("{path}: {message}")]
    Warc {
        /// The member path.
        path: String,
        /// A description of the first failure.
        message: String,
    },
}

/// An index entry or `ZipNum` block that does not resolve to the data it describes.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, thiserror::Error)]
pub enum IndexProblem {
    /// An index entry does not locate exactly one WARC record matching its declared digest.
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
        let layout = self.layout_problems();

        let manifest_bytes = match self.member_bytes(DATA_PACKAGE_PATH) {
            Ok(bytes) => Some(bytes),
            Err(Error::MissingMember(_)) => None,
            Err(error) => return Err(error),
        };

        let mut manifest = Vec::new();
        let package = manifest_bytes.as_deref().and_then(|bytes| {
            match serde_json::from_slice::<DataPackage<'_>>(bytes) {
                Ok(package) => Some(package),
                Err(error) => {
                    manifest.push(ManifestProblem::Unparseable(error.to_string()));
                    None
                }
            }
        });

        if let Some(package) = &package {
            self.manifest_problems(package, &mut manifest);
        }

        let signature = self.signature_status(manifest_bytes.as_deref())?;

        let fixity = if options.fixity && package.is_some() {
            Some(self.verify_fixity()?)
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
        let mut problems = Vec::new();

        if self.archive.index_for_name(DATA_PACKAGE_PATH).is_none() {
            problems.push(LayoutProblem::MissingDataPackage);
        }
        if self.archive.index_for_name(PAGES_PATH).is_none() {
            problems.push(LayoutProblem::MissingPages);
        }
        if self.warc_paths().next().is_none() {
            problems.push(LayoutProblem::NoWarcMembers);
        }
        if self.index_paths().next().is_none() {
            problems.push(LayoutProblem::NoIndexMembers);
        }

        problems
    }

    /// Check the parsed manifest's declarations against the specification and the ZIP contents.
    fn manifest_problems(&self, package: &DataPackage<'_>, problems: &mut Vec<ManifestProblem>) {
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
            if !is_valid_resource_name(&resource.name) {
                problems.push(ManifestProblem::InvalidResourceName(
                    resource.name.to_string(),
                ));
            }
            if !names.insert(resource.name.as_ref()) {
                problems.push(ManifestProblem::DuplicateResourceName(
                    resource.name.to_string(),
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
        &mut self,
        manifest_bytes: Option<&[u8]>,
    ) -> Result<SignatureStatus, Error> {
        let bytes = match self.member_bytes(DATA_PACKAGE_DIGEST_PATH) {
            Ok(bytes) => bytes,
            Err(Error::MissingMember(_)) => return Ok(SignatureStatus::Absent),
            Err(error) => return Err(error),
        };

        let digest = match serde_json::from_slice::<DataPackageDigest<'_>>(&bytes) {
            Ok(digest) => digest,
            Err(error) => {
                return Ok(SignatureStatus::Invalid(vec![
                    SignatureProblem::Unparseable(error.to_string()),
                ]));
            }
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

        Ok(if !problems.is_empty() {
            SignatureStatus::Invalid(problems)
        } else if signed.is_some() {
            SignatureStatus::Unverified
        } else {
            SignatureStatus::Unsigned
        })
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

        message.map(|message| ContentProblem::Pages {
            path: path.to_owned(),
            message,
        })
    }

    /// The first failure parsing a member as a `ZipNum` summary or a sorted CDXJ index, if any.
    fn index_content_problem(&mut self, path: &str) -> Option<ContentProblem> {
        if has_extension(path, "idx") {
            return self
                .zipnum_summary(path)
                .err()
                .map(|error| ContentProblem::ZipNum {
                    path: path.to_owned(),
                    message: error.to_string(),
                });
        }

        let items = match self.index(path) {
            Ok(items) => items,
            Err(error) => {
                return Some(ContentProblem::Index {
                    path: path.to_owned(),
                    message: error.to_string(),
                });
            }
        };

        let mut previous: Option<(String, Timestamp)> = None;
        for item in items {
            match item {
                Ok(item) => {
                    let current = (item.key.into_owned(), item.timestamp);
                    if previous.is_some_and(|previous| previous > current) {
                        return Some(ContentProblem::IndexOrder {
                            path: path.to_owned(),
                        });
                    }
                    previous = Some(current);
                }
                Err(error) => {
                    return Some(ContentProblem::Index {
                        path: path.to_owned(),
                        message: error.to_string(),
                    });
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

        message.map(|message| ContentProblem::Warc {
            path: path.to_owned(),
            message,
        })
    }

    /// Resolve every index entry to its WARC record and verify every `ZipNum` block digest.
    ///
    /// Members and lines that do not parse are skipped here; the content layer, which the index
    /// layer implies, reports them.
    fn capture_problems(&mut self) -> Vec<IndexProblem> {
        let mut problems = Vec::new();
        let paths = self.index_paths().map(str::to_owned).collect::<Vec<_>>();
        let mut referenced_data = Vec::new();

        for path in paths.iter().filter(|path| has_extension(path, "idx")) {
            let Ok(summary) = self.zipnum_summary(path) else {
                continue;
            };
            referenced_data.push(summary.data_path.clone());

            for block in &summary.blocks {
                match self.zipnum_block(block) {
                    Ok(bytes) => self.resolve_captures(path, &bytes, &mut problems),
                    Err(error) => problems.push(IndexProblem::Block {
                        summary_path: path.clone(),
                        offset: block.offset,
                        message: error.to_string(),
                    }),
                }
            }
        }

        for path in paths.iter().filter(|path| {
            !has_extension(path, "idx") && !referenced_data.iter().any(|data| data == *path)
        }) {
            if let Ok(bytes) = self.decoded_member_bytes(path) {
                self.resolve_captures(path, &bytes, &mut problems);
            }
        }

        problems
    }

    /// Resolve the capture on each parseable CDXJ line, checking `recordDigest` values and that
    /// each located range holds exactly one WARC record.
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

            if let Err(error) = self.read_capture_raw(&item.fields) {
                problems.push(IndexProblem::Capture {
                    index_path: index_path.to_owned(),
                    key: item.key.into_owned(),
                    timestamp: item.timestamp.to_string(),
                    message: error.to_string(),
                });
            }
        }
    }
}

/// Whether a resource name satisfies the Data Package restriction to lowercase `a-z0-9._-`.
fn is_valid_resource_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_name_validity() {
        assert!(is_valid_resource_name("pages.jsonl"));
        assert!(is_valid_resource_name("data-1_2.warc.gz"));
        assert!(!is_valid_resource_name(""));
        assert!(!is_valid_resource_name("Pages.JSONL"));
        assert!(!is_valid_resource_name("archive/data.warc"));
        assert!(!is_valid_resource_name("caf\u{e9}.warc"));
    }

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
