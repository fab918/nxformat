"""Publish the completed batch/prefetch experiments, excluding private traces."""
import csv,json,statistics,sys
from pathlib import Path

def main():
 cold,warm,out=map(Path,sys.argv[1:]);out.mkdir(exist_ok=True,parents=True)
 reports=[json.loads((p/'public-results.json').read_text()) for p in (cold,warm)]
 records=[]
 for phase,report in zip(('initial','replay'),reports):
  records += [dict(phase=phase,**row) for row in report['records']]
 keys=sorted({(r['phase'],r['corpus'],r['scenario'],r['budget_mib'],r['think_us'],r['mode']) for r in records})
 aggregates=[]
 for key in keys:
  group=[r for r in records if (r['phase'],r['corpus'],r['scenario'],r['budget_mib'],r['think_us'],r['mode'])==key]
  row=dict(zip(('phase','corpus','scenario','budget_mib','think_us','mode'),key));row['repetitions']=len(group)
  for field in group[0]['metrics']:
   row[field]=statistics.median(r['metrics'][field] for r in group)
  row['service_min_ms']=min(r['metrics']['service_ms'] for r in group);row['service_max_ms']=max(r['metrics']['service_ms'] for r in group)
  aggregates.append(row)
 (out/'batch-prefetch-results.json').write_text(json.dumps(dict(protocols=reports[:1][0]['method'],warm_protocol=reports[1]['method'],reader_sha256=reports[1]['reader_sha256'],probe_sha256=reports[1]['probe_sha256'],records=records,aggregates=aggregates),indent=2)+'\n',encoding='utf-8',newline='\n')
 with (out/'batch-prefetch-results.csv').open('w',encoding='utf-8',newline='') as f:
  writer=csv.DictWriter(f,fieldnames=list(aggregates[0]));writer.writeheader();writer.writerows(aggregates)
 def get(phase,corpus,scene,budget,think,mode):
  return next(r for r in aggregates if (r['phase'],r['corpus'],r['scenario'],r['budget_mib'],r['think_us'],r['mode'])==(phase,corpus,scene,budget,think,mode))
 md='''# Bounded batching and predictive prefetch experiments

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
'''
 selected=[('initial','real',s,32,t) for s in ('random4k','clustered4k','bursty4k','sequential64k') for t in (0,50)]
 selected += [('replay','real',s,32,0) for s in ('random4k','clustered4k','bursty4k','sequential64k')]
 selected += [('initial','real','clustered4k',0,0),('initial','mixed','sequential64k',32,50),('replay','raw','random4k',32,0)]
 for phase,corpus,scene,budget,think in selected:
  vals=[get(phase,corpus,scene,budget,think,m)['service_ms'] for m in ('off','batch','prefetch','guarded')]
  md+=f'| {phase} / {corpus} / {scene} | {budget} | {think} | '+' | '.join(f'{x:.4f}' for x in vals)+' |\n'
 md+='''
## Memory and useful work

Resident process peaks in MiB, and total decoder calls after worker shutdown.
For replay, decoder counts exclude priming. CPU/memory process totals include
priming. Payload-cache peak stats are sampled before shutdown; late speculative
work can finish during shutdown. The final decoder count includes that work.

| Trace | Mode | Resident MiB | Private commitment MiB | Decodes | Read calls | Merged/requested bytes |
|---|---|---:|---:|---:|---:|---:|
'''
 for phase,scene,budget,think in [('initial','clustered4k',0,0),('initial','bursty4k',32,50),('replay','random4k',32,0),('initial','sequential64k',32,50)]:
  for mode in ('off','batch','prefetch','guarded'):
   r=get(phase,'real',scene,budget,think,mode)
   md+=f"| {phase} / {scene} / {budget} MiB | {mode} | {r['resident']/2**20:.2f} | {r['private_commit']/2**20:.2f} | {r['decodes']:g} | {r['calls']:g} | {r['merged_bytes']/r['requested_bytes']:.2f} |\n"
 md+='''
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
'''
 md+=f'\nCompleted **{len(records)} isolated process runs**, each with full-output equality checking.\n'
 (out/'BATCH_PREFETCH.md').write_text(md,encoding='utf-8',newline='\n')
 for phase,scene,budget,think in [('initial','clustered4k',0,0),('initial','sequential64k',32,0),('replay','random4k',32,0),('initial','bursty4k',32,50)]:
  print(phase,scene,budget,think,[(m,round(get(phase,'real',scene,budget,think,m)['service_ms'],4)) for m in ('off','batch','prefetch','guarded')])
if __name__=='__main__':main()
