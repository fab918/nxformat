"""Switch 1 multi-Program tests."""
import sys
import subprocess
import tempfile
from pathlib import Path

ROOT=Path(__file__).resolve().parents[2]
import native_add_steps as b

def main():
    subprocess.run(["cargo","build","--release","--locked","--manifest-path",str(ROOT/"packer/Cargo.toml"),"--example","native_add","--example","switch1_review"],cwd=ROOT,check=True)
    review=b.EXE.with_name("switch1_review.exe" if sys.platform=="win32" else "switch1_review")
    with tempfile.TemporaryDirectory(prefix="nx-switch1-review-") as temp:
        root=Path(temp);fixtures=root/"sources";output=root/"output"
        subprocess.run([str(review),"fixtures",str(fixtures)],check=True,cwd=ROOT)
        sources=sorted(fixtures.glob("*.nsp"));report=b.run(output,sources)
        assert len(report["results"])==6
        converted=output/"converted.nx"
        subprocess.run([str(review),"convert",str(converted),*map(str,sources)],check=True,cwd=ROOT)
        subprocess.run([str(b.EXE),"verify",str(converted),*map(str,sources)],check=True,cwd=ROOT)
        reader=output/("native_reader.exe" if sys.platform=="win32" else "native_reader")
        subprocess.run([str(reader),str(converted),str(converted.with_suffix(".expected.tsv")),str(converted.with_suffix(".selection.tsv"))],check=True,cwd=ROOT)
        assert b.archive_evidence(output/"step-5.nx",b.load_validator(),False)==b.archive_evidence(converted,b.load_validator(),False)
    print("PASS multi-Program native conversion/addition: both executable scopes, sparse replacements, data inheritance, shared NCA, DLC, exact identities and independent C++17 selections")
    return 0
if __name__=="__main__":raise SystemExit(main())
