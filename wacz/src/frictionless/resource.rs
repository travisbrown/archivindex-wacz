//! The [Frictionless Data Resource](https://specs.frictionlessdata.io/data-resource/) descriptors
//! that make up the manifest's `resources` array.
//!
//! The WACZ specification requires the `name`, `path`, `hash`, and `bytes` properties for every
//! file; the remaining properties modeled here are optional metadata defined by the Data Resource
//! specification. As elsewhere in this crate, parsing is lenient: properties beyond those modeled
//! (for example `data` or `schema`) are preserved in [`Resource::extra`].

use std::borrow::Cow;

use bounded_static::ToStatic;

use crate::ExtraProperties;
use crate::digest::Sha256Digest;

use super::{License, Source};

/// A file in the WACZ as listed in the manifest.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
pub struct Resource<'a> {
    /// The file name (the final segment of its path).
    #[serde(borrow)]
    pub name: Cow<'a, str>,
    /// The path relative to the root of the WACZ.
    ///
    /// The Data Resource specification also allows URLs and arrays of paths here, but WACZ files
    /// are identified by a single path relative to the WACZ root.
    #[serde(borrow)]
    pub path: Cow<'a, str>,
    /// The SHA-256 digest of the file's contents.
    ///
    /// The Data Resource specification allows other digest algorithms, but the WACZ specification
    /// requires SHA-256.
    pub hash: Sha256Digest,
    /// The file size in bytes.
    pub bytes: u64,
    /// The profile identifier of this descriptor (for example `data-resource`).
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub profile: Option<Cow<'a, str>>,
    /// A short description of the file.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Cow<'a, str>>,
    /// A longer, possibly Markdown-formatted, description of the file.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub description: Option<Cow<'a, str>>,
    /// The file format (for example `warc` or `jsonl`), typically the path's extension.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub format: Option<Cow<'a, str>>,
    /// The media type (for example `application/warc`).
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub mediatype: Option<Cow<'a, str>>,
    /// The character encoding; UTF-8 when absent.
    #[serde(
        default,
        deserialize_with = "crate::attributes::borrowed_option_str",
        skip_serializing_if = "Option::is_none"
    )]
    pub encoding: Option<Cow<'a, str>>,
    /// The sources of the file's data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Source<'a>>,
    /// The licenses under which the file is provided.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub licenses: Vec<License<'a>>,
    /// Additional properties, preserved verbatim for round-tripping.
    #[serde(flatten)]
    pub extra: ExtraProperties,
}

impl<'a> Resource<'a> {
    /// Create a resource with the properties the WACZ specification requires, leaving all of the
    /// optional descriptive metadata unset.
    pub fn new(
        name: impl Into<Cow<'a, str>>,
        path: impl Into<Cow<'a, str>>,
        hash: Sha256Digest,
        bytes: u64,
    ) -> Self {
        Resource {
            name: name.into(),
            path: path.into(),
            hash,
            bytes,
            profile: None,
            title: None,
            description: None,
            format: None,
            mediatype: None,
            encoding: None,
            sources: Vec::new(),
            licenses: Vec::new(),
            extra: ExtraProperties::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), crate::ExtraPropertyError> {
        crate::validate_extra(
            "Resource",
            &self.extra,
            &[
                "name",
                "path",
                "hash",
                "bytes",
                "profile",
                "title",
                "description",
                "format",
                "mediatype",
                "encoding",
                "sources",
                "licenses",
            ],
        )?;
        for source in &self.sources {
            source.validate()?;
        }
        for license in &self.licenses {
            license.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bounded_static::IntoBoundedStatic;

    use super::*;

    #[test]
    fn modeled_extension_properties_are_rejected() {
        let mut resource = Resource::new("data", "data", Sha256Digest::compute(""), 0);
        resource
            .extra
            .insert("bytes".to_owned(), serde_json::Value::from(1));

        assert!(resource.validate().is_err());
    }

    /// A resource descriptor using the optional Data Resource metadata properties.
    const EXAMPLE: &str = r#"{
        "name": "data.warc.gz",
        "path": "archive/data.warc.gz",
        "hash": "sha256:0e7101316ba5d4b66f86a371ee615fbd20f9d3f32d32563ed2c829db062f7714",
        "bytes": 11469796,
        "profile": "data-resource",
        "title": "Example crawl",
        "description": "A crawl of *example.com*.",
        "format": "warc",
        "mediatype": "application/warc",
        "encoding": "utf-8",
        "sources": [{"title": "example.com", "path": "https://www.example.com"}],
        "licenses": [{"name": "CC-BY-4.0", "custom": true}],
        "schema": {"fields": []}
    }"#;

    #[test]
    fn deserialize_example_resource() -> Result<(), Box<dyn std::error::Error>> {
        let resource = serde_json::from_str::<Resource<'_>>(EXAMPLE)?;

        assert_eq!(resource.name, "data.warc.gz");
        assert_eq!(resource.bytes, 11_469_796);
        assert_eq!(resource.profile.as_deref(), Some("data-resource"));
        assert_eq!(resource.format.as_deref(), Some("warc"));
        assert_eq!(resource.mediatype.as_deref(), Some("application/warc"));
        assert_eq!(
            resource.sources,
            vec![Source {
                title: Some("example.com".into()),
                path: Some("https://www.example.com".into()),
                ..Source::default()
            }]
        );
        assert_eq!(resource.licenses[0].name.as_deref(), Some("CC-BY-4.0"));
        assert!(resource.licenses[0].extra.contains_key("custom"));
        assert!(resource.extra.contains_key("schema"));

        Ok(())
    }

    #[test]
    fn round_trip_preserves_extra_properties() -> Result<(), Box<dyn std::error::Error>> {
        let resource = serde_json::from_str::<Resource<'_>>(EXAMPLE)?.into_static();
        let encoded = serde_json::to_string(&resource)?;

        assert_eq!(serde_json::from_str::<Resource<'_>>(&encoded)?, resource);

        Ok(())
    }

    #[test]
    fn new_serializes_only_required_properties() -> Result<(), Box<dyn std::error::Error>> {
        let resource = Resource::new(
            "data.warc",
            "archive/data.warc",
            Sha256Digest::compute(""),
            0,
        );
        let encoded = serde_json::to_value(&resource)?;

        let keys: Vec<&str> = encoded
            .as_object()
            .expect("a resource serializes as an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["name", "path", "hash", "bytes"]);

        Ok(())
    }
}
