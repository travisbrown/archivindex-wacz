//! SHA-256 digests in the `sha256:<hex>` encoding used by WACZ manifests.

use std::fmt;
use std::io::Read;
use std::str::FromStr;

use bounded_static::{IntoBoundedStatic, ToBoundedStatic};
use serde::de::{Deserializer, Unexpected, Visitor};
use serde::ser::Serializer;
use sha2::Digest as _;

/// The prefix that identifies the digest algorithm in the string encoding.
const PREFIX: &str = "sha256:";

/// The length of the hexadecimal digest representation without its prefix.
const HEX_LENGTH: usize = 64;

/// An error type for digest string parsing.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The string does not begin with the `sha256:` prefix. WACZ manifests always identify the
    /// digest algorithm explicitly.
    #[error("missing sha256 digest prefix: {0}")]
    MissingPrefix(String),
    /// The hexadecimal representation after the prefix has the wrong length.
    #[error("invalid SHA-256 digest string length: {0}")]
    InvalidLength(usize),
    /// The value after the prefix is not valid hexadecimal.
    #[error("invalid hexadecimal encoding: {0}")]
    InvalidEncoding(String),
}

/// A SHA-256 digest, displayed and parsed in the `sha256:<hex>` encoding used by WACZ resource
/// manifests and digest files.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest(
    /// The digest bytes.
    pub [u8; 32],
);

impl Sha256Digest {
    /// Compute the digest of a byte buffer.
    #[must_use]
    pub fn compute<B: AsRef<[u8]>>(bytes: B) -> Self {
        Self(sha2::Sha256::digest(bytes.as_ref()).into())
    }

    /// Compute the digest of a stream, returning the digest and the number of bytes read.
    pub fn from_reader<R: Read>(mut reader: R) -> Result<(Self, u64), std::io::Error> {
        let mut hasher = sha2::Sha256::new();
        let mut buffer = [0; 8192];
        let mut length = 0;

        loop {
            let read = reader.read(&mut buffer)?;

            if read == 0 {
                break;
            }

            hasher.update(&buffer[..read]);
            length += read as u64;
        }

        Ok((Self(hasher.finalize().into()), length))
    }
}

impl ToBoundedStatic for Sha256Digest {
    type Static = Self;

    fn to_static(&self) -> Self::Static {
        *self
    }
}

impl IntoBoundedStatic for Sha256Digest {
    type Static = Self;

    fn into_static(self) -> Self::Static {
        self
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        data_encoding::HEXLOWER.encode_write(&self.0, f)
    }
}

impl FromStr for Sha256Digest {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s
            .strip_prefix(PREFIX)
            .ok_or_else(|| Error::MissingPrefix(s.to_owned()))?;

        if hex.len() != HEX_LENGTH {
            return Err(Error::InvalidLength(hex.len()));
        }

        let mut bytes = [0; 32];

        // Uppercase hexadecimal is accepted on input but always written as lowercase.
        data_encoding::HEXLOWER_PERMISSIVE
            .decode_mut(hex.as_bytes(), &mut bytes)
            .map_err(|_| Error::InvalidEncoding(hex.to_owned()))?;

        Ok(Self(bytes))
    }
}

impl serde::ser::Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::de::Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DigestVisitor;

        impl Visitor<'_> for DigestVisitor {
            type Value = Sha256Digest;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("struct Sha256Digest")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse().map_err(|_| {
                    serde::de::Error::invalid_value(Unexpected::Str(v), &"sha256 digest string")
                })
            }
        }

        deserializer.deserialize_str(DigestVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The well-known SHA-256 digest of the empty input.
    const EMPTY: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn compute_matches_known_value() {
        assert_eq!(Sha256Digest::compute([]).to_string(), EMPTY);
    }

    #[test]
    fn from_reader_returns_digest_and_length() -> Result<(), Box<dyn std::error::Error>> {
        let (digest, length) = Sha256Digest::from_reader(&b"abc"[..])?;

        assert_eq!(digest, Sha256Digest::compute(b"abc"));
        assert_eq!(length, 3);

        Ok(())
    }

    #[test]
    fn display_parse_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let digest = Sha256Digest::compute(b"abc");
        let parsed = digest.to_string().parse::<Sha256Digest>()?;

        assert_eq!(parsed, digest);

        Ok(())
    }

    #[test]
    fn parse_accepts_uppercase_hexadecimal() -> Result<(), Box<dyn std::error::Error>> {
        let parsed = EMPTY
            .to_uppercase()
            .replace("SHA256", "sha256")
            .parse::<Sha256Digest>()?;

        assert_eq!(parsed, Sha256Digest::compute([]));

        Ok(())
    }

    #[test]
    fn parse_rejects_missing_prefix() {
        assert!(matches!(
            EMPTY.trim_start_matches("sha256:").parse::<Sha256Digest>(),
            Err(Error::MissingPrefix(_))
        ));
    }

    #[test]
    fn parse_rejects_invalid_length() {
        assert!(matches!(
            "sha256:abcd".parse::<Sha256Digest>(),
            Err(Error::InvalidLength(4))
        ));
    }

    #[test]
    fn parse_rejects_invalid_hexadecimal() {
        let value = format!("sha256:{}", "z".repeat(64));

        assert!(matches!(
            value.parse::<Sha256Digest>(),
            Err(Error::InvalidEncoding(_))
        ));
    }

    #[test]
    fn serde_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let digest = Sha256Digest::compute(b"abc");
        let encoded = serde_json::to_string(&digest)?;

        assert_eq!(serde_json::from_str::<Sha256Digest>(&encoded)?, digest);

        Ok(())
    }
}
