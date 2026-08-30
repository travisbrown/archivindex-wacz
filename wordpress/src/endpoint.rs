//! `WordPress` REST API v2 collection endpoints.

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

/// Collection `rest_base` values intended to be excluded from custom endpoint discovery.
///
/// These are the core editor and theme collections `WordPress` registers as post types or
/// taxonomies but only serves to authenticated users. Applying this list is temporarily disabled.
pub const ENDPOINT_EXCLUSIONS: [&str; 6] = [
    "template-parts",
    "templates",
    "menus",
    "menu-items",
    "global-styles",
    "font-families",
];

/// A root resource listing the post types or taxonomies a site registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Registry {
    /// The `types` resource; its entries are checked for custom endpoints first.
    Types,
    /// The `taxonomies` resource.
    Taxonomies,
}

impl Registry {
    /// Both registries, in the order their entries are checked for custom endpoints.
    pub const ALL: [Self; 2] = [Self::Types, Self::Taxonomies];

    /// The resource's path relative to the installation root, as listed in [`ROOT_ENDPOINTS`].
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::Types => "wp-json/wp/v2/types",
            Self::Taxonomies => "wp-json/wp/v2/taxonomies",
        }
    }
}

/// A post type or taxonomy exposed by the `WordPress` REST API.
///
/// Values of this shape are returned under each property of the `/wp/v2/types` and
/// `/wp/v2/taxonomies` response objects. [`items_url`](Self::items_url) identifies the REST
/// collection advertised by the entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct EndpointType {
    /// The human-readable plural name.
    pub name: String,
    /// A description supplied by `WordPress` or the registering plugin.
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
    /// Parse a registry response into its entries, in response order.
    ///
    /// The response is a JSON object keyed by slug. Its key order is the order custom endpoints
    /// are archived in, so the entries are read as a sequence rather than a map.
    ///
    /// # Errors
    ///
    /// Returns the JSON error when the payload is not an object of registry entries.
    pub fn parse_registry(payload: &[u8]) -> Result<Vec<Self>, serde_json::Error> {
        serde_json::from_slice::<RegistryEntries>(payload).map(|entries| entries.0)
    }

    /// The first collection URL advertised by the `wp:items` link relation.
    #[must_use]
    pub fn items_url(&self) -> Option<&str> {
        self.links.wp_items.first().map(|link| link.href.as_str())
    }

    /// Every collection URL advertised by the `wp:items` link relation.
    pub fn items_urls(&self) -> impl Iterator<Item = &str> {
        self.links.wp_items.iter().map(|link| link.href.as_str())
    }

    /// The `rest_base` values of `entries` that name custom `wp/v2` collections, in order.
    ///
    /// Entries for supported [`Endpoint`]s, parameterized route patterns, and other namespaces
    /// (whose routes are not under `wp-json/wp/v2/`) are skipped. Repeated values are yielded each
    /// time they occur.
    pub fn custom_endpoints<'a>(
        entries: impl IntoIterator<Item = &'a Self>,
    ) -> impl Iterator<Item = &'a str> {
        entries
            .into_iter()
            .filter(|entry| {
                entry.rest_namespace == "wp/v2"
                    && entry.rest_base.parse::<Endpoint>().is_err()
                    // Temporarily probe the enumerated exclusions to determine which are public.
                    // && !ENDPOINT_EXCLUSIONS.contains(&entry.rest_base.as_str())
                    && !entry.rest_base.contains("(?P<")
            })
            .map(|entry| entry.rest_base.as_str())
    }
}

/// A registry response's entries in response order, ignoring their slug keys.
struct RegistryEntries(Vec<EndpointType>);

impl<'de> serde::de::Deserialize<'de> for RegistryEntries {
    fn deserialize<D: serde::de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RegistryVisitor;

        impl<'de> serde::de::Visitor<'de> for RegistryVisitor {
            type Value = RegistryEntries;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object of post types or taxonomies keyed by slug")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((_, entry)) =
                    map.next_entry::<serde::de::IgnoredAny, EndpointType>()?
                {
                    entries.push(entry);
                }

                Ok(RegistryEntries(entries))
            }
        }

        deserializer.deserialize_map(RegistryVisitor)
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

/// A supported REST API v2 collection endpoint. The variant order is the order endpoints are
/// archived in.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Endpoint {
    /// The `pages` collection.
    Pages,
    /// The `posts` collection.
    Posts,
    /// The `media` collection.
    Media,
    /// The `comments` collection.
    Comments,
    /// The `users` collection.
    Users,
    /// The `categories` collection.
    Categories,
    /// The `tags` collection.
    Tags,
    /// The `navigation` collection of block-theme navigation menus.
    Navigation,
}

impl Endpoint {
    /// Every supported endpoint, in the order they are probed and paged.
    pub const ALL: [Self; 8] = [
        Self::Pages,
        Self::Posts,
        Self::Media,
        Self::Comments,
        Self::Users,
        Self::Categories,
        Self::Tags,
        Self::Navigation,
    ];

