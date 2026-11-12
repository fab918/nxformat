# Unreleased

- Reject edited restoration that would omit unexported snapshots, and reject
  multiple RomFS sections explicitly. Keep extracted folders discoverable when
  recipe inspection fails, with explicit normal-import recovery via
  `--ignore-restore` or the desktop checkbox. Add machine-readable restoration
  status and `--require-restore` for scripts requiring a successful recipe.

- Edited restoration now stages only original metadata and source resources;
  no sparse or full-size original NX capsule is created. Existing recipes and
  the NX format are unchanged. Compact and portable recipe tests verify that
  a 16 MiB payload repacks with one output's space plus 128 KiB, while preserving
  native metadata and shared resources.

- Preserve native NACP, localized icons/titles, opaque metadata, execution
  metadata, source resources and the selected application metadata snapshot
  during edited restoration. Verify retained payloads while streaming them.
- Warn about worst-case recipe disk use before extraction and add optional
  `--portable-restore` / desktop portable mode for encoder-independent replay.
  The NX and existing restoration stream layouts are unchanged.

- Keep successfully extracted files when optional restoration-recipe creation
  fails. Add `extract --no-restore` and a desktop checkbox, enabled by default.
- Bound encoder discovery to four exhaustive searches per archive and cache
  successful parameters; unmatched frames always retain original literal bytes.
- Extract verified ExeFS from Programs without RomFS and resolve data-only
  updates through exact Program dependencies.

- Full extraction keeps `.nxrestore.zst`, compressed at Zstd level 13. Unchanged
  repacks restore the exact NX and retain embedded original-package recipes.
  Edited repacks keep file paths and snapshot associations, with no promise to
  rebuild original packages after modifications. NX format compatibility is
  unchanged.
- Compact restoration uses path IDs, an aggregate inventory fingerprint and
  derived IVFC hash runs: the tested 30XX recipe shrank from 8.7 MB to 1.8 MB.

- Fix issue #9: native extraction now includes verified RomFS, BKTR update views
  and DLC instead of silently exporting ExeFS alone. Multi-Program and version
  destinations are kept distinct.
- Show snapshot storage types, native extraction limitations and original-package
  reconstruction availability. Add **Export raw NCAs…** / `extract-ncas` to export
  stored NCA files without package reconstruction.
- Add bounded traversal, streaming IVFC verification and regression coverage for
  mixed base/patch reads, corruption, paths, memory limits and native roundtrips.

# NXFormat 1.0.3

NXFormat 1.0.3 fixes importing extracted game folders copied through macOS and
large conversions that failed with "Too many open files" on macOS.
The fix applies to NXConvert's desktop application, command-line tool and C API.
The NX wire formats and C/C++ reader API are unchanged.

## What is new in 1.0.3

- Raise the process's open-file limit at startup (macOS defaults to 256), close
  each NUP or source file as soon as it is read, and bound the pinned directory
  handles of extracted-tree imports on Unix. Large packages and dumps, such as
  a NUP of over 150000 files, no longer fail with `os error 24`.
- Recognize bounded AppleDouble `._name` sidecars when the corresponding
  `name` exists beside them, and Finder `.DS_Store` database headers. These
  host metadata files no longer block root-folder imports or become game files
  inside RomFS. Original source files remain untouched.
- Preserve similarly named ordinary game files, orphan sidecars, directories
  and malformed metadata instead of excluding files by name alone.
- Apply the same secure directory listing to CLI discovery, progress totals,
  memory estimates, content import and source verification.
- Add regression tests that compare the complete archive bytes and progress
  totals with a clean source tree, check source verification and preserve
  actual game files whose names start with `._`.

## Changes in 1.0.2

**NUP metadata**
- `pack-nup` reads the metadata of NUP files made by `nup` versions whose
  metadata types are shifted by one (NPDM = 0, NACP = 1, ...). Before, such
  games got the publisher as title, the build ID as publisher and the title as
  display version. Files with the specified types are read as before.

**NXConvert**
- Metadata sidebar to edit the title, publisher and icon, and restore the
  original icon.
- Large NUP sources are supported, the source size is shown apart from the
  estimated progress, and a conversion cancels faster.
- Folders of the same dump dropped one by one become a single NX.
- `nxconvert extract ARCHIVE.nx NEW_FOLDER` and the **Extract all…** button
  write every snapshot in the layout of a dump: `base/`, `updates/<version>/`
  and `dlc/<title>/<version>/`, with `exefs/` and `romfs/`. Each file is
  checked with SHA-256. Content kept as NCA files only gives its ExeFS.

**Smaller C library**
- The C API is built for size: about a third smaller. Zstandard and SHA-256
  keep full optimization, so conversion runs at the same speed.
- Release archives add `nxformat_static_nozstd` (CMake `nxformat::static_nozstd`):
  the static library without Zstandard, for programs that already link
  libzstd 1.5.7 or later. The program then holds a single copy of zstd.

## Changes in 1.0.1

- DXCI/DNSP packs keep the bundled system update as a Source Resource (NX 0.3,
  readable from 1.0.1 only); `--omit-firmware` leaves it out.
- `read()` returns a `PartialRead`, `Session` decodes on a worker pool, the
  block cache no longer serializes readers, and `Archive::lookup` uses a
  binary search.
- Robustness fixes: no recursion on snapshot trees, I/O errors on the newest
  generation are reported, and `add` with recipes is atomic.

## Features

- Convert supported already-decrypted DNSP/DXCI packages, extracted trees and
  NUP v1 archives into NX.
- Add updates and DLC, remove snapshots and compact unused space.
- Store new game-data blocks without compression using CLI `--no-compress`
  or the desktop **Non** preset. Remove existing game-data compression with
  `compact ARCHIVE.nx --no-compress`. Deduplication and integrity checks remain active.
- Read independent RAW/Zstandard blocks with per-slice CRC-32C checks; verify
  archive metadata, Media payloads and logical files with SHA-256.
- Keep supported native-package reconstruction recipes by default; exact
  rebuilding requires the recipe and all referenced file content.
- Embed commands through the C API, including callback-backed plaintext sources,
  progress reporting and cancellation.

NXConvert does not decrypt content or provide keys or game files. Emulator
compatibility depends on the emulator integration and the selected content.
This release is **NXFormat / NXConvert 1.0.3**, with separate 4096-byte header
pages. The exact binary header fields are defined in the format specification.

## Packages

| Package | Contents |
| --- | --- |
| Windows x64 | NXConvert app, shared/static libraries and headers |
| Linux x64 / arm64 | NXConvert app, shared/static libraries and headers |
| macOS universal | NXConvert app bundle, CLI, shared/static libraries and headers |
| Android arm64 | Shared/static libraries and headers, no desktop app |

Release archives include CMake package configuration. `SHA256SUMS` is published
as a separate attachment. `nxformat-xmake-1.0.3.zip` contains the xmake package
repository populated with those archive hashes. Extract it and register the
local directory with xmake. Publication adds no commits to the source repository.

Build NXConvert from source with Rust 1.95 or newer:

```bash
cargo build --release --locked --manifest-path app/Cargo.toml
```

The Rust writer and C API support Rust 1.80 or newer. The independent C++ reader
requires C++17 and a caller-supplied Zstandard decoder for compressed blocks.
See the [README](https://github.com/fab918/nxformat/blob/main/README.md), [CLI guide](https://github.com/fab918/nxformat/blob/main/docs/CLI.md), [reader guide](https://github.com/fab918/nxformat/blob/main/docs/READER.md)
and [format specification](https://github.com/fab918/nxformat/blob/main/docs/SPEC.md) for usage and integration.
