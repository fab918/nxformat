# NXFormat 1.0.1

NXFormat 1.0.1 keeps the cart's bundled system update inside the archive, so a
packed DXCI can be rebuilt byte for byte, and makes reads faster and more
robust, especially when several threads read at once. It includes the NXConvert
desktop application and CLI, the Rust archive writer, the C API and the
header-only C++17 reader.

> **Compatibility:** archives that contain firmware use NX 0.3 and only open in
> 1.0.1 or later readers. All other archives stay NX 0.2 and still open in 1.0.0.
> Update your emulator before sharing archives with firmware.

## What is new in 1.0.1

**Firmware preservation**
- DXCI/DNSP packs keep the bundled system update (`partition.hfs0`) by default,
  as a Source Resource. `rebuild` gives back the exact original package.
- `--omit-firmware`, or the per-game choice in the desktop app, leaves it out.
- Identical firmware shared by several packages is stored once.
- In the desktop app, recipe and firmware options only appear for DXCI/DNSP
  packages, and the firmware option only when the package has one.

**Faster reads**
- `read()` and `read_file()` return a `PartialRead`: verified blocks go
  straight into the caller's buffer, with no staging copy.
  `OpenOptions::atomic_reads` keeps the 1.0.0 all-or-nothing behaviour.
- `Session` decodes split Zstandard reads on a small worker pool
  (`decode_workers`, up to 4 by default): 1.6x to 2.8x faster sequential reads
  of compressed files on Switch 2 games.
- The block cache no longer serializes concurrent readers. With 8 threads,
  cached 4 KiB reads have a 0.9 µs p99 instead of 17 µs, with up to 3.5x the
  throughput.
- `Archive::lookup(directory, name)` finds a path by binary search
  (about 0.2 µs instead of 1 to 4 µs for a scan).

**More robust**
- Snapshot trees are checked without recursion, and the directory depth limit
  is 256 in every implementation. No archive can exhaust a thread stack.
- An I/O error while reading the newest generation is reported, instead of a
  silent fallback to an older generation.
- A fresh `pack` publishes a single generation, even when it adds updates or
  recipes.
- `add` with recipes is atomic: the content and its recipes are published
  together, or not at all.
- The session worker keeps running after a user cancellation callback fires.

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
This release is **NXFormat / NXConvert 1.0.1**, with separate 4096-byte header
pages. The exact binary header fields are defined in the format specification.

## Packages

| Package | Contents |
| --- | --- |
| Windows x64 | NXConvert app, shared/static libraries and headers |
| Linux x64 / arm64 | NXConvert app, shared/static libraries and headers |
| macOS universal | NXConvert app bundle, CLI, shared/static libraries and headers |
| Android arm64 | Shared/static libraries and headers, no desktop app |

Release archives include CMake package configuration. `SHA256SUMS` is published
as a separate attachment. `nxformat-xmake-1.0.1.zip` contains the xmake package
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
