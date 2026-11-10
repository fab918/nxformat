# Portable C++17 reader

Applies to **NXFormat / NXConvert 1.0.2**.

For persistent emulator use, see the runtime contract below. The optional
`nx_session.hpp` and `nx_disk_source.hpp` companions add owned positional IO,
shared bounded LRU, concurrent miss coalescing and sequential asynchronous
prefetch. They reuse the core reader below and change no wire format.

The implementation is in [reader/nx_reader.hpp](../reader/nx_reader.hpp). Generate the copyable distributable with `python reader/generate_header.py`; `dist/nx_reader.hpp` is the exact same file. The header uses only C++17 standard-library headers and does not require Rust, Cargo, an emulator runtime, or a mandatory global initializer. The optional CMake target `nxcore::reader` exposes the same single implementation. The reader consumes only source-independent `.nx` archives. DNSP/DXCI/DNCA import and conversion belong to the separate Rust engine used by the NXConvert GUI and CLI. There is no CXX bridge or C++20 aggregate. The reader does not decrypt or manage keys.

## API

- `nx::RandomAccessSource` is a caller-owned `size()` / `read_at()` interface. File, memory-map, and VFS adapters can implement it.
- `nx::Archive::open(source, options)` validates the 256-byte header and bounded Fast Catalog, including their digests, strings, media indexes, application/program/snapshot associations, and the per-snapshot metadata-history extension. When both slots have different generations and valid header CRCs, open also streams the Manifest and index digests before returning a Catalog, so it can select the older generation if the newest metadata is damaged.
- `Archive::applications()`, `programs()`, `snapshots()`, `localized_entries()`, `title()`, `publisher()`, `localized_language()`, and `localized_title()` expose discovery metadata immediately after `open()`.
- `Archive::read_media(index)` fetches one JPEG, PNG, NACP, NPDM, or opaque item on demand, validates its media type against Catalog ownership even before mount, and verifies its stored SHA-256.
- `Archive::mount()` validates the Manifest, `index_sha256`, Media descriptors and references, block descriptors, file references, tree topology, complete snapshot roots, and canonical snapshot identities. It does not decompress game blocks. A failed mount discards and releases all partially built Manifest/index state; the same Archive can be mounted again after a transient source I/O failure.
- `Archive::source_resources()` lists Catalog descriptors. After mount, `Archive::source_resource_file(index, name)` resolves a file in a resource (`not_found` if the resource has no such file, `bounds` for a bad index). Read it using the returned `FileId`.
- `Archive::root_directory(snapshot)` obtains a VFS root; `Archive::enumerate(directory, callback)` visits files and subdirectories without allocating in the reader. Directory entry names are `string_view`s into mounted Manifest storage.
- `Archive::lookup(directory, name)` finds one child entry by binary search over the directory's sorted names (O(log n)), for VFS `GetFile` / `GetSubdirectory` without enumerating the directory.
- `Archive::resolve_file(snapshot, path)` performs path lookup once and returns `FileInfo` with a `FileId`, type, logical size, and chunk size. `Archive::file_info(id)` obtains the same metadata for a previously resolved file.
- `Archive::read_file(FileId, offset, length, output, capacity)` performs a random read without repeating path lookup. The path overload is also available for one-shot reads. The snapshot is explicit; the reader never guesses an update chain. It returns a `PartialRead`: a `Status` (so `!read_file(...)`, `.error` and storing it in a `Status` work as in 1.0.0) plus `bytes`. It succeeds only when `bytes == length`. Each touched block (or 16 KiB RAW slice) is verified before any of its bytes reach the caller's buffer. By default blocks are committed one by one straight into the buffer, without a staging copy; after a failure `output[0, bytes)` holds verified bytes in file order and the rest is untouched or, where a RAW run was read straight into the buffer, zeroed. Unverified bytes are never exposed. `OpenOptions::atomic_reads = true` restores the 1.0.0 behaviour: a multi-block read is staged in a private buffer, the output is unchanged on any failure and `bytes` is 0. With verification disabled, an unverified RAW single-block slice reads only the requested data bytes into scratch before copying.
- `Archive::verify_block(id)` verifies one block; `Archive::verify_all()` checks media SHA-256 and block slice CRC-32C values, then reconstructs every file SHA-256.

