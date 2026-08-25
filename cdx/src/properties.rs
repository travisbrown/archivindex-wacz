//! Extension properties preserved by JSON-backed models.

/// Additional JSON properties preserved verbatim, in insertion order.
///
/// Lenient models keep the properties they do not model here so that documents written by other
/// tools survive a read-modify-write cycle.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct ExtraProperties(serde_json::Map<String, serde_json::Value>);

/// An extension property duplicates a modeled property.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("extension property `{property}` duplicates a modeled property of {model}")]
pub struct Error {
    /// The model containing the extension map.
    pub model: &'static str,
    /// The duplicated property name.
    pub property: String,
}

impl ExtraProperties {
    /// Check that none of the extension keys duplicate a modeled property.
    pub fn validate(&self, model: &'static str, reserved: &[&str]) -> Result<(), Error> {
        reserved
            .iter()
            .find(|property| self.contains_key(**property))
            .map_or(Ok(()), |property| {
                Err(Error {
                    model,
                    property: (*property).to_owned(),
                })
            })
    }
}

impl From<serde_json::Map<String, serde_json::Value>> for ExtraProperties {
    fn from(map: serde_json::Map<String, serde_json::Value>) -> Self {
        Self(map)
    }
}

impl From<ExtraProperties> for serde_json::Map<String, serde_json::Value> {
    fn from(properties: ExtraProperties) -> Self {
        properties.0
    }
}

impl std::ops::Deref for ExtraProperties {
    type Target = serde_json::Map<String, serde_json::Value>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ExtraProperties {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::ToBoundedStatic for ExtraProperties {
    type Static = Self;

    fn to_static(&self) -> Self::Static {
        self.clone()
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::IntoBoundedStatic for ExtraProperties {
    type Static = Self;

    fn into_static(self) -> Self::Static {
        self
    }
}
