"""Publish aggregate native-reader results, selected raw samples and honest plots.

Usage: python benchmarks/publish_low_memory_results.py RESULTS_DIR OUTPUT_DIR
Private request manifests and source paths are deliberately excluded.
"""
from __future__ import annotations
import csv
import gzip
import json
import statistics
import sys
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np

LABELS={'raw':'Synthetic RAW','zstd':'Synthetic mixed','real_A':'Real archive A'}
READERS={'nup_mapped':'NUP v1 / Eglantine','nx_verify_mapped':'NX verification cache'}
COLORS=['#d97706','#2563eb']

def main():
    source,output=map(lambda p:Path(p).resolve(),sys.argv[1:3])
    output.mkdir(parents=True,exist_ok=True)
    report=json.loads((source/'results.json').read_text(encoding='utf-8'))
    grouped={}
    for record in report['records']:
        for phase in record['phases']:
            key=(record['corpus'],record['scenario'],record['cache_mib'],record['variant'],phase['phase'])
            grouped.setdefault(key,[]).append((record,phase))
    rows=[]
    for key,samples in sorted(grouped.items()):
        corpus,scenario,budget,variant,phase=key
        median=lambda fn:statistics.median(fn(r,p) for r,p in samples)
        rows.append(dict(corpus=corpus,scenario=scenario,cache_mib=budget,variant=variant,phase=phase,
            repetitions=len(samples),reads_per_pass=samples[0][1]['count'],
            p50_us=median(lambda r,p:p['p50_us']),p99_us=median(lambda r,p:p['p99_us']),
            p50_min_us=min(p['p50_us'] for _,p in samples),p50_max_us=max(p['p50_us'] for _,p in samples),
            p99_min_us=min(p['p99_us'] for _,p in samples),p99_max_us=max(p['p99_us'] for _,p in samples),
            service_ms=median(lambda r,p:p['total_ms']),read_mib_s=median(lambda r,p:p['read_mib_per_second']),
            peak_resident_mib=median(lambda r,p:r['metrics']['peak_working_set']/2**20),
            peak_private_commit_mib=median(lambda r,p:r['metrics']['peak_private_commit']/2**20),
            payload_cache_mib=median(lambda r,p:r['metrics']['nx_cache_bytes']/2**20),
            verification_bytes=median(lambda r,p:r['metrics'].get('verification_bytes',0)),
            temporary_file_mib=median(lambda r,p:r['metrics']['spill_bytes']/2**20),
            open_mount_ms=median(lambda r,p:r['metrics']['open_mount_ns']/1e6)))
    with (output/'results.csv').open('w',newline='',encoding='utf-8') as f:
        writer=csv.DictWriter(f,fieldnames=list(rows[0]));writer.writeheader();writer.writerows(rows)
    public={k:v for k,v in report.items() if k!='records'}
    public['records']=report['records']
    public['nx_reference_commit']='b6a9de8'
    public['nup_repository']='https://github.com/fab918/Eglantine'
    public['aggregates']=rows
    public['hardware']='AMD Ryzen 7 9800X3D; Windows 11 26200; MSVC 19.51 /O2; static Zstd 1.5.8'
    public['date']='2026-11-15'
    (output/'results.json').write_text(json.dumps(public,indent=2)+'\n',encoding='utf-8')
    def get(c,s,b,v,phase=1):
        return next(r for r in rows if (r['corpus'],r['scenario'],r['cache_mib'],r['variant'],r['phase'])==(c,s,b,v,phase))
    plt.rcParams.update({'font.size':10,'axes.spines.top':False,'axes.spines.right':False,
                        'figure.facecolor':'white','axes.facecolor':'white','savefig.facecolor':'white'})
    corpora=[c for c in ('raw','zstd','real_A') if c in report['corpora']];width=.34
    fig,axes=plt.subplots(2,2,figsize=(13,8),layout='constrained')
    for column,budget in enumerate((8,32)):
        for row,metric in enumerate(('p50','p99')):
            ax=axes[row,column];x=np.arange(len(corpora));upper=1
            for j,(variant,label) in enumerate(READERS.items()):
                values=[get(c,'random_4096',budget,variant)[metric+'_us'] for c in corpora]
                low=[get(c,'random_4096',budget,variant)[metric+'_min_us'] for c in corpora]
                high=[get(c,'random_4096',budget,variant)[metric+'_max_us'] for c in corpora]
                upper=max(upper,max(high)*3)
                positions=x+(j-.5)*width
                ax.bar(positions,values,width,color=COLORS[j],label=label,
                       yerr=[np.array(values)-low,np.array(high)-values],capsize=3)
                for xx,value in zip(positions,values):ax.annotate(f'{value:g}',(xx,value),xytext=(0,6),textcoords='offset points',ha='center',fontsize=9)
            ax.set_yscale('log');ax.set_ylim(.05,upper)
            ax.set_xticks(x,[LABELS[c] for c in corpora]);ax.set_ylabel('Microseconds (log scale)')
            ax.set_title(f'{"Median" if metric=="p50" else "P99"} — {budget} MiB data-cache budget')
            ax.grid(axis='y',alpha=.2);ax.set_axisbelow(True)
    axes[0,0].legend(loc='upper right')
    fig.suptitle('Native C++ readers: repeated random 4 KiB reads',fontsize=17)
    fig.supxlabel('Files resolved before timing; bounded mapping on both sides. Error bars: min–max of 3 repetitions.\n1000 reads per pass; OS cache not flushed. Results describe these workloads, not FPS or every game.',fontsize=10)
    fig.savefig(output/'native-random-reads.png',dpi=160);plt.close(fig)
    fig,axes=plt.subplots(1,2,figsize=(13,5.6),layout='constrained')
    cases=[(c,b) for b in (8,32) for c in corpora];x=np.arange(len(cases))
    for ax,field,title in zip(axes,('peak_private_commit_mib','peak_resident_mib'),('Peak private commitment','Peak resident working set')):
        for j,(variant,label) in enumerate(READERS.items()):
            values=[get(c,'random_4096',b,variant)[field] for c,b in cases]
            bars=ax.bar(x+(j-.5)*width,values,width,color=COLORS[j],label=label)
            ax.bar_label(bars,fmt='%.1f',fontsize=8,padding=3)
        ax.set_xticks(x,[f'{LABELS[c]}\n{b} MiB' for c,b in cases],rotation=25,ha='right')
        ax.set_ylabel('MiB');ax.set_title(title);ax.grid(axis='y',alpha=.2);ax.set_axisbelow(True)
        ax.margins(y=.15)
    axes[0].legend()
    fig.suptitle('Memory: isolated reader processes, same requests and cache budgets',fontsize=16)
    fig.supxlabel('Median of 3 process peaks. Resident memory includes shared mapped file pages; private commitment is not physical RAM.\nRAW payload is not copied into the NX cache. Do not add mapped resident pages to the OS file cache again.',fontsize=9)
    fig.savefig(output/'native-memory.png',dpi=160);plt.close(fig)
    if 'real_A' in corpora:
        scenarios=['random_4096','sequential_32mib','interleaved_8files']
        names=['Random 4 KiB','Sequential 256 KiB','Interleaved 64 KiB']
        fig,axes=plt.subplots(1,2,figsize=(12,5.3),layout='constrained')
        for ax,field,title,unit in zip(axes,('p99_us','peak_resident_mib'),
                ('Read latency: P99','Peak resident working set'),('Microseconds','MiB')):
            for j,(variant,label) in enumerate(READERS.items()):
                values=[get('real_A',s,32,variant)[field] for s in scenarios]
                bars=ax.bar(np.arange(3)+(j-.5)*width,values,width,color=COLORS[j],label=label)
                ax.bar_label(bars,fmt='%.1f',padding=4,fontsize=10)
            ax.set_xticks(np.arange(3),names);ax.set_ylabel(unit);ax.set_title(title)
            ax.grid(axis='y',alpha=.2);ax.set_axisbelow(True);ax.margins(y=.2)
        axes[0].legend()
        fig.suptitle('Native readers: three access patterns on real archive A',fontsize=16)
        fig.supxlabel('32 MiB data-cache budget; repeated pass; median of 3 runs. Both readers use bounded mapped input.\nResident memory includes mapped pages; private commitment and all results are in the full report.',fontsize=10)
        fig.savefig(output/'representative-real-archive.png',dpi=160);plt.close(fig)
    fig,axes=plt.subplots(1,2,figsize=(12,5),layout='constrained')
    x=np.arange(len(corpora))
    for j,(variant,label) in enumerate(READERS.items()):
        values=[get(c,'random_4096',0,variant)['p99_us'] for c in corpora]
        bars=axes[0].bar(x+(j-.5)*width,values,width,color=COLORS[j],label=label)
        axes[0].bar_label(bars,fmt='%g',padding=4)
        values=[get(c,'random_4096',0,variant)['temporary_file_mib'] for c in corpora]
        bars=axes[1].bar(x+(j-.5)*width,values,width,color=COLORS[j],label=label)
        axes[1].bar_label(bars,fmt='%.1f',padding=4)
    axes[0].set_yscale('log');axes[0].set_ylabel('P99, microseconds (log scale)');axes[0].set_ylim(.1,300)
    axes[1].set_ylabel('Live temporary decoded-file storage, MiB');axes[1].set_ylim(0,40)
    for ax in axes:ax.set_xticks(x,[LABELS[c] for c in corpora]);ax.grid(axis='y',alpha=.2);ax.set_axisbelow(True)
    axes[0].legend();axes[0].set_title('Repeated random 4 KiB reads');axes[1].set_title('Disk-cache tradeoff')
    fig.suptitle('Counterexample: zero RAM payload cache does not mean no cache',fontsize=15)
    fig.supxlabel('NUP spills decoded Zstd files to temporary storage; NX does not.\nThis can favor NUP on repeated reads. Both readers use the same mapped input adapter.',fontsize=10)
    fig.savefig(output/'zero-cache-tradeoff.png',dpi=160);plt.close(fig)
    samples=output/'samples';samples.mkdir(exist_ok=True)
    for c in corpora:
        for scenario in ('random_4096','sequential_32mib'):
            budgets=(8,32) if scenario=='random_4096' else (32,)
            for b in budgets:
                for v in READERS:
                    for repetition in range(get(c,scenario,b,v)['repetitions']):
                        p=source/c/scenario/f'{b}-{repetition}-{v}.csv'
                        (samples/f'{c}-{scenario}-{b}-{repetition}-{v}.csv.gz').write_bytes(gzip.compress(p.read_bytes(),mtime=0))
    md='''# Native NUP/NX reader comparison — 15 November 2026

This replaces the earlier Python-NUP versus C++-NX comparison. Both readers are
native C++, compiled with the same MSVC Release flags and static Zstd library.
The NUP implementation is Eglantine's unmodified `NupContainer`; no emulator
source or game content is vendored. The reference NX reader is pinned to merged
1.0.4 commit `b6a9de8`; the feature reader adds verification-only RAW caching.

## Primary comparison

Files are resolved/opened before timing. Both sides use the same bounded Windows
mapping adapter. The new NX option requires a source that guarantees immutable
bytes; checksum validation is performed before the first use of each slice.
RAW payload is never retained in an application data cache. The verification
bitmap is bounded separately (1 MiB maximum, about 8 KiB per GiB of unique logical
data), and the usual Zstd payload-cache budget remains unchanged.

![Native random-read results](native-random-reads.png)

![Isolated process memory](native-memory.png)

The mapped provider keeps at most 64 address windows, normally 16 MiB each;
a boundary-crossing request can extend a window. These are file-backed views,
not private copies of RAW data. In-flight leases can keep an evicted view alive.
The figures include both resident memory and private commitment: neither alone
measures total system RAM. Windows file-cache pages outside the reader are not
counted. Do not add shared mapped pages to the system file cache a second time.
See [Microsoft's file mapping documentation](https://learn.microsoft.com/en-us/windows/win32/memory/file-mapping).

## Selected warm-pass numbers (32 MiB data-cache budget)

Median / P99 are in microseconds. Memory values are medians of process peaks.

| Corpus and scenario | NUP median / P99 | NX median / P99 | NUP private / resident MiB | NX private / resident MiB |
|---|---:|---:|---:|---:|
'''
    for c in corpora:
        for scenario in ('random_4096','hot_1mib','sequential_32mib','interleaved_8files'):
            n=get(c,scenario,32,'nup_mapped');x=get(c,scenario,32,'nx_verify_mapped')
            md+=f"| {LABELS[c]} · {scenario} | {n['p50_us']:g} / {n['p99_us']:g} | {x['p50_us']:g} / {x['p99_us']:g} | {n['peak_private_commit_mib']:.1f} / {n['peak_resident_mib']:.1f} | {x['peak_private_commit_mib']:.1f} / {x['peak_resident_mib']:.1f} |\n"
    md+='''
NUP is still faster in some warm-cache and sequential cases. Differences of
0.1 microsecond are close to the clock granularity and should not be presented
as a robust speedup. Under memory pressure, the per-file NUP LRU can repeatedly
decompress whole files; small NX blocks have a different cost. These are reader
and workload measurements, not a proof of universal format superiority.

## Zero-data-cache counterexample

![Zero-cache counterexample and temporary files](zero-cache-tradeoff.png)

With a zero RAM payload-cache budget, NUP's production reader spills decoded
files to temporary storage. NX currently decodes compressed blocks again on
misses instead of creating that disk cache. NUP can therefore win repeated
compressed-file reads at the cost of temporary disk space and OS file caching.
This case is included, not discarded from the results.

## Workloads, limits and reproducibility

- Synthetic RAW and mixed-compressibility corpora: identical logical 32 MiB,
  eight 4 MiB members, generated deterministically by
  `packer/examples/native_nup_benchmark.rs`. NUP stores each file RAW or as one
  Zstd frame; NX uses its normal RAW/Zstd decision and block sizing. Thus this
  compares concrete tools/policies, not an isolated codec or format-only effect.
- Real archive A: an already converted local archive, checked read-only. A seeded
  subset of 32 members (about 74 MiB logical) is used. No game title, member path,
  content identifier, proprietary bytes or private request manifest is published.
  This is one real corpus, not a representative sample of all games.
- Seven scenarios: size-weighted random 4/64/256 KiB reads, a hot region of at
  most 1 MiB, 90% hot/10% global reads, sequential 256 KiB traversal capped at 32 MiB,
  and 64 KiB reads interleaved across eight files.
- Data-cache budgets: 0, 8 and 32 MiB. Six variants retain stdio and mapped
  controls, including the original NX reader. Three fresh isolated processes
  per configuration, two trace passes, variant order rotated between repeats.
- First reader pass is **not cold storage**: the OS file cache is not flushed.
  Mapping, metadata parsing, file resolution and output checks are outside the
  individual read timers. Process peaks include their memory costs. Throughput
  sums timed read service, not whole-process elapsed time. This is not FPS.
- Native checksum policies differ: NX validates slices and caches their verified
  state for pinned immutable sources; NUP does not do that CRC check per read.
- Quantiles are computed per repetition, then their medians are reported; samples
  are not pooled. Error bars are observed min–max over three repeats, not confidence
  intervals. P99 on 1000 requests depends on ten tail samples. Sequential and
  interleaved traces are shorter; use their throughput as well as percentiles.

'''
    if 'real_A' not in corpora:
        start=md.index('- Real archive A:')
        end=md.index('- Seven scenarios:',start)
        md=md[:start]+md[end:]
    md+=f"The matrix ran **{len(report['records'])} isolated processes**, **{report['isolated_sampled_reads']:,} sampled reads** and separately compared **{report['fully_compared_reads']:,} complete read buffers**. Isolated modes cross-check a sampled checksum; that is distinct from the full-buffer comparisons.\n\n"
    md+='''[All aggregate configurations](results.csv) · [Per-repetition metrics and provenance](results.json) ·
[Selected individual samples](samples/). Older full-path lookup experiments are
kept locally as diagnostic controls, not used for the headline plots.

Run in a Windows MSVC developer shell with an existing Release Eglantine build:

```text
python benchmarks/run_low_memory_matrix.py --egl-build PATH --output results/reproduction --reads 1000 --repetitions 3 --budgets 0 8 32 --resolved
python benchmarks/publish_low_memory_results.py results/reproduction docs/benchmarks/low-memory-reader-cache
```

Optional `--real-nup FILE --real-nx FILE` uses matching local inputs. The external
NUP source revision and SHA-256 values are in `results.json`. The standalone
adapter links GPL-3.0-or-later emulator code and is GPL-3.0-or-later; that code or
its benchmark binary is not included in NXConvert release packages.
'''
    (output/'README.md').write_text(md,encoding='utf-8',newline='\n')

if __name__=='__main__':main()
