# archivindex

![GitHub last commit](https://img.shields.io/github/last-commit/travisbrown/archivindex)
[![build](https://github.com/travisbrown/archivindex/actions/workflows/ci.yml/badge.svg)](https://github.com/travisbrown/archivindex/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/travisbrown/archivindex/branch/main/graph/badge.svg)](https://codecov.io/gh/travisbrown/archivindex)
[![license](https://img.shields.io/github/license/travisbrown/archivindex)](https://github.com/travisbrown/archivindex/blob/main/LICENSE)

Tools for capturing web pages and packaging them as web archive collections.

## Crates

| Crate | Description |
| --- | --- |
| [`archivindex-wacz`](wacz/) | Reading and writing web archive collections in the [WACZ][wacz-spec] format |
| [`archivindex-packager`](packager/) | Packaging WARC captures as indexed WACZ distributions |
| [`archivindex-archiver`](archiver/) | Archiving web pages over HTTP into WARC files |
| [`archivindex-archiver-cli`](archiver-cli/) | An `archivindex-archiver` command-line front end |
| [`archivindex-warc-revisit-index`](warc-revisit-index/) | Persistent WARC payload-revisit and HTTP resource state |

The WARC reading and writing core these crates are built on lives in a separate repository,
[`archivindex-warc`][archivindex-warc], and is used here as a source dependency.

## Usage

The `archive` command reads one URL per line from standard input and writes a single WARC file:

```bash
echo https://example.com/ \
  | cargo run --bin archivindex-archiver -- archive --output example.warc.gz
```

The `archive-wp-comments` command captures comment batches from a WordPress REST API into a crawl
session. It fixes a creation-time cutoff and pages by comment ID. One sweep is sufficient when the
reported total is stable and matches the distinct IDs captured; otherwise a second consistency
sweep runs automatically. Pass `--second-sweep` to request that validation sweep unconditionally.
`--limit` can stop after a fixed number of successful batches (including validation recaptures):

```bash
cargo run --bin archivindex-archiver -- archive-wp-comments \
  --base-url https://example.com/ \
  --output comments.warc.gz \
  --session-name comments-2026 \
  --operator "A. Archivist" \
  --operator-email archivist@example.com \
  --revisit-index comments-state.sqlite3 \
  --limit 10
```

The `read-wp-comments` command writes the archived comments as JSON Lines in ascending comment ID
order. Conflicting captures of the same comment are reported through the warning log:

```bash
cargo run --bin archivindex-archiver -- read-wp-comments comments.warc.gz > comments.jsonl
```

The `warc-to-wacz` command converts a plain or gzip-compressed WARC file into an indexed WACZ. A
metadata record's `title` field supplies the linked page title. Captures whose metadata contains a
`via` field are written to `extraPages.jsonl`; all others are written to `pages.jsonl`. The first
`warcinfo` record's title and description become package metadata, and additional warcinfo record
IDs are reported as conversion warnings:

```bash
cargo run --bin archivindex-archiver -- warc-to-wacz capture.warc.gz \
  --output capture.wacz
```

For a plain input WARC, pass `--gzip-warc` to store it as `archive/data.warc.gz`. Each record is
written as an independent gzip member so indexed captures remain directly addressable:

```bash
cargo run --bin archivindex-archiver -- warc-to-wacz capture.warc \
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
