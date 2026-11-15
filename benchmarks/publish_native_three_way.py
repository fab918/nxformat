"""Publish a fresh native NUP/main/feature comparison, including process memory."""
import csv,gzip,hashlib,json,statistics,sys
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt

LABEL={'nup_mapped':'NUP v1','nx_original_mapped':'NX main','nx_verify_mapped':'NX new'}
BATCH={'nup':'NUP v1','main':'NX main','current':'NX new · direct','selective':'NX new · selective'}
COLORS=['#D48728','#8993A5','#3659E3']
def main():
 sync,batch,out=map(Path,sys.argv[1:]);out.mkdir(exist_ok=True,parents=True)
 s=json.loads((sync/'results.json').read_text());b=json.loads((batch/'public-results.json').read_text())
 for name,data in [('sync-processes',s),('batch-processes',b)]:
  (out/(name+'.json.gz')).write_bytes(gzip.compress((json.dumps(data,indent=2)+'\n').encode(),mtime=0))
 rows=[]
 for c in s['corpora']:
  for scenario in sorted({r['scenario'] for r in s['records']}):
   for budget in sorted({r['cache_mib'] for r in s['records']}):
    for v in sorted({r['variant'] for r in s['records']}):
     group=[r for r in s['records'] if (r['corpus'],r['scenario'],r['cache_mib'],r['variant'])==(c,scenario,budget,v)]
     for phase in (0,1):
      row=dict(corpus=c,scenario=scenario,budget_mib=budget,variant=v,phase=phase,repetitions=len(group))
      for f in ('p50_us','p99_us','total_ms','read_mib_per_second'):row[f]=statistics.median(r['phases'][phase][f] for r in group)
      for f in ('peak_working_set','peak_private_commit','spill_bytes','nx_cache_peak','open_mount_ns'):row[f]=statistics.median(r['metrics'][f] for r in group)
      row['verification_bytes']=statistics.median(r['metrics'].get('verification_bytes',0) for r in group)
      row['p99_min_us']=min(r['phases'][phase]['p99_us'] for r in group);row['p99_max_us']=max(r['phases'][phase]['p99_us'] for r in group)
      rows.append(row)
 def save_csv(name,records):
  with (out/name).open('w',encoding='utf-8',newline='') as f:
   writer=csv.DictWriter(f,fieldnames=list(records[0]));writer.writeheader();writer.writerows(records)
 save_csv('sync-results.csv',rows)
 brows=[]
 keys=sorted({(r['corpus'],r['scenario'],r['budget_mib'],r['warmup'],r['variant']) for r in b['records']})
 for key in keys:
  group=[r for r in b['records'] if (r['corpus'],r['scenario'],r['budget_mib'],r['warmup'],r['variant'])==key]
  row=dict(zip(('corpus','scenario','budget_mib','warmup','variant'),key));row['repetitions']=len(group)
  for f in ('service_ms','resident','private_commit','spill_bytes','calls','grouped_windows','merged_bytes','requested_bytes','merge_scratch_peak','payload_peak'):row[f]=statistics.median(r['metrics'][f] for r in group)
  row['verification_bytes']=statistics.median(r['metrics'].get('verification_bytes',0) for r in group)
  row['service_min_ms']=min(r['metrics']['service_ms'] for r in group);row['service_max_ms']=max(r['metrics']['service_ms'] for r in group)
  brows.append(row)
 save_csv('batch-results.csv',brows)
 def get(c,sc,budget,v,phase=1):return next(r for r in rows if (r['corpus'],r['scenario'],r['budget_mib'],r['variant'],r['phase'])==(c,sc,budget,v,phase))
 def bg(sc,budget,warm,v,c='real'):return next(r for r in brows if (r['corpus'],r['scenario'],r['budget_mib'],r['warmup'],r['variant'])==(c,sc,budget,warm,v))
 md=f'''# Fresh NUP v1 / NX main / NX new comparison — 15 November 2026

- **NX main (called master in the request):** `{b['main_commit']}`.
- **NX new:** `{b['feature_commit']}`; immutable-source verification reuse enabled.
- **NUP v1:** unchanged native Eglantine reader; source revision and hashes in the
  process reports. All variants compiled with the same MSVC Release flags and
  static Zstd. Same bounded mapped input provider; handles resolved before reads.
- Machine: AMD Ryzen 7 9800X3D, Windows 11 26200. OS cache not flushed. The first
  application pass is not a cold-disk test. Generated traces, not recorded gameplay.

## 1. Same synchronous read calls

No batching or prefetch. Three isolated repetitions per configuration; two passes.
The reader-only verification hint does not change ordinary reads by itself.
Times: median / P99 in microseconds, median of per-run quantiles on the repeated
pass. NX main verifies RAW slices each read; NX new reuses verification only for
pinned immutable inputs. NUP has a different integrity-check policy.

| Corpus / access pattern | Cache MiB | NUP v1 | NX main | NX new |
|---|---:|---:|---:|---:|
'''
 selection=[('raw','random_4096',32),('zstd','random_4096',32),('real_A','random_4096',32),('real_A','random_4096',8),('real_A','sequential_32mib',32),('real_A','interleaved_8files',32)]
 for c,sc,budget in selection:
  vals=[get(c,sc,budget,v) for v in LABEL]
  md+=f'| {c} / {sc} | {budget} | '+' | '.join(f"{r['p50_us']:g} / {r['p99_us']:g}" for r in vals)+' |\n'
 md+='''
### Memory for the same synchronous workloads

Peak **resident / private commitment**, MiB, medians of process peaks (both
passes combined). Private commitment is not physical RAM. Resident memory
includes mapped file pages; neither is total system memory. Do not count shared
mapped pages again as an additional copy of the OS file cache.

| Corpus / access pattern | Cache MiB | NUP v1 | NX main | NX new |
|---|---:|---:|---:|---:|
'''
 for c,sc,budget in selection:
  md+=f'| {c} / {sc} | {budget} | '+' | '.join(f"{get(c,sc,budget,v)['peak_working_set']/2**20:.2f} / {get(c,sc,budget,v)['peak_private_commit']/2**20:.2f}" for v in LABEL)+' |\n'
 md+='\n![Native SDK reads and memory](native-sdk-comparison.png)\n\n'
 md+='''## 2. Explicit pending-read batches

All four variants receive the same 16 already-pending requests, same timing
windows, buffers and output staging. NUP/main/new-direct perform ordinary reads;
new-selective also uses the locality gate, 125% span cap and verified-range hint.
This is an explicit integration policy, not an automatic improvement to every
synchronous API. No queue-fill delay is modeled. Five rotated repetitions.
Full original-order output SHA-256 is compared for every process and budget.

Times are cumulative foreground read service, ms. Resolution, priming, output-file
writing and destruction are excluded. Initial/replay mean decoded application
cache state; the OS cache is uncontrolled. Memory peaks include common full-output
staging (up to 32 MiB), priming, metadata and decoder scratch. A primed replay can still miss when the decoded working set exceeds the cache; the 128 MiB random control fits all eight files. They therefore must
not be compared directly to memory in the SDK-only table above.

| Real trace | Cache MiB | Primed | NUP v1 | NX main | NX new direct | NX new selective |
|---|---:|---|---:|---:|---:|---:|
'''
 cases=[('first_touch4k',128,False),('random4k',128,True),('random4k',32,True),('sequential64k',32,False),('sequential64k',32,True),('clustered4k',0,False),('clustered4k',32,True),('bursty4k',32,True)]
 for sc,budget,warm in cases:
  md+=f'| {sc} | {budget} | {warm} | '+' | '.join(f"{bg(sc,budget,warm,v)['service_ms']:.4f}" for v in BATCH)+' |\n'
 md+='''
### Memory for pending batches

Peak **resident / private commitment**, MiB, same cases and process peaks.

| Real trace | Cache MiB | Primed | NUP v1 | NX main | NX new direct | NX new selective |
|---|---:|---|---:|---:|---:|---:|
'''
 for sc,budget,warm in cases:
  md+=f'| {sc} | {budget} | {warm} | '+' | '.join(f"{bg(sc,budget,warm,v)['resident']/2**20:.2f} / {bg(sc,budget,warm,v)['private_commit']/2**20:.2f}" for v in BATCH)+' |\n'
 md+='\n![Native pending-batch comparison](pending-batch-comparison.png)\n\n![Memory for the same pending workloads](pending-batch-memory.png)\n\n'
 md+='''## Limits, checks and complete data

The selected first-access case targets eight NUP-compressed files. Of their eight
requested starting blocks in NX, five are RAW and three Zstd. Its result includes
writer storage-policy differences, not independent decompression alone. Archive
sizes and encoding counts for the synchronous subsets are in the process report.
NUP can win fully cached reads. With zero RAM cache, it can retain decoded content
in temporary files, while NX currently decodes again instead of doing that disk
caching. Live temporary-file bytes are reported separately; their OS-cache memory
outside the reader process is not measured. No FPS or universal speedup claim.

Per-repetition quantiles are aggregated without pooling. Sub-microsecond differences
approach clock granularity. Observed ranges are not statistical confidence intervals.
All memory/latency conclusions apply to these concrete tools, policies and inputs.

- [Synchronous aggregates](sync-results.csv) and [all synchronous process metrics](sync-processes.json.gz).
- [Batch aggregates](batch-results.csv) and [all batch process metrics](batch-processes.json.gz).

'''
 md+=f"The synchronous matrix ran **{len(s['records'])} processes**, **{s['isolated_sampled_reads']:,} sampled reads** and **{s['fully_compared_reads']:,} separate full-buffer comparisons**. The pending-batch campaign ran **{len(b['records'])} processes**, all with full-output equality checks. No game content or private member paths are published.\n\n"
 md+='''Reproduce in an MSVC developer shell using the native comparison tools:

```text
python benchmarks/run_low_memory_matrix.py --egl-build BUILD --synthetic-corpus CORPUS --real-nup LOCAL.nup --real-nx LOCAL.nx --output results/three-way --reads 1000 --repetitions 3 --budgets 0 8 32 --resolved
python benchmarks/run_native_three_way.py --egl-build BUILD --nup LOCAL.nup --nx LOCAL.nx --synthetic CORPUS --traces PRIVATE_TRACE_DIR --first-touch PRIVATE_FIRST_TOUCH.tsv --output results/three-way-batch
python benchmarks/publish_native_three_way.py results/three-way results/three-way-batch docs/benchmarks/three-way-reader-2026-11-15
```

Generate matching private traces with `run_batch_prefetch_probe.py` and
`run_real_compressed_traces.py`. The former produces `real/raw/mixed-*-private.tsv`.
The adapter links external GPL-3.0-or-later NUP code and is GPL-3.0-or-later;
no emulator source, benchmark binary or game content is vendored in releases.
'''
 (out/'README.md').write_text(md,encoding='utf-8',newline='\n')
 plt.rcParams.update({'font.family':'DejaVu Sans','font.size':10,'text.color':'#19243C','axes.spines.top':False,'axes.spines.right':False,'figure.facecolor':'#F6F8FC','axes.facecolor':'white','savefig.facecolor':'#F6F8FC'})
 fig,axes=plt.subplots(1,2,figsize=(13,6),layout='constrained');scenarios=['random_4096','sequential_32mib','interleaved_8files'];names=['Random 4 KiB','Sequential 256 KiB','Interleaved 64 KiB']
 for ax,field,title,unit in zip(axes,('p99_us','peak_working_set'),('Read latency · P99','Peak resident memory'),('Microseconds','MiB')):
  for j,(v,label) in enumerate((('nup_mapped','NUP v1'),('nx_verify_mapped','NX'))):
   values=[get('real_A',sc,32,v)[field]/(2**20 if field=='peak_working_set' else 1) for sc in scenarios]
   bars=ax.bar([i+(j-.5)*.3 for i in range(3)],values,.3,label=label,color=[COLORS[0],COLORS[2]][j]);ax.bar_label(bars,fmt='%.1f',padding=3,fontsize=8)
  ax.set_xticks(range(3),names);ax.set_title(title,fontweight='bold');ax.set_ylabel(unit+' · lower is better');ax.grid(axis='y',alpha=.2);ax.set_axisbelow(True);ax.margins(y=.25)
 axes[0].legend();fig.suptitle('NUP v1 / NX — repeated real-archive reads',fontsize=16,fontweight='bold');fig.supxlabel('Same native C++ read calls · 32 MiB data cache · median of 3 runs · no OS-cache flush\nPrivate commitment, limits and all measurements are in the report. No batching in this figure.',fontsize=10);fig.savefig(out/'native-sdk-comparison.png',dpi=170);plt.close(fig)
 fig,axes=plt.subplots(2,2,figsize=(13,8),layout='constrained');plotcases=[('first_touch4k',128,False,'8 first accesses · 4 KiB'),('random4k',128,True,'512 scattered reads · all files fit'),('sequential64k',32,False,'Sequential 32 MiB · initial'),('sequential64k',32,True,'Sequential 32 MiB · primed')]
 for ax,(sc,budget,warm,title) in zip(axes.flat,plotcases):
  vals=[bg(sc,budget,warm,v)['service_ms'] for v in ('nup','selective')]
  bars=ax.barh([0,1],vals,color=[COLORS[0],COLORS[2]]);ax.set_yticks([0,1],['NUP v1','NX']);ax.invert_yaxis()
  ax.bar_label(bars,labels=[f'{v:.4f}' if v<1 else f'{v:.2f}' for v in vals],padding=5)
  ax.set_xlim(0,max(vals)*1.25);ax.set_xlabel('Total read service · ms · lower is better');ratio=max(vals[0],vals[-1])/min(vals[0],vals[-1]);winner='NX' if vals[-1]<vals[0] else 'NUP v1';other='NUP v1' if vals[-1]<vals[0] else 'NX'
  gain=f'{ratio:.0f}' if ratio>=100 else f'{ratio:.1f}'
  ax.set_title(title+'  /  '+str(budget)+' MiB cache'+'\n'+winner+' ~'+gain+'× faster than '+other,fontsize=11,fontweight='bold');ax.grid(axis='x',alpha=.2);ax.set_axisbelow(True)
 fig.suptitle('Native readers with 16 already-pending requests',fontsize=17,fontweight='bold');fig.supxlabel('Median of 5 runs · NX uses explicit selective batching; NUP v1 uses ordinary calls.\nReal content, generated traces. First-access selection includes RAW/Zstd policy differences. Each panel has its own scale.',fontsize=10);fig.savefig(out/'pending-batch-comparison.png',dpi=170);plt.close(fig)
 fig,axes=plt.subplots(1,2,figsize=(13,6),layout='constrained')
 memcases=[('first_touch4k',128,False),('random4k',128,True),('sequential64k',32,False),('sequential64k',32,True)]
 names=['First accesses\n128 MiB cache','Scattered · cached\n128 MiB cache','Sequential · initial\n32 MiB cache','Sequential · cached\n32 MiB cache']
 for ax,field,title in zip(axes,('resident','private_commit'),('Peak resident memory','Peak private commitment')):
  for j,(v,label) in enumerate(zip(('nup','selective'),('NUP v1','NX'))):
   values=[bg(sc,budget,warm,v)[field]/2**20 for sc,budget,warm in memcases]
   bars=ax.bar([i+(j-.5)*.3 for i in range(4)],values,.3,label=label,color=[COLORS[0],COLORS[2]][j])
   ax.bar_label(bars,fmt='%.1f',padding=3,fontsize=8)
  ax.set_xticks(range(4),names,fontsize=8);ax.set_title(title,fontweight='bold');ax.set_ylabel('MiB · lower is better');ax.grid(axis='y',alpha=.2);ax.set_axisbelow(True);ax.margins(y=.2)
 axes[0].legend(fontsize=9)
 fig.suptitle('Memory on the same four read workloads',fontsize=17,fontweight='bold')
 fig.supxlabel('Median of 5 process peaks · same real archive and common output buffers · NX uses selective batching.\nResident memory includes mapped file pages. Private commitment is not physical RAM.',fontsize=10)
 fig.savefig(out/'pending-batch-memory.png',dpi=170);plt.close(fig)
 print('Published',len(rows),'sync and',len(brows),'batch configurations')
if __name__=='__main__':main()
