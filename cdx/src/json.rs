//! The JSON output model of the Internet Archive CDX Server API.
//!
//! This representation is an outer JSON array whose first row names the fields and whose
//! remaining rows contain string values. A resumable response may end with an empty row followed
//! by a one-value resume-key row.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeSeq;

use crate::capture::{self, Capture};
use crate::field::Field;

/// A CDX Server JSON document is structurally or semantically invalid.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    /// Records or a resume key were provided without a header.
    #[error("CDX JSON records require a nonempty header")]
    MissingHeader,
    /// A record has a different number of values from the header.
    #[error("CDX JSON record {record} has {actual} fields, expected {expected}")]
    FieldCount {
        /// Zero-based record index.
        record: usize,
        /// Number of fields named by the header.
        expected: usize,
        /// Number of values in the record.
        actual: usize,
    },
    /// The resume trailer has an invalid shape.
    #[error("invalid CDX JSON resume trailer")]
    InvalidResumeTrailer,
    /// A row cannot be converted to the common capture model.
    #[error("invalid CDX JSON record {record}")]
    Capture {
        /// Zero-based record index.
        record: usize,
        /// The invalid field.
        #[source]
        source: capture::Error,
    },
}

/// A header-driven CDX Server JSON document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document<'a> {
    fields: Vec<Cow<'a, str>>,
    records: Vec<Vec<Cow<'a, str>>>,
    resume_key: Option<Cow<'a, str>>,
}

impl<'a> Document<'a> {
    /// Construct a document after validating every row against the header.
    pub fn new(
        fields: Vec<Cow<'a, str>>,
        records: Vec<Vec<Cow<'a, str>>>,
        resume_key: Option<Cow<'a, str>>,
    ) -> Result<Self, Error> {
        let document = Self {
            fields,
            records,
            resume_key,
        };
        document.validate()?;
        Ok(document)
    }

    /// The names in the header row.
    #[must_use]
    pub fn fields(&self) -> &[Cow<'a, str>] {
        &self.fields
    }

    /// The record rows.
    #[must_use]
    pub fn records(&self) -> &[Vec<Cow<'a, str>>] {
        &self.records
    }

    /// The key used to request the next page, when present.
    #[must_use]
    pub fn resume_key(&self) -> Option<&str> {
        self.resume_key.as_deref()
    }

    /// Convert all rows to representation-neutral captures.
    pub fn into_captures(self) -> Result<CaptureList<'a>, Error> {
        let Self {
            fields,
            records,
            resume_key,
        } = self;
        let semantic_fields = fields
            .iter()
            .map(|name| Field::named(name))
            .collect::<Vec<_>>();
        let values = records
            .into_iter()
            .enumerate()
            .map(|(record, values)| {
                let fields = semantic_fields
                    .iter()
                    .cloned()
                    .zip(values)
                    .collect::<Vec<_>>();
                crate::capture::from_fields(&fields)
                    .map_err(|source| Error::Capture { record, source })
            })
            .collect::<Result<_, _>>()?;
        Ok(CaptureList { values, resume_key })
    }

    /// Detach this document from its input.
    #[must_use]
    pub fn into_owned(self) -> Document<'static> {
        Document {
            fields: own_row(self.fields),
            records: self.records.into_iter().map(own_row).collect(),
            resume_key: self.resume_key.map(|value| Cow::Owned(value.into_owned())),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        if self.fields.is_empty() && (!self.records.is_empty() || self.resume_key.is_some()) {
            return Err(Error::MissingHeader);
        }
        for (record, values) in self.records.iter().enumerate() {
            if values.len() != self.fields.len() {
                return Err(Error::FieldCount {
                    record,
                    expected: self.fields.len(),
                    actual: values.len(),
                });
            }
        }
        Ok(())
    }
}

impl serde::Serialize for Document<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.fields.is_empty() {
            return serializer.serialize_seq(Some(0))?.end();
        }
        let trailer_length = usize::from(self.resume_key.is_some()) * 2;
        let mut sequence =
            serializer.serialize_seq(Some(1 + self.records.len() + trailer_length))?;
        sequence.serialize_element(&self.fields)?;
        for record in &self.records {
            sequence.serialize_element(record)?;
        }
        if let Some(resume_key) = &self.resume_key {
            sequence.serialize_element(&Vec::<String>::new())?;
            sequence.serialize_element(&[resume_key])?;
        }
        sequence.end()
    }
}

impl<'a, 'de: 'a> serde::Deserialize<'de> for Document<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DocumentVisitor<'a>(PhantomData<&'a ()>);

        impl<'a, 'de: 'a> Visitor<'de> for DocumentVisitor<'a> {
            type Value = Document<'a>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a CDX JSON array with a header row")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let Some(Row(fields)) = sequence.next_element::<Row<'a>>()? else {
                    return Ok(Document {
                        fields: Vec::new(),
                        records: Vec::new(),
                        resume_key: None,
                    });
                };
                if fields.is_empty() {
                    return Err(serde::de::Error::custom(Error::MissingHeader));
                }

                let mut records = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(10_000));
                let mut resume_key = None;
                while let Some(Row(row)) = sequence.next_element::<Row<'a>>()? {
                    if row.is_empty() {
                        let Some(Row(key)) = sequence.next_element::<Row<'a>>()? else {
                            return Err(serde::de::Error::custom(Error::InvalidResumeTrailer));
                        };
                        if key.len() != 1 || sequence.next_element::<Row<'a>>()?.is_some() {
                            return Err(serde::de::Error::custom(Error::InvalidResumeTrailer));
                        }
                        resume_key = key.into_iter().next();
                        break;
                    }
                    let record = records.len();
                    if row.len() != fields.len() {
                        return Err(serde::de::Error::custom(Error::FieldCount {
                            record,
                            expected: fields.len(),
                            actual: row.len(),
                        }));
                    }
                    records.push(row);
                }

                Ok(Document {
                    fields,
                    records,
                    resume_key,
                })
            }
        }

        deserializer.deserialize_seq(DocumentVisitor(PhantomData))
    }
}

