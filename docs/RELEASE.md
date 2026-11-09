# NXFormat 1.0.1

NXFormat 1.0.1 includes the NXConvert desktop application and CLI, the Rust
archive writer, the C API and the header-only C++17 reader.

## What is new in 1.0.1

- Native packs keep the cart's bundled system update (`partition.hfs0`) by
  default as a Source Resource, so `rebuild` gives back the exact DXCI.
  `--omit-firmware` (or the desktop per-game choice) leaves it out. Archives
  with firmware are NX 0.3 and need a 1.0.1 reader; all others stay NX 0.2.
- The reader opens NX 0.2 and 0.3. Reads return a `PartialRead`: verified
  blocks are committed straight into the caller's buffer, and
  `OpenOptions::atomic_reads` keeps the 1.0.0 all-or-nothing behaviour.
- Snapshot trees are checked without recursion, and the directory depth limit
  is now 256 in every implementation. No archive can exhaust a thread stack.
- A fresh `pack` publishes one generation even when it adds updates or
  recipes, so readers open it without checking an older generation.
- An I/O error while reading the newest generation is reported instead of
  falling back to an older generation.
- `Session` decodes split Zstandard reads on a small worker pool
  (`decode_workers`, up to 4 by default) instead of one thread: 1.6x to 2.8x
  faster sequential reads of compressed files on Switch 2 games.
- The block cache no longer serializes concurrent readers: cache hits share a
  lock and counters are kept per thread. With 8 threads, cached 4 KiB reads
  have a 0.9 µs p99 instead of 17 µs, and throughput is up to 3.5x higher.

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
