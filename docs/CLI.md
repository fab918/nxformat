# NXConvert commands and integration

Applies to **NXFormat / NXConvert 1.0.1**.

NXConvert is a native application and command-line tool. Start `nxconvert`
without arguments to open the window; use `nxconvert --help` for the current
command list. Commands live in `packer/src/cli.rs`; `app/` provides the app
and executable, and `ffi/` exposes the same commands through a C API.

## Common commands

```text
nxconvert scan SOURCE|FOLDER...
nxconvert pack SOURCE... OUTPUT.nx [--no-recipe [--omit-firmware]]
nxconvert pack-tree PACKAGE OUTPUT.nx [--platform switch1|switch2]
nxconvert pack-nup INPUT.nup OUTPUT.nx --platform switch1|switch2
nxconvert contents ARCHIVE.nx
nxconvert add ARCHIVE.nx SOURCE... [--no-recipe [--omit-firmware]]
nxconvert add-tree ARCHIVE.nx PACKAGE [--platform switch1|switch2] [--update-version N]
nxconvert remove ARCHIVE.nx SNAPSHOT_IDENTITY...
nxconvert compact ARCHIVE.nx [--no-compress]
nxconvert verify ARCHIVE.nx [PACKAGE RECEIPT]
nxconvert check-nup INPUT.nup ARCHIVE.nx [--skip-dlc]
nxconvert set-metadata ARCHIVE.nx APPLICATION_ID [title=TEXT] [publisher=TEXT] [icon=FILE.jpg|png]
nxconvert recipe ARCHIVE.nx SOURCE|RECIPE.nxrecipe...
nxconvert rebuild ARCHIVE.nx FOLDER [RECIPE_ID...]
nxconvert check-recipe ARCHIVE.nx
```

`pack` and `add` accept supported already-decrypted DNSP/DXCI sources.
`contents` lists snapshot identities for removal. `pack-tree` imports
extracted directories; an explicit manifest can supply metadata the tree
cannot establish. `add-tree` adds supported `updates/<version>/` and `dlc/`
folders, or one update folder with `--update-version N`. Automatic tree imports
may infer application relationships from package layout and write review files;
use an explicit manifest when those assumptions do not apply. Run `--help` for
the full manifest-based forms and options.

`pack` and `add` keep a small reconstruction recipe inside the NX by default;
`--no-recipe` skips it. The recipe is read while the conversion runs, and a
recipe that cannot be kept causes an error. A failed recipe leaves the destination
of `pack` unchanged. With `add`, content groups already committed remain in the
archive. Use `nxconvert recipe ARCHIVE.nx SOURCE...` to retry storing the recipes.
Rebuilding requires the recipe and all referenced content; see [RECIPE.md](RECIPE.md).

`verify` checks physical blocks, Media hashes and logical file hashes. With
`PACKAGE RECEIPT`, it also compares extracted source bytes and provenance.
`check-nup` compares imported NUP paths, NACP and file bytes against NX; it does
not validate every NUP display-metadata TLV.

## Display title and artwork

The window offers an optional title and square artwork when native display
metadata is missing. JPEG and PNG inputs are limited to 16 MB; the crop editor
exports a 256 × 256 JPEG. Native artwork is preserved as supplied.

Command-line conversions accept `--fallback-title "Title"` and
`--fallback-icon image.jpg` (JPEG or PNG, at most 16 MiB). These only fill missing
display fields on non-DLC snapshots. They never replace native fields, modify
NACP bytes, supply execution identifiers, or change logical snapshot identity.
Fallback fields are recorded in an opaque display provenance payload.

`set-metadata` explicitly replaces supplied display fields for one application
in place. A replacement title clears localized titles; a replacement icon clears
localized icons. Unspecified fields and native NACP bytes stay intact. The edit
is attached to the selected display snapshot, or the newest non-DLC snapshot
when none is selected, and leaves logical snapshot identities unchanged. The
application ID is hexadecimal and must identify one platform unambiguously.

## Compression and resource options

