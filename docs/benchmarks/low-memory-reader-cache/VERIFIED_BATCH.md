# Verification-aware batching: warm sequential regression resolved

The locality-only prototype grouped large sequential reads even when verification
was already reusable. Its extra merge/scatter copy could then cost more than it
saved. The selector now queries `Archive::range_verification_cached()` for large
candidate requests. If every request in the pending window is already verified
(RAW bitmap) or decoded and verified (Zstd cache), it skips sorting and merging.
The ordinary reads still execute all normal checks. Small requests retain the
previous locality gate and 125% span cap.

The hint performs no source IO and retains no data. It returns false for unknown
state, invalid ranges, disabled verification or an unavailable bitmap/cache. It
is a scheduling hint, not an authorization to expose bytes without reading and
checking them. Zstd eviction after the query is safe: at worst the read has a cache
miss. The source must retain the existing immutability guarantee. Default reader
behavior and cache budgets are unchanged. The batching policy remains in the
benchmark prototype; SDK integrations can use the new hint explicitly.

## Measurements

Same traces and provider, seven rotated initial repetitions and nine replay
repetitions. Full original-order outputs checked in every process. Median
cumulative read service in ms; includes planning and staging. Session shutdown is
reported separately. OS cache not flushed. Generated traces on real content,
plus synthetic RAW/mixed controls. No captured gameplay trace or NUP timing here.

| Phase / corpus / trace / cache | Ordinary | Always group | Verification-aware group |
|---|---:|---:|---:|
| replay / real / sequential64k / 32 MiB | 1.9389 | 2.2565 | 1.8216 |
| replay / mixed / sequential64k / 32 MiB | 1.7577 | 1.9294 | 1.7452 |
| replay / real / random4k / 32 MiB | 0.1895 | 0.2922 | 0.1920 |
| initial / real / sequential64k / 32 MiB | 10.9369 | 9.2795 | 9.4658 |
| initial / real / clustered4k / 0 MiB | 9.8887 | 2.4271 | 2.4758 |
| replay / real / clustered4k / 32 MiB | 0.1443 | 0.1129 | 0.1119 |
| replay / raw / random4k / 32 MiB | 0.1305 | 0.7758 | 0.1317 |

All warm sequential and dispersed replay windows bypass grouping. The former
warm-sequential penalty is absent in this campaign; the clustered cache-disabled
and initial sequential gains remain. Small apparent wins/losses on bypassed paths
should be treated as run variation and hint overhead, not a guaranteed speedup.
This does not prove zero overhead for every CPU, cache mix, eviction race or input.

No new payload cache, threads or persistent policy state is added. The selector
uses existing verification state; merge scratch remains bounded to one logical
chunk (at most 1 MiB). Process memory still includes the probe's common output
staging buffer. [All individual metrics](verified-batch-results.json) retain RAM,
CPU, amplification, decoder counts and grouping decisions for inspection.

Tests cover unknown/partially verified RAW ranges, slice boundaries, invalid file
IDs and offsets, disabled bitmap, corrupt data, cached and uncacheable Zstd, and
mixed RAW/Zstd boundaries. Existing 61 fixtures, 6000 parser mutations and the
persistent Session concurrency/corruption tests pass.

Reproduce with the commands in [the locality experiment](ADAPTIVE_BATCH.md), using
the current probe and the same initial/replay modes. This report supersedes that
prototype's warm-sequential limitation; its earlier measurements remain intact.

Completed **573** process runs.
