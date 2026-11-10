"""Packs one platform's release archive.

    python tools/package_release.py --platform windows-x64 --version 1.0.0 \
        --lib-dir ffi/target/x86_64-pc-windows-msvc/release \
        --app app/target/x86_64-pc-windows-msvc/release/nxconvert.exe --out out

Layout of nxformat-<version>-<platform>.zip|.tar.gz:
    include/   nxformat.h (C API) and the header-only C++17 NX reader
    lib/       shared library (Unix) or import library (Windows), plus
               nxformat_static.a|.lib, nxformat_static_nozstd.a|.lib (the same
               without Zstandard, for programs that link their own libzstd);
               lib/cmake/nxformat for find_package
    bin/       nxformat.dll (Windows) and the nxconvert app (desktop);
               macOS also bin/NXConvert.app, Linux share/ (desktop entry, icon)
"""
import argparse
import hashlib
import re
import shutil
import subprocess
import sys
import tempfile
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# What a program linking the static library also links.
STATIC_LIBS = {
    'windows': ['ws2_32', 'userenv', 'ntdll', 'bcrypt', 'advapi32', 'kernel32'],
    'linux': ['pthread', 'dl', 'm'],
    'macos': [],
    'android': ['dl', 'm', 'log'],
}

# The Zstandard C files zstd-sys compiles into the static library.
ZSTD_SOURCES = {
    'debug', 'entropy_common', 'error_private', 'fse_decompress', 'pool', 'threading', 'xxhash',
    'zstd_common', 'fse_compress', 'hist', 'huf_compress', 'zstd_compress', 'zstd_compress_literals',
    'zstd_compress_sequences', 'zstd_compress_superblock', 'zstd_double_fast', 'zstd_fast',
    'zstd_lazy', 'zstd_ldm', 'zstd_opt', 'zstd_preSplit', 'zstdmt_compress', 'huf_decompress',
    'huf_decompress_amd64', 'zstd_ddict', 'zstd_decompress', 'zstd_decompress_block',
}

CMAKE_CONFIG = '''# nxformat {version} ({platform}).
#   nxformat::nxformat       the C API, shared library
#   nxformat::static         the C API, static library
#   nxformat::static_nozstd  the same without Zstandard: link your own libzstd 1.5.7 or later
#   nxformat::reader         the header-only C++17 NX reader (needs a zstd decoder)
get_filename_component(_nxformat_root "${{CMAKE_CURRENT_LIST_DIR}}/../../.." ABSOLUTE)
set(nxformat_VERSION {version})
if(NOT TARGET nxformat::nxformat)
  add_library(nxformat::nxformat SHARED IMPORTED)
  set_target_properties(nxformat::nxformat PROPERTIES
    INTERFACE_INCLUDE_DIRECTORIES "${{_nxformat_root}}/include"
    IMPORTED_LOCATION "${{_nxformat_root}}/{shared}"{implib})
  add_library(nxformat::static STATIC IMPORTED)
  set_target_properties(nxformat::static PROPERTIES
    INTERFACE_INCLUDE_DIRECTORIES "${{_nxformat_root}}/include"
    INTERFACE_COMPILE_DEFINITIONS NXFORMAT_STATIC
    INTERFACE_LINK_LIBRARIES "{static_libs}"
    IMPORTED_LOCATION "${{_nxformat_root}}/lib/{static}")
  add_library(nxformat::static_nozstd STATIC IMPORTED)
  set_target_properties(nxformat::static_nozstd PROPERTIES
    INTERFACE_INCLUDE_DIRECTORIES "${{_nxformat_root}}/include"
    INTERFACE_COMPILE_DEFINITIONS NXFORMAT_STATIC
    INTERFACE_LINK_LIBRARIES "{static_libs}"
    IMPORTED_LOCATION "${{_nxformat_root}}/lib/{static_nozstd}")
  add_library(nxformat::reader INTERFACE IMPORTED)
  target_compile_features(nxformat::reader INTERFACE cxx_std_17)
  set_target_properties(nxformat::reader PROPERTIES
    INTERFACE_INCLUDE_DIRECTORIES "${{_nxformat_root}}/include")
endif()
# The file to copy next to the program that uses nxformat::nxformat.
set(NXFORMAT_RUNTIME "${{_nxformat_root}}/{shared}")
unset(_nxformat_root)
'''

CMAKE_VERSION = '''set(PACKAGE_VERSION {version})
if(PACKAGE_FIND_VERSION VERSION_GREATER PACKAGE_VERSION OR
   NOT PACKAGE_FIND_VERSION_MAJOR EQUAL {major})
  set(PACKAGE_VERSION_COMPATIBLE FALSE)
else()
  set(PACKAGE_VERSION_COMPATIBLE TRUE)
  if(PACKAGE_FIND_VERSION VERSION_EQUAL PACKAGE_VERSION)
    set(PACKAGE_VERSION_EXACT TRUE)
  endif()
endif()
'''


