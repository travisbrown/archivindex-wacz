//! Canonical field names shared by CDX representations.

use std::borrow::Cow;
use std::fmt;

/// A semantic CDX field name.
///
/// Known aliases from CDXJ, the CDX Server API, and classic CDX legends map to dedicated
/// variants. Unknown names are retained verbatim.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Field<'a> {
    /// Searchable URL key (`urlkey`, classic `N` or `A`).
    UrlKey,
    /// Capture timestamp (`timestamp`, classic `b`).
    Timestamp,
    /// Original captured URL (`url` or `original`, classic `a`).
    Url,
    /// Response media type (`mime` or `mimetype`, classic `m`).
    Mime,
    /// HTTP response status (`status` or `statuscode`, classic `s`).
    Status,
    /// Payload digest (`digest`, classic `k`; old checksum `c` is kept distinct).
    Digest,
    /// Old-style checksum (classic `c`).
    OldDigest,
    /// Redirect target (`redirect`, classic `r`).
    Redirect,
    /// Robots or AIF meta flags (`robotflags`, classic `M`).
    RobotFlags,
    /// Stored record length (`length`, classic `S`).
    Length,
    /// Stored record offset (`offset`, classic `V`).
    Offset,
    /// Archive filename (`filename`, classic `g`).
    Filename,
    /// Digest of the complete stored record (`recordDigest`).
    RecordDigest,
    /// Resolved revisit record length (`orig.length`).
    OriginalLength,
    /// Resolved revisit record offset (`orig.offset`).
    OriginalOffset,
    /// Resolved revisit archive filename (`orig.filename`).
    OriginalFilename,
    /// Source IP address (classic `e`).
    IpAddress,
    /// ARC document length (classic `n`).
    ArcDocumentLength,
    /// A field from the original extended CDX legend.
    Legacy(LegacyField),
    /// A representation-specific or application-defined field.
    Other(Cow<'a, str>),
}

impl<'a> Field<'a> {
    /// Interpret a named CDXJ or CDX Server field.
    #[must_use]
    pub fn named(name: &'a str) -> Self {
        match name {
            "urlkey" => Self::UrlKey,
            "timestamp" => Self::Timestamp,
            "url" | "original" => Self::Url,
            "mime" | "mimetype" => Self::Mime,
            "status" | "statuscode" => Self::Status,
            "digest" => Self::Digest,
            "redirect" => Self::Redirect,
            "robotflags" | "meta" => Self::RobotFlags,
            "length" => Self::Length,
            "offset" => Self::Offset,
            "filename" => Self::Filename,
            "recordDigest" => Self::RecordDigest,
            "orig.length" => Self::OriginalLength,
            "orig.offset" => Self::OriginalOffset,
            "orig.filename" => Self::OriginalFilename,
            other => Self::Other(Cow::Borrowed(other)),
        }
    }

    /// Interpret a marker from a classic CDX legend.
    #[must_use]
    pub fn classic(marker: &'a str) -> Self {
        match marker {
            "N" | "A" => Self::UrlKey,
            "b" => Self::Timestamp,
            "a" => Self::Url,
            "m" => Self::Mime,
            "s" => Self::Status,
            "k" => Self::Digest,
            "c" => Self::OldDigest,
            "R" | "r" => Self::Redirect,
            "M" => Self::RobotFlags,
            "S" => Self::Length,
            "V" => Self::Offset,
            "g" => Self::Filename,
            "e" => Self::IpAddress,
            "n" => Self::ArcDocumentLength,
            "B" => Self::Legacy(LegacyField::NewsGroup),
            "C" => Self::Legacy(LegacyField::Category),
            "D" => Self::Legacy(LegacyField::CompressedDatOffset),
            "F" | "f" => Self::Legacy(LegacyField::Frame),
            "G" | "Q" => Self::Legacy(LegacyField::Language),
            "H" | "h" => Self::Legacy(LegacyField::Host),
            "I" | "i" => Self::Legacy(LegacyField::Image),
            "J" | "j" => Self::Legacy(LegacyField::Jump),
            "K" => Self::Legacy(LegacyField::Weird),
            "L" | "l" => Self::Legacy(LegacyField::Link),
            "P" | "p" => Self::Legacy(LegacyField::Path),
            "U" => Self::Legacy(LegacyField::Uniqueness),
            "X" | "x" => Self::Legacy(LegacyField::Href),
            "Y" | "y" => Self::Legacy(LegacyField::Source),
            "Z" | "z" => Self::Legacy(LegacyField::Script),
            "d" => Self::Legacy(LegacyField::UncompressedDatOffset),
            "o" => Self::Legacy(LegacyField::Port),
            "t" => Self::Legacy(LegacyField::Title),
            "v" => Self::Legacy(LegacyField::UncompressedArcOffset),
            "#" => Self::Legacy(LegacyField::Comment),
            other => Self::Other(Cow::Borrowed(other)),
        }
    }

