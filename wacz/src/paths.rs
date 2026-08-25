//! Typed WACZ member-path classification shared by reading and writing.

use crate::{ARCHIVE_PREFIX, INDEXES_PREFIX};

/// Whether `path` is relative, uses `/` separators, and has no empty, `.`, or `..` segments.
pub fn is_safe(path: &str) -> bool {
    !path.contains('\\')
        && !path.starts_with('/')
        && !path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

/// Whether `path` is a WARC member directly under the archive directory.
pub fn is_warc(path: &str) -> bool {
    direct_name(path, ARCHIVE_PREFIX)
        .and_then(warc_gzip)
        .is_some()
}

/// Whether a WARC name denotes gzip content: `Some(true)` for `.warc.gz`, `Some(false)` for
/// `.warc`, and `None` for anything else.
pub fn warc_gzip(name: &str) -> Option<bool> {
    if name.strip_suffix(".warc.gz").is_some() {
        Some(true)
    } else if name.strip_suffix(".warc").is_some() {
        Some(false)
    } else {
        None
    }
}

/// Whether `path` is a plain `.cdx` index directly under the indexes directory.
pub fn is_plain_index(path: &str) -> bool {
    direct_name(path, INDEXES_PREFIX).is_some_and(|name| name.strip_suffix(".cdx").is_some())
}

/// Whether `path` is a `ZipNum` `.cdx.gz` block file directly under the indexes directory.
pub fn is_zipnum_data(path: &str) -> bool {
    direct_name(path, INDEXES_PREFIX).is_some_and(|name| name.ends_with(".cdx.gz"))
}

/// Whether `path` is a `ZipNum` `.idx` summary directly under the indexes directory.
pub fn is_zipnum_summary(path: &str) -> bool {
    direct_name(path, INDEXES_PREFIX).is_some_and(|name| name.strip_suffix(".idx").is_some())
}

/// Whether `name` is a safe, directory-free `.cdx` index file name.
pub fn valid_index_name(name: &str) -> bool {
    !name.contains('/') && is_safe(name) && name.strip_suffix(".cdx").is_some()
}

/// Whether `name` uses the data package name alphabet (`a-z0-9._-`), which the Data Package
/// specification requires of both package and resource names.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

/// The path of a `ZipNum` member's counterpart: the `.idx` of a `.cdx.gz` block file and vice
/// versa.
pub fn zipnum_partner(path: &str) -> Option<String> {
    if is_zipnum_data(path) {
        Some(format!("{}idx", path.strip_suffix("cdx.gz")?))
    } else if is_zipnum_summary(path) {
        Some(format!("{}cdx.gz", path.strip_suffix("idx")?))
    } else {
        None
    }
}

fn direct_name<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let name = path.strip_prefix(prefix)?;
    (!name.is_empty() && !name.contains('/') && is_safe(path)).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies;
    use proptest::prelude::*;

    #[test]
    fn warc_names_report_their_content_compression() {
        assert_eq!(warc_gzip("data.warc"), Some(false));
        assert_eq!(warc_gzip("data.warc.gz"), Some(true));
        assert_eq!(warc_gzip("archive/data.warc"), Some(false));
        assert_eq!(warc_gzip("data.WARC"), None);
        assert_eq!(warc_gzip("data.gz"), None);
    }

    #[test]
    fn resource_names_use_the_data_package_alphabet() {
        assert!(valid_name("pages.jsonl"));
        assert!(valid_name("data-1_2.warc.gz"));
        assert!(!valid_name(""));
        assert!(!valid_name("Pages.JSONL"));
        assert!(!valid_name("archive/data.warc"));
        assert!(!valid_name("caf\u{e9}.warc"));
    }

    /// A `ZipNum` member and its counterpart name each other.
    #[test_strategy::proptest]
    fn zipnum_partners_are_mutual(#[strategy(strategies::zipnum_path())] path: String) {
        let partner = zipnum_partner(&path).expect("a ZipNum member has a counterpart");

        prop_assert_ne!(&partner, &path);
        prop_assert_eq!(zipnum_partner(&partner), Some(path.clone()));
        prop_assert_eq!(is_zipnum_data(&path), is_zipnum_summary(&partner));
        prop_assert_eq!(is_zipnum_summary(&path), is_zipnum_data(&partner));
    }

    /// Only `ZipNum` members have counterparts.
    #[test_strategy::proptest]
    fn only_zipnum_members_have_partners(#[strategy(strategies::member_path())] path: String) {
        prop_assert_eq!(
            zipnum_partner(&path).is_some(),
            is_zipnum_data(&path) || is_zipnum_summary(&path)
        );
    }

    /// A path is a member of at most one kind, and every recognized kind is a safe path.
    #[test_strategy::proptest]
    fn member_kinds_are_exclusive_and_safe(#[strategy(strategies::member_path())] path: String) {
        let kinds = [
            is_warc(&path),
            is_plain_index(&path),
            is_zipnum_data(&path),
            is_zipnum_summary(&path),
        ];
        let recognized = kinds.iter().filter(|kind| **kind).count();

        prop_assert!(recognized <= 1);
        prop_assert!(recognized == 0 || is_safe(&path));
    }

    /// The names the index writer accepts are exactly the plain indexes the reader recognizes.
    #[test_strategy::proptest]
    fn index_names_agree_with_index_paths(#[strategy(strategies::member_path())] name: String) {
        prop_assert_eq!(
            valid_index_name(&name),
            is_plain_index(&format!("{INDEXES_PREFIX}{name}"))
        );
    }
}
