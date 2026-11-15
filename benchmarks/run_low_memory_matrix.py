"""Read scenarios, isolated memory and byte checks for native NUP/NX readers.

Windows/MSVC developer shell, existing Release Eglantine build required.
Private game paths and request lists stay under --output; publish aggregates only.
Example:
  python benchmarks/run_low_memory_matrix.py --egl-build PATH --output results/matrix
Optional --real-nup FILE --real-nx FILE uses an already converted matching archive.
"""
from __future__ import annotations
import argparse
import csv
import json
import math
import os
from pathlib import Path
import random
import re
import statistics
import subprocess
import tempfile
import time

from nup_vs_nx import parse, walk
from run_native_nup_comparison import ROOT, build_tool, digest, run

VARIANTS = {
    'nup_stdio': ('nup', False, False, True),
    'nx_original_stdio': ('nx', False, False, True),
    'nx_verify_stdio': ('nx', False, True, False),
    'nup_mapped': ('nup', True, False, True),
    'nx_original_mapped': ('nx', True, False, True),
    'nx_verify_mapped': ('nx', True, True, False),
}

def files(nup):
    packages, directories, members, children, _ = parse(nup)
    base = next(p for p in packages if p['kind'] == 0)
    result = []
    walk(directories, members, children, base['romfs'], 'romfs', result)
    return [(path, member) for path, member in result if 262144 <= member['size'] <= 16*2**20]

def select_files(tree, real):
    if not real:
        return tree
    rng = random.Random(0x4e581504)
    shuffled = list(tree)
    rng.shuffle(shuffled)
    chosen, total = [], 0
    for member in shuffled:
        if total + member[1]['size'] > 128*2**20:
            continue
        chosen.append(member)
        total += member[1]['size']
        if len(chosen) == 32:
            break
    if len(chosen) < 4:
        raise RuntimeError('real corpus has too few bounded RomFS members')
    return chosen