    /// The canonical field name used by CDXJ and CDX Server JSON.
    #[must_use]
    pub fn as_name(&self) -> &str {
        match self {
            Self::UrlKey => "urlkey",
            Self::Timestamp => "timestamp",
            Self::Url => "url",
            Self::Mime => "mime",
            Self::Status => "status",
            Self::Digest => "digest",
            Self::OldDigest => "oldDigest",
            Self::Redirect => "redirect",
            Self::RobotFlags => "robotflags",
            Self::Length => "length",
            Self::Offset => "offset",
            Self::Filename => "filename",
            Self::RecordDigest => "recordDigest",
            Self::OriginalLength => "orig.length",
            Self::OriginalOffset => "orig.offset",
            Self::OriginalFilename => "orig.filename",
            Self::IpAddress => "ip",
            Self::ArcDocumentLength => "arc.length",
            Self::Legacy(field) => field.as_name(),
            Self::Other(name) => name,
        }
    }

    /// Detach this field name from borrowed input.
    #[must_use]
    pub fn into_owned(self) -> Field<'static> {
        match self {
            Self::Other(value) => Field::Other(Cow::Owned(value.into_owned())),
            Self::UrlKey => Field::UrlKey,
            Self::Timestamp => Field::Timestamp,
            Self::Url => Field::Url,
            Self::Mime => Field::Mime,
            Self::Status => Field::Status,
            Self::Digest => Field::Digest,
            Self::OldDigest => Field::OldDigest,
            Self::Redirect => Field::Redirect,
            Self::RobotFlags => Field::RobotFlags,
            Self::Length => Field::Length,
            Self::Offset => Field::Offset,
            Self::Filename => Field::Filename,
            Self::RecordDigest => Field::RecordDigest,
            Self::OriginalLength => Field::OriginalLength,
            Self::OriginalOffset => Field::OriginalOffset,
            Self::OriginalFilename => Field::OriginalFilename,
            Self::IpAddress => Field::IpAddress,
            Self::ArcDocumentLength => Field::ArcDocumentLength,
            Self::Legacy(field) => Field::Legacy(field),
        }
    }
}

/// A semantic field from the original extended CDX marker vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LegacyField {
    /// Usenet newsgroup.
    NewsGroup,
    /// Category.
    Category,
    /// Offset in a compressed DAT file.
    CompressedDatOffset,
    /// Frame metadata.
    Frame,
    /// Language metadata.
    Language,
    /// Host metadata.
    Host,
    /// Image metadata.
    Image,
    /// Jump metadata.
    Jump,
    /// The legacy `K` field, documented as “weird”.
    Weird,
    /// Link metadata.
    Link,
    /// URL path.
    Path,
    /// Uniqueness information.
    Uniqueness,
    /// Extracted `href` values.
    Href,
    /// Extracted `src` values.
    Source,
    /// Extracted script references.
    Script,
    /// Offset in an uncompressed DAT file.
    UncompressedDatOffset,
    /// URL port.
    Port,
    /// Document title.
    Title,
    /// Offset in an uncompressed ARC file.
    UncompressedArcOffset,
    /// Comment text.
    Comment,
}

impl LegacyField {
    /// A descriptive name suitable for a representation-neutral extension map.
    #[must_use]
    pub const fn as_name(self) -> &'static str {
        match self {
            Self::NewsGroup => "newsgroup",
            Self::Category => "category",
            Self::CompressedDatOffset => "dat.compressedOffset",
            Self::Frame => "frame",
            Self::Language => "language",
            Self::Host => "host",
            Self::Image => "image",
            Self::Jump => "jump",
            Self::Weird => "weird",
            Self::Link => "link",
            Self::Path => "path",
            Self::Uniqueness => "uniqueness",
            Self::Href => "href",
            Self::Source => "src",
            Self::Script => "script",
            Self::UncompressedDatOffset => "dat.offset",
            Self::Port => "port",
            Self::Title => "title",
            Self::UncompressedArcOffset => "arc.offset",
            Self::Comment => "comment",
        }
    }
}

impl fmt::Display for Field<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_name())
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::ToBoundedStatic for Field<'_> {
    type Static = Field<'static>;

    fn to_static(&self) -> Self::Static {
        self.clone().into_owned()
    }
}

#[cfg(feature = "bounded-static")]
impl bounded_static::IntoBoundedStatic for Field<'_> {
    type Static = Field<'static>;

    fn into_static(self) -> Self::Static {
        self.into_owned()
    }
}
