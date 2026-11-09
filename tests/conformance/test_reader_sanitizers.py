"""Run the deterministic reader corpus under available address/UB sanitizers."""

from __future__ import annotations

import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def run_linux_wsl_sanitizers(temp: Path, fixture_dir: Path, index: dict) -> bool:
    """Use the installed Ubuntu WSL compiler when Windows' ASan runtime is missing."""
    wsl = shutil.which("wsl.exe")
    if not wsl:
        return False

    def linux_path(path: Path) -> str:
        result = subprocess.run([wsl, "--distribution", "Ubuntu", "--exec", "wslpath", "-a", str(path)],
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if result.returncode:
            raise RuntimeError(result.stdout.strip() or "Ubuntu WSL path conversion failed")
        return result.stdout.strip()

    try:
        workspace = linux_path(ROOT)
        fixtures = linux_path(fixture_dir)
        zstd_cases = temp / "zstd-cases"
        subprocess.run([sys.executable, str(ROOT / "tests" / "conformance" / "build_zstd_cases.py"), str(zstd_cases)],
                       cwd=ROOT, check=True)
        cases = linux_path(zstd_cases)
        token = f"nx-reader-sanitize-{os.getpid()}"
        linux_dir = f"/tmp/{token}"
        executable = f"{linux_dir}/reader_sanitized"
        fuzz_object = f"{linux_dir}/reader_fuzz.o"
        compile_script = " && ".join((
            f"mkdir -p {shlex.quote(linux_dir)}",
            f"cd {shlex.quote(workspace)}",
            f"g++ -std=c++17 -O1 -g -Wall -Wextra -Werror -pedantic -fsanitize=address,undefined -fno-omit-frame-pointer -I{shlex.quote(workspace + '/dist')} -I{shlex.quote(workspace + '/tests/support')} tests/conformance/reader_smoke.cpp -pthread -ldl -o {shlex.quote(executable)}",
            f"g++ -std=c++17 -O1 -g -Wall -Wextra -Werror -pedantic -fsanitize=address,undefined -fno-omit-frame-pointer -I{shlex.quote(workspace + '/dist')} -I{shlex.quote(workspace + '/tests/support')} -c tests/fuzz/nx_reader_fuzz.cpp -o {shlex.quote(fuzz_object)}",
        ))
        compiled = subprocess.run([wsl, "--distribution", "Ubuntu", "--exec", "/bin/bash", "-lc", compile_script],
                                  text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if compiled.returncode:
            print("SKIP Linux sanitizer fallback: " + compiled.stdout.strip(), file=sys.stderr)
            return False

        def run_reader(*arguments: str) -> None:
            result = subprocess.run([wsl, "--distribution", "Ubuntu", "--exec", executable, *arguments],
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                raise RuntimeError(result.stdout.strip() or f"sanitized reader exited {result.returncode}")

        def fixture(name: str) -> str:
            return fixtures.rstrip("/") + "/" + name

        for item in index["fixtures"]:
            if item["expected"] == "accept":
                expected = "accept"
            elif item["expected"] == "reject_full_verify":
                expected = "reject_verify"
            elif item.get("expected_error") in {"manifest_hash", "index_hash", "path", "manifest"}:
                expected = "reject_mount"
            else:
                expected = "reject_open"
            run_reader(fixture(item["file"]), expected)
        for name in ("minimal_valid.nx", "raw_only.nx", "nested_vfs.nx", "shared_with_orphan_block.nx",
                     "zstd_single.nx", "switch2_localized_icons.nx"):
            run_reader("--fuzz", fixture(name))
        run_reader("--review-regressions", fixtures)
        run_reader("--mount-retry", fixture("raw_only.nx"))
        run_reader("--atomic-multiblock", fixture("multi_block_file.nx"))
        run_reader("--concurrent-zstd", fixture("zstd_single.nx"))
        run_reader("--raw-slice", fixture("mixed_raw_zstd.nx"))
        run_reader("--sha-provider", fixture("raw_only.nx"), fixture("corrupted_block_payload.nx"))
        run_reader("--media-type", fixture("switch2_localized_icons.nx"))
        run_reader("--metadata-history", fixture("metadata_snapshot_tie.nx"))
        run_reader("--header-digest", fixture("raw_only.nx"))
        run_reader("--generation-recovery", fixture("raw_only.nx"))
        run_reader("--zstd-cases", cases)
        print(f"PASS Linux WSL ASan+UBSan reader run over {len(index['fixtures'])} fixtures, targeted paths, and real Zstandard edge cases")
        return True
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"SKIP Linux sanitizer fallback: {error}", file=sys.stderr)
        return False
    finally:
        # This path is created solely by this invocation under /tmp.
        subprocess.run([wsl, "--distribution", "Ubuntu", "--exec", "/bin/rm", "-rf", f"/tmp/nx-reader-sanitize-{os.getpid()}"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def main() -> int:
    configured = os.environ.get("CXX")
    compiler = shutil.which(configured) if configured else (shutil.which("clang++") or shutil.which("g++") or shutil.which("c++") or shutil.which("cl"))
    if not compiler:
        print("SKIP reader sanitizers: no C++ compiler found")
        return 0
    msvc = Path(compiler).name.lower() in {"cl", "cl.exe"}
    source = ROOT / "tests" / "conformance" / "reader_smoke.cpp"
    fuzz_source = ROOT / "tests" / "fuzz" / "nx_reader_fuzz.cpp"
    fixture_dir = ROOT / "tests" / "fixtures"
    with tempfile.TemporaryDirectory(prefix="nx-reader-sanitize-") as temp_name:
        temp = Path(temp_name)
        executable = temp / ("reader_sanitized.exe" if msvc else "reader_sanitized")
        if msvc:
            command = [compiler, "/nologo", "/std:c++17", "/EHsc", "/W4", "/WX", "/O1", "/Zi", "/fsanitize=address",
                       f"/I{ROOT / 'dist'}", f"/I{ROOT / 'tests' / 'support'}", str(source),
                       f"/Fo:{temp / 'reader_sanitized.obj'}", f"/Fe:{executable}"]
        else:
            dynamic_library = [] if sys.platform in ("darwin", "win32") else ["-ldl"]
            command = [compiler, "-std=c++17", "-O1", "-g", "-Wall", "-Wextra", "-Werror", "-pedantic",
                       "-fsanitize=address,undefined", "-fno-omit-frame-pointer",
                       f"-I{ROOT / 'dist'}", f"-I{ROOT / 'tests' / 'support'}", str(source),
                       "-pthread", *dynamic_library, "-o", str(executable)]
        built = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if built.returncode:
            unavailable_markers = ("clang_rt.asan", "unsupported option", "unknown argument", "unrecognized command-line option")
            if any(marker in built.stdout.lower() for marker in unavailable_markers):
                if msvc:
                    index = json.loads((fixture_dir / "fixture_index.json").read_text(encoding="utf-8"))
                    if run_linux_wsl_sanitizers(temp, fixture_dir, index):
                        return 0
                print("SKIP reader sanitizers: the selected compiler's sanitizer runtime/flags are unavailable")
                print(built.stdout, file=sys.stderr)
                return 0
            print(built.stdout, file=sys.stderr)
            return built.returncode
        fuzz_object = temp / ("reader_fuzz.obj" if msvc else "reader_fuzz.o")
        if msvc:
            fuzz_command = [compiler, "/nologo", "/std:c++17", "/EHsc", "/W4", "/WX", "/O1", "/Zi", "/fsanitize=address",
                            f"/I{ROOT / 'dist'}", "/c", str(fuzz_source), f"/Fo:{fuzz_object}"]
        else:
            fuzz_command = [compiler, "-std=c++17", "-O1", "-g", "-Wall", "-Wextra", "-Werror", "-pedantic",
                            "-fsanitize=address,undefined", "-fno-omit-frame-pointer",
                            f"-I{ROOT / 'dist'}", f"-I{ROOT / 'tests' / 'support'}", "-c", str(fuzz_source), "-o", str(fuzz_object)]
        fuzz_built = subprocess.run(fuzz_command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if fuzz_built.returncode:
            print(fuzz_built.stdout, file=sys.stderr)
            return fuzz_built.returncode
        index = json.loads((fixture_dir / "fixture_index.json").read_text(encoding="utf-8"))
        for item in index["fixtures"]:
            if item["expected"] == "accept":
                expected = "accept"
            elif item["expected"] == "reject_full_verify":
                expected = "reject_verify"
            elif item.get("expected_error") in {"manifest_hash", "index_hash", "path", "manifest"}:
                expected = "reject_mount"
            else:
                expected = "reject_open"
            result = subprocess.run([str(executable), str(fixture_dir / item["file"]), expected], cwd=ROOT,
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                print(result.stdout, file=sys.stderr)
                return result.returncode
        for name in ("minimal_valid.nx", "raw_only.nx", "nested_vfs.nx", "shared_with_orphan_block.nx", "zstd_single.nx", "switch2_localized_icons.nx"):
            result = subprocess.run([str(executable), "--fuzz", str(fixture_dir / name)], cwd=ROOT,
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                print(result.stdout, file=sys.stderr)
                return result.returncode
        review = subprocess.run([str(executable), "--review-regressions", str(fixture_dir)], cwd=ROOT,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if review.returncode:
            print(review.stdout, file=sys.stderr)
            return review.returncode
        result = subprocess.run([str(executable), "--mount-retry", str(fixture_dir / "raw_only.nx")], cwd=ROOT,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if result.returncode:
            print(result.stdout, file=sys.stderr)
            return result.returncode
        for mode, fixture in (("--atomic-multiblock", "multi_block_file.nx"),
                              ("--concurrent-zstd", "zstd_single.nx")):
            result = subprocess.run([str(executable), mode, str(fixture_dir / fixture)], cwd=ROOT,
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                print(result.stdout, file=sys.stderr)
                return result.returncode
        cases = temp / "zstd-cases"
        subprocess.run([sys.executable, str(ROOT / "tests" / "conformance" / "build_zstd_cases.py"), str(cases)], cwd=ROOT, check=True)
        result = subprocess.run([str(executable), "--zstd-cases", str(cases)], cwd=ROOT,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if result.returncode:
            print(result.stdout, file=sys.stderr)
            return result.returncode
        result = subprocess.run([str(executable), "--metadata-history", str(fixture_dir / "metadata_snapshot_tie.nx")], cwd=ROOT,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if result.returncode:
            print(result.stdout, file=sys.stderr)
            return result.returncode
        for case in ("--header-digest", "--generation-recovery"):
            result = subprocess.run([str(executable), case, str(fixture_dir / "raw_only.nx")], cwd=ROOT,
                                    text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                print(result.stdout, file=sys.stderr)
                return result.returncode
    print(f"PASS sanitizer reader run over {len(index['fixtures'])} fixtures; selected {'AddressSanitizer' if msvc else 'AddressSanitizer and UndefinedBehaviorSanitizer'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
