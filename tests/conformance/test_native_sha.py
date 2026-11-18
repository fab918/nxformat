"""Check native and portable SHA-256 give the same result."""
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]


def main():
    compiler = shutil.which(os.environ.get('CXX', '')) or shutil.which('c++') or shutil.which('cl')
    if not compiler:
        raise SystemExit('C++17 compiler required')
    msvc = Path(compiler).name.lower() in {'cl', 'cl.exe'}
    crypto = []
    if os.name != 'nt' and os.sys.platform != 'darwin' and shutil.which('pkg-config'):
        result = subprocess.run(['pkg-config', '--cflags', '--libs', 'libcrypto'], text=True, capture_output=True)
        if not result.returncode:
            crypto = ['-DNX_USE_OPENSSL=1', *shlex.split(result.stdout)]
    with tempfile.TemporaryDirectory(prefix='nx-native-sha-') as folder:
        temp = Path(folder)
        for portable in (False, True):
            exe = temp / ('sha.exe' if msvc else 'sha')
            source = str(ROOT / 'tests/conformance/native_sha256.cpp')
            if msvc:
                args = [compiler, '/nologo', '/std:c++17', '/EHsc', '/W4', '/WX', '/O2',
                        f'/I{ROOT / "dist"}', source, f'/Fo:{temp / "sha.obj"}', f'/Fe:{exe}']
                if portable:
                    args += ['/DNX_DISABLE_NATIVE_SHA256=1']
            else:
                args = [compiler, '-std=c++17', '-O2', '-Wall', '-Wextra', '-Werror', '-pedantic',
                        f'-I{ROOT / "dist"}', source, '-pthread', '-o', str(exe), *crypto]
                if portable:
                    args += ['-DNX_DISABLE_NATIVE_SHA256=1']
            subprocess.run(args, check=True)
            subprocess.run([str(exe)], check=True)


if __name__ == '__main__':
    main()
