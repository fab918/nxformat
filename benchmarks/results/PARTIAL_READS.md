# Multi-block reads, cache and writer: measurements

Machine: Windows 11, MSVC 19 (VS 18) `/O2`, one NVMe drive, Python 3.14 driver.
OS file cache is not controlled: "cold" means a cold NX block cache (8 MiB
synthetic, fresh 64 MiB session over a 1 GiB working set for real games), not a
cold disk. p50/p99 are per read. "v1.0.0" is c70daf6. "per-block" is the
Lichii approach (a896bdd): one `Session::read` per block.

## What changed in the reader

- `Archive::read_file` and `Session::read` return a `PartialRead` (a `Status`
  plus `bytes`): verified blocks are committed straight into the caller buffer
  and the exact verified byte count is returned. This is the default for both;
  `OpenOptions::atomic_reads` restores staging (output unchanged on failure).
  The measurements below call this mode "partial" (they were taken when it was
  a separate `read_partial` method).
- Atomic multi-block reads stage into a private buffer (aligned RAW slices go
  there directly) and copy once.
- Cache: hash map index; blocks are reference-counted, so a hit pins under the
  lock and copies after it. Evicted blocks are recycled with their control
  block, map node and list node, so a steady-state miss allocates nothing.
  A buffer is reused only when its pin count (dropped with release after each
  copy, read with acquire) is zero; an 8-thread stress test with a two-block
  cache runs under ThreadSanitizer.
- RAW blocks stored back to back are fetched with one source read (up to
  1 MiB), straight into the output when the run covers whole 16 KiB slices;
  per-block retry after a failed run read keeps the partial prefix exact.
- RAW read counters are relaxed atomics: no cache lock on the RAW path.
- Admission: one compare-exchange; the mutex/condition variable only when all
  slots are busy.
- `Archive::lookup(directory, name)`: binary search over sorted entries.
- `verify_all` reuses one decode buffer and one stored-bytes buffer.

Guarantees kept: every byte returned is verified (RAW 16 KiB CRC-32C slices,
Zstd frames fully validated); unverified bytes never remain in the caller
buffer after a call (a failed direct RAW run is zeroed); bounded memory; no
format change.

## Synthetic archives (64 MiB, ~2:1 data)

Built with `benchmarks/partial_read_archives.py`, run with
`benchmarks/run_partial_read_benchmark.py` (1/2/4/8 threads; table shows 1 and
8). Totals per case: warm 200k/50k/2000/20000 reads, cold 20000/10000/500/2000.

