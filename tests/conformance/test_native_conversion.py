"""Independent synthetic native conversion and original-byte conformance."""
from __future__ import annotations
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

import test_rust_writer as writer

ROOT = Path(__file__).resolve().parents[2]


def compile_reader(directory: Path, source: Path | None = None) -> Path:
    compiler = shutil.which(os.environ.get("CXX", "")) or shutil.which("c++") or shutil.which("g++") or shutil.which("cl")
    if not compiler:
        raise RuntimeError("C++17 compiler is required")
    source = source or ROOT / "tests/conformance/native_output_reader.cpp"
    msvc = Path(compiler).name.lower() in {"cl", "cl.exe"}
    output = directory / ("native_reader.exe" if msvc else "native_reader")
    if msvc:
        command = [compiler, "/nologo", "/std:c++17", "/EHsc", "/W4", "/WX", "/O2", f"/I{ROOT / 'dist'}", f"/I{ROOT / 'tests/support'}", str(source), f"/Fo:{directory / 'native_reader.obj'}", f"/Fe:{output}"]
    else:
        command = [compiler,"-std=c++17","-Wall","-Wextra","-Werror","-pedantic","-O2",f"-I{ROOT / 'dist'}",f"-I{ROOT / 'tests/support'}",str(source),"-pthread",*([] if sys.platform in ("darwin", "win32") else ["-ldl"]),"-o",str(output)]
    subprocess.run(command, cwd=ROOT, check=True)
    return output


def verify(directory: Path, reader: Path) -> None:
    validator = writer.load_validator()
    expected = {}
    for line in (directory / "game.expected.tsv").read_text(encoding="utf-8").splitlines():
        index, name, source, offset, size = line.split("\t")
        with Path(source).open("rb") as stream:
            stream.seek(int(offset))
            data = stream.read(int(size))
        assert len(data) == int(size)
        expected.setdefault(int(index), {})[name] = data
    for name in ["game.nx", "game.repacked.nx", "game.rebuilt.nx"]:
        path = directory / name
        blob = path.read_bytes()
        validator.validate_archive(blob, verify_payloads=True)
        actual = writer.parse_file_contents(blob, validator)
        assert actual == [expected[i] for i in range(len(expected))]
        subprocess.run([str(reader), str(path), str(directory / "game.expected.tsv")], check=True, cwd=ROOT)
    print(f"PASS {directory.name}: native units preserved; Python + C++17, conversion + repack + reconstruction")


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="nx-native-conformance-") as temp:
        root = Path(temp)
        reader = compile_reader(root)
        subprocess.run([writer.cargo_command(), "run", "--release", "--locked", "--manifest-path", str(ROOT / "packer/Cargo.toml"), "--example", "native_convert", "--", "--synthetic", str(root)], cwd=ROOT, check=True)
        for name in ["base","latest","updates","dlc","all","collection"]:
            verify(root / name,reader)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
