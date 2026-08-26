//! The `datapackage.json` manifest and `datapackage-digest.json` formats.
//!
//! A WACZ manifest is a [Frictionless Data Package][data-package] descriptor that lists every other
//! file in the WACZ with its size and SHA-256 digest. The digest file records the digest of the
//! serialized manifest and may include a cryptographic signature.
//!
//! [data-package]: https://specs.frictionlessdata.io/data-package/
//!
//! Unmodeled properties are preserved in [`DataPackage::extra`] and [`Resource::extra`]. Modeled
//! properties are parsed strictly; [`compat`] handles nonstandard date-time forms found in the
//! wild.

use std::borrow::Cow;

use archivindex_cdx::properties::ExtraProperties;
use bounded_static::ToStatic;
use chrono::{DateTime, Utc};

use crate::digest::Sha256Digest;

pub mod compat;
pub mod resource;
pub mod signature;

use resource::Resource;
use signature::SignatureData;

/// The Frictionless Data Package profile identifier required by the WACZ specification.
pub const PROFILE: &str = "data-package";

/// The WACZ specification version targeted by this crate.
pub const WACZ_VERSION: &str = "1.1.1";

const CONTRIBUTOR_ROLES: [&str; 5] = [
    "author",
    "publisher",
    "maintainer",
    "wrangler",
    "contributor",
];

/// A violation of a Data Package metadata constraint.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConstraintError {
    /// The package name is empty or uses characters outside lowercase `a-z0-9._-`.
    #[error("invalid package name: {0}")]
    Name(String),
    /// A source has no title.
    #[error("source has no title")]
    SourceTitle,
    /// A license has neither a name nor a path.
    #[error("license has neither a name nor a path")]
    LicenseIdentity,
    /// A contributor has no title.
    #[error("contributor has no title")]
    ContributorTitle,
    /// A contributor's role is outside the specification's vocabulary.
    #[error("invalid contributor role: {0}")]
    ContributorRole(String),
    /// An extension property duplicates a modeled property.
    #[error(transparent)]
    Extra(#[from] archivindex_cdx::properties::Error),
}

macro_rules! optional_string_setter {
    ($name:ident, $field:ident, $docs:literal) => {
        #[doc = $docs]
        #[must_use]
        pub fn $name(mut self, value: impl Into<Cow<'static, str>>) -> Self {
            self.$field = Some(value.into());
            self
        }
    };
}

/// A valid source for authored package metadata.
///
/// Unlike [`Source`], which faithfully represents possibly-invalid input, this type requires the
/// title mandated by the Data Package specification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceMetadata {
    title: Cow<'static, str>,
    path: Option<Cow<'static, str>>,
    email: Option<Cow<'static, str>>,
    extra: ExtraProperties,
}

impl SourceMetadata {
    /// Create a source with its required human-readable title.
    #[must_use]
    pub fn new(title: impl Into<Cow<'static, str>>) -> Self {
        Self {
            title: title.into(),
            path: None,
            email: None,
            extra: ExtraProperties::default(),
        }
    }

    optional_string_setter!(path, path, "Set the source URL or relative path.");
    optional_string_setter!(email, email, "Set the source contact email address.");

    /// Set extension properties, replacing any previously configured properties.
    pub fn extra(
        mut self,
        value: ExtraProperties,
    ) -> Result<Self, archivindex_cdx::properties::Error> {
        value.validate("Source", SOURCE_PROPERTIES)?;
        self.extra = value;
        Ok(self)
    }
}

impl From<SourceMetadata> for Source<'static> {
    fn from(value: SourceMetadata) -> Self {
        Self {
            title: Some(value.title),
            path: value.path,
            email: value.email,
            extra: value.extra,
        }
    }
}

/// A valid license for authored package metadata.
///
/// Construction requires either the license's standard name or a path to its text, so the identity
/// required by the Data Package specification cannot be omitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LicenseMetadata {
    name: Option<Cow<'static, str>>,
    path: Option<Cow<'static, str>>,
    title: Option<Cow<'static, str>>,
    extra: ExtraProperties,
}

