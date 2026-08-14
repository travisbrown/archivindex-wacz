//! The `datapackage.json` manifest and `datapackage-digest.json` formats.
//!
//! A WACZ manifest is a [Frictionless Data Package][data-package] descriptor that lists every other
//! file in the WACZ with its size and SHA-256 digest. The digest file records the digest of the
//! serialized manifest and may include a cryptographic signature.
//!
//! [data-package]: https://specs.frictionlessdata.io/data-package/
//!
//! Parsing is lenient: properties beyond those modeled here are preserved in [`DataPackage::extra`]
//! and [`Resource::extra`] so that manifests written by other tools survive a read-modify-write
//! cycle. The [`signature`] submodule models the digest file's signature envelope, whose parsing is
//! strict as its specification requires.

use std::borrow::Cow;

use bounded_static::ToStatic;
use chrono::{DateTime, Utc};

use crate::ExtraProperties;
use crate::digest::Sha256Digest;

pub mod resource;
pub mod signature;

pub use resource::Resource;

use signature::SignatureData;

/// The Frictionless Data Package profile identifier required by the WACZ specification.
pub const PROFILE: &str = "data-package";

/// The WACZ specification version targeted by this crate.
pub const WACZ_VERSION: &str = "1.1.1";

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
    /// The Data Package specification restricts this to lowercase characters from `a-z0-9._-`; this
    /// crate does not enforce that.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    /// When the WACZ file was last modified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub main_page_url: Option<Cow<'a, str>>,
    /// The capture date to use when replaying the primary entry page.
    #[serde(
        rename = "mainPageDate",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub main_page_date: Option<DateTime<Utc>>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

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

/// A license under which a package or resource is provided.
///
/// The Data Package specification requires at least one of `name` or `path`; this crate does not
/// enforce that.
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

/// A person or organization who contributed to a package.
///
/// The Data Package specification requires `title` and restricts `role` to `author`, `publisher`,
/// `maintainer`, `wrangler`, and `contributor` (the default); this crate does not enforce either.
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

#[cfg(test)]
mod tests {
    use bounded_static::IntoBoundedStatic;

    use super::*;

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
