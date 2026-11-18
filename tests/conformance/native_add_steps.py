"""Run the native_add example step by step and check every intermediate NX."""
from __future__ import annotations

import contextlib
import ctypes
import hashlib
import mmap
import struct
import subprocess
import sys
from pathlib import Path

from test_native_conversion import compile_reader
from test_rust_writer import load_validator

ROOT = Path(__file__).resolve().parents[2]
EXE = ROOT / "packer/target/release/examples" / ("native_add.exe" if sys.platform == "win32" else "native_add")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


@contextlib.contextmanager
def deny_original_reads(paths):
    """On Windows, lock the original dumps so add cannot read them."""
    if sys.platform != "win32":
        yield
        return
    create = ctypes.windll.kernel32.CreateFileW
    create.argtypes = [ctypes.c_wchar_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p]
    create.restype = ctypes.c_void_p
    close = ctypes.windll.kernel32.CloseHandle
    close.argtypes = [ctypes.c_void_p]
    handles = []
    try:
        for path in paths:
            handle = create(str(path), 0x80000000, 0, None, 3, 0x80, None)
            if handle == ctypes.c_void_p(-1).value:
                raise OSError(f"cannot lock {path}")
            handles.append(handle)
        yield
    finally:
        for handle in handles:
            close(handle)


def archive_evidence(path, validator, full):
    """Snapshot identities and Build IDs of a validated archive."""
    with path.open("rb") as stream, mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_READ) as data:
        validator.validate_archive(data, verify_payloads=full)
        cat = struct.unpack_from("<Q", data, 24)[0]
        count = struct.unpack_from("<I", data, cat + 28)[0]
        table = cat + struct.unpack_from("<Q", data, cat + 64)[0]
        return [data[table + i * 120 + 56:table + i * 120 + 120] for i in range(count)]


def retained_frames(before, after):
    """Check every block of `before` is still in `after` with the same bytes."""
    def records(data):
        table, count = struct.unpack_from("<QQ", data, 88)
        integrity = struct.unpack_from("<Q", data, 104)[0]
        result = {}
        for i in range(count):
            off, stored, logical, codec, _, _, first = struct.unpack_from("<QIIBBHI", data, table + i * 24)
            slices = (logical + 16383) // 16384
            key = (logical, codec, data[integrity + first * 4:integrity + (first + slices) * 4])
            result.setdefault(key, []).append((off, stored))
        return result

    with before.open("rb") as a, after.open("rb") as b:
        with mmap.mmap(a.fileno(), 0, access=mmap.ACCESS_READ) as old, mmap.mmap(b.fileno(), 0, access=mmap.ACCESS_READ) as new:
            previous = records(old)
            current = records(new)
            for key, ranges in previous.items():
                for off, size in ranges:
                    assert any(size == n and old[off:off + size] == new[p:p + n] for p, n in current.get(key, [])), "retained frame changed"
            return sum(map(len, previous.values()))


def run(output, sources):
    output.mkdir(parents=True, exist_ok=True)
    sources = [p.resolve() for p in sources]
    before = {str(p): digest(p) for p in sources}
    subprocess.run([str(EXE), "prepare", str(output), *map(str, sources)], cwd=ROOT, check=True)
    reader = compile_reader(output)
    validator = load_validator()
    current = output / "base.nx"
    results = []
    for i, line in enumerate((output / "jobs.tsv").read_text().splitlines()):
        source, title, version = line.split("\t")
        added = output / f"step-{i}.nx"
        rebuilt = output / f"rebuilt-{i}.nx"
        original = digest(current)
        with deny_original_reads(sources):
            subprocess.run([str(EXE), "add", str(current), str(added), source, title, version], cwd=ROOT, check=True)
        assert digest(current) == original
        subprocess.run([str(EXE), "rebuild", str(current), str(rebuilt), title, version, *map(str, sources)], cwd=ROOT, check=True)
        same = archive_evidence(added, validator, False) == archive_evidence(rebuilt, validator, False)
        assert same, "snapshot identities or Build IDs differ"
        frames = retained_frames(current, added)
        for path in [added, rebuilt]:
            subprocess.run([str(EXE), "verify", str(path), *map(str, sources)], cwd=ROOT, check=True)
            subprocess.run([str(reader), str(path), str(path.with_suffix(".expected.tsv")), str(path.with_suffix(".selection.tsv"))], cwd=ROOT, check=True)
        results.append({"step": i, "identities_and_build_ids_equal": same, "retained_frames_byte_identical": frames})
        current = added
    assert before == {str(p): digest(p) for p in sources}, "original dump changed"
    return {"results": results}