def scenarios(tree, reads):
    rng = random.Random(0x4e581515)
    weights = [f['size'] for _, f in tree]
    result = {}
    for size in (4096, 65536, 262144):
        picked = rng.choices(tree, weights=weights, k=reads)
        result[f'random_{size}'] = [(p, rng.randrange(0, f['size']-size+1), size) for p,f in picked]
    hot = [tree[0], next((x for x in tree if x[1]['codec'] != tree[0][1]['codec']), tree[len(tree)//2])]
    result['hot_1mib'] = []
    result['mixed_90hot'] = []
    for _ in range(reads):
        p, f = rng.choice(hot)
        request = (p, rng.randrange(0, min(f['size'], 512*1024)-4096+1), 4096)
        result['hot_1mib'].append(request)
        if rng.randrange(10):
            result['mixed_90hot'].append(request)
        else:
            p, f = rng.choices(tree, weights=weights)[0]
            result['mixed_90hot'].append((p, rng.randrange(f['size']-4096+1), 4096))
    sequential, total = [], 0
    for p,f in tree:
        for offset in range(0, f['size'], 262144):
            n = min(262144, f['size']-offset)
            if total+n > 32*2**20:
                break
            sequential.append((p, offset, n))
            total += n
        if total >= 32*2**20:
            break
    result['sequential_32mib'] = sequential
    interleaved = []
    for offset in range(0, 512*1024, 65536):
        for p,f in tree[:8]:
            if offset+65536 <= f['size']:
                interleaved.append((p, offset, 65536))
    result['interleaved_8files'] = interleaved
    return result

def execute(exe, corpus, requests, output, budget, mode, mapped, verify, resolved=False):
    environment = os.environ.copy()
    for key in ('NX_BENCH_RAW_REUSE','NX_BENCH_RAW_VERIFY','NX_BENCH_MAPPED','NX_BENCH_ONLY'):
        environment.pop(key, None)
    environment['NX_BENCH_ONLY'] = mode
    if mapped:
        environment['NX_BENCH_MAPPED'] = '1'
    if verify:
        environment['NX_BENCH_RAW_VERIFY'] = '1'
    environment.pop('NX_BENCH_RESOLVED',None)
    if resolved:
        environment['NX_BENCH_RESOLVED']='1'
    with tempfile.TemporaryDirectory(prefix='spill-', dir=output.parent) as spill:
        result = subprocess.run([str(exe), str(corpus[0]), str(corpus[1]), str(requests), str(output),
                                 str(budget), '2', spill], check=True, capture_output=True,
                                text=True, env=environment)
    metrics = {k:int(v) for k,v in re.findall(r'(\w+)=(\d+)', result.stdout)}
    rows = list(csv.DictReader(output.open(encoding='utf-8')))
    return metrics, rows

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--egl-build', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--synthetic-corpus', type=Path)
    parser.add_argument('--real-nup', type=Path)
    parser.add_argument('--real-nx', type=Path)
    parser.add_argument('--reads', type=int, default=500)
    parser.add_argument('--repetitions', type=int, default=3)
    parser.add_argument('--budgets', type=int, nargs='+', default=[8,32,128])
    parser.add_argument('--skip-build', action='store_true')
    parser.add_argument('--resolved', action='store_true', help='resolve/open files before timing their reads')
    a = parser.parse_args()
    if bool(a.real_nup) != bool(a.real_nx):
        parser.error('supply both real archive paths')
    if a.reads < 1 or a.repetitions < 1 or any(b<0 for b in a.budgets):
        parser.error('reads and repetitions must be positive; budgets must be nonnegative')
    out = a.output.resolve()
    out.mkdir(parents=True, exist_ok=True)
    executables, provenance = {}, {}
    for name, original in [('original',True),('verification',False)]:
        folder = out/name
        folder.mkdir(exist_ok=True)
        if not a.skip_build:
            exe, meta = build_tool(a.egl_build.resolve(), folder, original)
            (folder/'provenance.json').write_text(json.dumps(meta,indent=2)+'\n', encoding='utf-8')
        executables[name] = folder/'native-nup-comparison.exe'
        provenance[name] = json.loads((folder/'provenance.json').read_text(encoding='utf-8'))
    generated = a.synthetic_corpus.resolve() if a.synthetic_corpus else out/'synthetic'
    if not a.synthetic_corpus:
        run(['cargo','run','--release','--locked','--manifest-path',ROOT/'packer/Cargo.toml',
             '--example','native_nup_benchmark','--',generated])
    corpora = {name:(generated/f'{name}.nup',generated/f'{name}.nx') for name in ('raw','zstd')}
    if a.real_nup:
        corpora['real_A'] = (a.real_nup.resolve(),a.real_nx.resolve())
    records, checks, corpus_info = [], [], {}
    journal = out/'journal.jsonl'
    with journal.open('w',encoding='utf-8') as log:
        for label, corpus in corpora.items():
            chosen = select_files(files(corpus[0]), label=='real_A')
            corpus_info[label] = dict(nup_bytes=corpus[0].stat().st_size,nx_bytes=corpus[1].stat().st_size,
                                     selected_members=len(chosen),selected_logical_bytes=sum(f['size'] for _,f in chosen),
                                     selected_raw_members=sum(f['codec']==0 for _,f in chosen),
                                     selected_zstd_members=sum(f['codec']==1 for _,f in chosen))
            for scenario, requests in scenarios(chosen,a.reads).items():
                folder = out/label/scenario
                folder.mkdir(parents=True,exist_ok=True)
                listing = folder/'private_requests.tsv'
                listing.write_text(''.join(f'{p}\t{o}\t{n}\n' for p,o,n in requests),encoding='utf-8')
                for mapped in (False, True):
                    metrics, rows = execute(executables['verification'],corpus,listing,
                                            folder/f'correctness-{int(mapped)}.csv',a.budgets[-1],'both',mapped,True,a.resolved)
                    if metrics['verified_reads'] != len(requests)*2:
                        raise RuntimeError('incomplete full-byte comparison')
                    checks.append(dict(corpus=label,scenario=scenario,mapped=mapped,reads=metrics['verified_reads']))
                for budget in a.budgets:
                    for repetition in range(a.repetitions):
                        variants = list(VARIANTS)
                        shift = repetition%len(variants)
                        variants = variants[shift:]+variants[:shift]
                        checksums = []
                        for variant in variants:
                            mode,mapped,verify,original = VARIANTS[variant]
                            sample = folder/f'{budget}-{repetition}-{variant}.csv'
                            metrics, rows = execute(executables['original' if original else 'verification'],
                                                    corpus,listing,sample,budget,mode,mapped,verify,a.resolved)
                            if len(rows)!=len(requests)*2:
                                raise RuntimeError('incomplete samples')
                            checksums.append(metrics['sample_checksum'])
                            record = dict(corpus=label,scenario=scenario,cache_mib=budget,
                                          repetition=repetition,variant=variant,metrics=metrics,phases=[])
                            for phase in (0,1):
                                selected = [r for r in rows if int(r['pass'])==phase]
                                ns = sorted(int(r[f'{mode}_ns']) for r in selected)
                                record['phases'].append(dict(phase=phase,count=len(ns),
                                    p50_us=statistics.median(ns)/1000,p99_us=ns[math.ceil(len(ns)*.99)-1]/1000,
                                    total_ms=sum(ns)/1e6,read_mib_per_second=sum(int(r['bytes']) for r in selected)/(2**20)/(sum(ns)/1e9)))
                            records.append(record)
                            log.write(json.dumps(record)+'\n');log.flush()
                        if len(set(checksums))!=1:
                            raise RuntimeError('isolated sampled outputs disagree')
                    print(label,scenario,budget,'completed',flush=True)
            (out/'progress.json').write_text(json.dumps(dict(records=records,checks=checks,corpora=corpus_info),indent=2),encoding='utf-8')
    report = dict(provenance=provenance,corpora=corpus_info,records=records,checks=checks,resolved_handles=a.resolved,
                  fully_compared_reads=sum(x['reads'] for x in checks),isolated_sampled_reads=sum(sum(p['count'] for p in r['phases']) for r in records),
                  method='Fresh isolated processes; same request trace; rotating variant order; two passes. '
                  'First reader pass is not a cold-disk measurement. OS file cache uncontrolled. '
                  'NUP C++ Eglantine source recompiled unchanged; NX original from pinned 1.0.4 commit b6a9de8. '
                  'Stdio compared with stdio; mapping compared with mapping. Windows peak resident '
                  'and private-commit memory include executable, metadata and scratch. Source file mapped '
                  'pages are included in resident memory but not private commitment. '
                  'Spill bytes are live temporary-file bytes at end of reads. Service throughput sums timed reads '
                  'and excludes CSV output, full-byte comparisons and open/mount. Private request paths are not for publication.')
    (out/'results.json').write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8')
    print('Completed',len(records),'isolated processes; full compared reads',report['fully_compared_reads'])

if __name__=='__main__':
    main()
