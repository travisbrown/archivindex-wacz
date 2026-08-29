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

Where a command takes a WARC output filename, compression follows that name: a name ending in
`.gz` compresses each record as an independent gzip member, while every other name produces a
plain WARC. The `archive` and `resume-archive` commands instead name their own output files and
never compress them.

Commands exit with status 0 when they complete without reportable problems. Status 1 means the
command completed with problems to report about its input, including when a usable but partial
archive was published. Operational failures use status 2.

### WordPress sites

The `archive` command captures a fixed, supported set of a WordPress site's REST API collections
into a crawl session. Every run begins with eleven requests that are never paged: the API roots
`wp-json`, `wp-json/wp/v2`, and `wp-json/wp/v2/types`, then a bare request of each supported
collection endpoint in the order `pages`,
`posts`, `categories`, `tags`, `users`, `comments`, `media`, `videos`. Each endpoint whose bare
request succeeded is then paged to its end, in that same order, with the time the archive started
as its `before` cutoff, ascending by ID, one hundred items per page; an endpoint answering 404 is
skipped. If a collection's `X-WP-TotalPages` changes while it is being paged, the largest value seen
decides when the first pass ends. Every collection is then read once more from page one. This
validation pass catches records shifted onto earlier pages by concurrent deletions and fails if
the advertised page count changes during the pass. The eleven initial captures are session seeds;
collection pages are discovered from the last probe or preceding page, which their metadata `via`
names.

`--base` names the site as a host with an optional path and no scheme, such as `example.com` or
`example.com/blog` (a trailing slash is removed); requests use HTTPS. `--output` names a directory,
which is created if needed. The session is written uncompressed to `<output>/<session name>.warc`,
where `--session-name` defaults to the base and the current epoch second joined by a hyphen, with
the slashes of a path replaced by hyphens, for example `example.com-blog-1787995936`. Successive
runs therefore accumulate in the directory for merging later. The command accepts the archiver's
TOML or JSON configuration schema through `--config`. Transport, WARC metadata, digest, and session
settings—including retry policy and request delay—belong in that file. For example, `site.toml`
could contain:

```toml
[operator]
name = "A. Archivist"
email = "archivist@example.com"

[session]
request-delay = "1s"
```

The capture limit counts successful captures, including the initial eleven:

```bash
cargo run --bin archivindex-wordpress -- archive \
  --config site.toml \
  --base example.com \
  --output archives \
  --revisit-index site-state.sqlite3 \
  --limit 500
```

The revisit index is consulted for payload digests and validators but is not updated by a session;
load a published WARC into it with `archivindex-warc load-revisit-index`.

A run that stops after the initial eleven captures—at the capture limit, after a page exhausts its
retries, or on an interrupt (`Ctrl-C`), which finishes the capture in progress and publishes the
WARC—prints the `resume-archive` command that continues it. A failure before those eleven captures
are finished is fatal; the partial WARC is published, but a new `archive` must start over. The
resumption is identified by the endpoint being paged, the last durably written page (zero when the
endpoint must restart), the most recently advertised page count when known, and the original
`before` cutoff, so a resumed run requests the same page URLs. It continues that endpoint, probes
and pages the endpoints after it, and writes a new file in the output directory under a fresh
default session name. The first resumed page is a session extra whose metadata `via` names the last
page of the earlier run, so the chain of pages continues across WARC files. The printed command is
shell-quoted and repeats `--config`. It repeats `--revisit-index` only when it also has a page count
that makes conditional 304 responses resumable. It never repeats `--cookie`, which must be added
again by hand:

```bash
cargo run --bin archivindex-wordpress -- resume-archive \
  --config site.toml \
  --base example.com \
  --output archives \
  --revisit-index site-state.sqlite3 \
  --endpoint comments \
  --last-page 11 \
  --total-pages 19 \
  --before 2026-08-20T00:00:00Z
```

### WordPress comments

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
still to request, including the first pages of sites it never reached. Other files and nested
directories are ignored:

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
