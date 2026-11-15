"""C++ NUP/NX comparison using an existing Release MSVC Eglantine build.

Run in a VS developer shell:
  python benchmarks/run_native_nup_comparison.py --egl-build PATH --output OUT

Compiles the unmodified external NupContainer source and our GPL adapter, using
the emulator build's flags and libraries. No emulator source/binary is vendored.
Requires compile_commands.json, build.ninja and the build's static libraries.
The output uses synthetic data only and contains every individual read sample.
"""
from __future__ import annotations
import argparse
import csv
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import statistics
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]

def argv(command):
    count = ctypes.c_int()
    parse = ctypes.windll.shell32.CommandLineToArgvW
    parse.argtypes = [ctypes.c_wchar_p, ctypes.POINTER(ctypes.c_int)]
    parse.restype = ctypes.POINTER(ctypes.c_wchar_p)
    pointer = parse(command, ctypes.byref(count))
    if not pointer:
        raise RuntimeError("cannot parse MSVC command")
    try:
        return [pointer[i] for i in range(count.value)]
    finally:
        ctypes.windll.kernel32.LocalFree.argtypes = [ctypes.c_void_p]
        ctypes.windll.kernel32.LocalFree(pointer)

def run(command, **kwargs):
    return subprocess.run([str(x) for x in command], check=True, **kwargs)

def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def summarize(rows, requests, label, budget, repetition):
    summaries = []
    for phase in (0, 1):
        for size in (4096, 65536, 262144):
            selected = [r for r in rows if int(r['pass']) == phase and int(r['bytes']) == size]
            for reader in ('nup', 'nx'):
                times = sorted(int(r[f'{reader}_ns'])/1000 for r in selected)
                summaries.append(dict(profile=label, cache_mib=budget, repetition=repetition,
                    phase='first-pass' if phase == 0 else 'repeat-pass', bytes=size,
                    reader=reader, count=len(times), p50_us=statistics.median(times),
                    p99_us=times[math.ceil(len(times)*.99)-1], mean_us=statistics.fmean(times),
                    max_us=times[-1], total_ms=sum(times)/1000))
    return summaries