impl LicenseMetadata {
    /// Create a license identified by its Open Definition name.
    #[must_use]
    pub fn named(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: Some(name.into()),
            path: None,
            title: None,
            extra: ExtraProperties::default(),
        }
    }

    /// Create a license identified by the URL or relative path to its text.
    #[must_use]
    pub fn at_path(path: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: None,
            path: Some(path.into()),
            title: None,
            extra: ExtraProperties::default(),
        }
    }

    optional_string_setter!(name, name, "Set the Open Definition license name.");
    optional_string_setter!(
        path,
        path,
        "Set the URL or relative path to the license text."
    );
    optional_string_setter!(title, title, "Set the human-readable license title.");

    /// Set extension properties, replacing any previously configured properties.
    pub fn extra(
        mut self,
        value: ExtraProperties,
    ) -> Result<Self, archivindex_cdx::properties::Error> {
        value.validate("License", LICENSE_PROPERTIES)?;
        self.extra = value;
        Ok(self)
    }
}

impl From<LicenseMetadata> for License<'static> {
    fn from(value: LicenseMetadata) -> Self {
        Self {
            name: value.name,
            path: value.path,
            title: value.title,
            extra: value.extra,
        }
    }
}

/// The contribution roles allowed by the Data Package specification.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ContributorRole {
    /// Created the package's contents.
    Author,
    /// Published the package.
    Publisher,
    /// Maintains the package.
    Maintainer,
    /// Prepared or transformed the data.
    Wrangler,
    /// Made another kind of contribution.
    #[default]
    Contributor,
}

impl ContributorRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Author => "author",
            Self::Publisher => "publisher",
            Self::Maintainer => "maintainer",
            Self::Wrangler => "wrangler",
            Self::Contributor => "contributor",
        }
    }
}

/// A valid contributor for authored package metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContributorMetadata {
    title: Cow<'static, str>,
    path: Option<Cow<'static, str>>,
    email: Option<Cow<'static, str>>,
    role: Option<ContributorRole>,
    organization: Option<Cow<'static, str>>,
    extra: ExtraProperties,
}

impl ContributorMetadata {
    /// Create a contributor with the required display name.
    #[must_use]
    pub fn new(title: impl Into<Cow<'static, str>>) -> Self {
        Self {
            title: title.into(),
            path: None,
            email: None,
            role: None,
            organization: None,
            extra: ExtraProperties::default(),
        }
    }

    optional_string_setter!(
        path,
        path,
        "Set a URL or relative path for the contributor."
    );
    optional_string_setter!(email, email, "Set the contributor's contact email address.");
    optional_string_setter!(
        organization,
        organization,
        "Set the contributor's organization."
    );

    /// Set the nature of the contribution.
    #[must_use]
    pub const fn role(mut self, value: ContributorRole) -> Self {
        self.role = Some(value);
        self
    }

    /// Set extension properties, replacing any previously configured properties.
    pub fn extra(
        mut self,
        value: ExtraProperties,
    ) -> Result<Self, archivindex_cdx::properties::Error> {
        value.validate("Contributor", CONTRIBUTOR_PROPERTIES)?;
        self.extra = value;
        Ok(self)
    }
}

impl From<ContributorMetadata> for Contributor<'static> {
    fn from(value: ContributorMetadata) -> Self {
        Self {
            title: Some(value.title),
            path: value.path,
            email: value.email,
            role: value.role.map(|role| Cow::Borrowed(role.as_str())),
            organization: value.organization,
            extra: value.extra,
        }
    }
}

