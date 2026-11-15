# Bounded batching and predictive prefetch experiments

Two experiments, using the existing C++ Session and a benchmark-only batch planner.
No production reader change is applied automatically.

- **Batch**: at most 16 already-pending requests. Sort by file/offset, combine
  requests within the same logical chunk into one bounded span, scatter results
  back to original order. Sort, extra reads and copies are included in timing.
  This requires requests to be available together; no queue-fill wait is modeled.
- **Prefetch**: existing Session policy after three consecutive reads, one block
  ahead, one worker. **Guarded** changes the trigger to eight consecutive reads.
  Both policies skip RAW blocks and stop predictions after seeks.
- **Off**: same Session, split reads and sequential prefetch disabled.

The matrix uses real archive A and generated RAW/mixed corpora. Access patterns
are generated, not captured emulator requests. Data-cache budgets are 8/32 MiB,
plus a deliberately cache-disabled clustered case. Every mode has identical
16-request timing windows, staging buffers and requested output. Five rotated
fresh-process repetitions. SHA-256 of every full, original-order output is checked
against the baseline. No proprietary bytes or private paths are published.

Initial passes start without decoded payload in the application cache; the OS
cache is not flushed. Replay primes the same Session with the entire trace first.
Optional caller CPU work is a synthetic busy wait of 50 microseconds per logical
request, paid after each 16-request window (800 microseconds). This is not a
measured game workload. Output file writes happen only after workers stop.

## Selected results

Cumulative measured read service, ms; medians of five runs, lower is better.
Includes planning and output staging, excludes caller work and Session destruction.
Shutdown and combined times are available separately, as are total CPU/work,
resident/private peaks, decoder calls, amplification and temporary merge capacity.
Memory peaks include the common full-output staging buffer (up to 32 MiB),
metadata and allocator/runtime costs; these are not application-only RAM figures.

| Phase / corpus / trace | Cache MiB | Caller work us/request | Off | Batch | Prefetch | Guarded |
|---|---:|---:|---:|---:|---:|---:|
| initial / real / random4k | 32 | 0 | 14.0444 | 14.5300 | 14.3064 | 14.2281 |
| initial / real / random4k | 32 | 50 | 14.3594 | 14.4998 | 14.1480 | 14.2686 |
| initial / real / clustered4k | 32 | 0 | 2.8745 | 2.8881 | 2.9946 | 2.9309 |
| initial / real / clustered4k | 32 | 50 | 3.0618 | 3.0976 | 2.9647 | 3.3824 |
| initial / real / bursty4k | 32 | 0 | 4.0826 | 4.0241 | 3.8224 | 3.9318 |
| initial / real / bursty4k | 32 | 50 | 4.0175 | 4.1382 | 3.8270 | 3.8837 |
| initial / real / sequential64k | 32 | 0 | 11.6743 | 9.9276 | 11.7071 | 11.7096 |
| initial / real / sequential64k | 32 | 50 | 12.2770 | 10.1890 | 12.6415 | 12.0941 |
| replay / real / random4k | 32 | 0 | 0.1906 | 0.2939 | 0.2111 | 0.2097 |
| replay / real / clustered4k | 32 | 0 | 0.2016 | 0.1442 | 0.1961 | 0.1928 |
| replay / real / bursty4k | 32 | 0 | 0.1483 | 0.1202 | 0.1803 | 0.1788 |
| replay / real / sequential64k | 32 | 0 | 1.9160 | 2.2826 | 2.0275 | 1.7879 |
| initial / real / clustered4k | 0 | 0 | 10.5034 | 2.8814 | 10.1585 | 9.8040 |
| initial / mixed / sequential64k | 32 | 50 | 13.5219 | 12.4349 | 13.7789 | 13.5940 |
| replay / raw / random4k | 32 | 0 | 0.1438 | 0.8777 | 0.1458 | 0.1439 |

## Memory and useful work

Resident process peaks in MiB, and total decoder calls after worker shutdown.
For replay, decoder counts exclude priming. CPU/memory process totals include
priming. Payload-cache peak stats are sampled before shutdown; late speculative
work can finish during shutdown. The final decoder count includes that work.

