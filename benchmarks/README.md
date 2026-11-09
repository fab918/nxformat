# Performance measurement

These tools exercise the production Rust writer and C++17 reader with synthetic,
redistributable inputs. Build Rust examples in Release. Reader scripts require
Python 3.14+, a C++17 compiler and a system Zstandard runtime.

## Persistent reader

```bash
python benchmarks/run_session_synthetic.py --quick --output results/session.csv
python benchmarks/run_session_synthetic.py --samples --output results/session-full.csv
python benchmarks/run_reader_cache_benchmark.py --output results/cache.csv
python benchmarks/run_reader_benchmarks.py --output results/reader.csv
```

The persistent harness uses `Session` and pinned positional `DiskSource` reads.
Loose files, RAW NX and Zstandard NX contain identical logical bytes and use
matching access offsets. Manifest SHA-256 and rolling content checksums are
checked before comparing results. NX payload reads verify 16 KiB CRC-32C slices;
Zstandard cache misses decode and check their complete blocks.

Quick mode uses 256 KiB blocks and 1,000 reads per worker. Full mode covers
64/128/256/512/1024 KiB blocks, compressible/incompressible 16 MiB files and five
repetitions by default. Traces include uniform/hot/boundary-crossing 4 KiB reads,
sequential 64 KiB/1 MiB reads and concurrent mixed access. Cache budgets and
prefetch are varied. Reports include P50/P95/P99/P99.9 service latency, decodes,
cache events and physical read volume. `--samples` retains individual reads.

Legacy CSV fields named `verify_read_sha256` and `sha_provider` are retained for
existing consumers. With the current reader they indicate block verification
and `none`/`crc32c`; SHA callback timing columns remain zero. The persistent
harness reports `hash_provider=crc32c`. Historical SHA-provider comparisons do
not describe the current block-read path.

## Writer and reader matrix

```bash
python benchmarks/run_final_performance.py --fixtures /tmp/nx-fixtures --output-dir /tmp/nx-results --repeats 5
python benchmarks/summarize_final_performance.py /tmp/nx-results
```

This runner creates fixtures using `packer/examples/session_perf_cases.rs`,
shuffles/interleaves repetitions, retains read samples and records process CPU,
RSS, allocations, metadata/cache capacities, decoder storage and source bytes.
`--reader` selects an independently built harness, including the actual NXEmu
VFS trace executable. Without it, the production C++17 Session harness is built.

Useful options:

- `--suites extended --profiles mixed`: random 64 KiB/1 MiB reads, sixteen-file
  scatter and synthetic loading traces, with replay and concurrent workers.
- `--workers 1,2,4,8 --worker-levels 3,9`: writer construction sweep; exact
  archive SHA-256 must agree across repetitions and worker counts.
- `--profiles marginal,sample-trap`: small compression gains and adversarial
  sparse sampling. These cases alone do not choose a production policy.
- `--memory-mib 128 --rss-interval-ms 2`: constrained memory and sampled RSS.
  The sampler contributes to process CPU and allocation measurements.

Product Zstandard levels are 1-13. To study 14-18 without changing support:

```bash
python benchmarks/prepare_level_control.py /tmp/nx-level-control
cargo build --release --locked --manifest-path /tmp/nx-level-control/packer/Cargo.toml --example session_perf_cases
python benchmarks/run_final_performance.py --fixtures /tmp/nx-level-fixtures --output-dir /tmp/nx-level-results --writer /tmp/nx-level-control/packer/target/release/examples/session_perf_cases --level-control-manifest /tmp/nx-level-control/EXPERIMENTAL_LEVEL_CONTROL.json --suites levels --levels 3,6,9,12,13,15,18 --repeats 5
```

The control changes the validation range and error text only. Its manifest
records source hashes. Window, memory, deduplication, RAW fallback, verification
and publication policies remain unchanged.

`bench_writer_scenarios`, `run_writer_dedup_comparison.py` and
`run_writer_compression.py` cover deterministic RAW/Zstandard construction,
shared snapshots, small files and changed blocks. `run_repack_benchmarks.py`
measures archive edits. Native conversion/addition drivers accept private local
inputs; retain those sources, detailed paths and result files outside this
repository (`results/` is ignored by git).

## NUP and native-package comparisons

- `nup_layout.py` inspects NUP storage and compressed-file sizes.
- `nup_vs_nx.py GAME.nup GAME.nx nx_read_list [--reads N] [--size BYTES]`
  compares identical NUP/NX ranges using the Python NUP reader and C++ NX reader.
