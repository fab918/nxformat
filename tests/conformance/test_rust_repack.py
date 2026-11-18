"""Repack tests, checked in Python and C++."""

from __future__ import annotations

import importlib.util
import os
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PACKER = ROOT / "packer" / "Cargo.toml"


def module_from_path(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def compile_reader(directory: Path) -> Path:
    source = ROOT / "tests" / "conformance" / "repack_output_reader.cpp"
    configured = os.environ.get("CXX")
    compiler = shutil.which(configured) if configured else (
        shutil.which("c++") or shutil.which("g++") or shutil.which("clang++") or shutil.which("cl")
    )
    if not compiler:
        raise RuntimeError("C++17 compiler is required for repack interoperability validation")
    msvc = Path(compiler).name.lower() in {"cl", "cl.exe"}
    executable = directory / ("repack_output_reader.exe" if msvc else "repack_output_reader")
    if msvc:
        command = [
            compiler,
            "/nologo",
            "/std:c++17",
            "/EHsc",
            "/W4",
            "/WX",
            "/O2",
            f"/I{ROOT / 'dist'}",
            f"/I{ROOT / 'tests' / 'support'}",
            str(source),
            f"/Fo:{directory / 'repack_output_reader.obj'}",
            f"/Fe:{executable}",
        ]
    else:
        dynamic_library = [] if sys.platform in ("darwin", "win32") else ["-ldl"]
        command = [
            compiler,
            "-std=c++17",
            "-O2",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pedantic",
            f"-I{ROOT / 'dist'}",
            f"-I{ROOT / 'tests' / 'support'}",
            str(source),
            *dynamic_library,
            "-pthread",
            "-o",
            str(executable),
        ]
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError("C++17 repack reader compile failed:\n" + result.stdout)
    return executable


def frames(blob: bytes) -> list[bytes]:
    index_offset = struct.unpack_from("<Q", blob, 88)[0]
    block_count = struct.unpack_from("<Q", blob, 96)[0]
    output = []
    for index in range(block_count):
        offset, stored = struct.unpack_from("<QI", blob, index_offset + index * 24)
        output.append(blob[offset : offset + stored])
    return output


def expected_noise(size: int) -> bytes:
    state = 0x13579BDF
    output = bytearray()
    while len(output) < size:
        state ^= (state << 13) & 0xFFFFFFFF
        state ^= state >> 17
        state ^= (state << 5) & 0xFFFFFFFF
        output.extend(state.to_bytes(4, "little"))
    return bytes(output[:size])


def selected_metadata(blob: bytes, writer_tests) -> tuple[int, bytes, int]:
    catalog_offset, catalog_size = struct.unpack_from("<QQ", blob, 24)
    catalog = blob[catalog_offset : catalog_offset + catalog_size]
    header = writer_tests.CATALOG_HEADER.unpack_from(catalog)
    app_offset = header[12]
    selected = struct.unpack_from("<I", catalog, app_offset + 68)[0]
    title_offset, title_size = struct.unpack_from("<II", catalog, app_offset + 28)
    pool_offset = header[18]
    title = catalog[pool_offset + title_offset : pool_offset + title_offset + title_size]
    icon = struct.unpack_from("<I", catalog, app_offset + 44)[0]
    return selected, title, icon


def assert_history_title(
    blob: bytes,
    expected_snapshot: int,
    expected_title: str,
    writer_tests,
    icon_bytes: bytes = b"synthetic PNG body",
    icon_type: int = 1,
) -> None:
    catalog_offset, catalog_size = struct.unpack_from("<QQ", blob, 24)
    catalog = blob[catalog_offset : catalog_offset + catalog_size]
    header = writer_tests.CATALOG_HEADER.unpack_from(catalog)
    extension_count, extension_offset = header[10], header[17]
    payload = None
    for index in range(extension_count):
        type_id, _, owner_kind, _, owner, offset, size = struct.unpack_from(
            "<IIHHIQQ", catalog, extension_offset + index * 32
        )
        if type_id == 0x00010004 and owner_kind == 1 and owner == 0:
            payload = catalog[offset : offset + size]
            break
    if payload is None:
        raise AssertionError("required Snapshot Metadata History extension is missing")
    _, _, _, row_count, _, _, _ = struct.unpack_from("<4sHHIIII", payload)
    pool_offset = header[18]
    for index in range(row_count):
        row = struct.unpack_from("<12I", payload, 24 + index * 48)
        if row[0] == expected_snapshot:
            title = catalog[pool_offset + row[1] : pool_offset + row[1] + row[2]].decode("utf-8")
            if title != expected_title:
                raise AssertionError(f"metadata history title changed: {title!r}")
            icon = row[5]
            if writer_tests.media_payload(blob, icon, icon_type) != icon_bytes:
                raise AssertionError("snapshot icon was not retained in metadata history")
            return
    raise AssertionError(f"history row for snapshot {expected_snapshot} is missing")


def assert_history_localized(
    blob: bytes,
    expected_snapshot: int,
    expected_language: str,
    expected_title: str,
    expected_icon: bytes,
    writer_tests,
) -> None:
    catalog_offset, catalog_size = struct.unpack_from("<QQ", blob, 24)
    catalog = blob[catalog_offset : catalog_offset + catalog_size]
    header = writer_tests.CATALOG_HEADER.unpack_from(catalog)
    pool_offset = header[18]
    for index in range(header[10]):
        type_id, _, owner_kind, _, owner, extension_offset, extension_size = struct.unpack_from(
            "<IIHHIQQ", catalog, header[17] + index * 32
        )
        if type_id != 0x00010004 or owner_kind != 1 or owner != 0:
            continue
        payload = catalog[extension_offset : extension_offset + extension_size]
        _, _, _, row_count, localized_count, _, _ = struct.unpack_from("<4sHHIIII", payload)
        locale_base = 24 + row_count * 48
        for row_index in range(row_count):
            row = struct.unpack_from("<12I", payload, 24 + row_index * 48)
            if row[0] != expected_snapshot:
                continue
            for local_index in range(row[7], row[7] + row[8]):
                local = struct.unpack_from("<6I", payload, locale_base + local_index * 24)
                language = catalog[pool_offset + local[0] : pool_offset + local[0] + local[1]].decode("utf-8")
                title = catalog[pool_offset + local[2] : pool_offset + local[2] + local[3]].decode("utf-8")
                if language != expected_language:
                    continue
                if title != expected_title:
                    raise AssertionError(f"localized snapshot title changed: {title!r}")
                if writer_tests.media_payload(blob, local[4], 1) != expected_icon:
                    raise AssertionError("localized snapshot icon changed in metadata history")
                return
            raise AssertionError(f"history row has no {expected_language} metadata")
        raise AssertionError(f"history row for snapshot {expected_snapshot} is missing")
    raise AssertionError("Snapshot Metadata History extension is missing")


def main() -> int:
    writer_tests = module_from_path("nx_writer_tests", ROOT / "tests" / "conformance" / "test_rust_writer.py")
    validator = writer_tests.load_validator()
    with tempfile.TemporaryDirectory(prefix="nx-rust-repack-") as temp_name:
        directory = Path(temp_name)
        subprocess.run(
            [
                writer_tests.cargo_command(),
                "run",
                "--locked",
                "--quiet",
                "--manifest-path",
                str(PACKER),
                "--example",
                "repack_conformance",
                "--",
                str(directory),
            ],
            cwd=ROOT,
            check=True,
        )
        source = (directory / "source.nx").read_bytes()
        output = (directory / "repacked.nx").read_bytes()
        repeated = (directory / "repacked_repeat.nx").read_bytes()
        selected_update = (directory / "metadata_selected_update.nx").read_bytes()
        fallback = (directory / "metadata_fallback.nx").read_bytes()
        if output != repeated:
            raise AssertionError("identical repack inputs/options did not produce byte-identical archives")
        source_stats = validator.validate_archive(source, verify_payloads=True)
        output_stats = validator.validate_archive(output, verify_payloads=True)
        selected_stats = validator.validate_archive(selected_update, verify_payloads=True)
        fallback_stats = validator.validate_archive(fallback, verify_payloads=True)
        if output_stats["snapshots"] != 3 or output_stats["blocks"] != 3:
            raise AssertionError(f"unexpected compact repack counts: {output_stats}")
        if output_stats["blocks"] >= source_stats["blocks"] + 3:
            raise AssertionError("cross-archive duplicate block was not reused")

        actual = writer_tests.parse_file_contents(output, validator)
        expected = [
            {"base.bin": bytes([0x22]) * (64 * 1024)},
            {
                "same.bin": bytes([0x22]) * (64 * 1024),
                "update.bin": bytes([0x44]) * (256 * 1024),
            },
            {"dlc.bin": expected_noise(64 * 1024)},
        ]
        if actual != expected:
            raise AssertionError("independently extracted repacked file contents differ")
        if writer_tests.parse_file_contents(selected_update, validator) != expected:
            raise AssertionError("metadata-source selection changed logical file contents")
        if writer_tests.parse_file_contents(fallback, validator) != [expected[0], expected[2]]:
            raise AssertionError("removing the update changed the retained base tree")

        catalog_offset, catalog_size = struct.unpack_from("<QQ", output, 24)
        catalog = output[catalog_offset : catalog_offset + catalog_size]
        header = writer_tests.CATALOG_HEADER.unpack_from(catalog)
        app_offset = header[12]
        snapshot_offset = header[14]
        snapshot_count = header[7]
        identities = [
            catalog[snapshot_offset + i * 120 + 56 : snapshot_offset + i * 120 + 88]
            for i in range(snapshot_count)
        ]
        source_catalog_offset = struct.unpack_from("<Q", source, 24)[0]
        source_catalog = source[source_catalog_offset : source_catalog_offset + struct.unpack_from("<Q", source, 32)[0]]
        source_header = writer_tests.CATALOG_HEADER.unpack_from(source_catalog)
        source_snapshot = source_catalog[
            source_header[14] + 56 : source_header[14] + 88
        ]
        if source_snapshot not in identities:
            raise AssertionError("retained snapshot canonical identity changed")
        selected = struct.unpack_from("<I", catalog, app_offset + 68)[0]
        if identities[selected] != source_snapshot:
            raise AssertionError("display metadata provenance no longer points at the source snapshot")
        if selected_metadata(output, writer_tests)[1] != b"Repack synthetic base":
            raise AssertionError("selected localized display metadata was not preserved exactly")
        icon_index = struct.unpack_from("<I", catalog, app_offset + 44)[0]
        if writer_tests.media_payload(output, icon_index, 1) != b"synthetic PNG body":
            raise AssertionError("original icon payload was not preserved byte-for-byte")
        assert_history_title(
            output,
            1,
            "Synthetic update",
            writer_tests,
            b"synthetic update JPEG",
            2,
        )
        assert_history_localized(
            output,
            1,
            "fr",
            "Mise à jour synthétique",
            b"synthetic localized PNG",
            writer_tests,
        )
        update_selected_index, update_title, update_icon = selected_metadata(selected_update, writer_tests)
        if update_title != b"Synthetic update":
            raise AssertionError("explicit update metadata source was not selected")
        if writer_tests.media_payload(selected_update, update_icon, 2) != b"synthetic update JPEG":
            raise AssertionError("update JPEG icon bytes were not preserved")
        assert_history_title(selected_update, 0, "Repack synthetic base", writer_tests)
        fallback_index, fallback_title, fallback_icon = selected_metadata(fallback, writer_tests)
        if fallback_title != b"Repack synthetic base" or fallback_index != 0:
            raise AssertionError("removing selected update did not restore base metadata")
        if writer_tests.media_payload(fallback, fallback_icon, 1) != b"synthetic PNG body":
            raise AssertionError("fallback icon was not restored from metadata history")
        fallback_catalog_offset, fallback_catalog_size = struct.unpack_from("<QQ", fallback, 24)
        fallback_catalog = fallback[
            fallback_catalog_offset : fallback_catalog_offset + fallback_catalog_size
        ]
        fallback_header = writer_tests.CATALOG_HEADER.unpack_from(fallback_catalog)
        for index in range(fallback_header[10]):
            extension = struct.unpack_from(
                "<IIHHIQQ", fallback_catalog, fallback_header[17] + index * 32
            )
            if extension[0] == 0x00010004:
                raise AssertionError("removed update metadata left a stale history extension")
        if frames(source)[0] not in frames(output):
            raise AssertionError("retained stored frame changed during repack")

        manifest_offset, manifest_size = struct.unpack_from("<QQ", output, 72)
        manifest = output[manifest_offset : manifest_offset + manifest_size]
        file_count = struct.unpack_from("<I", manifest, 20)[0]
        file_offset = struct.unpack_from("<Q", manifest, 40)[0]
        chunk_log2 = {manifest[file_offset + i * 64 + 16] for i in range(file_count)}
        if chunk_log2 != {16, 18}:
            raise AssertionError(f"mixed per-file chunk sizes were lost: {chunk_log2}")
        block_index_offset = struct.unpack_from("<Q", output, 88)[0]
        block_count = struct.unpack_from("<Q", output, 96)[0]
        codecs = {output[block_index_offset + i * 24 + 16] for i in range(block_count)}
        if codecs != {0, 1}:
            raise AssertionError(f"expected mixed RAW/Zstandard output, found codecs {codecs}")

        malformed = bytearray(output)
        struct.pack_into("<I", malformed, catalog_offset + app_offset + 44, 0xFFFFFFFE)
        catalog_bytes = malformed[catalog_offset : catalog_offset + catalog_size]
        malformed[120:152] = validator.sha(catalog_bytes)
        struct.pack_into("<I", malformed, 252, validator.crc32c(malformed[:252]))
        malformed[4096:4352] = malformed[:256]
        try:
            validator.validate_archive(bytes(malformed), verify_payloads=True)
        except validator.InvalidArchive:
            pass
        else:
            raise AssertionError("Python validator accepted a misassociated/out-of-range icon reference")
        malformed_path = directory / "invalid_metadata.nx"
        malformed_path.write_bytes(malformed)
        rust_reject = subprocess.run(
            [writer_tests.cargo_command(), "run", "--locked", "--quiet", "--manifest-path", str(PACKER),
             "--example", "repack_conformance", "--", "--inspect", str(malformed_path)],
            cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        if rust_reject.returncode == 0:
            raise AssertionError("Rust repack parser accepted an out-of-range icon reference")

        reader = compile_reader(directory)
        malformed_reader = subprocess.run(
            [str(reader), str(malformed_path), "reject"],
            cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        if malformed_reader.returncode:
            raise AssertionError("C++ reader accepted malformed repacked metadata: " + malformed_reader.stdout)
        for name, selected in (("repacked.nx", "base"), ("metadata_selected_update.nx", "update"), ("metadata_fallback.nx", "fallback")):
            result = subprocess.run(
                [str(reader), str(directory / name), selected],
                cwd=ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            if result.returncode:
                raise RuntimeError(result.stdout)
            print(result.stdout.strip())
        print(f"PASS Python structural/payload validation and exact logical comparison: {output_stats}, {selected_stats}, {fallback_stats}")
        print("PASS identity, provenance/history remapping, exact icons/frames, mixed chunks/codecs, deterministic bytes and malformed metadata rejection")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
