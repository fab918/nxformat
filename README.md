<p align="center">
  <img src="docs/nxformat.png" alt="NXFormat" width="900">
</p>

<p align="center">
  <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/github/license/fab918/nxformat" height="20" align="top"></a>
  <a href="https://github.com/fab918/nxformat/actions/workflows/ci.yml"><img alt="Tests and conformance" src="https://github.com/fab918/nxformat/actions/workflows/ci.yml/badge.svg?branch=main" height="20" align="top"></a>
  <a href="https://github.com/fab918/nxformat/releases/tag/V1.0.2"><img alt="NXFormat v1.0.2" src="https://img.shields.io/badge/release-v1.0.2-blue" height="20" align="top"></a>
  <img alt="Switch 1 and Switch 2" src="https://img.shields.io/badge/Switch-1%20%26%202-337ab7" height="20" align="top">
  <img alt="C++17 reader" src="https://img.shields.io/badge/reader-C%2B%2B17-00599C" height="20" align="top">
</p>

<h2 align="center">Store once. Read fast. Update without starting over.</h2>

<p align="center">
  <strong>NX is a modern archive format for decrypted Nintendo Switch and Switch 2 game dumps.</strong><br>
  It is built for fast reads, smart storage, and easy management of updates and DLC.
</p>

<p align="center">
  <a href="https://github.com/fab918/nxformat/releases/tag/V1.0.2"><strong>Download NXConvert 1.0.2</strong></a>
  &nbsp;·&nbsp;
  <a href="#performance-nx-vs-nup">NX vs NUP</a>
  &nbsp;·&nbsp;
  <a href="#quick-start">Quick start</a>
  &nbsp;·&nbsp;
  <a href="docs/SPEC.md">Format specification</a>
</p>