/// Collect every violation of the specification's metadata constraints.
fn constraint_errors(
    name: Option<&str>,
    sources: &[Source<'_>],
    licenses: &[License<'_>],
    contributors: &[Contributor<'_>],
    extra: &ExtraProperties,
) -> Vec<ConstraintError> {
    let mut errors = Vec::new();

    if let Err(error) = extra.validate("DataPackage", DATA_PACKAGE_PROPERTIES) {
        errors.push(error.into());
    }
    if let Some(name) = name
        && !crate::paths::valid_name(name)
    {
        errors.push(ConstraintError::Name(name.to_owned()));
    }
    for source in sources {
        source.push_constraint_errors(&mut errors);
    }
    for license in licenses {
        license.push_constraint_errors(&mut errors);
    }
    for contributor in contributors {
        contributor.push_constraint_errors(&mut errors);
    }

    errors
}

/// A WACZ `datapackage.json` manifest.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
pub struct DataPackage<'a> {
    /// The data package profile identifier (always [`PROFILE`] for WACZ files).
    #[serde(borrow)]
    pub profile: Cow<'a, str>,
    /// The version of the WACZ specification the file conforms to.
    #[serde(borrow)]
    pub wacz_version: Cow<'a, str>,
    /// The files in the WACZ, excluding the manifest and digest files.
    pub resources: Vec<Resource<'a>>,
    /// A short, URL-usable identifier for the package.
    ///
    /// The Data Package specification restricts this to lowercase characters from `a-z0-9._-`,
    /// which [`DataPackage::constraint_errors`] checks.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<Cow<'a, str>>,
    /// A globally unique identifier for the package, such as a UUID or DOI.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<Cow<'a, str>>,
    /// A short description of the collection.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// A longer, possibly Markdown-formatted, description of the collection.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<Cow<'a, str>>,
    /// Keywords describing the package.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_str_seq",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub keywords: Vec<Cow<'a, str>>,
    /// The URL of the package's home on the web.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub homepage: Option<Cow<'a, str>>,
    /// A URL or relative path locating an image representing the package.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub image: Option<Cow<'a, str>>,
    /// The version of the package; the Data Package specification recommends semantic versioning.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub version: Option<Cow<'a, str>>,
    /// The places the package's data originated from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Source<'a>>,
    /// The licenses under which the package is provided.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub licenses: Vec<License<'a>>,
    /// The people and organizations who contributed to the package.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributors: Vec<Contributor<'a>>,
    /// When the WACZ file was created.
    #[serde(
        default,
        deserialize_with = "crate::attributes::optional_rfc_3339_datetime",
        skip_serializing_if = "Option::is_none"
    )]
    pub created: Option<DateTime<Utc>>,
    /// When the WACZ file was last modified.
    #[serde(
        default,
        deserialize_with = "crate::attributes::optional_rfc_3339_datetime",
        skip_serializing_if = "Option::is_none"
    )]
    pub modified: Option<DateTime<Utc>>,
    /// A description of the software that created the WACZ file.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub software: Option<Cow<'a, str>>,
    /// The URL of the primary entry page for replay.
    #[serde(
        rename = "mainPageUrl",
        alias = "mainPageURL",
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub main_page_url: Option<Cow<'a, str>>,
    /// The capture date to use when replaying the primary entry page.
    #[serde(
        rename = "mainPageDate",
        default,
        deserialize_with = "crate::attributes::optional_rfc_3339_datetime",
        skip_serializing_if = "Option::is_none"
    )]
    pub main_page_date: Option<DateTime<Utc>>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl DataPackage<'_> {
    /// Return every metadata constraint violated by this package and its resources.
    ///
    /// Requirements that involve the rest of the archive, such as the resource list, are reported
    /// by [`crate::io::read::validate`] instead.
    #[must_use]
    pub fn constraint_errors(&self) -> Vec<ConstraintError> {
        let mut errors = constraint_errors(
            self.name.as_deref(),
            &self.sources,
            &self.licenses,
            &self.contributors,
            &self.extra,
        );
        for resource in &self.resources {
            resource.push_constraint_errors(&mut errors);
        }

        errors
    }
}