`--no-compress` stores all newly written game-data blocks as RAW, skipping
Zstandard encoding. It applies to `pack`, `pack-tree`, `pack-nup` and new data
written by additions, and overrides `--level` regardless of option order.
Existing compressed blocks retained by `add` stay unchanged. Use
`compact --no-compress` to remove compression from an existing NX archive.
Deduplication and integrity checks remain enabled. NUP imports still decode
compressed source data. Optional rebuild recipes use their own compressed
literal stream; use `--no-recipe` with native `pack`/`add` to skip recipes too.
The window exposes this mode as **Non**, to the left of **Small**, **Balanced**
and **Maximum**; the level slider is disabled while **Non** is selected.

```bash
nxconvert pack game.dnsp game.nx --no-compress --no-recipe
nxconvert pack-nup game.nup game.nx --platform switch2 --no-compress
```

`--level 1-13` selects Zstandard level (default 3). `--workers N` sets
compression workers; 0 selects automatically. `--compressed-chunk-kib`
accepts 0, 64, 128 or 256 and controls chunk size for compressible files
(default 64 KiB; 0 disables re-chunking). `--min-gain-bp N` keeps compression
only when it saves at least N/100 percent (default 2500, or 25%).
Memory and temporary-storage budgets use `--memory-mib` and `--temp-mib`; additional limits and verification
options are listed by `--help`.

Edits append by default. `compact` copies retained stored blocks into a compact
archive to reclaim space. To remove existing game-data compression:

```bash
nxconvert compact game.nx --no-compress
```

This command needs only the NX file. It strictly verifies the source, decodes
retained Zstandard blocks and writes them as RAW into a same-directory temporary
archive. It verifies the result before atomically replacing the original.
Files, snapshot identities, metadata, dependencies, shared blocks and rebuild
recipes are preserved; unused storage is discarded. Existing chunk sizes stay
the same. It may increase archive size, and enough temporary disk space is
required for the expanded result. Budget errors, corruption or cancellation
leave the original intact. Recipes retain their separate compressed literal
stream; this option changes game-data blocks, not the recipe encoding.
Automatic compaction is disabled unless an API caller enables it.

## Embedding and progress

The C API [nxformat.h](../ffi/include/nxformat.h) runs commands in process
with output callbacks and cancellation. Add `--frontend` for machine-readable
output. Conversion/edit commands report tab-separated `@TOTAL` and
`@PROGRESS` lines; `contents` reports records including `@APP`, `@SNAPSHOT`,
`@RECIPE`, `@SIZE`, `@ALLOCATED` and `@RECLAIMABLE` where available.

For a subprocess using `--frontend`, closing stdin requests cancellation.
In-process callers use their cancellation token instead; the command does
not read stdin when a sink or cancellation token is supplied.