- 🚀 **Modern format built for speed.** Designed to reduce decompression-related stutter. **104× faster median random reads than NUP** in the local benchmark described below. [See performance](#performance-nx-vs-nup).
- ✏️ **Updates and DLC, in place.** Add or remove content without copying the base game during normal edits.
- 🧩 **Store shared content once.** Reuse identical blocks across game files, updates and DLC.
- 🎮 **Switch 1 and Switch 2.** Convert supported decrypted **DNSP/DXCI** packages, extracted folders and NUP archives.
- 📦 **Get your original files back.** Native package conversions try to keep a recipe to recreate the original bytes (on by default, `--no-recipe` to skip it). Rebuilding needs all referenced content. [Learn more](docs/RECIPE.md).
- 🛡️ **Spot damaged game files.** Built-in integrity checks detect corruption during reads or a full archive check.
- 🛠️ **Emulator ready.** C++17 reader, Rust library and C API for Windows, Linux, macOS and Android (libraries). [Integration](#developer-integration).

> **Legal note**
> <sub>**NXConvert does not decrypt anything.** It only repackages game files you have already decrypted yourself, from games you own. It contains no keys and no copyrighted content.</sub>

## Quick start

**NXConvert 1.0.2** creates NXFormat 1.0.2 archives with separate header pages.
Download the [V1.0.2 release](https://github.com/fab918/nxformat/releases/tag/V1.0.2),
or build from source below. Desktop apps are available for Windows, Linux and
macOS; Android packages provide libraries only.

Build with Rust 1.95 or newer:

```bash
cargo build --release --locked --manifest-path app/Cargo.toml
./app/target/release/nxconvert --version
```

The executable is `app/target/release/nxconvert` (`nxconvert.exe` on Windows).
Use that path below, or add its directory to `PATH`.

Run `nxconvert` for the graphical interface, or use the CLI:

```bash
nxconvert pack game.dnsp game.nx            # Create an archive; recipe enabled
nxconvert add game.nx update.dnsp           # Add an update or DLC
nxconvert contents game.nx                  # List stored content
nxconvert remove game.nx SNAPSHOT_IDENTITY  # Remove a version or DLC
nxconvert verify game.nx                    # Check integrity
nxconvert compact game.nx                   # Recover unused physical space
nxconvert compact game.nx --no-compress     # Convert existing game-data blocks to RAW
nxconvert pack game.dnsp game-raw.nx --no-compress --no-recipe  # RAW data blocks
nxconvert pack-nup game.nup game.nx --platform switch2
```

See [CLI.md](docs/CLI.md) for extracted folders, metadata editing, limits and
machine-readable progress. Game boot support also depends on the emulator
integration; format validation alone does not establish game compatibility.

## Performance: NX vs NUP

Compression saves disk space, but decoding takes CPU time. When a game needs a few kilobytes from the middle of a compressed file, it should not have to decode everything before them. NX uses independent blocks to keep that work small.

A slow read can cause a hitch if a game is waiting for it. P99 measures the slow tail: roughly 99% of measured reads finish at or below this time. These read timings do not measure FPS or demonstrate an in-game stutter reduction.

![Random 4 KiB reads: NUP vs NX](docs/benchmarks/nup-vs-nx-random-reads.png)

**Why?** NUP stores a compressed file in one Zstandard frame. Reading far into that file may require decoding data from its beginning. NX uses smaller, independently accessible compressed blocks. Raw data can be read directly without Zstandard decoding.

![Random reads inside compressed files: NUP vs NX](docs/benchmarks/nup-vs-nx-compressed-files.png)

For reads inside compressed files, the tested median was **1.7 ms with NUP** versus **5.2 µs with NX**. The second chart also shows an especially slow NUP read, which is an example of a worst case, **not** a typical speedup.

**Small reads. Big difference.** In the local NUP reference-reader benchmark, NX completed random 4 KiB reads **104× faster at the median** and **about 94× faster at P99**.

> **Benchmark details:** Donkey Kong Bananza, update 3.2.0 and DLC (Switch 2); Windows 11; 300 seeded random 4 KiB reads; warm operating-system cache. The comparison uses the NUP Python reference reader with libzstd and the NX C++17 reader. These are results for the tested implementations and workload, not a universal format speedup or an FPS benchmark. A native-to-native comparison and a larger sample are needed before making broader claims.

The charts summarize an earlier local run whose individual read samples are not
included. The [NUP comparison tools](benchmarks/README.md#nup-and-native-package-comparisons)
can repeat this kind of measurement on your own dumps.

### Faster packing

![Packing a dump: NX vs NUP — Mario Kart World dump (Switch 2)](docs/benchmarks/nup-vs-nx-packing.png)

*NX skips compression with marginal savings, favoring faster packing and reads for a little extra space.*

## Developer integration

[ffi/include/nxformat.h](ffi/include/nxformat.h) runs any `nxconvert` command
in process: each output line goes to a callback, and a cancel handle stops the
run without leaving a partial file.

```c
const char* args[] = {"contents", "game.nx", "--frontend"};
nxformat_run(args, 3, on_line, user, cancel, error, sizeof error);
```

An emulator that decrypts games itself can hand nxformat the plaintext bytes
without writing them to disk or sharing keys: `nxformat_source_new` takes a
file name (`.nsp` or `.xci`), a size and a read callback, and
`nxformat_source_path` gives the path to pass to `pack`, `add` or `scan`.

```c
nxformat_source* game = nxformat_source_new("game.nsp", size, read_plaintext, ctx);
const char* args[] = {"pack", nxformat_source_path(game), "game.nx", "--frontend"};
nxformat_run(args, 4, on_line, user, cancel, error, sizeof error);
nxformat_source_free(game);
```

The [release workflow](.github/workflows/release.yml) builds one archive per
platform (Windows x64, Linux x64 and arm64, macOS universal, Android arm64):
`include/` (the C API and the C++ reader), `lib/` (shared and static library),
`bin/` (the Windows DLL and desktop NXConvert app). Android archives contain
libraries and headers only. `SHA256SUMS` is a separate release attachment.

For xmake, download `nxformat-xmake-1.0.2.zip` from the release and extract it.
It contains the package recipe with the published archive hashes. Point xmake
at that extracted directory:

```lua
add_repositories("nxformat /path/to/extracted/nxformat-xmake")
add_requires("nxformat")
target("app")
    add_packages("nxformat")
```

With CMake, unpack an archive and point `nxformat_DIR` at its
`lib/cmake/nxformat`:

```cmake
find_package(nxformat 1.0 REQUIRED CONFIG)
target_link_libraries(app PRIVATE nxformat::nxformat) # or nxformat::static, or nxformat::static_nozstd with your own libzstd
# Copy ${NXFORMAT_RUNTIME} next to the program.
```

Pushing a tag `VX.Y.Z` runs [release.yml](.github/workflows/release.yml): it
builds and tests every platform, publishes the release and attaches a populated
xmake repository generated from [packages/n/nxformat](packages/n/nxformat/xmake.lua).
Publishing adds no commits to the source repository. If the release already
exists, the workflow validates its published xmake package without rebuilding
or replacing assets. Windows consumers explicitly target MSVC (`-p windows
-a x64 --toolchain=msvc`), including when invoked from Git Bash.

## Tests

```text
# Requires Rust, Python 3.14+, a C++17 compiler and a Zstandard runtime.
cargo test --release --locked --manifest-path packer/Cargo.toml
cargo clippy --all-targets --manifest-path packer/Cargo.toml -- -D warnings
cargo test --release --locked --manifest-path ffi/Cargo.toml
cargo build --release --locked --manifest-path ffi/Cargo.toml
python tools/test_c_api.py
python tests/conformance/check_phase0.py
python tests/conformance/validate_fixtures.py
python tests/conformance/test_reader.py
python tests/conformance/test_rust_writer.py
```

[Tests and conformance](.github/workflows/ci.yml) run on every push and pull request, and can also be started manually. Windows, Linux and macOS run Rust, C++ reader, C API, app and packaged CMake consumer checks. The workflow also checks deterministic fixtures and the generated header, and runs Linux AddressSanitizer, UndefinedBehaviorSanitizer and a one-minute coverage-guided fuzzing job. A release cannot publish unless these checks pass. Real game files are never used in CI.

A code coverage percentage is not currently published. Coverage-guided fuzzing is not a line or branch coverage report.

## What is in this repository

| Folder | Content |
| --- | --- |
| [packer/](packer/) | Rust library: convert, edit, verify, rebuild the original files. Its `cli` module holds the `nxconvert` commands. |
| [app/](app/) | NXConvert: a small native window (egui) and the same commands from a shell. |
| [ffi/](ffi/) | C API ([ffi/include/nxformat.h](ffi/include/nxformat.h)): runs the `nxconvert` commands inside another program. |
| [packages/](packages/) | xmake package of the release builds. |
| [reader/](reader/) | Header-only C++17 reader. [dist/](dist/) holds the copy used by emulators. |
| [docs/](docs/) | Format specification and usage guides. |
| [tests/](tests/) | Conformance fixtures, Python validator, C++ reader tests, fuzz target. |
| [benchmarks/](benchmarks/) | Reader and writer benchmark scripts. |
| [integration/](integration/) | NXEmu loader patch and its export tool. |

The Rust side writes NX files. The C++ reader only reads them: an emulator
links the reader for play, and the nxformat library for every change.

## Documentation and development

- [RELEASE.md](docs/RELEASE.md): V1.0.2 release notes and platform packages.
- [SPEC.md](docs/SPEC.md): wire format and versioning.
- [CLI.md](docs/CLI.md): commands, native/NUP imports, extracted-tree declarations and resource limits.
- [READER.md](docs/READER.md): C++ API, persistent sessions, emulator integration and access profiles.
- [SECURITY.md](docs/SECURITY.md): validation rules, resource ceilings and integrity policies.
- [RECIPE.md](docs/RECIPE.md): optional exact-source reconstruction.
- [benchmarks/README.md](benchmarks/README.md): performance tools and real-content acceptance checks.
- [tests/](tests/), [benchmarks/](benchmarks/): tests, fixtures and benchmark tooling.

The project is licensed under the [MIT License](LICENSE). NXFormat is an independent project and is not affiliated with Nintendo.

## NX vs NUP

NUP made it possible to package decrypted Switch 1 and Switch 2 trees in one file. NX builds on that idea with a greater focus on **random reads, content changes, and predictable version handling**.

| Capability | NUP reference format and tools | NX |
| --- | --- | --- |
| Random reads in compressed files | One Zstd frame per file; seeking can require earlier decompression | **Independent compressed blocks** |
| Compression policy | Compress if it saves any space | **Selective Zstd; raw storage for marginal savings** |
| Deduplication | Identical files | **Identical blocks** |
| Several updates and DLC in one archive | Yes | **Yes** |
| Add or remove game content later | No dedicated payload add/remove command in the reference CLI | **Incremental add/remove** |
| Explicit update dependency tracking | Limited in the package model | **Explicit composition and dependencies** |
| Data verification | TOC and per-file SHA-256 | **CRC-32C reads, SHA-256 and full verification** |
| Rebuild original decrypted package bytes | No equivalent documented in NUP | **Optional embedded rebuild recipes** |
| Import NUP | Native format | **Supported by NXConvert** |

This comparison covers the published NUP specification and reference tools. NUP also supports multiple packages, metadata editing, and SHA-256 verification.

Read the [NUP specification](https://github.com/a55uka/nup/blob/meow/SPEC.md), the [NUP reference CLI](https://github.com/a55uka/nup/blob/meow/README.md), and the [NX format specification](docs/SPEC.md).