/// Builder for the caller-controlled metadata in a WACZ data package.
///
/// [`crate::io::write::WaczWriter`] supplies the structural properties (`profile`, `wacz_version`,
/// and `resources`) when it finishes the archive. It also supplies defaults for `created` and
/// `software` when those properties are not set here.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DataPackageBuilder {
    name: Option<Cow<'static, str>>,
    id: Option<Cow<'static, str>>,
    title: Option<Cow<'static, str>>,
    description: Option<Cow<'static, str>>,
    keywords: Vec<Cow<'static, str>>,
    homepage: Option<Cow<'static, str>>,
    image: Option<Cow<'static, str>>,
    version: Option<Cow<'static, str>>,
    sources: Vec<SourceMetadata>,
    licenses: Vec<LicenseMetadata>,
    contributors: Vec<ContributorMetadata>,
    created: Option<DateTime<Utc>>,
    modified: Option<DateTime<Utc>>,
    software: Option<Cow<'static, str>>,
    main_page_url: Option<Cow<'static, str>>,
    main_page_date: Option<DateTime<Utc>>,
    extra: ExtraProperties,
}

impl DataPackageBuilder {
    /// Create an empty metadata builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the short, URL-usable package identifier.
    ///
    /// The Data Package specification restricts names to lowercase characters from
    /// `a-z0-9._-`.
    pub fn name(mut self, value: impl Into<Cow<'static, str>>) -> Result<Self, ConstraintError> {
        let value = value.into();
        if !crate::paths::valid_name(&value) {
            return Err(ConstraintError::Name(value.into_owned()));
        }
        self.name = Some(value);
        Ok(self)
    }

    optional_string_setter!(id, id, "Set the globally unique package identifier.");
    optional_string_setter!(title, title, "Set the short collection description.");
    optional_string_setter!(
        description,
        description,
        "Set the longer, optionally Markdown-formatted description."
    );
    optional_string_setter!(homepage, homepage, "Set the package homepage URL.");
    optional_string_setter!(image, image, "Set the package image URL or relative path.");
    optional_string_setter!(version, version, "Set the package version.");
    optional_string_setter!(software, software, "Set the creating software description.");
    optional_string_setter!(main_page_url, main_page_url, "Set the primary replay URL.");

    /// Set all package keywords, replacing any previously configured keywords.
    #[must_use]
    pub fn keywords<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Cow<'static, str>>,
    {
        self.keywords = values.into_iter().map(Into::into).collect();
        self
    }

    /// Set all package sources, replacing any previously configured sources.
    #[must_use]
    pub fn sources<I>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = SourceMetadata>,
    {
        self.sources = values.into_iter().collect();
        self
    }

    /// Set all package licenses, replacing any previously configured licenses.
    #[must_use]
    pub fn licenses<I>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = LicenseMetadata>,
    {
        self.licenses = values.into_iter().collect();
        self
    }

    /// Set all package contributors, replacing any previously configured contributors.
    #[must_use]
    pub fn contributors<I>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = ContributorMetadata>,
    {
        self.contributors = values.into_iter().collect();
        self
    }

    /// Set the package creation time instead of using the writer's current-time default.
    #[must_use]
    pub const fn created(mut self, value: DateTime<Utc>) -> Self {
        self.created = Some(value);
        self
    }

    /// Set the package modification time.
    #[must_use]
    pub const fn modified(mut self, value: DateTime<Utc>) -> Self {
        self.modified = Some(value);
        self
    }

    /// Set the capture date for the primary replay URL.
    #[must_use]
    pub const fn main_page_date(mut self, value: DateTime<Utc>) -> Self {
        self.main_page_date = Some(value);
        self
    }

    /// Set extension properties, replacing any previously configured properties.
    pub fn extra(
        mut self,
        value: ExtraProperties,
    ) -> Result<Self, archivindex_cdx::properties::Error> {
        value.validate("DataPackage", DATA_PACKAGE_PROPERTIES)?;
        self.extra = value;
        Ok(self)
    }

