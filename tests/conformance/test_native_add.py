"""Tests for adding native content to an existing NX."""
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT=Path(__file__).resolve().parents[2]
import native_add_steps as additions

def main():
    subprocess.run(["cargo","build","--release","--locked","--manifest-path",str(ROOT/"packer/Cargo.toml"),"--example","native_add"],cwd=ROOT,check=True)
    with tempfile.TemporaryDirectory(prefix="nx-add-conformance-") as temp:
        root=Path(temp);sources=root/"sources"
        subprocess.run([str(additions.EXE),"synthetic",str(sources)],cwd=ROOT,check=True)
        report=additions.run(root/"outputs",sorted(sources.glob("*.nsp")))
        assert len(report["results"])==5
        assert all(row["identities_and_build_ids_equal"] and row["retained_frames_byte_identical"]>0 for row in report["results"])
    print("PASS existing-NX native additions: three updates, data-only executable inheritance, two DLCs, exact frames/identities; Rust/C++17 byte equivalence")
    return 0

if __name__=="__main__":raise SystemExit(main())
