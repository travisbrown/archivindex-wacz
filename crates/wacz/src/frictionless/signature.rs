//! The `signedData` signature envelope of the `datapackage-digest.json` file.

use std::borrow::Cow;

use bounded_static::ToStatic;
use chrono::{DateTime, Utc};

use crate::digest::Sha256Digest;

/// The `signedData` signature envelope of a digest file, as defined by the [WACZ signing
/// specification](https://specs.webrecorder.net/wacz-auth/latest/).
///
/// The specification defines two formats, distinguished by their fields: an anonymous signature
/// carrying only a public key, and a domain-ownership identity signature carrying a certificate
/// chain and a signed timestamp.
///
/// The specification requires that the envelope contain no properties beyond those of its format;
/// parsing is correspondingly strict, and an envelope with unlisted properties (or with fields from
/// both formats) is rejected.
/// Parsing does not verify cryptographic signatures or certificate chains.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum SignatureData<'a> {
    /// A signature backed by a TLS certificate for a domain, countersigned by an RFC 3161
    /// timestamp server.
    DomainIdentity(#[serde(borrow)] DomainIdentitySignature<'a>),
    /// A signature with a bare public key, whose ownership is established out-of-band.
    Anonymous(#[serde(borrow)] AnonymousSignature<'a>),
}

impl SignatureData<'_> {
    /// The SHA-256 digest of `datapackage.json` that was signed.
    #[must_use]
    pub const fn hash(&self) -> &Sha256Digest {
        match self {
            Self::DomainIdentity(signature) => &signature.hash,
            Self::Anonymous(signature) => &signature.hash,
        }
    }

    /// When the signature was created.
    #[must_use]
    pub const fn created(&self) -> &DateTime<Utc> {
        match self {
            Self::DomainIdentity(signature) => &signature.created,
            Self::Anonymous(signature) => &signature.created,
        }
    }

    /// The software that created the signature.
    #[must_use]
    pub fn software(&self) -> &str {
        match self {
            Self::DomainIdentity(signature) => &signature.software,
            Self::Anonymous(signature) => &signature.software,
        }
    }

    /// The version of the software that created the signature.
    #[must_use]
    pub fn version(&self) -> &str {
        match self {
            Self::DomainIdentity(signature) => &signature.version,
            Self::Anonymous(signature) => &signature.version,
        }
    }

    /// The base64-encoded signature of [`hash`](SignatureData::hash).
    #[must_use]
    pub fn signature(&self) -> &str {
        match self {
            Self::DomainIdentity(signature) => &signature.signature,
            Self::Anonymous(signature) => &signature.signature,
        }
    }
}

/// An anonymous signature with a public key for external verification.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnonymousSignature<'a> {
    /// The SHA-256 digest of `datapackage.json` that was signed.
    pub hash: Sha256Digest,
    /// When the signature was created.
    pub created: DateTime<Utc>,
    /// The software that created the signature.
    #[serde(borrow)]
    pub software: Cow<'a, str>,
    /// The version of the software that created the signature.
    #[serde(borrow)]
    pub version: Cow<'a, str>,
    /// The base64-encoded signature of [`hash`](Self::hash).
    #[serde(borrow)]
    pub signature: Cow<'a, str>,
    /// The base64-encoded ECDSA public key validating [`signature`](Self::signature).
    #[serde(borrow)]
    pub public_key: Cow<'a, str>,
}

/// A domain-ownership identity signature.
///
/// The envelope includes a TLS certificate for [`domain`](Self::domain) and an RFC 3161
/// countersignature for the creation time. Parsing does not verify either signature.
#[derive(Clone, Debug, Eq, PartialEq, ToStatic, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DomainIdentitySignature<'a> {
    /// The SHA-256 digest of `datapackage.json` that was signed.
    pub hash: Sha256Digest,
    /// When the signature was created.
    pub created: DateTime<Utc>,
    /// The software that created the signature.
    #[serde(borrow)]
    pub software: Cow<'a, str>,
    /// The version of the software that created the signature.
    #[serde(borrow)]
    pub version: Cow<'a, str>,
    /// The base64-encoded signature of [`hash`](Self::hash) by the key of
    /// [`domain_cert`](Self::domain_cert).
    #[serde(borrow)]
    pub signature: Cow<'a, str>,
    /// The hostname whose certificate created the signature.
    #[serde(borrow)]
    pub domain: Cow<'a, str>,
    /// The PEM certificate chain validating [`signature`](Self::signature).
    #[serde(borrow)]
    pub domain_cert: Cow<'a, str>,
    /// The base64-encoded RFC 3161 timestamp server signature of [`signature`](Self::signature).
    #[serde(borrow)]
    pub time_signature: Cow<'a, str>,
    /// The PEM certificate chain validating [`time_signature`](Self::time_signature).
    #[serde(borrow)]
    pub timestamp_cert: Cow<'a, str>,
    /// An optional cross-signed PEM certificate chain providing an alternative trust path for
    /// [`signature`](Self::signature), should the domain certificate be compromised.
    #[serde(
        default,
        with = "archivindex_serde::borrowable_str::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub cross_signed_cert: Option<Cow<'a, str>>,
}