`Result<T>` carries either a value or an `ErrorCode` and message. Catalog title/language views and directory entry names refer to bytes owned by the archive and remain valid until its destruction. `FileId`, `DirectoryHandle`, and `FileInfo.id` are numeric handles scoped to the mounted `Archive`; do not reuse them with another archive or after destruction. The enumeration callback returns `true` to continue and `false` to stop. Enumeration itself allocates no memory; callback work may allocate. The source object and its backing storage must outlive the archive and all backing bytes MUST remain immutable from open() until destruction. File adapters must prevent concurrent modification; remote adapters must pin an object generation. Cached reads may return the earlier verified bytes if callers violate this contract; full verification bypasses the decoded cache. Capturing indexes strengthens verification but does not turn NX into an authenticated format. After a successful mount, concurrent reads are safe only when the source adapter, Zstandard callback/context, and optional `BlockSha256` callback/context are thread-safe. The reader does not serialize access to caller-owned objects. Block reads are verified with per-slice CRC-32C (SSE4.2 or ARMv8 CRC selected at run time, slicing-by-8 otherwise); the `BlockSha256` callback is kept for source compatibility but is no longer called on reads. A RAW read fetches and checks only the 16 KiB slices that cover it; a Zstandard read decodes the whole frame and checks all its slices. Platform-native callback adapters are optional caller/test integrations and are not linked by `dist/nx_reader.hpp`.

## Memory and decoding

The reader loads the Catalog (maximum 16 MiB) and Manifest into bounded owned storage. The metadata-history extension adds no I/O beyond the Catalog read; its row counts are bounded by Catalog size. Mount captures the 24-byte hot index and 4-byte slice CRC-32C entries into bounded owned storage and verifies their combined `index_sha256` before parsing. Reads use these captured descriptors/digests; replacing payload and digest on the source after mount cannot bypass block verification. Default `max_index_bytes` is 256 MiB. This deliberately trades index memory for stable integrity and avoids a second source scan and tiny descriptor/digest I/O on every request. Default mount budgets are 128 MiB for the Manifest, 250,000 directories, 500,000 files, 1 million entries, 4 million block references and 4 million blocks. Snapshot identity construction is capped at 1 million nodes and 64 MiB of combined path bytes. Callers can lower these budgets for an emulator's mount policy. The maximum requested read buffer defaults to 16 MiB. Verified single-block reads use one decoded block buffer (up to 1 MiB), plus a stored-frame buffer for Zstandard (up to 1 MiB + 64 KiB); they avoid a separate result-sized staging allocation. An unverified RAW single-block slice uses a requested-length scratch buffer to preserve output if the source reports failure after a partial write; a full RAW block goes directly into its block buffer. Atomic multi-block reads stage up to the requested output size (16 MiB by default), reuse one decoded buffer, and leave caller output unchanged after any block failure. Partial reads need no staging buffer. RAW blocks stored back to back are fetched with one source read of at most 1 MiB, then verified slice by slice and committed in file order. Runs of whole 16 KiB slices are read straight into the output (or staging) buffer, with no scratch buffer or copy; unaligned edges use a scratch buffer of at most one block. If that read fails the reader retries block by block, so a partial read still reports an exact prefix.

Zstandard is supplied through `OpenOptions::zstd`. The callback must enforce the NX one-frame rules: standard frame only, no dictionary ID, declared content size equal to the expected output, at most a 1 MiB window, no skippable or concatenated frames, exact input consumption, and output no larger than 1 MiB. The reader checks the callback's success, consumed bytes, and output length; it cannot audit the internals of a caller's decoder. Decoder workspace is the caller's responsibility.

## Validation and testing

