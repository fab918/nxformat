"""Tests for nx::Session (synthetic content)."""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]


def run(command: list[str]) -> None:
    subprocess.run(command, cwd=ROOT, check=True, timeout=180)


def main() -> None:
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument("--cases", type=Path, help="reuse runtime_cases output without Cargo")
    parser.add_argument("--sanitize", choices=("address,undefined", "thread"))
    args = parser.parse_args()
    run([os.sys.executable, str(ROOT / "reader/generate_header.py"), "--check"])
    compiler = shutil.which(os.environ.get("CXX", "")) or shutil.which("c++") or shutil.which("cl")
    if not compiler:
        raise SystemExit("A C++17 compiler is required")
    msvc = Path(compiler).name.lower() in {"cl", "cl.exe"}
    with tempfile.TemporaryDirectory(prefix="nx-session-") as temporary:
        temp = Path(temporary)
        cases = args.cases or temp / "cases"
        if not args.cases:
            run(["cargo", "run", "--quiet", "--locked", "--manifest-path", "packer/Cargo.toml",
                 "--example", "runtime_cases", "--", str(cases)])
        # Corrupt the second RAW block, leaving indices valid. A multi-block
        # failed read must not expose even the valid prefix to its caller.
        raw = bytearray((cases / "raw.nx").read_bytes())
        table = struct.unpack_from("<Q", raw, 88)[0]
        payload = struct.unpack_from("<Q", raw, table + 24)[0]
        mid = bytearray(raw)
        raw[payload + 12] ^= 1
        # Same block, bad sixth 16 KiB slice: partial reads keep the first five.
        mid[payload + 5 * 16 * 1024 + 3] ^= 1
        # Never alter supplied cases, even when reusing a private work folder.
        testing = temp / "cases-to-test"
        testing.mkdir()
        for name in ("raw.nx", "zstd.nx"):
            shutil.copyfile(cases / name, testing / name)
        (testing / "corrupt.nx").write_bytes(raw)
        (testing / "corrupt_mid.nx").write_bytes(mid)
        executable = temp / ("session.exe" if msvc else "session")
        source = str(ROOT / "tests/conformance/session_reader.cpp")
        if msvc:
            if args.sanitize:
                raise SystemExit("Use GCC/Clang for the selected sanitizer")
            run([compiler, "/nologo", "/std:c++17", "/EHsc", "/W4", "/WX", "/O2",
                 f"/I{ROOT / 'dist'}", f"/I{ROOT / 'tests/support'}", source,
                 f"/Fo:{temp / 'session.obj'}", f"/Fe:{executable}"])
        else:
            flags = ["-O2"] if not args.sanitize else ["-g", "-O1", "-fno-omit-frame-pointer", f"-fsanitize={args.sanitize}"]
            if args.sanitize and os.sys.platform.startswith("linux"):
                flags += ["-fno-pie", "-no-pie"]
            run([compiler, "-std=c++17", "-Wall", "-Wextra", "-Werror", "-pedantic", *flags,
                 f"-I{ROOT / 'dist'}", f"-I{ROOT / 'tests/support'}", source, "-pthread",
                 *([] if os.sys.platform in ("darwin", "win32") else ["-ldl"]), "-o", str(executable)])
        run([str(executable), str(testing)])


if __name__ == "__main__":
    main()
