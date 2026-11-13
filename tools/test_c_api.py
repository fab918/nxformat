"""Builds ffi/tests/smoke.c against the built nxformat library and runs it.

Usage: python tools/test_c_api.py [--lib-dir ffi/target/release] [--static]
"""
import argparse
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument('--lib-dir', type=Path, default=ROOT / 'ffi/target/release')
    parser.add_argument('--include-dir', type=Path, default=ROOT / 'ffi/include')
    parser.add_argument('--static', action='store_true')
    a = parser.parse_args()
    lib_dir = a.lib_dir.resolve()
    source = ROOT / 'ffi/tests/smoke.c'
    fixture = ROOT / 'tests/fixtures/dlc_only_application.nx'
    with tempfile.TemporaryDirectory() as work:
        work = Path(work)
        system = platform.system()
        if system == 'Windows':
            exe = work / 'smoke.exe'
            if a.static:
                cmd = ['cl', '/nologo', '/MD', '/DNXFORMAT_STATIC', f'/I{a.include_dir}', str(source),
                       f'/Fe{exe}', str(lib_dir / 'nxformat.lib'),
                       'ws2_32.lib', 'userenv.lib', 'ntdll.lib', 'bcrypt.lib', 'advapi32.lib', 'kernel32.lib', 'synchronization.lib']
            else:
                cmd = ['cl', '/nologo', '/MD', f'/I{a.include_dir}', str(source), f'/Fe{exe}',
                       str(lib_dir / 'nxformat.dll.lib')]
                shutil.copy(lib_dir / 'nxformat.dll', work)
        else:
            exe = work / 'smoke'
            cc = os.environ.get('CC', 'cc')
            if a.static:
                cmd = [cc, f'-I{a.include_dir}', str(source), '-o', str(exe), str(lib_dir / 'libnxformat.a'),
                       '-lpthread', '-ldl', '-lm']
            else:
                cmd = [cc, f'-I{a.include_dir}', str(source), '-o', str(exe), f'-L{lib_dir}', '-lnxformat',
                       f'-Wl,-rpath,{lib_dir}']
        print(' '.join(cmd))
        subprocess.run(cmd, check=True, cwd=work)
        result = subprocess.run([str(exe), str(fixture)], capture_output=True, text=True)
        print(result.stdout, result.stderr, sep='')
        expected = f'@SIZE\t{fixture.stat().st_size}'
        if result.returncode != 0 or expected not in result.stdout:
            print('C API smoke test failed', file=sys.stderr)
            return 1
    print('C API smoke test passed')
    return 0


if __name__ == '__main__':
    sys.exit(main())
