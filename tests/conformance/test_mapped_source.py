"""Compile and exercise the optional bounded Windows disk mapping adapter."""
from pathlib import Path
import subprocess
import tempfile
from test_native_conversion import compile_reader

ROOT = Path(__file__).resolve().parents[2]

def main():
    with tempfile.TemporaryDirectory(prefix='nx-mapped-probe-') as temporary:
        root = Path(temporary)
        executable = compile_reader(root, ROOT/'tests/conformance/mapped_source_probe.cpp')
        subprocess.run([str(executable),str(root)],check=True,cwd=ROOT)

if __name__=='__main__':
    main()
