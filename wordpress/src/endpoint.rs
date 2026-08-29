//! WordPress REST API v2 collection endpoints.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

/// API resources captured before collection endpoints are probed.
pub const ROOT_ENDPOINTS: [&str; 8] = [
    "wp-json",
    "wp-json/wp/v2",
    "wp-json/wp/v2/types",
    "wp-json/wp/v2/taxonomies",
    "wp-json/wp/v2/block-types",
    "wp-json/wp/v2/block-patterns/categories",
    "wp-json/wp/v2/block-patterns/patterns",
    "wp-json/wp/v2/menu-locations",
];

/// REST routes that endpoint discovery should not treat as unknown collections.
///
/// Entries may be installation-relative REST paths, like the archive roots, or collection
/// `rest_base` values.
pub static ENDPOINT_EXCLUSIONS: &[&str] = &[
    "wp-json",
    "wp-json/wp/v2",
    "wp-json/wp/v2/types",
    "wp-json/wp/v2/taxonomies",
    "wp-json/wp/v2/block-types",
    "wp-json/wp/v2/block-patterns/categories",
    "wp-json/wp/v2/block-patterns/patterns",
    "wp-json/wp/v2/menu-locations",
    "template-parts",
    "templates",
    "menus",
    "menu-items",
    "global-styles",
    "font-families",
];

/// A post type or taxonomy exposed by the WordPress REST API.
///
/// Values of this shape are returned under each property of the `/wp/v2/types` and
/// `/wp/v2/taxonomies` response objects. [`items_url`](Self::items_url) identifies the REST
/// collection advertised by the entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct EndpointType {
    /// The human-readable plural name.
    pub name: String,
    /// A description supplied by WordPress or the registering plugin.
    pub description: String,
    /// Whether entries can have parents of the same type.
    pub hierarchical: bool,
    /// The internal post type or taxonomy name.
    pub slug: String,
    /// The collection route relative to its REST namespace.
    pub rest_base: String,
    /// The REST namespace containing the collection, normally `wp/v2`.
    pub rest_namespace: String,
    /// Taxonomies registered for a post type. Empty for taxonomy responses.
    #[serde(default)]
    pub taxonomies: Vec<String>,
    /// Post types registered for a taxonomy. Empty for post type responses.
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(rename = "_links")]
    links: EndpointTypeLinks,
}

impl EndpointType {
    /// The first collection URL advertised by the `wp:items` link relation.
    #[must_use]
    pub fn items_url(&self) -> Option<&str> {
        self.links.wp_items.first().map(|link| link.href.as_str())
    }

    /// Every collection URL advertised by the `wp:items` link relation.
    pub fn items_urls(&self) -> impl Iterator<Item = &str> {
        self.links.wp_items.iter().map(|link| link.href.as_str())
    }

    /// Sorted, deduplicated `rest_base` values that are neither known nor excluded endpoints.
    #[must_use]
    pub fn unknown_endpoints<'a>(
        endpoint_types: impl IntoIterator<Item = &'a Self>,
    ) -> Vec<&'a str> {
        let mut unknown = endpoint_types
            .into_iter()
            .filter(|endpoint_type| {
                !Endpoint::ALL
                    .iter()
                    .any(|endpoint| endpoint.name() == endpoint_type.rest_base)
                    && !endpoint_type.is_excluded()
            })
            .map(|endpoint_type| endpoint_type.rest_base.as_str())
            .collect::<Vec<_>>();
        unknown.sort_unstable();
        unknown.dedup();
        unknown
    }

    fn is_excluded(&self) -> bool {
        ENDPOINT_EXCLUSIONS.iter().any(|exclusion| {
            *exclusion == self.rest_base
                || exclusion
                    .strip_prefix("wp-json/")
                    .and_then(|path| path.strip_prefix(&self.rest_namespace))
                    .and_then(|path| path.strip_prefix('/'))
                    .is_some_and(|path| path == self.rest_base)
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct EndpointTypeLinks {
    #[serde(rename = "wp:items", default)]
    wp_items: Vec<EndpointTypeLink>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct EndpointTypeLink {
    href: String,
}

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
}

impl Endpoint {
    /// Every endpoint, in the order they are probed and paged.
    pub const ALL: [Self; 7] = [
        Self::Pages,
        Self::Posts,
        Self::Categories,
        Self::Tags,
        Self::Users,
        Self::Comments,
        Self::Media,
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
     comments, media"
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{Endpoint, EndpointType};

    #[test]
    fn endpoint_names_parse_exactly() {
        for endpoint in Endpoint::ALL {
            assert_eq!(endpoint.name().parse::<Endpoint>().ok(), Some(endpoint));
        }
        assert!("Posts".parse::<Endpoint>().is_err());
        assert!("posts/".parse::<Endpoint>().is_err());
    }

    #[test]
    fn endpoint_types_parse_type_and_taxonomy_responses() {
        let response = br#"{
            "post": {
                "name": "Posts",
                "description": "",
                "hierarchical": false,
                "slug": "post",
                "rest_base": "posts",
                "rest_namespace": "wp/v2",
                "taxonomies": ["category", "post_tag"],
                "_links": {"wp:items": [{"href": "https://example.com/wp-json/wp/v2/posts"}]}
            },
            "category": {
                "name": "Categories",
                "description": "",
                "hierarchical": true,
                "slug": "category",
                "rest_base": "categories",
                "rest_namespace": "wp/v2",
                "types": ["post"],
                "_links": {"wp:items": [{"href": "https://example.com/wp-json/wp/v2/categories"}]}
            }
        }"#;

        let entries: HashMap<String, EndpointType> =
            serde_json::from_slice(response).expect("an endpoint response");
        let post = &entries["post"];
        assert_eq!(
            post.items_url(),
            Some("https://example.com/wp-json/wp/v2/posts")
        );
        assert_eq!(post.taxonomies, ["category", "post_tag"]);
        assert!(post.types.is_empty());

        let category = &entries["category"];
        assert_eq!(
            category.items_urls().collect::<Vec<_>>(),
            ["https://example.com/wp-json/wp/v2/categories"]
        );
        assert_eq!(category.types, ["post"]);
        assert!(category.taxonomies.is_empty());
    }

    #[test]
    fn unknown_endpoints_exclude_known_roots_and_static_exclusions() {
        let response = br#"{
            "post": {
                "name": "Posts", "description": "", "hierarchical": false,
                "slug": "post", "rest_base": "posts", "rest_namespace": "wp/v2",
                "_links": {"wp:items": []}
            }
        }"#;
        let mut entries: HashMap<String, EndpointType> =
            serde_json::from_slice(response).expect("an endpoint response");
        let prototype = entries["post"].clone();
        for (key, rest_base) in [
            ("types", "types"),
            ("template", "templates"),
            ("fonts", "font-families"),
            ("video", "videos"),
            ("duplicate-video", "videos"),
            ("product", "product"),
        ] {
            entries.insert(
                key.to_owned(),
                EndpointType {
                    rest_base: rest_base.to_owned(),
                    ..prototype.clone()
                },
            );
        }

        assert_eq!(
            EndpointType::unknown_endpoints(entries.values()),
            ["product", "videos"]
        );
    }
}
