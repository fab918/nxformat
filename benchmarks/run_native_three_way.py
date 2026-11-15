"""Native NUP/NX comparison; only generated inputs are required.

Requires Windows/MSVC and an external Release Eglantine build. Its NUP source
is compiled in place, never copied into this repository. Results and any optional
private trace manifests are written only under --output.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import subprocess
import tempfile

from nup_vs_nx import parse, walk
from run_native_nup_comparison import ROOT, build_tool


def generate_cases(args):
    directory = args.output / 'private-traces'
    directory.mkdir(exist_ok=True)
    rng = random.Random(0x4e585517)
    cases = []
    for corpus in ('real', 'raw', 'mixed'):
        if corpus == 'real':
            if not args.nup:
                continue
            packages, dirs, files, children, _ = parse(args.nup)
            tree = []
            base = next(p for p in packages if p['kind'] == 0)
            walk(dirs, files, children, base['romfs'], 'romfs', tree)
            members = sorted(
                [(p, f) for p, f in tree if f['codec'] == 1 and f['size'] >= 131072],
                key=lambda x: x[1]['size'], reverse=True,
            )[:8]
            if len(members) != 8 or any(f['size'] < 65536 for _, f in members):
                raise RuntimeError('real corpus needs eight compressed members of at least 64 KiB')
            pair = (args.nup, args.nx)
        else:
            members = [(f'romfs/asset-{i}.bin', {'size': 4 * 2**20}) for i in range(8)]
            name = 'raw' if corpus == 'raw' else 'zstd'
            pair = (args.synthetic / f'{name}.nup', args.synthetic / f'{name}.nx')
        traces = {
            'random4k': [(p, rng.randrange(f['size'] - 4096 + 1), 4096)
                         for p, f in rng.choices(members, k=512)],
            'sequential64k': [],
        }
        total = 0
        for path, info in members:
            for offset in range(0, info['size'] - 65536 + 1, 65536):
                if total >= 32 * 2**20:
                    break
                traces['sequential64k'].append((path, offset, 65536))
                total += 65536
            if total >= 32 * 2**20:
                break
        if corpus == 'real':
            traces['clustered4k'], traces['bursty4k'] = [], []
            for _ in range(32):
                path, info = rng.choice(members)
                offset = rng.randrange((info['size'] - 65536) // 65536) * 65536
                traces['clustered4k'] += [(path, offset + rng.randrange(65536 - 4096 + 1), 4096)
                                          for _ in range(16)]
            for _ in range(64):
                path, info = rng.choice(members)
                offset = rng.randrange((info['size'] - 65536) // 65536) * 65536
                traces['bursty4k'] += [(path, offset + i * 4096, 4096) for i in range(8)]
        for scenario, trace in traces.items():
            manifest = directory / f'{corpus}-{scenario}-private.tsv'
            manifest.write_text(''.join(f'{p}\t{o}\t{n}\n' for p, o, n in trace), encoding='utf-8')
            budgets = (0, 8, 32) if scenario == 'clustered4k' else (
                (8, 32, 128) if corpus == 'real' and scenario == 'random4k' else (8, 32))
            for budget in budgets:
                for warm in (False, True):
                    cases.append((corpus, scenario, pair, manifest, budget, warm))
        if corpus == 'real':
            manifest = directory / 'first-touch-private.tsv'
            manifest.write_text(''.join(f"{p}\t{f['size']-65536}\t4096\n" for p, f in members), encoding='utf-8')
            for warm in (False, True):
                cases.append((corpus, 'first_touch4k', pair, manifest, 128, warm))
    return cases


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--egl-build', type=Path, required=True)
    parser.add_argument('--nup', type=Path, help='optional local real archive')
    parser.add_argument('--nx', type=Path, help='matching local NX archive')
    parser.add_argument('--synthetic', type=Path, help='reuse generated synthetic corpus')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repetitions', type=int, default=5)
    parser.add_argument('--main-ref', default='b6a9de8', help='pinned baseline used for the README')
    args = parser.parse_args()
    if not 1 <= args.repetitions <= 20:
        parser.error('repetitions must be 1..20')
    if bool(args.nup) != bool(args.nx):
        parser.error('supply both --nup and --nx')
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    if args.nup:
        args.nup, args.nx = args.nup.resolve(), args.nx.resolve()
    main_sha = subprocess.check_output(['git', 'rev-parse', args.main_ref], cwd=ROOT, text=True).strip()
    feature_sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    executables, provenance = {}, {}
    for key, original in [('main', True), ('feature', False)]:
        folder = args.output / key
        folder.mkdir(exist_ok=True)
        executables[key], provenance[key] = build_tool(
            args.egl_build.resolve(), folder, original, main_sha,
            ROOT / 'benchmarks/native_three_way_batch.cpp',
        )
        if provenance[key]['nup_sources_dirty']:
            raise RuntimeError('external NUP source must be unchanged')
    if args.synthetic:
        args.synthetic = args.synthetic.resolve()
    else:
        args.synthetic = args.output / 'synthetic'
        subprocess.run(['cargo', 'run', '--release', '--locked', '--manifest-path',
                        str(ROOT / 'packer/Cargo.toml'), '--example', 'native_nup_benchmark',
                        '--', str(args.synthetic)], check=True)
    records, expected = [], {}
    variants = ['nup', 'main', 'current', 'selective']
    for corpus, scenario, pair, trace, budget, warm in generate_cases(args):
        for repetition in range(args.repetitions):
            rotation = variants[repetition % 4:] + variants[:repetition % 4]
            for variant in rotation:
                executable = executables['main' if variant in ('main', 'nup') else 'feature']
                output = args.output / 'output.bin'
                environment = os.environ.copy()
                environment['NX_BENCH_MAPPED'] = '1'
                with tempfile.TemporaryDirectory(prefix='spill-', dir=args.output) as spill:
                    result = subprocess.run(
                        [str(executable), *map(str, pair), str(trace), str(output), str(budget),
                         spill, variant, str(int(warm))],
                        capture_output=True, text=True, check=True, env=environment,
                    )
                digest = hashlib.sha256(output.read_bytes()).hexdigest()
                key = (corpus, scenario)
                if key in expected and expected[key] != digest:
                    raise RuntimeError('full original-order output mismatch')
                expected[key] = digest
                records.append(dict(corpus=corpus, scenario=scenario, budget_mib=budget,
                                    warmup=warm, repetition=repetition, variant=variant,
                                    metrics=json.loads(result.stdout), output_sha256=digest))
            (args.output / 'progress.json').write_text(json.dumps({'records': records}, indent=2), encoding='utf-8')
        print(corpus, scenario, budget, warm, 'completed', flush=True)
    report = dict(main_commit=main_sha, feature_commit=feature_sha, provenance=provenance,
                  method='Native C++ readers, same compiler, static Zstd and mapped provider. '
                  'Common 16-pending-request windows and output buffers. No prefetch. '
                  'Only selective uses grouping; current uses ordinary feature reads. '
                  'RAW verification reuse enabled for feature variants. '
                  'Full output SHA-256 equality checked. No OS-cache flush. '
                  'Warmup primes the same reader but cannot guarantee the working set fits. '
                  'Memory peaks include common staging, metadata, priming and decoder scratch. '
                  'Temporary decoded files counted separately. Generated traces, not gameplay.',
                  records=records)
    (args.output / 'results.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    print('completed', len(records), 'isolated runs')


if __name__ == '__main__':
    main()