- `native_vs_nx.py GAME.dnsp GAME.nx nx_read_list [--reads N] [--size BYTES]`
  compares native-package and NX ranges.
- `nup_v1_bench.py WORK --nup NUP_TOOL --nxconvert NXCONVERT --reader NX_READ_LIST --seed SEED.nup`
  builds a deterministic synthetic workload from a synthetic NUP executable seed (`NX_NUP_DUMP=SEED.nup cargo test
  --release --manifest-path packer/Cargo.toml --lib dump_synthetic_nup`). It compares streaming and whole-file-cached NUP reads with NX, and measures
  NX add/remove/compact. Keep the work directory outside this repository.

`nx_read_list.cpp` currently uses Windows file APIs. Build it with a C++17
compiler, the `dist/` and `tests/support/` include directories, and the
Zstandard runtime used by `real_zstd.hpp`.
The NUP scripts require the reference NUP Python package and its dependencies;
these are separate from the dependency-free NX conformance scripts.

The NUP comparison mixes Python and C++ implementations; a native comparison
and larger repeated samples are needed for general speedup claims.
NUP whole-file caching can make repeat reads faster at a larger memory cost.

## Integrity and tail latency

The standalone integrity benchmark compares serial CRC slices with the writer's
interleaved implementation and checks identical results before timing:

```bash
rustc -O --edition=2021 benchmarks/integrity_benchmark.rs -o /tmp/nx-integrity
/tmp/nx-integrity 2048 > /tmp/nx-integrity.csv
```

The argument is MiB per timed sample. Five repetitions cover 16-1024 KiB blocks;
serial/interleaved order alternates. Run on an idle machine. A CRC microbenchmark
speedup does not imply the same gain for packing or gameplay.

`run_tail_performance.py` compares two reader executables on existing writer
fixtures. `run_captured_performance.py` replays captured access profiles;
`run_adaptive_performance.py` compares fixed and profile-guided block selection.
The [profile protocol](../docs/READER.md#access-profile-protocol) binds observations
to exact source files and snapshots. Detailed diagnostic clocks require
`-DNX_READER_DIAGNOSTICS` and `NX_BENCH_TIMING=1`; normal builds omit them.

## Interpretation

Percentiles describe individual I/O service times. They are not FPS, frame times
or measured stutter reduction. A 300-read P99 depends on only a few tail samples;
use at least 10,000 random reads, multiple seeds and repeated runs for public
comparisons. Separate compressed/uncompressed content and match cache and
verification policies across implementations.

The harnesses warm or leave uncontrolled the operating-system cache. Labels
such as cold/replay refer to application caches and do not establish cold-storage
performance. CPU and wall throughput include correctness hashing outside each
timed read. Allocation counters omit provider/C allocations and allocator
bookkeeping; writer accounting is an estimate, not process RSS. Report these
limits alongside measurements. Do not pool per-repetition percentiles.

The README's NUP comparison charts summarize separate local runs. Their original individual read samples are not distributed; the comparison
tools and separate settings-run reports listed above are now included. They compare the Python NUP
reference reader using libzstd with the C++ NX reader; the results are specific
to those implementations and workloads. A native-to-native comparison and a
larger, representative sample are needed for broader claims.

## Real-content acceptance

Keep original decrypted sources, detailed paths, content IDs and private samples
outside source control, build trees and CI. Use read-only input copies and record
source hashes before/after testing. Keep outputs and work files separately.

For both Switch generations, validate at least one base with two updates and DLC,
a multi-Program title where available, and localized native metadata:

1. Record validated native platform, application/Program IDs, content versions,
   exact dependencies, content IDs and full Build IDs. Unknown values stay absent.
2. Compare resolved snapshot paths, sizes and every file byte against independent
   extraction. Check deleted files, unresolved partial-update rejection and native
   patch/base associations. Verify original icon bytes and version-specific NACP.
3. Add updates/DLC, remove the selected display version, then compact and fully
   verify. Confirm retained identities, metadata origins and dependencies.
4. Exercise a real emulator boot and loading sequence through the integrated
   reader. Archive validation and loader/parser acceptance do not prove execution.
5. Measure repeated random/sequential reads, open/mount, packing, full verification,
   CPU, RSS, allocations, temporary-disk peak, archive ratio and physical I/O.
   Record tool revisions, hardware, filesystem, Zstandard version and cache policy.

Synthetic fixtures establish the tested container behavior, not general Switch
or emulator compatibility. Record results against this matrix before broad
compatibility or gameplay-performance claims.
