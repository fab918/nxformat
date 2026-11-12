"""Compare recipe creation against a previously built restoration_bench binary.

uv run --python 3.14 --with google-crc32c benchmarks/restore_read_reuse.py \
    --before /path/to/baseline/restoration_bench --report /tmp/read-reuse.json
Uses artificial unique RAW and foreign-Zstd blocks, never game data. All large
files live in TemporaryDirectory and are removed. Requires about 4 GiB at --gib 1.
"""
from pathlib import Path
import argparse
import hashlib
import json
import platform
import shutil
import struct
import statistics
import subprocess
import tempfile
import time
import restore_unreproducible as fixture

ROOT = Path(__file__).resolve().parents[1]

def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--before-revision', default='unspecified', help='revision used to build --before')
    parser.add_argument('--gib', type=int, default=1, choices=range(1,9))
    parser.add_argument('--samples', type=int, default=3, choices=range(1,10))
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    before = args.before.resolve(strict=True)
    if shutil.disk_usage(tempfile.gettempdir()).free < (4*args.gib+1)*1024**3:
        parser.error('not enough temporary disk for source, exported payload, recipe and exact restored NX')
    subprocess.run(['cargo','build','--release','--locked','--manifest-path',str(ROOT/'packer/Cargo.toml'),'--example','restoration_bench'], cwd=ROOT, check=True)
    after = ROOT/'packer/target/release/examples/restoration_bench'
    report = {'platform': platform.platform(), 'baseline_revision': args.before_revision, 'after_change': 'Borrow decoded RAW/Zstd source buffers instead of allocating and rereading each block', 'logical_bytes_per_case': args.gib*1024**3,
              'samples_per_binary': args.samples,
              'method': 'Same source files and filesystem, alternating binary order; no OS cache flush. Recipe SHA-256 compared across all runs. Baseline must be built before the buffer-reuse change.',
              'limitations': 'Elapsed times include inventory hashing and compression. Logical reads eliminated are derived from stored block lengths, not measured physical device I/O. Warm OS cache may absorb redundant reads; runs are not CPU-isolated and these figures do not predict slow-device performance.',
              'cases': []}
    with tempfile.TemporaryDirectory(prefix='nx-read-reuse-') as tmp:
        for codec in ['raw','foreign-zstd']:
            directory = Path(tmp)/codec
            directory.mkdir()
            archive, tree, blocks, payload_sha = fixture.generate(directory,args.gib,raw=codec=='raw')
            recipe = tree/'.nxrestore.zst'
            runs = []
            recipe_sha = None
            for sample in range(args.samples):
                for label in (['before','after'] if sample%2==0 else ['after','before']):
                    executable = before if label=='before' else after
                    started = time.perf_counter()
                    run = subprocess.run([str(executable),str(archive),str(tree)],capture_output=True,text=True,check=True)
                    wall = time.perf_counter()-started
                    stats = json.loads(run.stdout.strip().splitlines()[-1])
                    actual = digest(recipe)
                    if recipe_sha is None: recipe_sha = actual
                    assert actual == recipe_sha, 'recipe bytes changed across binaries'
                    runs.append({'binary':label,'sample':sample,'process_wall_seconds':wall,**stats})
                    recipe.unlink()
                    print(f'{codec} {label} sample={sample} recipe_seconds={stats["elapsed_seconds"]:.3f}',flush=True)
            # Exact unchanged replay is verified separately from timed samples.
            checked = subprocess.run([str(after),str(archive),str(tree),'--verify'],capture_output=True,text=True,check=True)
            assert json.loads(checked.stdout.strip().splitlines()[-1])['restored_exactly'] is True
            with archive.open('rb') as source:
                assert hashlib.file_digest(source,'sha256').hexdigest() == digest(directory/'verified-restoration.nx')
            # The generator emits one unique stored block per MiB; use the
            # source block table to count bytes, excluding headers/alignment.
            with archive.open('rb') as source:
                page = source.read(fixture.fixture.DATA_START)
                # Decode the canonical NX header via fixture's documented offsets.
                index_at = struct.unpack_from('<Q',page,88)[0]
                source.seek(index_at)
                records = [fixture.fixture.BLOCK.unpack(source.read(fixture.fixture.BLOCK.size)) for _ in range(blocks)]
            eliminated = sum(r[1] for r in records)
            medians = {label:statistics.median(r['elapsed_seconds'] for r in runs if r['binary']==label) for label in ['before','after']}
            report['cases'].append({'codec':codec,'blocks':blocks,'archive_bytes':archive.stat().st_size,'payload_sha256':payload_sha,'recipe_sha256':recipe_sha,'exact_replay_verified':True,'logical_stored_block_rereads_eliminated_bytes':eliminated,'median_recipe_seconds':medians,'runs':runs})
            # Keep peak disk independent of the number of cases.
            shutil.rmtree(directory)
    if args.report: args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2))

if __name__=='__main__': main()
