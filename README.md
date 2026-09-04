# archivindex-wacz

![GitHub last commit](https://img.shields.io/github/last-commit/travisbrown/archivindex-wacz)
[![build](https://github.com/travisbrown/archivindex-wacz/actions/workflows/ci.yml/badge.svg)](https://github.com/travisbrown/archivindex-wacz/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/travisbrown/archivindex-wacz/branch/main/graph/badge.svg)](https://codecov.io/gh/travisbrown/archivindex-wacz)
[![license][license-badge]][gpl-3.0]

Rust libraries and a command-line tool for packaging WARC captures as WACZ files and reading them.

## Crates

| Crate                                             | Description                                                                 |
| ------------------------------------------------- | --------------------------------------------------------------------------- |
| [`archivindex-wacz`](crates/wacz/)                | Reading and writing web archive collections in the [WACZ][wacz-spec] format |
| [`archivindex-packager`](crates/packager/)        | Packaging WARC captures as indexed WACZ files                               |
| [`archivindex-packager-cli`](tools/packager-cli/) | The `archivindex-packager` command-line tool                                |

Git dependencies come from two repositories: [`archivindex`][archivindex] provides SURT, CDX,
and shared utilities; [`archivindex-warc`][archivindex-warc] provides WARC reading and writing
and the archiver used in integration tests.

## Usage

The `warc-to-wacz` command converts a plain or gzip-compressed WARC file into an indexed WACZ.
The output path must end in `.wacz`, and an existing file is not overwritten:

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

Gzip input is detected from its contents and recompressed as independent gzip members, regardless
of `--gzip-warc`. Use `--compressed-index` to write a gzip-compressed CDXJ index with a `ZipNum`
summary instead of plain CDXJ.

The gzip compression level applies to WARC records and `ZipNum` blocks; it ranges from 0 through
9 and defaults to 6.
Use `--zip-compression-level` to set the ZIP DEFLATE level for metadata and plain indexes; it ranges
from 1 through 264 and defaults to 6. Levels above 9 use Zopfli and may be substantially slower.

The command exits with status 0 on success, 1 when conversion completes with warnings, and 2 on
failure. HTTP captures missing a usable status or payload digest remain in the WARC but are not
indexed and produce warnings.

### Package metadata and page lists

The first `warcinfo` record supplies the package title and description. If it has no `title`,
its `isPartOf` value supplies the title. Additional `warcinfo` records produce warnings.

Linked metadata supplies page titles and can override capture URLs with `pageUrl`. If the first
`warcinfo` declares `pageList: metadata`, only indexed captures with a linked `pageUrl` enter the
page lists; otherwise, every indexed capture gets a page entry. Entries with linked `via`
metadata go in `pages/extraPages.jsonl`; the rest go in `pages/pages.jsonl`.

### Library output

Use `WaczFileWriter::create` (or `WaczWriter::create`) to stage a WACZ file beside its
destination. Calling `finish` publishes it without overwriting an existing file and returns a
`File`. For in-memory or other seekable outputs, `WaczWriter::new` accepts a sink; `finish`
flushes and returns it.

## License

This project is licensed under the [GNU General Public License, version 3][gpl-3.0]; see
[LICENSE](LICENSE) for the full text.

[archivindex]: https://github.com/travisbrown/archivindex
[archivindex-warc]: https://github.com/travisbrown/archivindex-warc
[gpl-3.0]: https://www.gnu.org/licenses/gpl-3.0.html
[license-badge]: https://img.shields.io/badge/license-GPL--3.0-orange
[wacz-spec]: https://specs.webrecorder.net/wacz/1.1.1/
