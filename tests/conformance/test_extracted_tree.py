"""Extracted-tree import tests."""
import argparse
from pathlib import Path
import subprocess
import tempfile
from test_native_conversion import compile_reader

ROOT = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--large-mib', type=int, default=128)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='nx-extracted-conformance-') as tmp:
        build = Path(tmp)
        output = args.output.resolve() if args.output else build / 'output'
        subprocess.run(['cargo', 'run', '--release', '--locked', '--manifest-path',
                        str(ROOT / 'packer/Cargo.toml'), '--example', 'extracted_cases',
                        '--', str(output), str(args.large_mib)], cwd=ROOT, check=True)
        reader = compile_reader(build)
        subprocess.run([str(reader), str(output / 'extracted.nx'),
                        str(output / 'expected.tsv'), str(output / 'selection.tsv')],
                       cwd=ROOT, check=True)
    print('PASS independent Rust/C++17 full extracted-file equivalence; no loader/execution claim')


if __name__ == '__main__':
    main()
