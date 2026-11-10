# NXFormat 1.0.2

NXFormat 1.0.2 fixes the titles of games converted from recent NUP files and
makes the C library smaller. It includes the NXConvert desktop application and
CLI, the Rust archive writer, the C API and the header-only C++17 reader. The
NX format is unchanged: archives stay readable by 1.0.1 readers, and by 1.0.0
readers when they hold no firmware.

## What is new in 1.0.2

**NUP metadata**
- `pack-nup` reads the metadata of NUP files made by `nup` versions whose
  metadata types are shifted by one (NPDM = 0, NACP = 1, ...). Before, such
  games got the publisher as title, the build ID as publisher and the title as
  display version. Files with the specified types are read as before.

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
This release is **NXFormat / NXConvert 1.0.2**, with separate 4096-byte header
pages. The exact binary header fields are defined in the format specification.

## Packages

| Package | Contents |
| --- | --- |
| Windows x64 | NXConvert app, shared/static libraries and headers |
| Linux x64 / arm64 | NXConvert app, shared/static libraries and headers |
| macOS universal | NXConvert app bundle, CLI, shared/static libraries and headers |
| Android arm64 | Shared/static libraries and headers, no desktop app |

Release archives include CMake package configuration. `SHA256SUMS` is published
as a separate attachment. `nxformat-xmake-1.0.2.zip` contains the xmake package
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