    pub(crate) fn into_data_package(
        self,
        resources: Vec<Resource<'static>>,
    ) -> DataPackage<'static> {
        DataPackage {
            profile: Cow::Borrowed(PROFILE),
            wacz_version: Cow::Borrowed(WACZ_VERSION),
            resources,
            name: self.name,
            id: self.id,
            title: self.title,
            description: self.description,
            keywords: self.keywords,
            homepage: self.homepage,
            image: self.image,
            version: self.version,
            sources: self.sources.into_iter().map(Into::into).collect(),
            licenses: self.licenses.into_iter().map(Into::into).collect(),
            contributors: self.contributors.into_iter().map(Into::into).collect(),
            created: self.created,
            modified: self.modified,
            software: self.software,
            main_page_url: self.main_page_url,
            main_page_date: self.main_page_date,
            extra: self.extra,
        }
    }
}

const DATA_PACKAGE_PROPERTIES: &[&str] = &[
    "profile",
    "wacz_version",
    "resources",
    "name",
    "id",
    "title",
    "description",
    "keywords",
    "homepage",
    "image",
    "version",
    "sources",
    "licenses",
    "contributors",
    "created",
    "modified",
    "software",
    "mainPageUrl",
    "mainPageDate",
];

const SOURCE_PROPERTIES: &[&str] = &["title", "path", "email"];
const LICENSE_PROPERTIES: &[&str] = &["name", "path", "title"];
const CONTRIBUTOR_PROPERTIES: &[&str] = &["title", "path", "email", "role", "organization"];

/// A WACZ `datapackage-digest.json` file.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
pub struct DataPackageDigest<'a> {
    /// The path of the manifest the digest covers (always `datapackage.json`).
    #[serde(borrow)]
    pub path: Cow<'a, str>,
    /// The SHA-256 digest of the serialized manifest bytes.
    pub hash: Sha256Digest,
    /// A signature over the manifest digest.
    #[serde(
        rename = "signedData",
        borrow,
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub signed_data: Option<SignatureData<'a>>,
}

/// A place a package's or resource's data originated from.
///
/// The Data Package specification makes all of these properties optional.
#[derive(Clone, Debug, Default, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
// Every field is optional, so no `#[serde(borrow)]` field ties the deserializer's input lifetime to
// `'a`; state the bound explicitly to allow borrowing from the input.
#[serde(bound(deserialize = "'de: 'a"))]
pub struct Source<'a> {
    /// A human-readable title of the source.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// A URL or relative path locating the source.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub path: Option<Cow<'a, str>>,
    /// A contact email address for the source.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub email: Option<Cow<'a, str>>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl Source<'_> {
    pub(crate) fn push_constraint_errors(&self, errors: &mut Vec<ConstraintError>) {
        if let Err(error) = self.extra.validate("Source", SOURCE_PROPERTIES) {
            errors.push(error.into());
        }
        if self.title.is_none() {
            errors.push(ConstraintError::SourceTitle);
        }
    }
}

/// A license under which a package or resource is provided.
///
/// At least one of `name` or `path` is required by the Data Package specification.
#[derive(Clone, Debug, Default, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
// Every field is optional, so no `#[serde(borrow)]` field ties the deserializer's input lifetime to
// `'a`; state the bound explicitly to allow borrowing from the input.
#[serde(bound(deserialize = "'de: 'a"))]
pub struct License<'a> {
    /// An [Open Definition license identifier](https://opendefinition.org/licenses/api/), for
    /// example `CC-BY-4.0`.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<Cow<'a, str>>,
    /// A URL or relative path locating the license text.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub path: Option<Cow<'a, str>>,
    /// A human-readable title of the license.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl License<'_> {
    pub(crate) fn push_constraint_errors(&self, errors: &mut Vec<ConstraintError>) {
        if let Err(error) = self.extra.validate("License", LICENSE_PROPERTIES) {
            errors.push(error.into());
        }
        if self.name.is_none() && self.path.is_none() {
            errors.push(ConstraintError::LicenseIdentity);
        }
    }
}

