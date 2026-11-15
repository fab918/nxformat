# Verification-only RAW cache

Repeated RAW reads can reuse their integrity-check result instead of retaining
another copy of the data. This is an opt-in C++ reader optimization; the NX wire
format, writer and C API are unchanged.

```cpp
nx::OpenOptions options;
options.cache_raw_verification = true;
options.max_raw_verification_bytes = 1024 * 1024;
options.max_cached_block_bytes = 32 * 1024 * 1024;
options.max_cached_blocks = 1024;
// Configure the usual Zstd decoder/context as required by the application.
```

The first read verifies every covered 16 KiB integrity slice before returning
its bytes. Later reads reuse that result only if the source explicitly guarantees
immutable bytes. No RAW payload is stored in the data cache. The bitmap uses one
bit per integrity slice, approximately 8 KiB per GiB, with a separate default
cap of 1 MiB. If the whole bitmap would exceed the cap, verification falls back
to the ordinary per-read path. This also works with a zero data-cache budget.
The existing bounded compressed-block cache continues to cache decoded Zstd
blocks on demand. Its capacity is not reduced by the verification metadata.

`cache_stats().raw_verification_bytes` reports bitmap storage separately from
payload-cache `bytes`. It excludes allocator/control-block overhead. A complete
`verify_all` still checks the physical contents. Cancellation, checksum failures
and concurrent reads remain supported. Archive copies share verification state.

## Immutable sources and optional mapping

`RandomAccessSource::immutable_reads()` defaults to false. Only return true when
the adapter enforces stable bytes for its entire lifetime, such as a version-pinned
remote object or an OS handle that excludes writers. An advisory lock alone is
insufficient. Uncertified sources retain normal per-read verification.

The Windows `DiskSource::open()` already excludes concurrent write handles and
can use verification caching without mapping. `DiskSource::open_mapped(path)`
additionally enables bounded read-only file views with MSVC-compatible Windows
SEH support. Other Windows compilers report unsupported for `open_mapped()` and
can use ordinary `open()`. POSIX `DiskSource::open()`
retains its original behavior and does not certify immutability; `open_mapped`
currently reports unsupported there. Neither optimization is enabled implicitly
for existing integrations.

An adapter may implement `immutable_view(offset, length)` to return a direct
pointer and an owner lease. The bytes must remain immutable and valid while the
lease exists; with an empty owner they must remain valid until source destruction.
Returning an empty view falls back to positional reads and bounded scratch space.
The source must outlive its Archive as before.

Fallible mapped views provide `ImmutableReadView::guard`. The reader executes
mapped CRC and copy operations within that guard. Ordinary stable RAM views leave
it null. An optional `guarded_copy` leaf speeds up already-verified copies
without the generic operation callback. A custom provider must protect its
fallible views and keep operation state/owners outside its SEH frame.
Consumers that access raw pointers themselves
must also use the guard; returning a leased pointer alone does not prevent IO faults.

The Windows adapter catches only read `EXCEPTION_IN_PAGE_ERROR` faults whose
address is inside the view being accessed. Other faults and C++ exceptions are
not swallowed. Both the positional mapped-copy path and the direct RAW CRC/copy
path report IO failure instead of exposing successful output. The failed
range loses its cached verification so a retry checks its CRC again. Default
partial reads scrub the attempted block on an in-page fault and retain previously
completed blocks. Atomic single-block reads
stage fallible mapped copies before committing caller output; this explicit atomic
mode can therefore use an extra request-sized temporary buffer/copy. Multi-block
atomic reads already stage privately. The ordinary nonmapped path is unchanged.
See [Microsoft's mapped-file IO guidance](https://learn.microsoft.com/en-us/windows/win32/memory/reading-and-writing-from-a-file-view).

Windows mapping retains up to 64 address windows, normally 16 MiB each, rather
than mapping the entire archive. Boundary-crossing requests can use a window of
up to 32 MiB. In-flight reader leases may keep evicted views alive. These windows
reserve file-backed address ranges, not 1 GiB of private RAW copies. Leases also
retain the pinned file handle, excluding writers until the final lease is released.
Existing writes through mappings created before opening the source are outside
the immutable-source contract, as are external/device-level modifications.

Mapped pages can still consume resident physical RAM and page tables. Private
commitment and resident working set measure different costs; neither is total
system RAM. See [Microsoft's mapping documentation](https://learn.microsoft.com/en-us/windows/win32/memory/file-mapping)
and the [measured resident/private results](../README.md#performance-nx-vs-nup).
This optimization removes repeated CRC work and private RAW duplicates; it does
not guarantee lower resident memory than every other reader on every workload.

## Validation

The conformance tests cover ordinary and direct-view immutable sources, bitmap
limits, zero payload cache, shared Archive copies, concurrent reads and corruption
without exposing invalid output. Failed mounts release their bitmap; partial
reads preserve the exact verified prefix on corruption, while atomic reads keep
all caller output unchanged. Large positional reads without direct views retain
multi-block IO coalescing, including verification reuse on subsequent reads.
The mapping probe exercises eviction across
70 windows, retained leases, cross-window reads, concurrency and write exclusion
after the source object is destroyed. Windows fault-injection tests cover CRC,
uncached/cached copies, partial/atomic output, retry and exception filtering. The
injector translates faults on private guard pages to the documented in-page
exception; it does not disconnect a real device. The standard parser and persistent-session
tests continue to exercise the default fallback path.

## Optional scheduling hint

`Archive::range_verification_cached(file, offset, length)` reports whether all slices currently have reusable RAW verification or verified Zstd cache entries, without source IO or data retention. It never replaces normal read checks; eviction may change the answer immediately. Batching callers can use it to avoid extra copies for already-cheap large reads. [Reproducible selector](../benchmarks/native_three_way_batch.cpp).
