"""Compile the portable reader and exercise it against the conformance corpus."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from compression import zstd  # noqa: F401 - pins the real-frame test runtime

ROOT = Path(__file__).resolve().parents[2]


def main() -> int:
    sync = subprocess.run([sys.executable, str(ROOT / "reader" / "generate_header.py"), "--check"],
                          cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if sync.returncode:
        print(sync.stdout, file=sys.stderr)
        return sync.returncode
    source = ROOT / "tests" / "conformance" / "reader_smoke.cpp"
    fuzz_source = ROOT / "tests" / "fuzz" / "nx_reader_fuzz.cpp"
    fixtures = ROOT / "tests" / "fixtures"
    compiler_name = os.environ.get("CXX")
    compiler = shutil.which(compiler_name) if compiler_name else (shutil.which("c++") or shutil.which("g++") or shutil.which("cl"))
    if not compiler:
        print("no C++17 compiler found", file=sys.stderr)
        return 2
    is_msvc = Path(compiler).name.lower() in {"cl", "cl.exe"}
    with tempfile.TemporaryDirectory(prefix="nx-reader-") as temp:
        executable = Path(temp) / ("reader_smoke.exe" if is_msvc else "reader_smoke")
        if is_msvc:
            command = [compiler, "/nologo", "/std:c++17", "/EHsc", "/W4", "/WX", "/O2", f"/I{ROOT / 'dist'}", f"/I{ROOT / 'tests' / 'support'}", str(source), f"/Fo:{Path(temp) / 'reader_smoke.obj'}", f"/Fe:{executable}"]
        else:
            dynamic_library = [] if sys.platform in ("darwin", "win32") else ["-ldl"]
            command = [compiler, "-std=c++17", "-O2", "-Wall", "-Wextra", "-Werror", "-pedantic", f"-I{ROOT / 'dist'}", f"-I{ROOT / 'tests' / 'support'}", str(source), "-pthread", *dynamic_library, "-o", str(executable)]
        build = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if build.returncode:
            print(build.stdout, file=sys.stderr)
            return build.returncode
        fuzz_object = Path(temp) / ("nx_reader_fuzz.obj" if is_msvc else "nx_reader_fuzz.o")
        if is_msvc:
            fuzz_command = [compiler, "/nologo", "/std:c++17", "/EHsc", "/W4", "/WX", "/O2", f"/I{ROOT / 'dist'}", f"/I{ROOT / 'tests' / 'support'}", "/c", str(fuzz_source), f"/Fo:{fuzz_object}"]
        else:
            fuzz_command = [compiler, "-std=c++17", "-O2", "-Wall", "-Wextra", "-Werror", "-pedantic", f"-I{ROOT / 'dist'}", f"-I{ROOT / 'tests' / 'support'}", "-c", str(fuzz_source), "-o", str(fuzz_object)]
        fuzz_build = subprocess.run(fuzz_command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if fuzz_build.returncode:
            print(fuzz_build.stdout, file=sys.stderr)
            return fuzz_build.returncode
        index = json.loads((fixtures / "fixture_index.json").read_text(encoding="utf-8"))
        for item in index["fixtures"]:
            if item["expected"] == "accept":
                expected = "accept"
            elif item["expected"] == "reject_full_verify":
                expected = "reject_verify"
            elif item.get("expected_error") in {"manifest_hash", "index_hash", "path", "manifest"}:
                expected = "reject_mount"
            else:
                expected = "reject_open"
            result = subprocess.run([str(executable), str(fixtures / item["file"]), expected], cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                print(result.stdout, file=sys.stderr)
                return result.returncode
        fuzz_names = (
            "minimal_valid.nx", "raw_only.nx", "multiple_programs.nx",
            "localized_metadata_icons.nx", "same_application_id_cross_platform.nx",
            "dlc_without_program.nx",
        )
        for name in fuzz_names:
            result = subprocess.run([str(executable), "--fuzz", str(fixtures / name)], cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                print(result.stdout, file=sys.stderr)
                return result.returncode
        review = subprocess.run([str(executable), "--review-regressions", str(fixtures)], cwd=ROOT, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if review.returncode:
            print(review.stdout, file=sys.stderr)
            return review.returncode
        print(review.stdout.strip())
        retry = subprocess.run([str(executable), "--mount-retry", str(fixtures / "raw_only.nx")], cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if retry.returncode:
            print(retry.stdout, file=sys.stderr)
            return retry.returncode
        atomic = subprocess.run([str(executable), "--atomic-multiblock", str(fixtures / "multi_block_file.nx")], cwd=ROOT,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if atomic.returncode:
            print(atomic.stdout, file=sys.stderr)
            return atomic.returncode
        concurrent = subprocess.run([str(executable), "--concurrent-zstd", str(fixtures / "zstd_single.nx")], cwd=ROOT,
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if concurrent.returncode:
            print(concurrent.stdout, file=sys.stderr)
            return concurrent.returncode
        zstd_cases = Path(temp) / "zstd-cases"
        subprocess.run([sys.executable, str(ROOT / "tests" / "conformance" / "build_zstd_cases.py"), str(zstd_cases)], cwd=ROOT, check=True)
        zstd_result = subprocess.run([str(executable), "--zstd-cases", str(zstd_cases)], cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if zstd_result.returncode:
            print(zstd_result.stdout, file=sys.stderr)
            return zstd_result.returncode
        special_cases = (
            ["--generation-recovery", str(fixtures / "raw_only.nx")],
            ["--raw-slice", str(fixtures / "mixed_raw_zstd.nx")],
            ["--sha-provider", str(fixtures / "raw_only.nx"), str(fixtures / "corrupted_block_payload.nx")],
            ["--media-type", str(fixtures / "switch2_localized_icons.nx")],
            ["--metadata-history", str(fixtures / "metadata_snapshot_tie.nx")],
            ["--header-digest", str(fixtures / "raw_only.nx")],
        )
        for command_args in special_cases:
            result = subprocess.run([str(executable), *command_args], cwd=ROOT, text=True,
                                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                print(result.stdout, file=sys.stderr)
                return result.returncode
            print(result.stdout.strip())
        print(f"PASS synchronized C++17 reader compiled, exercised {len(index['fixtures'])} fixtures, ran mount retry, atomic multi-block I/O failure, concurrent RAW/Zstandard reads and real-Zstandard cases, and survived {1000 * len(fuzz_names)} deterministic parser mutations")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