ASSETS = ROOT / 'app/assets'
DESKTOP_ENTRY = """[Desktop Entry]
Type=Application
Name=NXConvert
Comment=Convert games to NX and manage NX archives
Exec=nxconvert
Icon=nxconvert
Terminal=false
Categories=Utility;Archiving;
"""
INFO_PLIST = """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>NXConvert</string>
  <key>CFBundleDisplayName</key><string>NXConvert</string>
  <key>CFBundleIdentifier</key><string>io.github.fab918.nxconvert</string>
  <key>CFBundleExecutable</key><string>nxconvert</string>
  <key>CFBundleIconFile</key><string>nxconvert</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"""


def add_app_icon(stage: Path, app: Path, system: str, version: str) -> None:
    """Give the app its icon where the platform takes it from outside the
    binary (Windows embeds it in nxconvert.exe at build time).

    macOS: bin/NXConvert.app, a bundle around the same binary, opens the
    window from Finder. Linux: a desktop entry and its icon, under share/.
    """
    if system == 'macos':
        bundle = stage / 'bin/NXConvert.app/Contents'
        (bundle / 'MacOS').mkdir(parents=True)
        (bundle / 'Resources').mkdir()
        shutil.copy(app, bundle / 'MacOS/nxconvert')
        shutil.copy(ASSETS / 'nxconvert.icns', bundle / 'Resources')
        (bundle / 'Info.plist').write_text(INFO_PLIST.format(version=version), newline='\n')
    elif system == 'linux':
        icons = stage / 'share/icons/hicolor/512x512/apps'
        icons.mkdir(parents=True)
        shutil.copy(ASSETS / 'nxconvert.png', icons)
        (stage / 'share/applications').mkdir(parents=True)
        (stage / 'share/applications/nxconvert.desktop').write_text(DESKTOP_ENTRY, newline='\n')


def llvm_ar() -> Path:
    """llvm-ar from the llvm-tools rustup component: it rewrites the symbol
    table of COFF, ELF and Mach-O archives alike."""
    sysroot = subprocess.run(['rustc', '--print', 'sysroot'], check=True, capture_output=True, text=True).stdout.strip()
    host = next(line.split()[1] for line in subprocess.run(['rustc', '-vV'], check=True, capture_output=True,
                                                            text=True).stdout.splitlines() if line.startswith('host:'))
    tool = Path(sysroot) / 'lib/rustlib' / host / 'bin' / ('llvm-ar.exe' if sys.platform == 'win32' else 'llvm-ar')
    if not tool.exists():
        raise SystemExit(f'{tool} is missing: run rustup component add llvm-tools')
    return tool


def strip_zstd(archive: Path, ar: Path) -> None:
    """Removes the Zstandard objects from one single-architecture archive."""
    members = subprocess.run([ar, 't', archive], check=True, capture_output=True, text=True).stdout.splitlines()
    zstd = [m for m in members
            if (match := re.fullmatch(r'[0-9a-f]{16}-(\w+)\.o', m))
            and match.group(1) in ZSTD_SOURCES]
    if len(zstd) < 20:
        raise SystemExit(f'{archive}: found only {len(zstd)} Zstandard objects')
    subprocess.run([ar, 'd', archive, *zstd], check=True)


