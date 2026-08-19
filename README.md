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
| [`archivindex-archiver`](archiver/) | Archiving web pages over HTTP into the WACZ format |
| [`archivindex-archiver-cli`](archiver-cli/) | An `archivindex-archiver` command-line front end |

The WARC reading and writing core these crates are built on lives in a separate repository,
[`archivindex-warc`][archivindex-warc], and is used here as a source dependency.

## Usage

The `archive` command reads one URL per line from standard input and writes a single WACZ file:

```bash
echo https://example.com/ \
  | cargo run --bin archivindex-archiver -- archive --output example.wacz
```

The `archive-wp-comments` command captures comment batches from a WordPress REST API into a crawl
session. It fixes a creation-time cutoff, pages by comment ID, and repeats complete sweeps until no
new IDs appear, so comments shifted between pages by concurrent deletions are not missed. `--limit`
can stop after a fixed number of successful batches (including validation recaptures):

```bash
cargo run --bin archivindex-archiver -- archive-wp-comments \
  --base-url https://example.com/ \
  --output comments.wacz \
  --session-name comments-2026 \
  --operator "A. Archivist" \
  --operator-email archivist@example.com \
  --limit 10
```

The `read-wp-comments` command writes the archived comments as JSON Lines in ascending comment ID
order. Conflicting captures of the same comment are reported through the warning log:

```bash
cargo run --bin archivindex-archiver -- read-wp-comments comments.wacz > comments.jsonl
```

## License

This project is licensed under the [MIT License](https://opensource.org/license/mit). See
[LICENSE](LICENSE) for the full text.

[archivindex-warc]: https://github.com/travisbrown/archivindex-warc
[wacz-spec]: https://specs.webrecorder.net/wacz/1.1.1/
