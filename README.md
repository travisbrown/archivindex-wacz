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
| [`archivindex-wordpress`](wordpress/) | Capturing and reading WordPress REST API resources |
| [`archivindex-packager-cli`](packager-cli/) | An `archivindex-packager` command-line front end |
| [`archivindex-wordpress-cli`](wordpress-cli/) | An `archivindex-wordpress` command-line front end |

These crates are built on two libraries that live in separate repositories and are used here as
source dependencies: [`archivindex-core`][archivindex-core] provides the SURT, CDX, and
command-line support crates, and [`archivindex-warc`][archivindex-warc] provides WARC reading and
writing, the archiver, and its revisit index.

## Usage

Each library with a command-line front end has its own binary, so a command below is invoked
through the binary that owns it.

WARC output compression follows the output filename: a name ending in `.gz` compresses each record
as an independent gzip member, while every other name produces a plain WARC.

Commands exit with status 0 when they complete without reportable problems. Status 1 means the
command completed with problems to report about its input, including when a usable but partial
archive was published. Operational failures use status 2.

### WordPress comments

The `archive-comments` command captures comment batches from a WordPress REST API into a crawl
session. It fixes a creation-time cutoff and pages by comment ID. One sweep is sufficient when the
reported total is stable and matches the distinct IDs captured; otherwise a second consistency
sweep runs automatically. Pass `--second-sweep` to request that validation sweep unconditionally.
The command accepts the archiver's TOML or JSON configuration schema through `--config`. Transport,
WARC metadata, digest, and session settings—including retry policy and request delay—belong in that
file. For example, `comments.toml` could contain:

```toml
[operator]
name = "A. Archivist"
email = "archivist@example.com"

[session]
request-delay = "1s"
```

The capture limit counts successful batches, including the pages a validation sweep repeats:

```bash
cargo run --bin archivindex-wordpress -- archive-comments \
  --config comments.toml \
  --base-url https://example.com/ \
  --output comments.warc.gz \
  --session-name comments-2026 \
  --revisit-index comments-state.sqlite3 \
  --limit 10
```

The revisit index is consulted for payload digests and validators but is not updated by a session;
load a published WARC into it with `archivindex-warc load-revisit-index`.

A session that stops before the traversal ends—at the capture limit, after a page exhausts its
retries, or on an interrupt (`Ctrl-C`), which finishes the capture in progress and publishes the
WARC—reports the pages it had yet to request. The `resume-comments` command continues those pages
in a new session through the same snapshot cutoff. A resumed page after page one is a session extra
whose metadata `via` names the preceding page, so the chain of pages continues across WARC files:

```bash
cargo run --bin archivindex-wordpress -- resume-comments \
  --config comments.toml \
  --output comments-2026-continued.warc.gz \
  --session-name comments-2026-continued \
  --revisit-index comments-state.sqlite3 \
  --url 'https://example.com/wp-json/wp/v2/comments?before=2026-08-20T00:00:00Z&orderby=id&order=asc&page=11&per_page=100'
```

The `read-comments` command writes the archived comments as JSON Lines in ascending comment ID
order. Conflicting captures of the same comment are reported through the warning log:

```bash
cargo run --bin archivindex-wordpress -- read-comments comments.warc > comments.jsonl
```

The `check-comments` command verifies that the WARC has an HTTP 200 response or revisit record
inferred as `application/json` for every page from one through the greatest valid
`X-WP-TotalPages` value it contains. Multi-site WARCs are grouped by comments endpoint and checked
independently in domain-name order, so one site's pages or totals never affect another's. The
command accepts plain or gzip-compressed WARCs and exits with status 1 when any collection is
incomplete. A changing `X-WP-TotalPages` value is also reported per collection with the signed
differences between successive values and produces status 1:

```bash
cargo run --bin archivindex-wordpress -- check-comments comments.warc.gz
```

The `complete-comments` command requests only the missing pages, preserving the source archive's
paging URL byte-for-byte except for the decimal `page` value. It writes the new request and
response records to a separate WARC whose first record is the source WARC's original `warcinfo`.
Each completed page after page one has a metadata `via` naming its preceding page. An output name
ending in `.gz` produces a gzip-compressed WARC; every other output name produces a plain WARC. The
output is never overwritten. While requests are running, completed captures, including their `via`
metadata, are flushed directly to `<output>.partial` in the output directory. The partial file is
atomically promoted after successful completion. Capture starts are spaced by the configured
`session.request-delay`:

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

The input may instead be a directory. In that case every `.warc` or `.warc.gz` file directly in
the directory is parsed, including multi-site WARCs produced by earlier update runs. Anchors are
merged by WordPress installation: the latest archived comment is retained across all input files,
or the latest request `before` cutoff when none of them contains a comment. Each site is added once
to the update session in domain-name order, and its pages are captured before the next site's
begin. Its first page is a session seed and therefore has no metadata `via`; every later page
points to the preceding page for that same site. An update that stops early reports the pages
still to request, including the first pages of sites it never reached, for `resume-comments`.
Other files and nested directories are ignored:

```bash
cargo run --bin archivindex-wordpress -- update-comments historical-comments/ \
  --output comments-update.warc.gz \
  --session-name comments-update-2026-08-20
```

### WACZ packaging

The `warc-to-wacz` command converts a plain or gzip-compressed WARC file into an indexed WACZ. A
metadata record's `title` field supplies the linked page title. Captures whose metadata contains a
`via` field are written to `extraPages.jsonl`; all others are written to `pages.jsonl`. The first
`warcinfo` record's title (or, without one, the collection it `isPartOf`, which is how a crawl
session records its identifier) and description become package metadata, and additional warcinfo
record IDs are reported as conversion warnings:

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

This project is licensed under the [GNU General Public License, version 3][gpl-3.0]; see
[LICENSE](LICENSE) for the full text.

[archivindex-core]: https://github.com/travisbrown/archivindex-core
[archivindex-warc]: https://github.com/travisbrown/archivindex-warc
[gpl-3.0]: https://www.gnu.org/licenses/gpl-3.0.html
[wacz-spec]: https://specs.webrecorder.net/wacz/1.1.1/