/// Captures decoded from a CDX Server JSON document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureList<'a> {
    /// The decoded captures.
    pub values: Vec<Capture<'a>>,
    /// The key used to request the next page.
    pub resume_key: Option<Cow<'a, str>>,
}

impl CaptureList<'_> {
    /// Detach this list from its input.
    #[must_use]
    pub fn into_owned(self) -> CaptureList<'static> {
        CaptureList {
            values: self.values.into_iter().map(Capture::into_owned).collect(),
            resume_key: self.resume_key.map(|value| Cow::Owned(value.into_owned())),
        }
    }
}

impl<'a, 'de: 'a> serde::Deserialize<'de> for CaptureList<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Document::deserialize(deserializer)?
            .into_captures()
            .map_err(serde::de::Error::custom)
    }
}

/// Compatibility name for the common seven-field CDX Server result model.
pub type ItemList<'a> = CaptureList<'a>;

/// Compatibility name for extended CDX Server result models.
pub type ExtendedItemList<'a> = CaptureList<'a>;

#[derive(serde::Deserialize)]
struct Row<'a>(
    #[serde(borrow, deserialize_with = "crate::attributes::borrowed_str_seq")] Vec<Cow<'a, str>>,
);

fn own_row(values: Vec<Cow<'_, str>>) -> Vec<Cow<'static, str>> {
    values
        .into_iter()
        .map(|value| Cow::Owned(value.into_owned()))
        .collect()
}

#[cfg(feature = "bounded-static")]
impl bounded_static::ToBoundedStatic for Document<'_> {
    type Static = Document<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::IntoBoundedStatic for Document<'_> {
    type Static = Document<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::ToBoundedStatic for CaptureList<'_> {
    type Static = CaptureList<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::IntoBoundedStatic for CaptureList<'_> {
    type Static = CaptureList<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = concat!(
        "[[\"urlkey\",\"timestamp\",\"url\",\"mime\",\"status\",\"digest\",\"length\"],",
        "[\"com,example)/\",\"20201007212236\",\"https://example.com/\",",
        "\"text/html\",\"200\",\"sha1:TEST\",\"1300\"],[],[\"next-key\"]]"
    );

    #[test]
    fn decodes_header_rows_and_resume_key() -> Result<(), Box<dyn std::error::Error>> {
        let list: CaptureList<'_> = serde_json::from_str(EXAMPLE)?;
        assert_eq!(list.values.len(), 1);
        assert_eq!(list.values[0].status, Some(200));
        assert_eq!(list.values[0].length, Some(1300));
        assert_eq!(list.resume_key.as_deref(), Some("next-key"));
        Ok(())
    }

    #[test]
    fn decodes_wayback_aliases_and_unusual_values() -> Result<(), Box<dyn std::error::Error>> {
        let input = concat!(
            "[[\"urlkey\",\"timestamp\",\"original\",\"mimetype\",\"statuscode\",",
            "\"digest\",\"length\"],[\"com,example)/\",\"20201007212236\",",
            "\"https://example.com/\",\"application/problem+json\",\"530\",",
            "\"INVALID-DIGEST\",\"-1\"]]"
        );
        let document: Document<'_> = serde_json::from_str(input)?;
        assert!(matches!(document.fields[0], Cow::Borrowed(_)));
        assert!(matches!(document.records[0][0], Cow::Borrowed(_)));

        let list = document.into_captures()?;
        assert_eq!(
            list.values[0].mime.as_deref(),
            Some("application/problem+json")
        );
        assert_eq!(list.values[0].status, Some(530));
        assert_eq!(list.values[0].digest.as_deref(), Some("INVALID-DIGEST"));
        assert_eq!(list.values[0].length, Some(-1));
        Ok(())
    }

    #[test]
    fn raw_document_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let document: Document<'_> = serde_json::from_str(EXAMPLE)?;
        let serialized = serde_json::to_string(&document)?;
        assert_eq!(serde_json::from_str::<Document<'_>>(&serialized)?, document);
        Ok(())
    }

    #[test]
    fn empty_document_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        let document: Document<'_> = serde_json::from_str("[]")?;
        assert_eq!(serde_json::to_string(&document)?, "[]");
        Ok(())
    }

    #[test]
    fn rejects_short_rows() {
        let result = serde_json::from_str::<Document<'_>>(
            "[[\"urlkey\",\"timestamp\"],[\"com,example)/\"]]",
        );
        assert!(result.is_err());
    }
}