#[cfg(test)]
mod tests {
    use bounded_static::IntoBoundedStatic;

    use super::*;
    use crate::frictionless::DataPackageDigest;

    /// A digest file with an anonymous signature envelope, shaped as in the signing specification.
    const ANONYMOUS_DIGEST: &str = r#"{
        "path": "datapackage.json",
        "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885",
        "signedData": {
            "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885",
            "created": "2020-10-07T21:22:36Z",
            "software": "example-signer",
            "version": "0.1.0",
            "signature": "base64-encoded-signature",
            "publicKey": "base64-encoded-public-key"
        }
    }"#;

    /// A digest file with a domain-identity signature envelope, shaped as in the signing
    /// specification.
    const DOMAIN_DIGEST: &str = r#"{
        "path": "datapackage.json",
        "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885",
        "signedData": {
            "hash": "sha256:ec1f44ab13e2c94b0ddf66e9673d585ba4a77e6f8c9cc30d8665da434557e885",
            "created": "2020-10-07T21:22:36Z",
            "software": "example-signer",
            "version": "0.1.0",
            "signature": "base64-encoded-signature",
            "domain": "signing.example.com",
            "domainCert": "pem-certificate-chain",
            "timeSignature": "base64-encoded-timestamp-signature",
            "timestampCert": "pem-timestamp-certificate-chain"
        }
    }"#;

    #[test]
    fn deserialize_anonymous_signature() -> Result<(), Box<dyn std::error::Error>> {
        let digest = serde_json::from_str::<DataPackageDigest<'_>>(ANONYMOUS_DIGEST)?;
        let signed_data = digest.signed_data.expect("signature should be present");

        assert_eq!(signed_data.hash(), &digest.hash);
        assert_eq!(signed_data.software(), "example-signer");
        assert_eq!(signed_data.version(), "0.1.0");
        assert_eq!(signed_data.signature(), "base64-encoded-signature");

        match signed_data {
            SignatureData::Anonymous(signature) => {
                assert_eq!(signature.public_key, "base64-encoded-public-key");
            }
            SignatureData::DomainIdentity(_) => panic!("expected an anonymous signature"),
        }

        Ok(())
    }

    #[test]
    fn deserialize_domain_identity_signature() -> Result<(), Box<dyn std::error::Error>> {
        let digest = serde_json::from_str::<DataPackageDigest<'_>>(DOMAIN_DIGEST)?;
        let signed_data = digest.signed_data.expect("signature should be present");

        assert_eq!(signed_data.hash(), &digest.hash);

        match signed_data {
            SignatureData::DomainIdentity(signature) => {
                assert_eq!(signature.domain, "signing.example.com");
                assert_eq!(signature.domain_cert, "pem-certificate-chain");
                assert_eq!(
                    signature.time_signature,
                    "base64-encoded-timestamp-signature"
                );
                assert_eq!(signature.timestamp_cert, "pem-timestamp-certificate-chain");
                assert_eq!(signature.cross_signed_cert, None);
            }
            SignatureData::Anonymous(_) => panic!("expected a domain-identity signature"),
        }

        Ok(())
    }

    #[test]
    fn signature_round_trips() -> Result<(), Box<dyn std::error::Error>> {
        // A domain-identity envelope with the optional cross-signed certificate present.
        let mut cross_signed = serde_json::from_str::<serde_json::Value>(DOMAIN_DIGEST)?;
        cross_signed["signedData"]["crossSignedCert"] =
            serde_json::Value::String("pem-cross-signed-chain".to_owned());

        for original in [
            ANONYMOUS_DIGEST.to_owned(),
            DOMAIN_DIGEST.to_owned(),
            serde_json::to_string(&cross_signed)?,
        ] {
            let digest = serde_json::from_str::<DataPackageDigest<'_>>(&original)?.into_static();
            let encoded = serde_json::to_string(&digest)?;

            assert_eq!(
                serde_json::from_str::<DataPackageDigest<'_>>(&encoded)?,
                digest
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&encoded)?,
                serde_json::from_str::<serde_json::Value>(&original)?
            );
        }

        Ok(())
    }

    /// The specification forbids properties beyond those of each signature format.
    #[test]
    fn signature_rejects_unlisted_properties() -> Result<(), Box<dyn std::error::Error>> {
        for original in [ANONYMOUS_DIGEST, DOMAIN_DIGEST] {
            let mut value = serde_json::from_str::<serde_json::Value>(original)?;
            value["signedData"]["custom"] = serde_json::Value::Bool(true);
            let augmented = serde_json::to_string(&value)?;

            assert!(serde_json::from_str::<DataPackageDigest<'_>>(&augmented).is_err());
        }

        Ok(())
    }

    /// An envelope mixing the fields of both signature formats conforms to neither.
    #[test]
    fn signature_rejects_mixed_formats() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = serde_json::from_str::<serde_json::Value>(DOMAIN_DIGEST)?;
        value["signedData"]["publicKey"] =
            serde_json::Value::String("base64-encoded-public-key".to_owned());
        let augmented = serde_json::to_string(&value)?;

        assert!(serde_json::from_str::<DataPackageDigest<'_>>(&augmented).is_err());

        Ok(())
    }
}