The reader is exercised against every deterministic fixture by `python tests/conformance/test_reader.py`. That test compiles C++17, mounts valid archives, checks expected open/mount/full-verification failures, exercises one-block RAW/Zstandard and multi-block reads, checks malformed frames, verifies corrupt blocks before output is exposed, injects mount and partial-write I/O failures, changes the source header after `open()` to confirm `mount()` uses the captured Manifest digest, checks direct RAW slice byte counts, validates mixed RAW/Zstandard reads and the optional synchronized SHA callback, rejects incompatible media and metadata-history associations, and performs deterministic parser mutations. It also checks the generated header is synchronized. CMake has a C++17 reader smoke target. `tests/fuzz/nx_reader_fuzz.cpp` is a libFuzzer entry point for coverage-guided runs with AddressSanitizer and UndefinedBehaviorSanitizer.

Structural mount verifies the block-table checksum before trusting either table, but it does not prove game-data integrity until blocks are read or `verify_all()` runs. Full verification checks each physical block, including unreferenced blocks, and each file's logical digest. The header checksums do not authenticate publisher origin.

## Reproducible synthetic benchmark

Run `python benchmarks/run_reader_benchmarks.py --output benchmarks/results/reader_warm_cache.csv` with Python 3.14+, a C++17 compiler, and a system Zstandard runtime. The generator creates 20 deterministic archives with one 16 MiB file, two data profiles, RAW/Zstandard, and 64/128/256/512/1024 KiB blocks. It measures Catalog open/title access, icon reads, mount, random 4 KiB/64 KiB/1 MiB reads, sequential 1 MiB reads, path versus resolved-ID access, and reads with verification disabled or slice CRC-32C enabled. CSV rows include p50/p95/p99 latency, throughput, allocation count, process peak working set, archive size, Data-region bytes read, and median time inside the Zstandard and SHA callbacks. Raw reader samples include per-request byte volume and phase times. Legacy SHA callback timing columns remain zero for block reads in the current format. The optional legacy SHA adapters use CNG, OpenSSL EVP or CommonCrypto. They add no mandatory link dependency and are not used by current block reads.