| archive | state | workload | threads | v1.0.0 atomic p50/p99 µs | v1.0.0 per-block p50/p99 µs | new atomic p50/p99 µs | new partial p50/p99 µs | MiB/s: v1.0.0 atomic / per-block / new partial |
|---|---|---|---:|---:|---:|---:|---:|---:|
| raw_64k | warm | rand256k | 1 | 54 / 113 | 54 / 88 | 47 / 120 | 44 / 79 | 4319 / 4359 / 5258 |
| raw_64k | warm | rand256k | 8 | 124 / 325 | 120 / 242 | 108 / 285 | 101 / 211 | 14998 / 15923 / 18872 |
| raw_64k | warm | rand4m | 1 | 1376 / 1641 | 891 / 1277 | 1197 / 1428 | 604 / 765 | 2886 / 4418 / 6574 |
| raw_64k | warm | rand4m | 8 | 3159 / 4012 | 1757 / 6070 | 2339 / 3039 | 1185 / 1563 | 10091 / 15766 / 26663 |
| raw_64k | warm | seq1m | 1 | 347 / 443 | 224 / 321 | 293 / 398 | 155 / 284 | 2809 / 4339 / 6148 |
| raw_64k | warm | seq1m | 8 | 659 / 1028 | 341 / 1040 | 581 / 979 | 213 / 372 | 11827 / 21095 / 35756 |
| raw_256k | warm | rand256k | 1 | 39 / 113 | 34 / 64 | 38 / 141 | 35 / 97 | 5757 / 6978 / 6390 |
| raw_256k | warm | rand256k | 8 | 102 / 462 | 98 / 302 | 100 / 411 | 100 / 292 | 16198 / 18286 / 18115 |
| raw_256k | warm | rand4m | 1 | 1045 / 1322 | 472 / 779 | 955 / 1135 | 364 / 506 | 3785 / 7998 / 10875 |
| raw_256k | warm | rand4m | 8 | 2626 / 3560 | 1518 / 3416 | 2193 / 2856 | 1138 / 1570 | 11991 / 18268 / 27960 |
| raw_256k | warm | seq1m | 1 | 268 / 354 | 116 / 226 | 237 / 355 | 94 / 221 | 3616 / 8203 / 8935 |
| raw_256k | warm | seq1m | 8 | 538 / 986 | 184 / 808 | 508 / 852 | 154 / 339 | 14338 / 32705 / 48061 |
| zstd_64k | warm | rand256k | 1 | 7 / 11 | 5 / 10 | 6 / 10 | 4 / 7 | 34403 / 44974 / 58291 |
| zstd_64k | warm | rand256k | 8 | 79 / 285 | 79 / 271 | 10 / 29 | 8 / 46 | 22159 / 22415 / 192199 |
| zstd_64k | warm | rand4m | 1 | 679 / 836 | 83 / 126 | 611 / 729 | 73 / 94 | 5850 / 46687 / 53802 |
| zstd_64k | warm | rand4m | 8 | 2632 / 4320 | 1566 / 2322 | 1927 / 2768 | 149 / 418 | 11909 / 20024 / 191086 |
| zstd_64k | warm | seq1m | 1 | 161 / 216 | 20 / 34 | 157 / 209 | 20 / 36 | 5922 / 48594 / 49436 |
| zstd_64k | warm | seq1m | 8 | 467 / 971 | 330 / 645 | 395 / 758 | 33 / 110 | 16186 / 23199 / 210892 |
| zstd_64k | cold | rand256k | 1 | 59 / 121 | 88 / 130 | 59 / 115 | 54 / 106 | 4141 / 3026 / 4590 |
| zstd_64k | cold | rand256k | 8 | 166 / 1015 | 195 / 376 | 134 / 650 | 121 / 636 | 10158 / 10173 / 14476 |
| zstd_64k | cold | rand4m | 1 | 1452 / 1793 | 1227 / 1579 | 1411 / 1780 | 874 / 1338 | 2837 / 3494 / 4804 |
| zstd_64k | cold | rand4m | 8 | 3137 / 5584 | 3099 / 4195 | 2707 / 4573 | 1676 / 3116 | 9744 / 10267 / 18673 |
| zstd_64k | cold | seq1m | 1 | 290 / 470 | 310 / 482 | 289 / 449 | 169 / 292 | 3324 / 3222 / 5919 |
| zstd_64k | cold | seq1m | 8 | 704 / 1892 | 789 / 1291 | 660 / 1315 | 422 / 1595 | 10541 / 9725 / 17026 |
| zstd_256k | warm | rand256k | 1 | 7 / 12 | 4 / 7 | 6 / 9 | 4 / 7 | 35862 / 60756 / 62283 |
| zstd_256k | warm | rand256k | 8 | 64 / 256 | 35 / 324 | 8 / 19 | 5 / 13 | 26696 / 34733 / 335286 |
| zstd_256k | warm | rand4m | 1 | 830 / 1021 | 80 / 114 | 768 / 939 | 67 / 99 | 4811 / 48984 / 58297 |
| zstd_256k | warm | rand4m | 8 | 2467 / 4514 | 1123 / 1642 | 1985 / 2905 | 120 / 381 | 12217 / 28467 / 214176 |
| zstd_256k | warm | seq1m | 1 | 185 / 255 | 19 / 30 | 162 / 222 | 18 / 27 | 5305 / 50365 / 55225 |
| zstd_256k | warm | seq1m | 8 | 467 / 827 | 262 / 460 | 345 / 783 | 18 / 45 | 16953 / 31216 / 358421 |
| zstd_256k | cold | rand256k | 1 | 69 / 203 | 91 / 133 | 67 / 194 | 60 / 144 | 3021 / 2935 / 4098 |
| zstd_256k | cold | rand256k | 8 | 193 / 1187 | 178 / 551 | 171 / 1144 | 171 / 967 | 8144 / 9859 / 9621 |
| zstd_256k | cold | rand4m | 1 | 1087 / 1476 | 869 / 1218 | 1030 / 1378 | 489 / 848 | 3672 / 4893 / 8228 |
| zstd_256k | cold | rand4m | 8 | 2803 / 8320 | 2799 / 3967 | 3218 / 9518 | 1557 / 7056 | 10096 / 11368 / 17481 |
| zstd_256k | cold | seq1m | 1 | 251 / 384 | 209 / 273 | 298 / 426 | 127 / 224 | 3864 / 4808 / 7894 |
| zstd_256k | cold | seq1m | 8 | 751 / 3610 | 648 / 1168 | 758 / 3551 | 383 / 1957 | 9205 / 11802 / 17309 |

