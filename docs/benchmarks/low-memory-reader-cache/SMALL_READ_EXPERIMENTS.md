# Small cached-read optimization experiments

Goal: reduce the overhead of repeated small reads without extra payload caching,
weaker integrity checks, or regressions in other scenarios.

Two approaches were prototyped: copying up to 4 KiB under the existing shared
cache lock to avoid shared ownership/pin atomics, and an API-level shortcut for
small reads wholly contained in a verified cached compressed block. Large reads,
misses and RAW data retained the ordinary path. Neither prototype is retained.

The API-level shortcut passed the reader conformance suite (61 fixtures and
6000 parser mutations) and persistent-session tests, including concurrency,
eviction and corruption rejection. Additional tests exercise cancellation,
invalid file IDs, offset overflow and insufficient output buffers on cached
reads, in both partial and atomic modes. Those regression tests are retained.

## Longer A/B measurements

Nine alternating process repetitions per variant, 128 passes of each trace.
The first pass is excluded. Each value is the median of nine per-process mean
read service times, in nanoseconds. This is a steady-state microbenchmark, not a
comparison of gameplay loading time. Budgets: 128 MiB for compressed scatter,
32 MiB for the remaining cases. OS cache remains uncontrolled.

| Trace | Existing NX | API shortcut prototype | Change in time |
|---|---:|---:|---:|
| Real compressed files · scattered 4 KiB | 203.6 | 196.2 | -3.6% |
| Synthetic RAW · random 4 KiB | 191.5 | 202.2 | +5.6% |
| Synthetic mixed · random 4 KiB | 201.4 | 171.3 | -14.9% |
| Real mixed content · random 4 KiB | 205.5 | 212.7 | +3.5% |
| Real content · sequential 256 KiB | 4789.8 | 4439.9 | -7.3% |

The compressed-scatter gain is small, while RAW and real mixed reads regress in
this run. Sequential results vary substantially between repetitions. Earlier
short campaigns also produced inconsistent changes; a small win in one run is
insufficient to claim an improvement without regression. The cache-only variant
was also inconclusive. **Keep the existing reader.** No new blanket speed or RAM
claim is derived from these experiments.

[Individual repetitions and prototype provenance](small-read-prototype-results.json)
are retained so the rejected approach can be assessed. The published NUP v1/NX
charts remain the earlier measured snapshot; they are not replaced by prototype
numbers. This campaign does not rerun NUP and cannot establish a new NUP ratio.

## Next engineering directions

- Profile the entire small-read call path before changing the cache algorithm.
  Descriptor decoding, block lookup, locks, reference counts and the buffer copy
  all contribute; their relative cost needs sampled profiling.
- Explore an explicit prepared-file read API so integrations can reuse validated
  file/block resolution. Keep any block lease within existing memory limits and
  account for its lifetime during eviction; this is not yet implemented.
- Group caller-submitted reads targeting the same block when such requests are
  available together. This needs an integration/batch API rather than guessing
  arbitrary future random offsets.
- Improve admission/prefetch decisions using the existing session access signals,
  with mixed and concurrent traces as controls. Unconditional prefetch would
  waste RAM and CPU on genuinely random reads.

The NUP cached-file path can copy directly from its whole decoded file. NX's
advantage on first accesses does not guarantee that its block lookup will beat
that path for every already-cached 4 KiB read.

[Rejected prototype patch](rejected-small-read-shortcut.patch) is provided for review only; it is not applied to the SDK.