    /// The collection's name, which is the last segment of its endpoint path.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pages => "pages",
            Self::Posts => "posts",
            Self::Media => "media",
            Self::Comments => "comments",
            Self::Users => "users",
            Self::Categories => "categories",
            Self::Tags => "tags",
            Self::Navigation => "navigation",
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
    "unknown WordPress endpoint {0:?}; expected one of pages, posts, media, comments, users, \
     categories, tags, navigation"
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

/// A collection an archive probes and pages: a supported endpoint or one a registry advertised.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Collection {
    /// A supported endpoint, probed for its own sake.
    Known(Endpoint),
    /// A custom endpoint, probed via the registry response that advertised it.
    Custom {
        /// The collection's `rest_base`, which is the last segment of its endpoint path.
        name: String,
        /// The registry whose response advertised the collection.
        registry: Registry,
    },
}

impl Collection {
    /// The collection's name, which is the last segment of its endpoint path.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Known(endpoint) => endpoint.name(),
            Self::Custom { name, .. } => name,
        }
    }

    /// The registry a custom collection was advertised by; `None` for a supported endpoint.
    #[must_use]
    pub const fn registry(&self) -> Option<Registry> {
        match self {
            Self::Known(_) => None,
            Self::Custom { registry, .. } => Some(*registry),
        }
    }

    /// The supported endpoint or custom collection among `custom` named `name`.
    pub fn find<'a>(name: &str, custom: impl IntoIterator<Item = &'a Self>) -> Option<Self> {
        name.parse::<Endpoint>().map_or_else(
            |_| {
                custom
                    .into_iter()
                    .find(|collection| collection.name() == name)
                    .cloned()
            },
            |endpoint| Some(Self::Known(endpoint)),
        )
    }
}

impl From<Endpoint> for Collection {
    fn from(endpoint: Endpoint) -> Self {
        Self::Known(endpoint)
    }
}

impl fmt::Display for Collection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Collection, ENDPOINT_EXCLUSIONS, Endpoint, EndpointType, ROOT_ENDPOINTS, Registry,
    };

    /// A registry entry with a `rest_base` and namespace, in the shape `WordPress` returns.
    fn entry(rest_base: &str, rest_namespace: &str) -> String {
        format!(
            r#"{{"name": "", "description": "", "hierarchical": false, "slug": "{rest_base}",
                "rest_base": "{rest_base}", "rest_namespace": "{rest_namespace}",
                "_links": {{"wp:items": [{{"href": "https://example.com/x"}}]}}}}"#
        )
    }

    #[test]
    fn endpoint_names_parse_exactly() {
        for endpoint in Endpoint::ALL {
            assert_eq!(endpoint.name().parse::<Endpoint>().ok(), Some(endpoint));
        }
        assert!("Posts".parse::<Endpoint>().is_err());
        assert!("posts/".parse::<Endpoint>().is_err());
        assert_eq!(
            "navigation".parse::<Endpoint>().ok(),
            Some(Endpoint::Navigation)
        );
    }

    #[test]
    fn registries_are_roots() {
        for registry in Registry::ALL {
            assert!(ROOT_ENDPOINTS.contains(&registry.path()));
        }
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

        let entries = EndpointType::parse_registry(response).expect("a registry response");
        let [post, category] = entries.as_slice() else {
            panic!("expected two entries, got {entries:?}");
        };
        assert_eq!(
            post.items_url(),
            Some("https://example.com/wp-json/wp/v2/posts")
        );
        assert_eq!(post.taxonomies, ["category", "post_tag"]);
        assert!(post.types.is_empty());

        assert_eq!(
            category.items_urls().collect::<Vec<_>>(),
            ["https://example.com/wp-json/wp/v2/categories"]
        );
        assert_eq!(category.types, ["post"]);
        assert!(category.taxonomies.is_empty());

        assert!(EndpointType::parse_registry(b"[]").is_err());
        assert!(EndpointType::parse_registry(b"{\"post\": {}}").is_err());
    }

    #[test]
    fn custom_endpoints_keep_response_order_and_skip_known_and_route_pattern_entries() {
        let response = format!(
            "{{\"video\": {}, \"post\": {}, \"template\": {}, \"product\": {}, \"again\": {}, \
             \"plugin\": {}, \"pattern\": {}, \"menu\": {}}}",
            entry("videos", "wp/v2"),
            entry("posts", "wp/v2"),
            entry(ENDPOINT_EXCLUSIONS[1], "wp/v2"),
            entry("product", "wp/v2"),
            entry("videos", "wp/v2"),
            entry("things", "plugin/v1"),
            entry(
                r"font-families/(?P<font_family_id>[\\d]+)/font-faces",
                "wp/v2",
            ),
            entry("navigation", "wp/v2"),
        );

        let entries = EndpointType::parse_registry(response.as_bytes()).expect("entries");

        assert_eq!(
            EndpointType::custom_endpoints(&entries).collect::<Vec<_>>(),
            ["videos", "templates", "product", "videos"]
        );
    }

    #[test]
    fn a_collection_is_found_by_name_among_the_supported_and_custom_endpoints() {
        let custom = [Collection::Custom {
            name: "videos".to_owned(),
            registry: Registry::Taxonomies,
        }];

        assert_eq!(
            Collection::find("media", &custom),
            Some(Collection::from(Endpoint::Media))
        );
        assert_eq!(Collection::find("videos", &custom), Some(custom[0].clone()));
        assert_eq!(Collection::find("Videos", &custom), None);
        assert_eq!(Collection::find("videos", &[]), None);
        assert_eq!(custom[0].name(), "videos");
        assert_eq!(custom[0].to_string(), "videos");
        assert_eq!(custom[0].registry(), Some(Registry::Taxonomies));
        assert_eq!(Collection::from(Endpoint::Pages).registry(), None);
    }
}