## Real Switch 2 archives

DK Bananza + Update 3.2.0 + DLC: 9.6 GB, 77,387 files, 3 snapshots.
SMBW NS2E: 9.0 GB, 26,449 files. Driver: `benchmarks/run_real_archive_benchmark.py`
with `real_archive_benchmark.cpp`; reads on the largest snapshot in path order.

### v1.0.0 against the first new default (before RAW runs)

| game | state | workload | threads | v1.0.0 atomic p50/p99 µs | v1.0.0 per-block p50/p99 µs | new default p50/p99 µs |
|---|---|---|---:|---:|---:|---:|
| DK Bananza | warm | rand1m | 1 | 282 / 456 | 107 / 172 | 101 / 162 |
| DK Bananza | warm | rand1m | 8 | 553 / 1179 | 274 / 549 | 259 / 498 |
| DK Bananza | warm | seq1m | 1 | 262 / 359 | 108 / 178 | 118 / 296 |
| DK Bananza | warm | seq1m | 8 | 502 / 3870 | 189 / 4071 | 166 / 5350 |
| DK Bananza | warm | asset | 1 | 569 / 1363 | 55 / 230 | 46 / 89 |
| DK Bananza | warm | asset | 8 | 1006 / 10223 | 906 / 6071 | 61 / 178 |
| DK Bananza | cold | rand1m | 1 | 494 / 1879 | 273 / 1931 | 264 / 1652 |
| DK Bananza | cold | rand1m | 8 | 844 / 17059 | 437 / 14657 | 391 / 15355 |
| DK Bananza | cold | seq1m | 1 | 487 / 2161 | 371 / 1941 | 296 / 1485 |
| DK Bananza | cold | seq1m | 8 | 600 / 19414 | 361 / 14726 | 324 / 12449 |
| DK Bananza | cold | asset | 1 | 185 / 3315 | 99 / 3400 | 106 / 2645 |
| DK Bananza | cold | asset | 8 | 437 / 6049 | 193 / 5484 | 165 / 3810 |
| SMBW | warm | rand1m | 1 | 296 / 476 | 128 / 221 | 120 / 222 |
| SMBW | warm | rand1m | 8 | 636 / 9876 | 255 / 13678 | 252 / 9731 |
| SMBW | warm | seq1m | 1 | 435 / 755 | 203 / 420 | 197 / 399 |
| SMBW | warm | seq1m | 8 | 510 / 24763 | 268 / 4724 | 228 / 9764 |
| SMBW | warm | asset | 1 | 1258 / 2628 | 516 / 1418 | 479 / 980 |
| SMBW | warm | asset | 8 | 1820 / 3118 | 649 / 1621 | 490 / 27352 |
| SMBW | cold | rand1m | 1 | 300 / 405 | 157 / 235 | 152 / 222 |
| SMBW | cold | rand1m | 8 | 818 / 1399 | 567 / 1034 | 449 / 759 |
| SMBW | cold | seq1m | 1 | 292 / 392 | 148 / 195 | 143 / 205 |
| SMBW | cold | seq1m | 8 | 614 / 8303 | 453 / 5997 | 275 / 13105 |
| SMBW | cold | asset | 1 | 616 / 2297 | 283 / 1008 | 274 / 998 |
| SMBW | cold | asset | 8 | 913 / 20000 | 391 / 27936 | 327 / 32264 |

### RAW runs and direct writes (same run, before → after)