def without_zstd(source: Path, target: Path, system: str) -> None:
    """Copies the static library without its Zstandard objects."""
    ar = llvm_ar()
    if system != 'macos':
        shutil.copy(source, target)
        strip_zstd(target, ar)
        return
    # A universal archive: each architecture is stripped on its own.
    archs = subprocess.run(['lipo', '-archs', source], check=True, capture_output=True, text=True).stdout.split()
    with tempfile.TemporaryDirectory() as tmp:
        thin = []
        for arch in archs:
            part = Path(tmp) / f'{arch}.a'
            if len(archs) == 1:
                shutil.copy(source, part)
            else:
                subprocess.run(['lipo', source, '-thin', arch, '-output', part], check=True)
            strip_zstd(part, ar)
            thin.append(part)
        subprocess.run(['lipo', '-create', *thin, '-output', target], check=True)


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument('--platform', required=True, help='windows-x64, linux-x64, linux-arm64, macos-universal, android-arm64')
    p.add_argument('--version', required=True)
    p.add_argument('--lib-dir', type=Path, required=True)
    p.add_argument('--app', type=Path, help='the nxconvert binary (desktop platforms)')
    p.add_argument('--out', type=Path, required=True)
    a = p.parse_args()
    system = a.platform.split('-')[0]
    name = f'nxformat-{a.version}-{a.platform}'
    stage = a.out / name
    if stage.exists():
        print(f'{stage} already exists', file=sys.stderr)
        return 1
    (stage / 'include').mkdir(parents=True)
    (stage / 'lib/cmake/nxformat').mkdir(parents=True)
    (stage / 'bin').mkdir()

    shutil.copy(ROOT / 'LICENSE', stage)
    shutil.copy(ROOT / 'ffi/include/nxformat.h', stage / 'include')
    for header in sorted((ROOT / 'dist').glob('*.hpp')):
        shutil.copy(header, stage / 'include')

    lib = a.lib_dir
    implib = ''
    if system == 'windows':
        shutil.copy(lib / 'nxformat.dll', stage / 'bin')
        shutil.copy(lib / 'nxformat.dll.lib', stage / 'lib/nxformat.lib')
        shutil.copy(lib / 'nxformat.lib', stage / 'lib/nxformat_static.lib')
        shared, static = 'bin/nxformat.dll', 'nxformat_static.lib'
        implib = '\n    IMPORTED_IMPLIB "${_nxformat_root}/lib/nxformat.lib"'
    elif system == 'macos':
        shutil.copy(lib / 'libnxformat.dylib', stage / 'lib')
        shutil.copy(lib / 'libnxformat.a', stage / 'lib/libnxformat_static.a')
        shared, static = 'lib/libnxformat.dylib', 'libnxformat_static.a'
        implib = '\n    IMPORTED_SONAME "@rpath/libnxformat.dylib"'
    else:
        shutil.copy(lib / 'libnxformat.so', stage / 'lib')
        shutil.copy(lib / 'libnxformat.a', stage / 'lib/libnxformat_static.a')
        shared, static = 'lib/libnxformat.so', 'libnxformat_static.a'
        implib = '\n    IMPORTED_SONAME "libnxformat.so"'
    static_nozstd = static.replace('_static', '_static_nozstd')
    without_zstd(stage / 'lib' / static, stage / 'lib' / static_nozstd, system)
    if a.app:
        shutil.copy(a.app, stage / 'bin')
        add_app_icon(stage, a.app, system, a.version)
    if not any((stage / 'bin').iterdir()):
        (stage / 'bin').rmdir()

    (stage / 'lib/cmake/nxformat/nxformatConfig.cmake').write_text(
        CMAKE_CONFIG.format(version=a.version, platform=a.platform, shared=shared, implib=implib,
                            static=static, static_nozstd=static_nozstd,
                            static_libs=';'.join(STATIC_LIBS[system])), newline='\n')
    (stage / 'lib/cmake/nxformat/nxformatConfigVersion.cmake').write_text(
        CMAKE_VERSION.format(version=a.version, major=a.version.split(".")[0]), newline='\n')
    (stage / 'README.txt').write_text(
        f'nxformat {a.version} for {a.platform}\n'
        'https://github.com/fab918/nxformat\n\n'
        'include/nxformat.h  C API: runs the nxconvert commands in process\n'
        'include/*.hpp       header-only C++17 NX reader\n'
        'lib/                libraries; lib/cmake/nxformat for CMake find_package\n'
        'bin/                runtime library (Windows) and the nxconvert app\n'
        + ('bin/NXConvert.app   the same app as a macOS bundle, with its icon\n'
           if system == 'macos' and a.app else '')
        + ('share/              desktop entry and icon for the nxconvert app\n'
           if system == 'linux' and a.app else '')
        + '\n'
        'Static linking: define NXFORMAT_STATIC and link nxformat_static with: '
        + (' '.join(STATIC_LIBS[system]) or 'nothing else') + '\n'
        'nxformat_static_nozstd is the same without Zstandard, for programs that\n'
        'link their own libzstd (1.5.7 or later) and want a single copy of it.\n', newline='\n')

    if system == 'windows':
        archive = a.out / f'{name}.zip'
        with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as z:
            for f in sorted(stage.rglob('*')):
                if f.is_file():
                    z.write(f, f.relative_to(stage).as_posix())
    else:
        archive = a.out / f'{name}.tar.gz'
        with tarfile.open(archive, 'w:gz') as t:
            for f in sorted(stage.rglob('*')):
                if f.is_file():
                    info = t.gettarinfo(f, f.relative_to(stage).as_posix())
                    info.uid = info.gid = 0
                    info.uname = info.gname = ''
                    with open(f, 'rb') as data:
                        t.addfile(info, data)
    shutil.rmtree(stage)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    print(f'{digest}  {archive.name}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
