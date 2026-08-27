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

Capture output is uncompressed by default. Archiving commands use the `gzip-warc` configuration
setting to compress each WARC record as an independent gzip member.

Commands exit with status 0 when they complete without reportable problems. Status 1 means the
command completed with problems to report about its input, including when a usable but partial
archive was published. Operational failures use status 2.

### WordPress comments

The `archive-comments` command captures comment batches from a WordPress REST API into a crawl
session. It fixes a creation-time cutoff and pages by comment ID. One sweep is sufficient when the
reported total is stable and matches the distinct IDs captured; otherwise a second consistency
sweep runs automatically. Pass `--second-sweep` to request that validation sweep unconditionally.
The command accepts the archiver's TOML or JSON configuration schema through `--config`. Transport,
WARC metadata, digest, and session settings—including retry policy, request delay, and titles—belong
in that file. For example, `comments.toml` could contain:

```toml
gzip-warc = true

[operator]
name = "A. Archivist"
email = "archivist@example.com"

[session]
request-delay = "1s"
titles = true
```

The capture limit counts successful batches, including validation recaptures:

```bash
cargo run --bin archivindex-wordpress -- archive-comments \
  --config comments.toml \
  --base-url https://example.com/ \
  --output comments.warc.gz \
  --session-name comments-2026 \
  --revisit-index comments-state.sqlite3 \
  --limit 10
```

The `read-comments` command writes the archived comments as JSON Lines in ascending comment ID
order. Conflicting captures of the same comment are reported through the warning log:

```bash
cargo run --bin archivindex-wordpress -- read-comments comments.warc > comments.jsonl
```

The `check-comments` command verifies that the WARC has an HTTP 200 response or revisit record
inferred as `application/json` for every page from one through the greatest valid
`X-WP-TotalPages` value it contains. The command accepts plain or gzip-compressed WARCs and exits
with status 1 when coverage is incomplete. A changing `X-WP-TotalPages` value is also reported with
the signed differences between successive values and produces status 1:

```bash
cargo run --bin archivindex-wordpress -- check-comments comments.warc.gz
```

The `complete-comments` command requests only the missing pages, preserving the source archive's
paging URL byte-for-byte except for the decimal `page` value. It writes the new request and
response records to a separate WARC whose first record is the source WARC's original `warcinfo`.
The output uses the source archive's compression and is never overwritten:

```bash
cargo run --bin archivindex-wordpress -- complete-comments \
  comments.warc.gz comments-completion.warc.gz \
  --config comments.toml
```

After a complete historical capture, `update-comments` starts a bounded incremental run. It uses
the latest archived comment's `date_gmt`, falling back to the historical request's `before` cutoff
when no comments were returned. The new run sends that instant minus `--overlap` as `after` and the
current time as `before`; the overlap defaults to one day:

```bash
cargo run --bin archivindex-wordpress -- update-comments comments.warc.gz \
  --output comments-update.warc.gz \
  --session-name comments-update-2026-08-20 \
  --overlap 1day \
  --config comments.toml
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