Geometric mean throughput +7.6 %; never slower beyond noise.

| game | state | workload | threads | before p50/p99 µs | after p50/p99 µs | MiB/s before / after | allocs/read before / after |
|---|---|---|---:|---:|---:|---:|---:|
| DK Bananza | warm | rand4k | 1 | 5 / 13 | 5 / 13 | 1056 / 1042 | 0.6 / 0.6 |
| DK Bananza | warm | rand4k | 8 | 7 / 61 | 7 / 60 | 2467 / 2466 | 0.6 / 0.6 |
| DK Bananza | warm | rand64k | 1 | 13 / 34 | 13 / 31 | 5667 / 6078 | 0.7 / 0.7 |
| DK Bananza | warm | rand64k | 8 | 18 / 103 | 18 / 104 | 19914 / 19644 | 0.7 / 0.7 |
| DK Bananza | warm | rand1m | 1 | 101 / 155 | 91 / 142 | 12112 / 13333 | 1.4 / 1.0 |
| DK Bananza | warm | rand1m | 8 | 242 / 456 | 215 / 415 | 38542 / 42662 | 1.3 / 1.0 |
| DK Bananza | warm | seq1m | 1 | 100 / 194 | 82 / 127 | 12667 / 15048 | 0.6 / 0.0 |
| DK Bananza | warm | seq1m | 8 | 140 / 306 | 114 / 201 | 63863 / 76910 | 0.6 / 0.0 |
| DK Bananza | warm | asset | 1 | 41 / 61 | 41 / 63 | 55604 / 55137 | 0.1 / 0.0 |
| DK Bananza | warm | asset | 8 | 71 / 221 | 70 / 216 | 203408 / 207550 | 0.1 / 0.0 |
| DK Bananza | cold | rand4k | 1 | 6 / 66 | 6 / 66 | 202 / 200 | 1.8 / 1.8 |
| DK Bananza | cold | rand4k | 8 | 13 / 137 | 14 / 140 | 1059 / 1032 | 1.9 / 1.9 |
| DK Bananza | cold | rand64k | 1 | 22 / 112 | 22 / 103 | 1752 / 1779 | 3.6 / 3.6 |
| DK Bananza | cold | rand64k | 8 | 46 / 296 | 45 / 306 | 6943 / 6904 | 3.5 / 3.5 |
| DK Bananza | cold | rand1m | 1 | 177 / 627 | 161 / 628 | 3611 / 3725 | 27.3 / 27.0 |
| DK Bananza | cold | rand1m | 8 | 300 / 2361 | 262 / 2161 | 13505 / 13911 | 26.4 / 26.7 |
| DK Bananza | cold | seq1m | 1 | 166 / 599 | 147 / 611 | 3234 / 3333 | 31.3 / 30.8 |
| DK Bananza | cold | seq1m | 8 | 252 / 1668 | 221 / 1596 | 11328 / 11725 | 22.6 / 21.4 |
| DK Bananza | cold | asset | 1 | 82 / 2144 | 75 / 2149 | 6120 / 6708 | 9.7 / 8.9 |
| DK Bananza | cold | asset | 8 | 153 / 3585 | 139 / 3576 | 23051 / 24368 | 9.6 / 9.1 |
| SMBW | warm | rand4k | 1 | 5 / 15 | 5 / 14 | 653 / 656 | 1.0 / 1.0 |
| SMBW | warm | rand4k | 8 | 18 / 71 | 18 / 70 | 1479 / 1492 | 1.0 / 1.0 |
| SMBW | warm | rand64k | 1 | 14 / 33 | 14 / 33 | 3921 / 3908 | 1.1 / 1.1 |
| SMBW | warm | rand64k | 8 | 36 / 121 | 36 / 124 | 12217 / 12054 | 1.1 / 1.1 |
| SMBW | warm | rand1m | 1 | 115 / 199 | 102 / 168 | 8344 / 9572 | 1.9 / 1.5 |
| SMBW | warm | rand1m | 8 | 326 / 584 | 296 / 538 | 23967 / 26449 | 1.9 / 1.5 |
| SMBW | warm | seq1m | 1 | 118 / 241 | 92 / 213 | 7348 / 9607 | 1.0 / 0.0 |
| SMBW | warm | seq1m | 8 | 210 / 398 | 142 / 290 | 35302 / 49362 | 1.0 / 0.0 |
| SMBW | warm | asset | 1 | 361 / 466 | 305 / 377 | 10010 / 11867 | 1.0 / 0.0 |
| SMBW | warm | asset | 8 | 586 / 994 | 426 / 747 | 41398 / 55627 | 1.0 / 0.0 |
| SMBW | cold | rand4k | 1 | 6 / 15 | 6 / 16 | 569 / 561 | 1.0 / 1.0 |
| SMBW | cold | rand4k | 8 | 18 / 81 | 18 / 80 | 1436 / 1419 | 1.0 / 1.0 |
| SMBW | cold | rand64k | 1 | 17 / 37 | 17 / 38 | 3274 / 3273 | 1.1 / 1.1 |
| SMBW | cold | rand64k | 8 | 38 / 137 | 39 / 142 | 11040 / 10786 | 1.1 / 1.1 |
| SMBW | cold | rand1m | 1 | 151 / 222 | 136 / 193 | 6569 / 7266 | 1.9 / 1.5 |
| SMBW | cold | rand1m | 8 | 426 / 738 | 380 / 636 | 18283 / 20427 | 1.9 / 1.5 |
| SMBW | cold | seq1m | 1 | 142 / 182 | 123 / 164 | 6862 / 7913 | 1.0 / 0.0 |
| SMBW | cold | seq1m | 8 | 347 / 562 | 300 / 521 | 21440 / 24917 | 1.0 / 0.0 |
| SMBW | cold | asset | 1 | 173 / 514 | 142 / 436 | 8172 / 9757 | 1.0 / 0.1 |
| SMBW | cold | asset | 8 | 459 / 1442 | 387 / 1256 | 24057 / 27997 | 1.0 / 0.1 |