See [the README](../README.md#developer-integration) for CMake, xmake and C
examples, and [READER.md](READER.md) for the independent C++17 reader.

## Source import contract

Recognition uses validated source headers and internal fields. No import path
loads keys, decrypts content or downloads game data. Encrypted, malformed,
unsupported or ambiguous inputs are rejected.

| Source | Supported input |
| --- | --- |
| DNSP | PFS0 package containing validated decrypted DNCA units |
| DXCI | Decrypted gamecard container with validated HFS0 partitions |
| Extracted tree | Complete `exefs/`, `romfs/` and optional `data/` views |
| NUP v1 | RAW/Zstandard files, declared ExeFS/RomFS and optional `data/` |
| Standalone DNCA | Inventory only; conversion requires enough package association evidence |
| NSZ/XCZ or encrypted NSP/XCI | Unsupported |

Native conversion re-inspects sources before writing. Application, Program,
version, content and dependency identities come from native records. A native
update stores its exact base and patch NCA set; the emulator's existing native
loader resolves their patch structures. A snapshot is a complete native content
set, not a materialized patched RomFS. Multi-Program titles retain one complete
view and exact Build ID per Program/version. Data-only updates retain executable
identity from their exact base; missing update display metadata remains absent.

`add` can read its base directly from the existing NX and inherit its file/block
references. The original base package is unnecessary. Retained compressed frames
are copied unchanged; only unmatched new blocks are compressed. Multiple bases
require an exact base selector through the library API. Unresolved dependencies,
conflicting native units and ambiguous Program families are rejected.

Native `InventoryMode::Strict` checks supported metadata-section hash trees; it
does not authenticate Nintendo signatures or validate every native RomFS/BKTR
execution path. NX `IntegrityPolicy::Strict` independently checks NX payloads and
file digests. See [SECURITY.md](SECURITY.md) for Fast/Strict editing policies.

## NUP migration

```text
nxconvert pack-nup game.nup game.nx --platform switch2 --temp-mib 131072
nxconvert pack-nup game.nup game.nx --platform switch1 --skip-dlc
```

NUP v1 has no authoritative platform field, so `--platform` is required. Base,
update, standalone-update and non-executable DLC packages are supported. An update
replaces the base ExeFS when supplied and shadows individual RomFS/data paths;
other base files remain available. DLC uses its declared add-on title ID.
Executable add-ons and unresolved or ambiguous package relationships are rejected.
Future NUP versions require explicit support.

Before extraction, declared expansion is checked against import and temporary-disk
limits. Runtime accounting covers decoded files, native metadata and copy fallback
when a hard link cannot be created. Staged files remain charged while NX is being
written; the writer receives only the remaining temporary-disk budget. Owned
staging directories are removed on failure, cancellation and success. The limit
counts file bytes, not filesystem allocation overhead or available free space.

The CLI also writes `.nximport`, `.assumptions.txt` and `.nxreceipt` review
files beside the output. `--no-trail` suppresses these files for `pack-nup`,
automatic `pack-tree` and `add-tree`. These record its declarations and interpretation of NUP metadata. Review them when a
source's relationships are uncertain; use an explicit extracted-tree manifest
when those assumptions do not apply.

For library callers, `convert_nup` applies both limits automatically.
`stage_nup_with_options` stages separately; pass
`stage.remaining_writer_options(&options)` to `convert_extracted` so staging and
writer work share one budget. `stage_nup` without writer options uses
`ImportLimits::max_total_bytes` as its staging bound.

## Extracted-tree declarations

Automatic `pack-tree` accepts a complete supported tree or recognizable package
layout. Directory names and numeric folder names do not establish application
identity, snapshot type or content version. Use an explicit manifest when the
source cannot establish these facts without assumptions:

```text
nxconvert pack-tree PACKAGE MANIFEST.nximport OUTPUT.nx RECEIPT.nxreceipt
nxconvert pack-tree PACKAGE MANIFEST.nximport OUTPUT.nx RECEIPT.nxreceipt --latest
```

`NXIMPORT` version 1 is UTF-8 with actual tab-separated fields and LF/CRLF records.
Blank lines, comments, escaping, duplicate keys and unknown fields are rejected.
`TAB` below stands for one tab:

```text
NXIMPORT TAB 1
snapshot TAB key TAB relative-root TAB platform TAB application-id TAB program-id TAB role TAB type TAB version TAB completeness TAB materialized-v1 TAB npdm-profile
```

| Field | Values |
| --- | --- |
| key | 1-64 ASCII letters, digits, `_` or `-` |
| relative-root | `.` or a portable relative path within the package |
| platform | `switch1` or `switch2`, explicitly declared |
| application-id | Nonzero 16-digit hexadecimal ID |
| program-id | Nonzero 16-digit hexadecimal ID, or `-` when resolved from NPDM/absent for DLC |
| role | `primary`, `auxiliary` or `none` for non-executable DLC |
| type | `base`, `update`, `standalone-update` or `dlc` |
| version | Opaque unsigned 32-bit content version |
| completeness | `complete` or `partial` |
| npdm-profile | `none`, `meta-acid2048-v1` or `meta-acid3072-v1` |

Records following a snapshot declaration may include:

```text
dependency TAB key TAB required-base|requires TAB target-key
compose TAB key TAB replace-delete-v1 TAB base-key TAB base-identity-hex64
remove TAB key TAB exact-path
exclude TAB key TAB exact-path
build TAB key TAB expected-build-id-hex64
content TAB key TAB content-id-hex32
addon TAB key TAB addon-title-id-hex16
title|publisher|display-version TAB key TAB declared-text
nacp TAB key TAB relative-control.nacp
icon TAB key TAB native-language-name TAB relative-icon-path
```

DLC needs a source content ID or a declared add-on title ID. Executable or partial
DLC is unsupported. Declared text remains provenance; native display metadata is
projected from explicit NACP/icon records. NACP is exactly 0x4000 bytes; icons keep
their original JPEG/PNG bytes. Native language names such as `AmericanEnglish`
map to lowercase BCP-47 identifiers. The first populated NACP language supplies
the default title/publisher; other versions retain their own metadata history.
Newer unsupported NACP title encodings remain preserved as opaque source bytes.

A partial update requires `replace-delete-v1`, an exact parent NX identity, matching
application/platform/Program and a later numeric version. Removal paths must
exist. Files replace matching paths; file/directory collisions need explicit
removal. The final composed executable view is validated again. `--latest` keeps
the latest complete version per Program, selected DLC and their dependency closure.

The extracted-name profile is portable ASCII. Absolute paths, `..`, case collisions,
reserved Windows names, links, reparse points and special files are rejected.
Only exact existing `.listing` artifacts may be excluded. NPDM/NSO structures and
required segment hashes are checked; RSA signatures are not authenticated.

Default import bounds: 1 MiB manifest, 1,024 snapshots, 100,000 nodes per view,
64 MiB inventory, depth 64, 4,096-byte paths, 1 TiB per file, 4 TiB cumulative
logical views, 256 MiB NSO segments, 512 MiB images and 4 GiB decoded executable
bytes. Sources must stay stable during conversion. Publication is atomic for NX;
the caller must retain the separate provenance receipt.

`ImportReceipt` uses the bounded `NXPROVENANCE` version 1 protocol:

```text
NXPROVENANCE TAB 1
manifest TAB manifest-bytes-hex
snapshot TAB key TAB nx-identity-hex64
fact TAB key TAB program|build TAB value-or-- TAB origin TAB evidence-hex TAB location-hex
module TAB key TAB path TAB build-id-or-- TAB verified-hash-count
```

Origins are `unknown`, `declared`, `native-structure` or `derived`. The receipt binds
observations to manifest/native/base evidence; it is not signed or embedded in NX.
`match_archive()` checks those bindings after edits. Loss of the receipt does not
prevent the archive from serving its files and identities.

## Firmware preservation (1.0.1)

Native DXCI conversion retains its original system-update HFS0 in an independent
Source Resource. `contents` reports it with `@RESOURCE` records
(`application_id`, `kind`, `file_size`, `path`). Native firmware validity is the
consumer's responsibility. Game snapshots and their identities are unchanged.

```sh
nxconvert pack game.dxci game.nx
mkdir -p rebuilt-folder
nxconvert rebuild game.nx rebuilt-folder
# Save space explicitly; no byte-exact source reconstruction is promised:
nxconvert pack game.dxci smaller.nx --omit-firmware --no-recipe
```

Firmware preservation and rebuild recipes are separate: `--no-recipe` alone
keeps firmware. `--omit-firmware` requires `--no-recipe`, applies only to newly
imported content, and does not silently strip resources already in an NX.

**Reader compatibility.** An archive that holds a Source Resource is written as
NX 0.3; every other archive stays NX 0.2. Readers from NXFormat 1.0.0, and the
emulators built with them, reject NX 0.3, so a DXCI packed with default options
opens only in readers from 1.0.1 or later. Update the emulator before sharing
such archives, or pack with `--omit-firmware --no-recipe` to keep NX 0.2. The
resource is mandatory (`flags=1`) on purpose: a 1.0.0 editor copies unknown
optional extensions verbatim and would leave its directory index pointing at
the wrong tree after a repack. One identical resource (same kind and files) is
kept once per game, even when several sources carry it.
