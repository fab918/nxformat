"""Stream a valid multi-GiB NX with unique, non-reproducible Zstd frames.

Run: uv run --python 3.14 --with google-crc32c benchmarks/restore_unreproducible.py
Requires about 3 * --gib GiB of temporary disk; uses bounded payload memory.
Frames include Zstd checksums, which the NX writer's encoder does not emit.
Thus every candidate must be rejected byte-for-byte, never approximated.
"""
from pathlib import Path
from compression import zstd
import argparse, hashlib, json, random, shutil, struct, subprocess, sys, tempfile, time
import google_crc32c

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests/conformance"))
import build_fixtures as fixture

class StreamedFile:
    def __init__(self, size, digest): self.size, self.digest = size, digest
    def __len__(self): return self.size

def generate(directory, gib, raw=False):
    size = int(gib * 1024**3)
    archive = directory / "foreign.nx"
    tree = directory / "extracted"
    (tree / "base/romfs").mkdir(parents=True)
    records, checks = [], []
    whole = hashlib.sha256()
    rng = random.Random(918)
    with archive.open("wb") as output, (tree / "base/romfs/payload.bin").open("wb") as extracted:
        output.write(bytes(fixture.DATA_START))
        for _ in range(size // 1048576):
            logical = rng.randbytes(1048576)
            whole.update(logical)
            extracted.write(logical)
            stored = logical if raw else zstd.compress(logical, options={zstd.CompressionParameter.compression_level: 3, zstd.CompressionParameter.checksum_flag: 1})
            output.write(bytes(fixture.align(output.tell(), 16) - output.tell()))
            records.append((output.tell(), len(stored), len(logical), 0 if raw else 1, 0, 0, len(checks)))
            checks.extend(google_crc32c.value(logical[i:i + fixture.CHECK_SIZE]) for i in range(0, len(logical), fixture.CHECK_SIZE))
            output.write(stored)
        data_end = output.tell()
        original_sha = fixture.sha
        fixture.sha = lambda b: b.digest if isinstance(b, StreamedFile) else original_sha(b)
        app = 0x0100000000000001
        snapshots = [{"root": 0, "program": 0, "type": 0, "version": 0}]
        manifest, logical_sizes, identities = fixture.make_manifest(
            [[("romfs", 1, 1)], [("payload.bin", 2, 0)]],
            [{"bytes": StreamedFile(size, whole.digest()), "block_ids": list(range(len(records))), "chunk_log2": 20}],
            [0], 1, app, [app], [(0, 0)])
        catalog = fixture.make_catalog(app, 1, "Foreign Zstd benchmark", None, [{"id": app, "application": 0}], snapshots, logical_sizes, identities, [], fixture.NO_INDEX)
        def put(blob):
            output.write(bytes(fixture.align(output.tell(), 16) - output.tell()))
            offset = output.tell(); output.write(blob); return offset
        catalog_at = put(catalog)
        manifest_at = put(manifest)
        index = b"".join(fixture.BLOCK.pack(*r) for r in records)
        index_at = put(index)
        integrity = b"".join(struct.pack("<I", c) for c in checks)
        integrity_at = put(integrity)
        header = bytearray(fixture.DATA_START)
        fixture.finish_header(header, (output.tell(), catalog_at, len(catalog), 0, 0,
            fixture.DATA_START, data_end - fixture.DATA_START, manifest_at, len(manifest),
            index_at, len(records), integrity_at, len(integrity)), catalog, manifest, index + integrity)
        output.seek(0); output.write(header)
    return archive, tree, len(records), whole.hexdigest()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gib", type=int, default=3)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--verify", action="store_true", help="also restore and verify exact NX bytes; needs one more archive of disk space")
    args = parser.parse_args()
    if not 1 <= args.gib <= 64: parser.error("--gib must be 1..64")
    if shutil.disk_usage(tempfile.gettempdir()).free < ((4 if args.verify else 3) * args.gib + 1) * 1024**3:
        parser.error("not enough temporary disk for source, extracted bytes and literal recipe")
    subprocess.run(["cargo", "build", "--release", "--locked", "--manifest-path", str(ROOT / "packer/Cargo.toml"), "--example", "restoration_bench"], check=True)
    exe = ROOT / "packer/target/release/examples" / ("restoration_bench.exe" if sys.platform == "win32" else "restoration_bench")
    with tempfile.TemporaryDirectory(prefix="nx-foreign-zstd-") as tmp:
        started = time.perf_counter()
        archive, tree, blocks, content_sha = generate(Path(tmp), args.gib)
        generated_seconds = time.perf_counter() - started
        command = [str(exe), str(archive), str(tree)]
        if args.verify: command.append("--verify")
        result = subprocess.run(command, capture_output=True, text=True)
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        report = json.loads(result.stdout.strip().splitlines()[-1])
        report.update(logical_bytes=args.gib * 1024**3, blocks=blocks, legacy_worst_case_attempts=blocks * 65,
            payload_sha256=content_sha, generation_seconds=generated_seconds)
        if args.verify: assert report["restored_exactly"] is True
        assert report["literal_fallback_blocks"] == blocks
        assert report["exhaustive_searches"] == 4
        assert report["compression_attempts"] <= blocks + 4 * 65
        print(json.dumps(report, indent=2))
        if args.report: args.report.write_text(json.dumps(report, indent=2) + "\n")

if __name__ == "__main__": main()