The benchmark pre-reads each archive to warm the operating-system file cache. It does not reliably clear caches or identify the storage device, so it reports warm file-cache results and makes no cold-cache or SSD claim. Companion sample CSVs preserve each measured latency and deterministic read offsets and lengths. Synthetic data does not establish performance on representative Switch archives; the default block policy still requires validation on representative workloads. See [the real-content testing procedure](../benchmarks/README.md#real-content-acceptance); generated CSVs are written to the selected output directory.

## Review corrections: bounded work and optional cache

`OpenOptions::max_total_snapshot_nodes` defaults to 16 million node visits across all snapshots, and `max_total_snapshot_path_bytes` to 512 MiB of cumulative paths. Per-snapshot limits still apply. Shared trees count each traversal. Exceeding any budget returns resource_limit. Optional `is_cancelled(context)` returns true to stop index capture, tree traversal or block loading with ErrorCode::cancelled; the callback/context must support concurrent use if reads do. Failed/cancelled mount clears captured indexes and partial state and can be retried. Standard-library sorts and a caller's blocking read/decoder callback cannot be interrupted mid-call.

A verified decoded-block LRU cache is opt-in with `max_cached_block_bytes` (default zero; `nx::Session` defaults to 64 MiB). Only Zstandard blocks are cached: RAW slices are re-read from the OS cache. `max_cached_blocks` (default 128) supplies an additional entry-count bound. For example, set 8 MiB and 128 entries for repeated small reads. Payload storage and entry count are both bounded; metadata, caller output, per-call decode/copy buffers and decoder workspace are additional. It is scoped to an Archive and synchronized across concurrent reads. The index is a hash map. Cached blocks are shared, reference-counted buffers: a hit pins the block under the cache lock and copies the requested slice after releasing it, so eviction never frees bytes still being copied and readers do not serialize on the copy. An evicted buffer is reused for a later miss only when no reader holds or is copying it (a per-block pin count, released after each copy and read with acquire ordering). Only successfully verified blocks enter it; unverified reads do not use or populate it. verify_block()/verify_all() bypass it so physical payload verification is never replaced by a cache hit. Cache hits copy only the requested slice of verified bytes, without another source read, decode, CRC or full-block copy. This is an implementation option, not a wire-format change.

Snapshot::npdm_index and Snapshot::required_firmware expose version-specific execution metadata from required extension 0x00010005. Program::npdm_index is a single-snapshot field; new writers emit the snapshot extension. A Program NPDM with multiple snapshots is rejected as ambiguous.

## Persistent session runtime

`dist/nx_session.hpp` owns one source and one mounted `Archive`. Catalog,
Manifest, file references and authenticated block descriptors stay resident.
Resolve `FileId` once, then use `Session::read()`, which has the `Archive::read_file()` contract. The optional companion
`nx_disk_source.hpp` pins an OS handle and implements positional IO: overlapped
offset reads on Windows, `pread` on POSIX. Rename/replacement does not retarget
an existing session. Windows denies concurrent writers; POSIX locking is
advisory, so other writers must honor the immutable-source contract.
The Rust `read_snapshot_file_range()` validation helper is not used by this
runtime path; its per-call archive parsing behavior is unchanged.

Default `SessionOptions`:

| Setting | Default | Meaning |
|---|---:|---|
| `reader.max_cached_block_bytes` | 64 MiB | Verified decoded payload budget |
| `reader.max_cached_blocks` | 1,024 | Additional bound on LRU entries |
| `reader.max_read_bytes` | 4 MiB | Largest range accepted by one call |
| `reader.atomic_reads` | false | `read()` keeps caller output unchanged on failure (staged); default commits verified blocks directly |
| `parallel_reads` | 8 | Bound on concurrent scratch/decoder work, maximum 64 |
| `sequential_prefetch` | true | Warms the next Zstandard block of a sequential handle; RAW blocks are never warmed |
| `split_reads` | true | A multi-block Zstandard read hands its later blocks to the decode workers, while concurrent readers do not outnumber them |
| `decode_workers` | 0 (auto) | Background decode threads; auto is min(4, hardware threads / 2), at least 1; maximum 16 |
| `sequential_reads` | 3 | Adjacent successful reads before prediction |
| `lookahead_blocks` | 1 | Maximum 2 |
| `prefetch_queue` | 128 | Maximum 256 jobs |

These are cache/work limits, **not a total process RSS limit**. Mounted metadata,
map/list entries, caller buffers, decoder workspace and active range buffers
are additional. Existing mount budgets remain enforced. The optional
`nxcore::runtime` CMake target is enabled with `NX_ENABLE_RUNTIME_READER=ON` and
links the existing reader plus platform threads. The core reader remains usable
without this companion or a runtime initializer.

`nx_native_sha256.hpp` supplies optional legacy SHA callback adapters; current
block reads use CRC-32C and do not invoke these callbacks.

The LRU is shared by all files and snapshots of the same archive. Its key is the
archive-local physical block ID. List splicing and eviction avoid scanning the
whole cache. Thirty-two fixed miss-lock stripes prevent simultaneous decoding
of the same block without an unbounded map of synchronization objects. IO,
decompression and hashing run outside the LRU mutex; different stripes decode
in parallel. Stripe collisions can serialize unrelated misses. A waiter reuses
a verified resident result; eviction or a budget smaller than a block can still
require another, sequential decode. Full verification deliberately bypasses the
cache and is not coalesced with demand reads.

Each open virtual file owns a `ReadCursor`. Prediction follows adjacent ranges,
not extensions. The same next-block prediction is submitted only once per
sequential zone. A seek changes its generation: stale queued work performs no
payload IO; already running work may finish one bounded block and populate the
verified cache. Uncacheable and RAW blocks are not prefetched. A read that spans
several uncached Zstandard blocks keeps the first 1/(W+1) of its blocks, W
being the decode worker count, and queues the rest, last block first, ahead of
speculative jobs; the workers decode them in parallel while the caller decodes
from the start. A block a worker is decoding makes the caller wait for it
rather than decode it twice. Those jobs take a demand slot, since they finish
an admitted read. Speculative work reserves a demand slot when the configured
parallelism exceeds one. With parallelism one,
background work necessarily shares that slot. Disable prefetch for workloads
where this contention is undesirable. Destruction stops and joins the workers;
there are no detached tasks.
Queued foreground readers take admission priority over new speculative work.
Admission takes a slot with one atomic compare-exchange; the gate mutex and
condition variable are used only when every slot is busy.

Windows `DiskSource` reuses a source-owned pool of up to eight read events;
concurrency above that uses scoped overflow events. NXEmu uses a source-owned,
lazy Zstandard context pool bounded by `parallel_reads` (maximum 64 contexts).
Neither implementation retains destructible state in caller-thread TLS.
Resources are freed with the source after sessions and in-flight calls finish,
even when caller threads outlive unloading of the loader module. This fixes the
DLL-unload leak found in the earlier thread-local implementation.

RAW reads allocate no compressed-frame buffer and invoke no decoder. Cached
reads copy only the requested slice, never the entire block. A verified RAW
read fetches and checks only the 16 KiB slices covering the requested bytes. No mmap or
unverified direct-read shortcut was added to the emulator path.

By default `Session::read()` writes verified blocks straight into the caller's
buffer and returns the exact verified byte count in `bytes`, which a VFS `Read`
can return as is: it succeeds only when every byte verified, and after a
failure the buffer may hold a verified prefix (never unverified bytes). Set
`reader.atomic_reads` to keep the buffer unchanged on corruption, IO failure
and cancellation, at the cost of one staging buffer and copy per multi-block
call. An NXEmu VFS request larger than 4 MiB is split
into bounded calls.

`Session::stats()` exposes hits, misses, coalesced waits, evictions, decoded
payload bytes/entries/peak, RAW reads, compressed decode count/time, stored bytes
read, and prefetch submitted/completed/cancelled/failed/dropped. A concurrent
miss later coalesced is counted both as a miss and a subsequent hit; those
counters are events, not mutually exclusive request classifications.

Source, cancellation, SHA and decoder callbacks must be thread-safe, remain
alive and not throw. Zstandard must consume exactly one independent frame.
`Session` rejects disabling block verification. `stop()` cancels queued and
subsequent demand work and checks cancellation after in-flight callbacks return;
it cannot forcibly interrupt an OS read or external decoder callback.

## NXEmu integration

`vfs_nx.cpp` supplies read-only `VfsFile`/`VfsDirectory` objects. Handles retain
the session, names enumerate from the mounted Manifest, and reads use resolved
numeric IDs. Host loading uses a pinned `DiskSource`; generic immutable
`VirtualFile` input is also supported with serialized source IO. Format
detection may already hold the frontend's original VFS handle; the session pins
its own persistent handle once, and never reopens it per range.

`AppLoader_NX` is an archive-selection adapter to the existing native path:
existing CNMT/NCA readers, the existing NCA filesystem driver (including BKTR),
the existing deconstructed-directory NSO/NPDM loader, and RomFS factory. It does
not implement another NSO loader, native patch interpreter or extractor.

Selection has independent application ID, exact snapshot identity/version and
Program ID arguments. A zero Program ID means the unique primary Program.
The ordinary host route chooses its newest non-DLC version. A nonzero native
program index resolves the CNMT ID offset and actual NCA owner, never
`primary_id + index`. Conflicting explicit Program/snapshot/index selections
fail. Every applicable native Program must have an exact corresponding Catalog
snapshot and dependency closure; all associated main Build IDs and NPDM Program
IDs are checked, including Programs other than the one being launched.
Process preparation uses this snapshot's numeric version and Host storage
selection, rather than versions/locations inferred from installed update IDs.

Catalog display title/icon use the independently selected display snapshot.
Process NACP uses the selected version's own Control NCA, or the unchanged
Control inherited through its exact native Base dependency when the update has
no replacement. `ControlSnapshot()` identifies that actual origin. This runtime
inheritance does not create display metadata for an otherwise metadata-less
snapshot, and never borrows another update's Control. Display selection never
chooses an executable. Native file sets remain intact behind the VFS.
The displayed version string uses the selected snapshot's own Catalog value;
it does not present an inherited Base NACP version as new update metadata.

The selected view implements the existing ContentProvider interface. It exposes
base/inherited/replaced Programs and other native assets, pairing patch NCAs
with exact base records by content type/ID offset (Control also uses its native
owner). Existing NCA code performs the composition. Ambiguous associations fail.
Required snapshot identities must resolve inside the selected application;
cycles and missing bases fail. DLC selection obeys its native required version.
Installed external versions cannot shadow owned selected content, or make an
ineligible archived DLC available. Unrelated system content still uses the
normal provider. `ContentProvider::OwnsTitle()` also prevents explicit storage
selection and legacy patch calls from substituting NAND/SD content for a scoped
NX title. The RomFS factory retains the selected provider's lifetime.

The existing NPDM reader now bounds its input/subtables before allocating KAC
storage, checks META/ACI0/ACID and the permitted Program range, and uses offsets
relative to the enclosing ACI0/ACID for file permissions. The synthetic builder
now writes its SDK add-on version at the correct u32 header offset. Loader tests
use artificial NPDM bytes; fabricated NSOs contain no executable game code.

## Emulator integration patch

The optional [NXEmu patch](../integration/nxemu-reader.patch) contains the VFS,
loader adapter, reader headers and standalone validation harnesses. Check it
against the intended NXEmu revision before applying:

```text
git apply --check /path/to/nxemu-reader.patch
git apply /path/to/nxemu-reader.patch
```

Regenerate from an integrated NXEmu checkout with
`python tools/export_nxemu_integration.py --nxemu /path/to/nxemu`. The exporter
checks all five vendored headers against `dist/` and validates the patch using a
disposable Git index. Retain the [MIT license](../LICENSE) when redistributing.

Generate standalone inputs with the `runtime_cases` and `runtime_native_cases`
Rust examples. The patch includes `build_native_test.py`, `build_trace_posix.py`
and the DLL lifecycle test. The native harness checks VFS bytes, dependency and
Program selection, decoder recovery and module teardown. These checks do not
launch a game. Process loading and real gameplay require separate acceptance
tests; see [benchmarks](../benchmarks/README.md#real-content-acceptance).

## Access profile protocol

This is an external performance-input protocol, not an NX wire extension,
SnapshotMetadata, native provenance or executable compatibility evidence.
The normal native loader and persistent NX VFS remain the loading path.

### Capture

Set `NXEMU_VFS_TRACE_DIR` to an existing private directory **before starting**
NXEmu. Each mounted NX source creates a uniquely named `nxread-*.tsv` there.
Capture finalizes automatically after 30 seconds; the game can remain open.
The frontend may make a separate metadata-only mount: inspect the traces and
select the execution session explicitly. No game payload bytes are recorded.

The hook surrounds successful logical `NxFile::Read` calls, including a
successfully completed prefix if a later segment fails. Rejected/zero-byte
reads do not become evidence. Registration records the exact Application,
Program, snapshot version/type, content ID, Build ID, canonical snapshot
identity, logical path/length and Manifest SHA-256. Reads record requested-start
order, per-handle ID, per-thread ID, completed offset/size and wall latency.
The Manifest is authenticated by the existing mount; the capture itself is
untrusted performance input. The writer verifies its bindings against sources.

Disabled: one environment lookup at mount, two null-pointer checks per read;
no capture clock, queue, thread, file, payload hash or allocation on the read
path. Enabled: one session-owned writer thread, a 4096-event ring, at most
512 registered paths of 4096 bytes, 16384 thread IDs and 100000 accepted reads.
Queue overflow, metadata/registration failure and event-limit losses invalidate
the profile. Logging never changes successful file bytes. A deadline/close
drains the ring, writes a footer and joins before module unload. A killed
process or failed output usually lacks a complete footer and must be rejected.
Only the NX-backed VFS is instrumented: a live DNSP/DXCI loader capture is not
implemented. Native content is the positional-read replay reference below.

### NXREAD1 TSV

ASCII fields, decimal unsigned integers except explicitly hexadecimal fields.
Paths are UTF-8 encoded as hexadecimal bytes; no quoting or path inference.

```
NXREAD1
F  file_id platform app_hex program_hex_or_- type version content_hex build_hex identity_hex size file_sha256_hex path_utf8_hex
R  sequence file_id handle_id thread_id offset completed_bytes start_ns latency_ns
Z  accepted_records dropped_records
```

Separators above are tabs. IDs in the Application/Program fields are 16 hex
digits, content IDs 32, Build IDs/identities/file digests 64. Timestamps use the
same process's monotonic nanosecond clock. Completion order may differ across
threads; the sequence is assigned before the read. Profiles count contiguous
accesses independently for each `(file, handle, thread)` stream, so interleaved
sequential readers cannot manufacture random-access evidence.

The Rust parser streams at most 64 MiB, bounds a line to 16384 bytes, files to
4096, streams to 16384 and read records to 100000. It rejects malformed paths,
duplicate sequences/files, unknown file IDs, overflowing/out-of-range accesses,
unordered per-stream events, trailing data and missing/inconsistent/lossy
footers. The parser is bounded but does not interrupt a caller-provided `Read`
that itself blocks. Do not treat a hash or declared identity as native proof.

### Writer and native conversion

Use `CapturedReadProfiles::read`, then `attach(input, true)`, or supply a
`CapturedFileProfile` through `FileInput::with_captured_profile`. The writer
checks scope/path/size, Build ID, streamed SHA-256 and recomputed canonical
snapshot identity before publication. Native orchestration uses
`convert_inventory_profiled`; every supplied profile must match one of its
staged native snapshots. An incompatible version/file is an error, not a hint
silently assigned to another version. Existing transactional publication,
resource ceilings, cancellation, block comparison and output verification apply.

Private conversion example (benchmark/conformance driver, not a new CLI):

```powershell
$env:NX_BENCH_ADAPTIVE = '1'
$env:NX_BENCH_READ_PROFILE = 'D:\private\nxread-execution.tsv'
cargo run --release --manifest-path packer/Cargo.toml --example native_convert -- --convert-only D:\private\game.nx D:\private\game.dxci
```

Unknown or insufficient observations retain min(256 KiB, configured bound),
including short metadata files. A short physical tail remains exactly its
original length. Selecting 64 KiB still requires 10000 successful observations,
no read above 64 KiB and at most 1% contiguous transitions. This rule is an
explicit conservative policy, not an assertion about filename extensions.

Index-budget selection ranks candidates by estimated decompression bytes saved
per extra index byte, with a stable key tie-break. Greedy selection skips a
candidate that does not fit and tries the next; no knapsack solver. Charges
count all occurrences conservatively at 96 bytes per extra reference/block.
Eligible files with the same captured digest/length share a selection group;
this only groups partition choices. Source bytes are still checked and exact
deduplication comparisons remain unchanged. Unprofiled/ineligible versions
keep their conservative partition and may share fewer blocks with a differently
partitioned version; canonical identities and byte correctness are unaffected.
Planning keys are fixed size; candidate storage and traversal depth are charged
to writer accounting. There is no archive-wide all-or-nothing fallback.

### Held-out replay

The existing `session_trace_benchmark` has a `--captured` mode. It resolves all
captured identities, Program IDs, Build IDs, paths, lengths and digests against
the archive before timing. Reads use the actual NXEmu VFS when linked to the
loader; the native reference reads the corresponding NCA ranges in the original
DNSP/DXCI using the existing expected manifest. There is no full extraction.
Large requests are streamed with a 4 MiB buffer; reported logical latency sums
those transfer times. A SHA-256 over returned bytes must agree across variants.
The checksum is outside per-read latency, included in CPU/throughput.

`run_captured_performance.py` performs three shuffled repetitions of native,
Fixed 256 KiB, Adaptive, Fixed 64 KiB and capture-enabled Fixed 256 KiB, each
with a fresh NX cache and an immediate replay. The NX LRU is 64 MiB and prefetch
is off. OS caches are uncontrolled; “cold” never means a cold physical disk.
This measures serial I/O replay, not game frametimes or the emulation thread.
Trace timestamps/thread identities are retained; replay serializes request-start
order rather than claiming to reproduce the original thread scheduling.
