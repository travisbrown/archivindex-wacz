//! Serde helpers shared across the WACZ wire formats.

use std::borrow::Cow;
use std::fmt::{self, Display};
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{Deserializer, SeqAccess, Unexpected, Visitor};
use serde::ser::Serializer;

/// A visitor that borrows string data from the input when possible.
///
/// Serde's derived `Cow<str>` deserialization always allocates; this is the zero-copy alternative
/// behind the `borrowed_*` helpers in this module.
struct StrVisitor;

impl<'de> Visitor<'de> for StrVisitor {
    type Value = Cow<'de, str>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("string")
    }

    fn visit_borrowed_str<E: serde::de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(v))
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(v.to_owned()))
    }

    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(Cow::Owned(v))
    }
}

/// Deserialize an optional string field, borrowing from the input when possible.
///
/// Serde's `#[serde(borrow)]` does not reach inside `Option`, so optional `Cow` fields use this
/// helper instead.
pub fn borrowed_option_str<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Cow<'de, str>>, D::Error> {
    struct OptionVisitor;

    impl<'de> Visitor<'de> for OptionVisitor {
        type Value = Option<Cow<'de, str>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("optional string")
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_str(StrVisitor).map(Some)
        }
    }

    deserializer.deserialize_option(OptionVisitor)
}

/// Deserialize a sequence of strings, borrowing from the input when possible.
///
/// Serde's `#[serde(borrow)]` does not reach inside `Vec`, so string-array fields use this helper
/// instead.
pub fn borrowed_str_seq<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Cow<'de, str>>, D::Error> {
    /// A newtype giving `Cow<str>` a borrowing `Deserialize` impl for use as a sequence element.
    struct BorrowedStr<'a>(Cow<'a, str>);

    impl<'de> serde::de::Deserialize<'de> for BorrowedStr<'de> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_str(StrVisitor).map(BorrowedStr)
        }
    }

    struct SeqVisitor;

    impl<'de> Visitor<'de> for SeqVisitor {
        type Value = Vec<Cow<'de, str>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("sequence of strings")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(BorrowedStr(value)) = seq.next_element()? {
                values.push(value);
            }

            Ok(values)
        }
    }

    deserializer.deserialize_seq(SeqVisitor)
}

/// Deserialize an optional unsigned integer that may be encoded as either a JSON number or a
/// decimal string (the encoding written by pywb-family CDXJ indexers).
pub fn optional_integer<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64> + FromStr,
{
    struct IntegerVisitor<T>(PhantomData<T>);

    impl<'de, T: TryFrom<u64> + FromStr> Visitor<'de> for IntegerVisitor<T> {
        type Value = Option<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("unsigned integer or unsigned integer string")
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            T::try_from(v).map(Some).map_err(|_| {
                serde::de::Error::invalid_value(
                    Unexpected::Unsigned(v),
                    &"unsigned integer in range",
                )
            })
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            v.parse().map(Some).map_err(|_| {
                serde::de::Error::invalid_value(Unexpected::Str(v), &"unsigned integer string")
            })
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_any(Self(PhantomData))
        }
    }

    deserializer.deserialize_option(IntegerVisitor(PhantomData))
}

/// Serialize an optional integer as a decimal string, following the pywb CDXJ convention.
// `serialize_with` functions receive a reference to the field, so the `&Option<T>` signature is
// required by serde.
#[allow(clippy::ref_option)]
pub fn optional_integer_str<S: Serializer, T: Display>(
    value: &Option<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => serializer.collect_str(v),
        None => serializer.serialize_none(),
    }
}

/// Serialize a required integer as the decimal string convention used by CDXJ.
pub fn integer_str<S: Serializer, T: Display>(value: &T, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}
