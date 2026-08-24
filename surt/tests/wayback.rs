//! Conformance with `urlkey` values served by the Wayback Machine's CDX API.

use archivindex_surt::url::Canonicalizer;

#[test]
fn matches_wayback_machine_urlkeys() {
    let mut total = 0;
    let mut failures = Vec::new();

    for line in include_str!("data/wayback-urlkeys.tsv")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (original, expected) = line.split_once('\t').expect("tab-separated line");
        total += 1;

        match Canonicalizer::WAYBACK.surt(original) {
            Ok(key) if key.as_str() == expected => {}
            other => failures.push(format!(
                "{original}\n  expected: {expected}\n  got:      {other:?}"
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {total} keys differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
