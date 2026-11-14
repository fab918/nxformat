"""Validate Switch 2 Editions and multi-Program imports with unchanged readers."""
from pathlib import Path
import subprocess
import tempfile

from test_native_conversion import compile_reader
from test_rust_writer import load_validator

ROOT = Path(__file__).resolve().parents[2]


def main():
    with tempfile.TemporaryDirectory(prefix="nx-switch2-nup-") as tmp:
        root = Path(tmp)
        reader = compile_reader(root)
        subprocess.run([
            "cargo", "run", "--release", "--locked", "--manifest-path",
            str(ROOT / "packer/Cargo.toml"), "--example", "switch2_nup", "--", str(root),
        ], cwd=ROOT, check=True)
        # The Rust check-nup compares every source file. The independent readers
        # additionally validate the catalog, identities, dependencies and payloads.
        validator = load_validator()
        archives = sorted(root.glob("*.nx"))
        assert len(archives) == 20
        for archive in archives:
            validator.validate_archive(archive.read_bytes(), verify_payloads=True)
            subprocess.run([str(reader), str(archive), str(archive.with_suffix(".tsv"))], check=True, cwd=ROOT)
    print("PASS 20 Switch 2 NUP imports: raw/zstd, staged/streamed, Editions, separate bases, indexed updates and programs 1/15; Python + C++17")


if __name__ == "__main__":
    main()
