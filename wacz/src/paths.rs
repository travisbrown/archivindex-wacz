//! Typed WACZ member-path classification shared by reading and writing.

use crate::{ARCHIVE_PREFIX, INDEXES_PREFIX};

pub fn is_safe(path: &str) -> bool {
    !path.contains('\\')
        && !path.starts_with('/')
        && !path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
}

pub fn is_warc(path: &str) -> bool {
    direct_name(path, ARCHIVE_PREFIX)
        .and_then(warc_gzip)
        .is_some()
}

pub fn warc_gzip(name: &str) -> Option<bool> {
    if name.strip_suffix(".warc.gz").is_some() {
        Some(true)
    } else if name.strip_suffix(".warc").is_some() {
        Some(false)
    } else {
        None
    }
}

pub fn is_plain_index(path: &str) -> bool {
    direct_name(path, INDEXES_PREFIX).is_some_and(|name| name.strip_suffix(".cdx").is_some())
}

pub fn is_zipnum_data(path: &str) -> bool {
    direct_name(path, INDEXES_PREFIX).is_some_and(|name| name.ends_with(".cdx.gz"))
}

pub fn is_zipnum_summary(path: &str) -> bool {
    direct_name(path, INDEXES_PREFIX).is_some_and(|name| name.strip_suffix(".idx").is_some())
}

pub fn valid_index_name(name: &str) -> bool {
    !name.contains('/') && is_safe(name) && name.strip_suffix(".cdx").is_some()
}

pub fn valid_resource_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

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
        assert!(valid_resource_name("pages.jsonl"));
        assert!(valid_resource_name("data-1_2.warc.gz"));
        assert!(!valid_resource_name(""));
        assert!(!valid_resource_name("Pages.JSONL"));
        assert!(!valid_resource_name("archive/data.warc"));
        assert!(!valid_resource_name("caf\u{e9}.warc"));
    }
}
