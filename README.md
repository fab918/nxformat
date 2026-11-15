<p align="center">
  <img src="docs/nxformat.png" alt="NXFormat" width="900">
</p>

<p align="center">
  <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/github/license/fab918/nxformat" height="20" align="top"></a>
  <a href="https://github.com/fab918/nxformat/actions/workflows/ci.yml"><img alt="Tests and conformance" src="https://github.com/fab918/nxformat/actions/workflows/ci.yml/badge.svg?branch=main" height="20" align="top"></a>
  <a href="https://github.com/fab918/nxformat/releases/tag/V1.0.4"><img alt="NXFormat v1.0.4" src="https://img.shields.io/badge/release-v1.0.4-blue" height="20" align="top"></a>
  <img alt="Switch 1 and Switch 2" src="https://img.shields.io/badge/Switch-1%20%26%202-337ab7" height="20" align="top">
  <img alt="C++17 reader" src="https://img.shields.io/badge/reader-C%2B%2B17-00599C" height="20" align="top">
</p>

<h2 align="center">Store once. Read fast. Update without starting over.</h2>

<p align="center">
  <strong>NX is a modern archive format for decrypted Nintendo Switch and Switch 2 game dumps.</strong><br>
  It is built for fast reads, smart storage, and easy management of updates and DLC.
</p>

<p align="center">
  <a href="https://github.com/fab918/nxformat/releases/tag/V1.0.4"><strong>Download NXConvert 1.0.4</strong></a>
  &nbsp;·&nbsp;
  <a href="#performance-nx-vs-nup">NX vs NUP</a>
  &nbsp;·&nbsp;
  <a href="#quick-start">Quick start</a>
  &nbsp;·&nbsp;
  <a href="docs/SPEC.md">Format specification</a>
</p>

- 🚀 **Fast reads with bounded caches.** Independent compressed blocks and verification-only RAW caching keep repeated reads fast without retaining RAW payload copies. [Native reader comparison](#performance-nx-vs-nup).
- ✏️ **Updates and DLC, in place.** Add or remove content without copying the base game during normal edits.
- 🧩 **Store shared content once.** Reuse identical blocks across game files, updates and DLC.
- 🎮 **Switch 1 and Switch 2.** Convert supported decrypted **DNSP/DXCI** packages, extracted folders and NUP archives.
- 📦 **Get your original files back.** Native package conversions try to keep a recipe to recreate the original bytes (on by default, `--no-recipe` to skip it). Rebuilding needs all referenced content. [Learn more](docs/RECIPE.md).
- 🛡️ **Spot damaged game files.** Built-in integrity checks detect corruption during reads or a full archive check.
- 🛠️ **Emulator ready.** C++17 reader, Rust library and C API for Windows, Linux, macOS and Android (libraries). [Integration](#developer-integration).

> **Legal note**
> <sub>**NXConvert does not decrypt anything.** It only repackages game files you have already decrypted yourself, from games you own. It contains no keys and no copyrighted content.</sub>

## Quick start

**NXConvert 1.0.4** writes NX archives with separate header pages.
The existing NX wire formats are unchanged.
Download the [V1.0.4 release](https://github.com/fab918/nxformat/releases/tag/V1.0.4),
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

NX reads independent blocks and reuses integrity checks for immutable data.
This keeps small reads fast without caching whole decoded files.

The comparison below uses **native C++ readers**, the same compiler, Zstd library
and mapped input provider. **NX** enables verification reuse and the explicit
selective-batching prototype.
Each reader receives the same **16 already-pending requests**, with no queue-fill
wait. Values are medians of five runs on the same real archive.

![Read performance: NUP v1 and NX](docs/benchmarks/native-read-performance.png)

**Read time in milliseconds — lower is better.**

| Workload | Cache budget | NUP v1 | NX |
|---|---:|---:|---:|
| 8 first accesses · 4 KiB | 128 MiB | 86.129 | **0.758** |
| 512 scattered reads · all files cached | 128 MiB | **0.081** | 0.161 |
| Sequential 32 MiB · first pass | 32 MiB | 26.989 | **9.418** |
| Sequential 32 MiB · repeated pass | 32 MiB | 2.663 | **1.788** |

NUP v1 wins the cached scattered-read trace, while NX uses much less memory.
The targeted first-access case is about **114× faster than NUP v1**. It includes
storage-policy differences: five requested NX starting blocks are RAW and three
are Zstd, while all eight NUP files are compressed.

![Memory usage: NUP v1 and NX](docs/benchmarks/native-read-memory.png)

**Peak resident / private memory in MiB — same workloads, lower is better.**

| Workload | NUP v1 | NX |
|---|---:|---:|
| 8 first accesses | 222.3 / 134.5 | 33.9 / 17.6 |
| 512 scattered reads · all files cached | 222.3 / 134.6 | 54.8 / 26.5 |
| Sequential 32 MiB · first pass | 128.8 / 83.3 | 98.1 / 50.3 |
| Sequential 32 MiB · repeated pass | 113.2 / 67.6 | 97.9 / 50.0 |

On the cached scattered-read trace, **NUP v1 uses about 4.1× the resident memory**
of NX. NX retains no additional RAW payload cache.

These are generated access patterns on real content, not recorded gameplay or
FPS measurements. The OS cache was not flushed. Memory includes common benchmark
output buffers, metadata and decoder scratch; private commitment is not physical
RAM, and resident memory includes mapped file pages. Selective batching requires
an integration with pending requests; it is not automatically used by every reader.

[Method and reproduction tools](benchmarks/README.md#native-nup-v1-comparison)
cover generated RAW/mixed controls, optional local real inputs, cache budgets,
resident/private memory and temporary-file usage. Detailed run outputs stay local
under the ignored `results/` directory.

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

For xmake, download `nxformat-xmake-1.0.4.zip` from the release and extract it.
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

- [RELEASE.md](docs/RELEASE.md): V1.0.4 release notes and platform packages.
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
