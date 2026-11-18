"""Build C/C++ programs against a release archive and run them."""
from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import tempfile
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main() -> None:
    parser = argparse.ArgumentParser(__doc__)
    parser.add_argument('--archive-dir', type=Path, required=True)
    args = parser.parse_args()
    archives = sorted(p for p in args.archive_dir.glob('nxformat-*')
                      if p.is_file() and (p.name.endswith('.tar.gz') or p.suffix == '.zip'))
    if len(archives) != 1:
        parser.error('archive-dir must contain exactly one release archive')
    archive = archives[0]
    match = re.match(r'nxformat-(\d+)\.(\d+)\.(\d+)-', archive.name)
    if not match:
        parser.error('archive has no semantic version')
    version = '.'.join(match.groups())
    with tempfile.TemporaryDirectory(prefix='nxformat-package-test-') as name:
        work = Path(name)
        package = work / 'package'
        package.mkdir()
        if archive.suffix == '.zip':
            with zipfile.ZipFile(archive) as source:
                source.extractall(package)
        else:
            with tarfile.open(archive) as source:
                source.extractall(package, filter='data')
        shutil.copyfile(ROOT / 'ffi/tests/smoke.c', work / 'smoke.c')
        shutil.copyfile(ROOT / 'tests/conformance/reader_cxx17_smoke.cpp', work / 'reader.cpp')
        (work / 'CMakeLists.txt').write_text('''cmake_minimum_required(VERSION 3.21)
project(nxformat_consumer LANGUAGES C CXX)
set(CMAKE_CXX_STANDARD 11)
find_package(nxformat VERSION_PLACEHOLDER EXACT REQUIRED CONFIG)
add_executable(shared smoke.c)
target_link_libraries(shared PRIVATE nxformat::nxformat)
add_custom_command(TARGET shared POST_BUILD COMMAND ${CMAKE_COMMAND} -E copy_if_different
    $<TARGET_FILE:nxformat::nxformat> $<TARGET_FILE_DIR:shared>)
add_executable(static smoke.c)
target_link_libraries(static PRIVATE nxformat::static)
add_executable(reader reader.cpp)
target_link_libraries(reader PRIVATE nxformat::reader)
enable_testing()
add_test(NAME shared COMMAND shared FIXTURE_PLACEHOLDER)
add_test(NAME static COMMAND static FIXTURE_PLACEHOLDER)
add_test(NAME reader COMMAND reader)
'''.replace('VERSION_PLACEHOLDER', version).replace(
            'FIXTURE_PLACEHOLDER', '"' + (ROOT / 'tests/fixtures/dlc_only_application.nx').as_posix() + '"'),
            encoding='utf-8')
        build = work / 'build'
        commands = [
            ['cmake', '-S', str(work), '-B', str(build), '-DCMAKE_BUILD_TYPE=Release',
             f'-Dnxformat_DIR={(package / "lib/cmake/nxformat").as_posix()}'],
            ['cmake', '--build', str(build), '--config', 'Release', '--parallel', '2'],
            ['ctest', '--test-dir', str(build), '-C', 'Release', '--output-on-failure'],
        ]
        for command in commands:
            subprocess.run(command, check=True, timeout=180)
    print(f'PASS {archive.name}: packaged shared/static C API and C++17 reader')


if __name__ == '__main__':
    main()
