# Selective batching: avoid the dispersed-read penalty

This benchmark-only prototype keeps a bounded 16-request pending window. Before
sorting, it checks whether the first three requests address the same file and
logical chunk. Otherwise it uses the ordinary path, without sorting or merging.
When enabled, each merged span is limited to 125% of the useful requested bytes.
There is no extra decoded-data cache. Scratch storage remains bounded to one
logical chunk (at most 1 MiB); memory and copies remain included in the probe.

Same mapped provider, requests and common output buffers as the previous report.
Seven rotated repetitions for initial passes, nine for replay. The OS cache was
not flushed; no caller think time. Times below are medians of cumulative read
service in ms. These are generated access patterns on real/synthetic content,
not recorded game loading times. All original-order full outputs were checked.

| Phase / corpus / trace / cache | Ordinary | Always group | Selective group |
|---|---:|---:|---:|
| replay / real / random4k / 32 MiB | 0.2106 | 0.3087 | 0.1821 |
| initial / real / clustered4k / 0 MiB | 11.8616 | 2.9387 | 2.8943 |
| initial / real / sequential64k / 32 MiB | 11.1173 | 9.6401 | 9.5153 |
| replay / real / clustered4k / 32 MiB | 0.1810 | 0.1288 | 0.1637 |
| replay / real / bursty4k / 32 MiB | 0.2649 | 0.1657 | 0.1505 |
| replay / raw / random4k / 32 MiB | 0.1822 | 0.9890 | 0.1793 |
| replay / real / sequential64k / 32 MiB | 1.6913 | 2.1823 | 2.3386 |

The earlier +54% is not an invariant: in this new campaign the always-grouped
real random replay costs about +47%. The selective version bypasses grouping
in all 32 windows of that trace, removing that observed penalty. Its small
apparent win over ordinary reads should not be treated as a guaranteed speedup.
The clustered cache-disabled and initial sequential gains remain substantial.

**Remaining regression:** sequential replay still groups requests that are close
but already cheap to read. The classifier detects locality, not the actual cost
of a warmed RAW range. It is therefore not ready to replace every read path.
False positives and false negatives remain possible; requests after the first
three may have a different pattern. The span cap limits amplification rather
than proving a throughput win. Serial callers cannot supply future requests
without waiting, so this experiment applies to already-pending batches only.

[All per-process measurements](adaptive-batch-results.json) include memory,
read-call counts, grouping decisions, decodes, byte amplification and run-level
variation. The common full-output buffer remains part of measured process RAM.
NUP is not timed in this experiment; the SDK defaults remain unchanged.

```text
python benchmarks/run_batch_prefetch_probe.py --nup LOCAL.nup --nx LOCAL.nx --synthetic SYNTHETIC_CORPUS --output results/adaptive --modes off batch adaptive --think 0 --repetitions 7
python benchmarks/run_batch_prefetch_probe.py --nup LOCAL.nup --nx LOCAL.nx --synthetic SYNTHETIC_CORPUS --output results/adaptive-warm --modes off batch adaptive --think 0 --repetitions 9 --warm
```

Completed **573** process runs.

[Verification-aware selector resolves the warm sequential regression in the next campaign](VERIFIED_BATCH.md), without changing default reads or adding a payload cache.