| Trace | Mode | Resident MiB | Private commitment MiB | Decodes | Read calls | Merged/requested bytes |
|---|---|---:|---:|---:|---:|---:|
| initial / clustered4k / 0 MiB | off | 34.05 | 18.69 | 48 | 512 | 1.00 |
| initial / clustered4k / 0 MiB | batch | 34.04 | 18.73 | 3 | 32 | 0.90 |
| initial / clustered4k / 0 MiB | prefetch | 34.05 | 18.73 | 48 | 512 | 1.00 |
| initial / clustered4k / 0 MiB | guarded | 34.05 | 18.69 | 48 | 512 | 1.00 |
| initial / bursty4k / 32 MiB | off | 35.71 | 19.80 | 13 | 512 | 1.00 |
| initial / bursty4k / 32 MiB | batch | 35.71 | 19.78 | 13 | 64 | 1.00 |
| initial / bursty4k / 32 MiB | prefetch | 38.19 | 21.54 | 26 | 512 | 1.00 |
| initial / bursty4k / 32 MiB | guarded | 38.16 | 21.52 | 26 | 512 | 1.00 |
| replay / random4k / 32 MiB | off | 51.71 | 25.81 | 0 | 512 | 1.00 |
| replay / random4k / 32 MiB | batch | 52.01 | 25.92 | 0 | 506 | 1.17 |
| replay / random4k / 32 MiB | prefetch | 52.05 | 25.97 | 0 | 512 | 1.00 |
| replay / random4k / 32 MiB | guarded | 52.05 | 25.95 | 0 | 512 | 1.00 |
| initial / sequential64k / 32 MiB | off | 94.12 | 48.88 | 0 | 512 | 1.00 |
| initial / sequential64k / 32 MiB | batch | 94.38 | 49.19 | 0 | 143 | 1.00 |
| initial / sequential64k / 32 MiB | prefetch | 94.51 | 49.12 | 0 | 512 | 1.00 |
| initial / sequential64k / 32 MiB | guarded | 94.50 | 49.12 | 0 | 512 | 1.00 |

## Interpretation and limits

Batching can save repeated work when the decoded-data cache is absent, or combine
sequential calls. It adds planning, staging and sometimes read amplification.
It is not a safe default for a synchronous random-read interface. It also does
not promise to speed up the original fully cached 512-read trace.

Prefetch competes for CPU and cache space. In a short sequential burst followed
by a seek, it can decode blocks the caller never requests. Raising the trigger
to eight reads does not automatically prevent that: these bursts are eight reads
long. Tight read loops can outrun the worker. Compare useful work, RAM and total
work time as well as foreground service latency. Random-trace changes without
submitted predictions are noise/worker overhead, not a prefetch success.

Some of the largest files compressed by the NUP writer are stored RAW by the
normal NX compression policy. The real sequential prefix here contains no Zstd
decodes, so it cannot demonstrate a decode-prefetch benefit. The mixed synthetic
control contains Zstd blocks and tests the actual predictor. The earlier eight
first-access trace has three Zstd and five RAW requested NX blocks: its ~109x
result includes storage-policy differences, not only independent decompression.

No default is changed from this experiment. Full medians and observed run ranges
for every configuration are in [CSV](batch-prefetch-results.csv); all per-process
metrics and content-equality fingerprints are in [JSON](batch-prefetch-results.json).
CPU accounting is whole-process Windows accounting, not a sampled hot-path profile.
Adapter read_at byte counters do not include direct RAW mapped views and are not
physical disk traffic. No NUP reader is timed in this campaign.

Reproduce in a Windows MSVC developer shell:

```text
python benchmarks/run_batch_prefetch_probe.py --nup LOCAL.nup --nx LOCAL.nx --synthetic SYNTHETIC_CORPUS --output results/batch-prefetch
python benchmarks/run_batch_prefetch_probe.py --nup LOCAL.nup --nx LOCAL.nx --synthetic SYNTHETIC_CORPUS --output results/batch-prefetch-warm --warm
python benchmarks/publish_batch_prefetch.py results/batch-prefetch results/batch-prefetch-warm docs/benchmarks/low-memory-reader-cache
```

Generate the synthetic corpus using the native comparison example described in
the main benchmark report. Inputs must contain matching logical files. Private
request manifests and output bytes remain under ignored results directories.

Completed **840 isolated process runs**, each with full-output equality checking.