/// A person or organization who contributed to a package.
///
/// The Data Package specification requires `title` and restricts `role` to
/// [`CONTRIBUTOR_ROLES`] (`contributor` by default).
#[derive(Clone, Debug, Default, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
// Every field is optional, so no `#[serde(borrow)]` field ties the deserializer's input lifetime to
// `'a`; state the bound explicitly to allow borrowing from the input.
#[serde(bound(deserialize = "'de: 'a"))]
pub struct Contributor<'a> {
    /// The name of the contributor.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// A URL or relative path with more information about the contributor.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub path: Option<Cow<'a, str>>,
    /// A contact email address for the contributor.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub email: Option<Cow<'a, str>>,
    /// The nature of the contribution.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub role: Option<Cow<'a, str>>,
    /// The organization the contributor belongs to.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub organization: Option<Cow<'a, str>>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl Contributor<'_> {
    pub(crate) fn push_constraint_errors(&self, errors: &mut Vec<ConstraintError>) {
        if let Err(error) = self.extra.validate("Contributor", CONTRIBUTOR_PROPERTIES) {
            errors.push(error.into());
        }
        if self.title.is_none() {
            errors.push(ConstraintError::ContributorTitle);
        }
        if let Some(role) = &self.role
            && !CONTRIBUTOR_ROLES.contains(&role.as_ref())
        {
            errors.push(ConstraintError::ContributorRole(role.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use bounded_static::IntoBoundedStatic;
    use proptest::prelude::*;

    use super::*;
    use crate::strategies;

    #[test_strategy::proptest]
    fn data_packages_round_trip(
        #[strategy(strategies::data_package())] package: DataPackage<'static>,
    ) {
        let text = serde_json::to_string(&package).unwrap();
        let parsed = serde_json::from_str::<DataPackage<'_>>(&text).unwrap();

        prop_assert_eq!(parsed.into_static(), package);
    }

    #[test_strategy::proptest]
    fn the_main_page_url_alias_is_accepted(
        #[strategy(strategies::data_package())] package: DataPackage<'static>,
    ) {
        let mut value = serde_json::to_value(&package).unwrap();
        let object = value
            .as_object_mut()
            .expect("invariant violation: a manifest serializes as an object");

        // Manifests written by some tools spell the property with an uppercase acronym.
        if let Some(url) = object.remove("mainPageUrl") {
            object.insert("mainPageURL".to_owned(), url);
        }

        let text = serde_json::to_string(&value).unwrap();
        let parsed = serde_json::from_str::<DataPackage<'_>>(&text).unwrap();

        prop_assert_eq!(parsed.into_static(), package);
    }

    fn extra(property: &str) -> ExtraProperties {
        let mut extra = ExtraProperties::default();
        extra.insert(property.to_owned(), serde_json::Value::Null);
        extra
    }

    /// The constraint errors of a value that is checked as part of a package.
    fn errors_of(check: impl Fn(&mut Vec<ConstraintError>)) -> Vec<ConstraintError> {
        let mut errors = Vec::new();
        check(&mut errors);
        errors
    }

    #[test]
    fn modeled_extension_properties_are_rejected_at_every_manifest_level() {
        assert!(DataPackageBuilder::new().extra(extra("resources")).is_err());
        assert!(SourceMetadata::new("source").extra(extra("path")).is_err());
        assert!(
            LicenseMetadata::named("CC0-1.0")
                .extra(extra("name"))
                .is_err()
        );
        assert!(
            ContributorMetadata::new("Archivist")
                .extra(extra("role"))
                .is_err()
        );
        for errors in [
            errors_of(|errors| {
                Source {
                    extra: extra("path"),
                    ..Source::default()
                }
                .push_constraint_errors(errors);
            }),
            errors_of(|errors| {
                License {
                    extra: extra("name"),
                    ..License::default()
                }
                .push_constraint_errors(errors);
            }),
            errors_of(|errors| {
                Contributor {
                    extra: extra("role"),
                    ..Contributor::default()
                }
                .push_constraint_errors(errors);
            }),
        ] {
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, ConstraintError::Extra(_)))
            );
        }
    }

    #[test]
    fn required_metadata_properties_are_checked() {
        let package = serde_json::from_str::<DataPackage<'_>>(
            r#"{
                "profile": "data-package",
                "wacz_version": "1.1.1",
                "resources": [],
                "name": "Example Collection",
                "sources": [{"path": "https://www.example.com/"}],
                "licenses": [{"title": "Some license"}],
                "contributors": [{"role": "editor"}]
            }"#,
        )
        .expect("a parseable manifest");

        assert_eq!(
            package.constraint_errors(),
            vec![
                ConstraintError::Name("Example Collection".to_owned()),
                ConstraintError::SourceTitle,
                ConstraintError::LicenseIdentity,
                ConstraintError::ContributorTitle,
                ConstraintError::ContributorRole("editor".to_owned()),
            ]
        );
    }

    #[test]
    fn conforming_metadata_has_no_constraint_errors() {
        let package =
            serde_json::from_str::<DataPackage<'_>>(EXAMPLE).expect("a parseable manifest");

        assert_eq!(package.constraint_errors(), Vec::new());
    }

    /// The example manifest from the WACZ 1.1.1 specification, with contextual properties added.
    const EXAMPLE: &str = r#"{
        "profile": "data-package",
        "wacz_version": "1.1.1",
        "name": "example-collection",
        "id": "urn:uuid:735c0f4b-b054-4bb2-a5b6-2b4c27ba0bc7",
        "title": "Example collection",
        "keywords": ["example", "crawl"],
        "homepage": "https://www.example.com/collections/example",
        "version": "1.0.0",
        "licenses": [{"name": "CC-BY-4.0"}],
        "contributors": [{"title": "An Archivist", "role": "author"}],
        "created": "2020-10-07T21:22:36Z",
        "mainPageUrl": "https://www.example.com/page",
        "custom": {"key": "value"},
        "resources": [
            {
                "name": "pages.jsonl",
                "path": "pages/pages.jsonl",
                "hash": "sha256:8a7fc0d302700bed02294404a627ddbbf0e35487565b1c6181c729dff8d2fff6",
                "bytes": 75
            },
            {
                "name": "data.warc",
                "path": "archive/data.warc",
                "hash": "sha256:0e7101316ba5d4b66f86a371ee615fbd20f9d3f32d32563ed2c829db062f7714",
                "bytes": 11469796
            }
        ]
    }"#;

    #[test]
    fn deserialize_example_manifest() -> Result<(), Box<dyn std::error::Error>> {
        let package = serde_json::from_str::<DataPackage<'_>>(EXAMPLE)?;

        assert_eq!(package.profile, PROFILE);
        assert_eq!(package.wacz_version, WACZ_VERSION);
        assert_eq!(package.name.as_deref(), Some("example-collection"));
        assert_eq!(package.title.as_deref(), Some("Example collection"));
        assert_eq!(package.keywords, vec!["example", "crawl"]);
        assert_eq!(package.version.as_deref(), Some("1.0.0"));
        assert_eq!(
            package.licenses,
            vec![License {
                name: Some("CC-BY-4.0".into()),
                ..License::default()
            }]
        );
        assert_eq!(
            package.contributors,
            vec![Contributor {
                title: Some("An Archivist".into()),
                role: Some("author".into()),
                ..Contributor::default()
            }]
        );
        assert_eq!(
            package.main_page_url.as_deref(),
            Some("https://www.example.com/page")
        );
        assert_eq!(package.resources.len(), 2);
        assert_eq!(package.resources[1].name, "data.warc");
        assert_eq!(package.resources[1].bytes, 11_469_796);
        assert!(package.extra.contains_key("custom"));

        Ok(())
    }

    #[test]
    fn round_trip_preserves_extra_properties() -> Result<(), Box<dyn std::error::Error>> {
        let package = serde_json::from_str::<DataPackage<'_>>(EXAMPLE)?.into_static();
        let encoded = serde_json::to_string(&package)?;

        assert_eq!(serde_json::from_str::<DataPackage<'_>>(&encoded)?, package);

        Ok(())
    }

    #[test]
    fn data_package_builder_covers_the_complete_metadata_surface() {
        let created = "2020-10-07T21:22:36Z".parse().expect("test date");
        let modified = "2020-10-08T21:22:36Z".parse().expect("test date");
        let main_page_date = "2020-10-07T20:00:00Z".parse().expect("test date");
        let mut extra = ExtraProperties::default();
        extra.insert("custom".to_owned(), serde_json::json!({ "key": "value" }));

        let package = DataPackageBuilder::new()
            .name("example-collection")
            .expect("valid package name")
            .id("urn:uuid:735c0f4b-b054-4bb2-a5b6-2b4c27ba0bc7")
            .title("Example collection")
            .description("An example archive")
            .keywords(["example", "crawl"])
            .homepage("https://example.com/collection")
            .image("images/collection.png")
            .version("1.0.0")
            .sources([SourceMetadata::new("example.com")])
            .licenses([LicenseMetadata::named("CC-BY-4.0")])
            .contributors([ContributorMetadata::new("An Archivist").role(ContributorRole::Author)])
            .created(created)
            .modified(modified)
            .software("example-archiver/1.0")
            .main_page_url("https://example.com/")
            .main_page_date(main_page_date)
            .extra(extra.clone())
            .expect("non-colliding extension properties")
            .into_data_package(Vec::new());

        assert_eq!(package.profile, PROFILE);
        assert_eq!(package.wacz_version, WACZ_VERSION);
        assert_eq!(package.name.as_deref(), Some("example-collection"));
        assert_eq!(
            package.id.as_deref(),
            Some("urn:uuid:735c0f4b-b054-4bb2-a5b6-2b4c27ba0bc7")
        );
        assert_eq!(package.title.as_deref(), Some("Example collection"));
        assert_eq!(package.description.as_deref(), Some("An example archive"));
        assert_eq!(package.keywords, ["example", "crawl"]);
        assert_eq!(
            package.homepage.as_deref(),
            Some("https://example.com/collection")
        );
        assert_eq!(package.image.as_deref(), Some("images/collection.png"));
        assert_eq!(package.version.as_deref(), Some("1.0.0"));
        assert_eq!(package.sources.len(), 1);
        assert_eq!(package.licenses.len(), 1);
        assert_eq!(package.contributors.len(), 1);
        assert_eq!(package.contributors[0].role.as_deref(), Some("author"));
        assert_eq!(package.created, Some(created));
        assert_eq!(package.modified, Some(modified));
        assert_eq!(package.software.as_deref(), Some("example-archiver/1.0"));
        assert_eq!(
            package.main_page_url.as_deref(),
            Some("https://example.com/")
        );
        assert_eq!(package.main_page_date, Some(main_page_date));
        assert_eq!(package.extra, extra);
        assert_eq!(package.resources, []);
    }

    #[test]
    fn deserialize_digest() -> Result<(), Box<dyn std::error::Error>> {
        let digest = serde_json::from_str::<DataPackageDigest<'_>>(
            r#"{
                "path": "datapackage.json",
                "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885"
            }"#,
        )?;

        assert_eq!(digest.path, crate::DATA_PACKAGE_PATH);
        assert!(digest.signed_data.is_none());

        Ok(())
    }
}