def build_tool(build, output, original=False, original_ref="b6a9de8", adapter=None):
    entry = next(x for x in json.loads((build/'compile_commands.json').read_text())
                 if x['file'].replace('\\', '/').endswith('/nup_container.cpp'))
    source = Path(entry['file'])
    flags = argv(entry['command'])
    compiler = flags[0]
    flags = [x for x in flags[1:] if not x.startswith(('/Fo', '/Fd'))
             and x not in ('-c', '/c')
             and x.replace('\\', '/').lower() != entry['file'].replace('\\', '/').lower()]
    objects = []
    if original:
        (output/'original_nx_reader.hpp').write_bytes(subprocess.check_output(
            ['git', 'show', f'{original_ref}:reader/nx_reader.hpp'], cwd=ROOT))
        flags.extend(['-DNX_BENCH_ORIGINAL', f'-I{output}'])
    with (output/'build.log').open('w', encoding='utf-8') as log:
        for name, path in [('adapter', adapter or ROOT/'benchmarks/native_nup_comparison.cpp'), ('nup', source)]:
            obj = output/f'{name}.obj'
            run([compiler, *flags, f'-I{ROOT / "dist"}', '-c', path, f'/Fo{obj}', f'/Fd{output / "bench.pdb"}'],
                cwd=build, stdout=log, stderr=subprocess.STDOUT)
            objects.append(obj)
        line = next(x for x in (build/'build.ninja').read_text().splitlines()
                    if x.startswith('  LINK_LIBRARIES =') and 'core.lib' in x)
        libraries = argv('link '+line.split(' = ', 1)[1])[1:]
        # Some unrelated emulator targets are optional/not built (e.g. Catch2).
        libraries = [x for x in libraries if (build/x).exists() or ('/' not in x and '\\' not in x)]
        exe = output/'native-nup-comparison.exe'
        run(['link', '/NOLOGO', f'/OUT:{exe}', *objects, *libraries], cwd=build,
            stdout=log, stderr=subprocess.STDOUT)
    repo = source.parents[3]
    revision = subprocess.check_output(['git', '-C', repo, 'rev-parse', 'HEAD'], text=True).strip()
    dirty = subprocess.check_output(['git', '-C', repo, 'status', '--porcelain', '--',
                                    'src/core/file_sys/nup_container.cpp', 'src/core/file_sys/nup_container.h'], text=True).strip()
    return exe, {'repository_commit': revision, 'nup_source_sha256': digest(source),
                 'nup_header_sha256': digest(source.with_suffix('.h')), 'nup_sources_dirty': bool(dirty),
                 'nx_header_sha256': digest(output/'original_nx_reader.hpp' if original else ROOT/'dist/nx_reader.hpp'),
                 'original_reader': original, 'compiler': Path(compiler).name,
                 'nx_reference_commit': original_ref if original else None,
                 'adapter_sha256': digest(adapter or ROOT/'benchmarks/native_nup_comparison.cpp'),
                 'mapped_views_sha256': digest(ROOT/'dist/nx_mapped_views.hpp')}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--egl-build', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--reads', type=int, default=1000)
    parser.add_argument('--repetitions', type=int, default=3)
    parser.add_argument('--corpus', type=Path, help='reuse an existing synthetic corpus for controlled comparisons')
    parser.add_argument('--original-reader', action='store_true', help='compile the pinned 1.0.4 reader as a control')
    parser.add_argument('--reader-mode', choices=('both', 'nx', 'nup'), default='both',
                        help='use separate nx/nup processes when measuring each reader memory')
    args = parser.parse_args()
    if os.name != 'nt':
        parser.error('this adapter currently requires Windows/MSVC')
    if args.reads < 1 or args.repetitions < 1:
        parser.error('reads and repetitions must be positive')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    exe, provenance = build_tool(args.egl_build.resolve(), output, args.original_reader)
    corpus = args.corpus.resolve() if args.corpus else output/'corpus'
    if not args.corpus:
        run(['cargo', 'run', '--release', '--locked', '--manifest-path', ROOT/'packer/Cargo.toml',
             '--example', 'native_nup_benchmark', '--', corpus])
    rng = random.Random(0x4e582026)
    requests = [(f'romfs/asset-{rng.randrange(8)}.bin', rng.randrange(4*2**20-size+1), size)
                for size in (4096, 65536, 262144) for _ in range(args.reads)]
    listing = output/'requests.tsv'
    listing.write_text(''.join(f'{p}\t{o}\t{n}\n' for p,o,n in requests), encoding='utf-8')
    summaries = []
    executions = []
    for label in ('raw', 'zstd'):
        for budget in (8, 512):
            for repetition in range(args.repetitions):
                samples = output/f'{label}-{budget}mib-{repetition}.csv'
                with tempfile.TemporaryDirectory(prefix='nup-bench-spill-', dir=output) as spill:
                    environment = os.environ.copy()
                    environment['NX_BENCH_ONLY'] = args.reader_mode
                    result = run([exe, corpus/f'{label}.nup', corpus/f'{label}.nx', listing, samples,
                                  budget, 2, spill], capture_output=True, text=True, env=environment)
                rows = list(csv.DictReader(samples.open(encoding='utf-8')))
                assert len(rows) == len(requests)*2
                summaries.extend(x for x in summarize(rows, requests, label, budget, repetition)
                                 if args.reader_mode == 'both' or x['reader'] == args.reader_mode)
                executions.append(dict(profile=label, cache_mib=budget, repetition=repetition,
                                       output=result.stdout.strip(),
                                       metrics={k:int(v) for k,v in re.findall(r'(\w+)=(\d+)',result.stdout)}))
                print(label, budget, repetition, result.stdout.strip(), flush=True)
    provenance['adapter_sha256'] = digest(ROOT/'benchmarks/native_nup_comparison.cpp')
    provenance['raw_verification'] = (not args.original_reader and args.reader_mode != 'nup' and
                                      'NX_BENCH_RAW_VERIFY' in os.environ)
    provenance['reader_mode'] = args.reader_mode
    report = dict(provenance=provenance, os=platform.platform(), cpu=platform.processor(), executions=executions,
                  requests_sha256=digest(listing), corpus_sha256={p.name: digest(p) for p in corpus.iterdir()},
                  methodology='Same compiler/static Zstd library, stdio backing, requests, cache budget. '
                  'NUP production LRU/spill; NX verified-block LRU with default CRC verification. '
                  'Both mode alternates reader order and compares buffers outside timers. '
                  'Single-reader modes compare sampled output checksums across runs; use both mode to fully check bytes. '
                  'OS cache not flushed. '
                  'First pass is NOT all cold misses; repeated pass preserves each reader cache. '
                  'Each repetition starts a new process. No open/mount or packing timings.',
                  summaries=summaries)
    (output/'results.json').write_text(json.dumps(report, indent=2)+'\n', encoding='utf-8')

if __name__ == '__main__':
    main()