### Lookup and mount

| | DK Bananza | SMBW |
|---|---:|---:|
| per-path lookup, enumerate scan (current consumers) | 3.2-3.9 µs | 1.07 µs |
| per-path lookup, `Archive::lookup` | 0.24 µs | 0.19 µs |
| mount | ~60 ms | ~26 ms |

Mount was profiled phase by phase (index and Manifest SHA-256, parse, snapshot
identities). A per-directory sort memo and a SHA-256 small-update path gave no
measurable gain and were dropped.

### Full verification (`verify_all`, warm OS cache)

| | v1.0.0 | new |
|---|---:|---:|
| SMBW | 6.14 s, 82,022 allocations | 5.98 s, 9 allocations |
| DK Bananza | 7.40 s, 144,429 allocations | 7.30 s, 12 allocations |

## Writer and repack (Rust)

- Repack source verification decodes each block once: its CRC is checked the
  first time a file hashes it, and unreferenced blocks are checked at the end.
- Dedup reread (`Output::read_at`) copies from the write buffer or reads the
  file without flushing; it only flushes when the range straddles both.
- Frame and dedup comparison buffers keep their initialized length instead of
  being zero-filled on every block.

Strict repack of DK Bananza to a new file (warm OS cache, second run):
31.4 s → 29.0 s; source block decodes 238,544 → 119,895; output verification
decodes 339,759 → 221,110.

`bench_writer_scenarios` dedup time (3 runs each, median):

| scenario | codec | before | after |
|---|---|---:|---:|
| repetitive | raw | 2.45 ms | 1.13 ms |
| repetitive | zstd | 5.24 ms | 3.10 ms |
| snapshots-sharing-most | raw | 7.23 ms | 6.88 ms |
| snapshots-sharing-most | zstd | 11.93 ms | 10.89 ms |
| limited-modifications | raw | 7.38 ms | 6.87 ms |
| limited-modifications | zstd | 12.30 ms | 10.55 ms |

## Not done, with reasons

- Thread-local RAW scratch and lock-free stats on the hot path were measured
  at 1-2 % before RAW runs; RAW runs now remove the scratch for aligned reads.
- Zero-fill of new cache buffers only happens while the cache fills; recycling
  removes it in steady state.
- More prefetch workers: cold large reads are already split between caller
  and worker; left as future work, measure with real game traces first.
