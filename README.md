# archivindex

![GitHub last commit](https://img.shields.io/github/last-commit/travisbrown/archivindex)
[![build](https://github.com/travisbrown/archivindex/actions/workflows/ci.yml/badge.svg)](https://github.com/travisbrown/archivindex/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/travisbrown/archivindex/branch/main/graph/badge.svg)](https://codecov.io/gh/travisbrown/archivindex)
[![license](https://img.shields.io/github/license/travisbrown/archivindex)](https://github.com/travisbrown/archivindex/blob/main/LICENSE)

Tools for capturing web pages and packaging them as web archive collections.

## Crates

| Crate | Description |
| --- | --- |
| [`archivindex-surt`](surt/) | SURT keys and URL canonicalization for web archive indexes |
| [`archivindex-cdx`](cdx/) | Data models for classic CDX, CDXJ, and CDX Server JSON |
| [`archivindex-wacz`](wacz/) | Reading and writing web archive collections in the [WACZ][wacz-spec] format |
| [`archivindex-packager`](packager/) | Packaging WARC captures as indexed WACZ distributions |
| [`archivindex-wordpress`](wordpress/) | Capturing and reading WordPress REST API resources |
| [`archivindex-packager-cli`](packager-cli/) | An `archivindex-packager` command-line front end |
| [`archivindex-wordpress-cli`](wordpress-cli/) | An `archivindex-wordpress` command-line front end |

The WARC reading and writing core these crates are built on lives in a separate repository,
[`archivindex-warc`][archivindex-warc], and is used here as a source dependency. The archiver and
its revisit index live there too.

## Usage

Each library with a command-line front end has its own binary, so a command below is invoked
through the binary that owns it.

Capture output is uncompressed by default. Pass `--gzip` and use a `.warc.gz` output name to
compress each WARC record as an independent gzip member.

Commands exit with status 0 when they complete without reportable problems. Status 1 means the
command completed with problems to report about its input, including when a usable but partial
archive was published. Operational failures use status 2.

### WordPress comments

The `archive-comments` command captures comment batches from a WordPress REST API into a crawl
session. It fixes a creation-time cutoff and pages by comment ID. One sweep is sufficient when the
reported total is stable and matches the distinct IDs captured; otherwise a second consistency
sweep runs automatically. Pass `--second-sweep` to request that validation sweep unconditionally.
`--limit` can stop after a fixed number of successful batches (including validation recaptures).
Use `--request-delay` to wait a specified number of seconds between batch requests:

```bash
cargo run --bin archivindex-wordpress -- archive-comments \
  --base-url https://example.com/ \
  --output comments.warc \
  --session-name comments-2026 \
  --operator "A. Archivist" \
  --operator-email archivist@example.com \
  --revisit-index comments-state.sqlite3 \
  --request-delay 1 \
  --limit 10
```

The `read-comments` command writes the archived comments as JSON Lines in ascending comment ID
order. Conflicting captures of the same comment are reported through the warning log:

```bash
cargo run --bin archivindex-wordpress -- read-comments comments.warc > comments.jsonl
```

### WACZ packaging

The `warc-to-wacz` command converts a plain or gzip-compressed WARC file into an indexed WACZ. A
metadata record's `title` field supplies the linked page title. Captures whose metadata contains a
`via` field are written to `extraPages.jsonl`; all others are written to `pages.jsonl`. The first
`warcinfo` record's title and description become package metadata, and additional warcinfo record
IDs are reported as conversion warnings:

```bash
cargo run --bin archivindex-packager -- warc-to-wacz capture.warc.gz \
  --output capture.wacz
```

For a plain input WARC, pass `--gzip-warc` to store it as `archive/data.warc.gz`. Each record is
written as an independent gzip member so indexed captures remain directly addressable:

```bash
cargo run --bin archivindex-packager -- warc-to-wacz capture.warc \
  --output capture.wacz \
  --gzip-warc \
  --gzip-compression-level 9
```

The gzip compression level ranges from 0 through 9 and defaults to 6.
Use `--zip-compression-level` to set the ZIP DEFLATE level for WACZ metadata and indexes; it ranges
from 1 through 264 and defaults to 6. Levels above 9 use Zopfli and may be substantially slower.

## License

This project is licensed under the [MIT License](https://opensource.org/license/mit). See
[LICENSE](LICENSE) for the full text.

[archivindex-warc]: https://github.com/travisbrown/archivindex-warc
[wacz-spec]: https://specs.webrecorder.net/wacz/1.1.1/
