"""Writer tests, checked in Python and C++."""

from __future__ import annotations

import hashlib
import importlib.util
import os
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CARGO_MANIFEST = ROOT / "packer" / "Cargo.toml"
FIXTURE_VALIDATOR = ROOT / "tests" / "conformance" / "validate_fixtures.py"
FILE = struct.Struct("<QIIBBH32s12s")
ENTRY = struct.Struct("<QIIBBHI")
DIRECTORY = struct.Struct("<4I")
BLOCK = struct.Struct("<QIIBBHI")
CATALOG_HEADER = struct.Struct("<4sHHQ8I8Q16s")
SNAPSHOT = struct.Struct("<IBBHIIQII16sII32s32s")
PROGRAM = struct.Struct("<QIBBH16s4I16s")
NO_INDEX = 0xFFFFFFFF


def load_validator():
    spec = importlib.util.spec_from_file_location("nx_reference_validator", FIXTURE_VALIDATOR)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def cargo_command() -> str:
    configured = os.environ.get("CARGO")
    if configured:
        return configured
    found = shutil.which("cargo")
    if found:
        return found
    if os.name == "nt":
        candidate = Path.home() / ".cargo" / "bin" / "cargo.exe"
        if candidate.is_file():
            return str(candidate)
    raise RuntimeError("Cargo is required to run the Rust writer integration test")


def current_header(blob: bytes, validator) -> bytes:
    """NX 0.2: return `blob` with the selected (newest valid) header slot at 0."""
    best = None
    for slot in (0, 4096):
        raw = blob[slot : slot + 256]
        if raw[:4] == b"NXAR" and validator.crc32c(raw[:252]) == struct.unpack_from("<I", raw, 252)[0]:
            if best is None or struct.unpack_from("<Q", raw, 216)[0] > struct.unpack_from("<Q", best, 216)[0]:
                best = raw
    assert best is not None, "no valid header slot"
    return bytes(best) + bytes(blob[256:])


def parse_file_contents(blob: bytes, validator) -> list[dict[str, bytes]]:
    blob = current_header(blob, validator)
    catalog_off = struct.unpack_from("<Q", blob, 24)[0]
    manifest_off = struct.unpack_from("<Q", blob, 72)[0]
    block_index_off = struct.unpack_from("<Q", blob, 88)[0]
    block_count = struct.unpack_from("<Q", blob, 96)[0]
    manifest = blob[manifest_off : manifest_off + struct.unpack_from("<Q", blob, 80)[0]]
    directory_count, file_count, _, reference_count = struct.unpack_from("<4I", manifest, 16)
    dir_off, file_off, entry_off, ref_off, path_off, _ = struct.unpack_from("<6Q", manifest, 32)
    snapshot_count = struct.unpack_from("<I", blob, catalog_off + 28)[0]
    snapshot_off = catalog_off + struct.unpack_from("<Q", blob, catalog_off + 64)[0]
    roots = [struct.unpack_from("<I", blob, snapshot_off + i * 120 + 12)[0] for i in range(snapshot_count)]
    directories = [DIRECTORY.unpack_from(manifest, dir_off + i * 16) for i in range(directory_count)]
    files = [FILE.unpack_from(manifest, file_off + i * 64) for i in range(file_count)]
    references = [struct.unpack_from("<I", manifest, ref_off + i * 4)[0] for i in range(reference_count)]
    blocks = [BLOCK.unpack_from(blob, block_index_off + i * 24) for i in range(block_count)]

    def visit(directory_id: int, prefix: str, active: set[int], result: dict[str, bytes]) -> None:
        if directory_id in active:
            raise AssertionError("writer output contains a directory cycle")
        active.add(directory_id)
        first, count, flags, reserved = directories[directory_id]
        assert flags == 0 and reserved == 0
        for index in range(first, first + count):
            name_off, name_len, target, kind, flags, reserved0, reserved1 = ENTRY.unpack_from(manifest, entry_off + index * 24)
            assert flags == reserved0 == reserved1 == 0
            name = manifest[name_off : name_off + name_len].decode("utf-8")
            path = f"{prefix}/{name}" if prefix else name
            if kind == 1:
                visit(target, path, active, result)
            else:
                size, first_ref, block_n, chunk_log2, file_flags, reserved0, digest, reserved1 = files[target]
                assert chunk_log2 in (16, 17, 18, 19, 20) and file_flags == reserved0 == 0 and reserved1 == bytes(12)
                data = bytearray()
                for reference_i in range(first_ref, first_ref + block_n):
                    block_id = references[reference_i]
                    data_off, stored, logical, codec, flags, reserved0, _first_check = blocks[block_id]
                    assert codec in (0, 1) and flags == reserved0 == 0
                    payload = blob[data_off : data_off + stored]
                    assert len(payload) == stored
                    if codec == 0:
                        assert stored == logical
                        data.extend(payload)
                    else:
                        decoder = validator.zstd.ZstdDecompressor(
                            options={validator.zstd.DecompressionParameter.window_log_max: 20}
                        )
                        decoded = decoder.decompress(payload, max_length=logical + 1)
                        assert len(decoded) == logical and decoder.eof and not decoder.unused_data
                        data.extend(decoded)
                assert len(data) == size
                result[path] = bytes(data)
        active.remove(directory_id)

    output: list[dict[str, bytes]] = []
    for root in roots:
        result: dict[str, bytes] = {}
        visit(root, "", set(), result)
        output.append(result)
    return output


def expected_contents() -> dict[str, list[dict[str, bytes]]]:
    big = bytes(((i * 17 + 11) & 0xFF) for i in range(700_123))
    chunk = 256 * 1024
    a = bytes([0xA1]) * chunk
    b = bytes([0xB2]) * chunk
    c = bytes([0xC3]) * chunk
    d = bytes([0xD4]) * chunk
    tail = bytes([0xE5]) * 137
    baseline = [
        {"removed.bin": b"removed in update", "shared.bin": b"stable file across complete snapshots"},
        {"new.bin": b"added in update", "shared.bin": b"stable file across complete snapshots"},
        {},
    ]
    return {
        "removed_update.nx": [baseline[0], baseline[2]],
        "removed_dlc.nx": baseline[:2],
        "removed_base.nx": baseline[1:],
        "added_update.nx": baseline[:2] + [{"shared.bin": b"stable file across complete snapshots", "future.bin": b"third version"}, {}],
        "added_dlc.nx": baseline + [{"extra.bin": b"new DLC content"}],
        "empty.nx": [{}],
        "raw_multiblock.nx": [
            {"empty.bin": b"", "folder/big.bin": big, "note.txt": b"NX writer stream test"}
        ],
        "snapshot_metadata.nx": [
            {"removed.bin": b"removed in update", "shared.bin": b"stable file across complete snapshots"},
            {"new.bin": b"added in update", "shared.bin": b"stable file across complete snapshots"},
            {},
        ],
        "dlc_only.nx": [{"dlc.bin": b"unassociated DLC tree"}],
        "multiple_programs.nx": [
            {"first.bin": b"first executable tree"},
            {
                "second.bin": b"second executable tree",
                "shared.bin": b"first executable tree",
            },
        ],
        "global_dedup.nx": [
            {
                "a.bin": a + b,
                "changed.bin": a + c,
                "duplicate.bin": a + b,
                "empty.bin": b"",
            },
            {
                "a.bin": a + b,
                "changed.bin": a + d,
                "copy.bin": a + b,
                "tail.bin": tail,
            },
            {"cross.bin": a},
        ],
        "zstd_only.nx": [
            {"compressible.bin": bytes([0x5A]) * (2 * chunk + chunk // 2)}
        ],
        "zstd_no_readback.nx": [
            {"compressible.bin": bytes([0x5A]) * (2 * chunk + chunk // 2)}
        ],
        "mixed_codecs.nx": [
            {
                "compressible.bin": bytes([0x5A]) * (2 * chunk + chunk // 2),
                "incompressible.bin": deterministic_noise(chunk),
            }
        ],
    }


def deterministic_noise(size: int) -> bytes:
    state = 0x13579BDF
    result = bytearray()
    while len(result) < size:
        state ^= (state << 13) & 0xFFFFFFFF
        state ^= state >> 17
        state ^= (state << 5) & 0xFFFFFFFF
        result.extend(state.to_bytes(4, "little"))
    return bytes(result[:size])


def assert_codec_counts(blob: bytes, raw: int, zstd: int) -> None:
    index_offset = struct.unpack_from("<Q", blob, 88)[0]
    block_count = struct.unpack_from("<Q", blob, 96)[0]
    codecs = [blob[index_offset + i * BLOCK.size + 16] for i in range(block_count)]
    if codecs.count(0) != raw or codecs.count(1) != zstd:
        raise AssertionError(f"expected codec counts RAW={raw}, ZSTD={zstd}; got {codecs.count(0)}/{codecs.count(1)}")


def assert_global_dedup_layout(blob: bytes, validator) -> None:
    block_index_off = struct.unpack_from("<Q", blob, 88)[0]
    block_count = struct.unpack_from("<Q", blob, 96)[0]
    manifest_off, manifest_size = struct.unpack_from("<QQ", blob, 72)
    manifest = blob[manifest_off : manifest_off + manifest_size]
    _, file_count, _, reference_count = struct.unpack_from("<4I", manifest, 16)
    _, _, _, ref_off, _, _ = struct.unpack_from("<6Q", manifest, 32)
    references = [struct.unpack_from("<I", manifest, ref_off + index * 4)[0]
                  for index in range(reference_count)]
    if block_count != 5 or file_count != 8:
        raise AssertionError(
            f"global dedup expected five physical blocks and eight File records, got {block_count}/{file_count}"
        )
    if set(references) != set(range(block_count)):
        raise AssertionError("writer output contains an orphan or dangling block reference")
    physical: set[tuple[int, bytes]] = set()
    for index in range(block_count):
        offset, stored, logical, codec, flags, reserved0, _first_check = BLOCK.unpack_from(
            blob, block_index_off + index * BLOCK.size
        )
        reserved1 = 0
        if codec not in (0, 1) or flags != 0 or reserved0 != 0 or reserved1 != 0:
            raise AssertionError("unexpected block descriptor fields")
        payload = blob[offset : offset + stored]
        if codec == 0:
            if stored != logical:
                raise AssertionError("RAW block stored size differs from logical size")
            decoded = payload
        else:
            decoder = validator.zstd.ZstdDecompressor(
                options={validator.zstd.DecompressionParameter.window_log_max: 20}
            )
            decoded = decoder.decompress(payload, max_length=logical + 1)
            if len(decoded) != logical or not decoder.eof or decoder.unused_data:
                raise AssertionError("block is not exactly one independent Zstandard frame")
        key = (logical, decoded)
        if len(payload) != stored or key in physical:
            raise AssertionError("writer emitted duplicate or truncated physical blocks")
        physical.add(key)
    if len(physical) != block_count:
        raise AssertionError("writer emitted duplicate physical blocks")


def add_chunk_size_expected(expected: dict[str, list[dict[str, bytes]]]) -> None:
    for chunk_kib in (64, 128, 256, 512, 1024):
        expected[f"chunk_{chunk_kib}k.nx"] = [
            {"chunk.bin": bytes([0x6B]) * (2 * chunk_kib * 1024 + 37)}
        ]


def native_metadata_rows(blob: bytes) -> list[tuple[int, int, int, int]]:
    off, size = struct.unpack_from("<QQ", blob, 24)
    catalog = blob[off:off+size]
    header = CATALOG_HEADER.unpack_from(catalog)
    rows = []
    for i in range(header[10]):
        type_id, _, _, _, _, at, length = struct.unpack_from("<IIHHIQQ", catalog, header[17] + i * 32)
        if type_id == 0x00010005:
            count = struct.unpack_from("<I", catalog, at + 8)[0]
            assert length == 16 + count * 16
            rows.extend(struct.unpack_from("<4I", catalog, at + 16 + j * 16) for j in range(count))
    return rows


def assert_source_identifiers(blob: bytes) -> None:
    catalog_off = struct.unpack_from("<Q", blob, 24)[0]
    catalog_size = struct.unpack_from("<Q", blob, 32)[0]
    catalog = blob[catalog_off : catalog_off + catalog_size]
    header = CATALOG_HEADER.unpack_from(catalog)
    program_count = header[6]
    snapshot_count = header[7]
    dependency_count = header[9]
    program_off = header[13]
    snapshot_off = header[14]
    dependency_off = header[16]
    string_off, string_size = header[18], header[19]
    assert program_count == 1 and snapshot_count == 3
    app_id, platform = struct.unpack_from("<QB", catalog, header[12])
    assert app_id == 0x0100_0000_0000_3000 and platform == 2
    program = PROGRAM.unpack_from(catalog, program_off)
    assert program[0] == 0x0100_0000_0000_3001
    assert program[5] == bytes([0x20]) * 16
    if program[8] != NO_INDEX:
        raise AssertionError("writer emitted ambiguous Program NPDM")
    native_rows = native_metadata_rows(blob)
    if len(native_rows) != 2:
        raise AssertionError("missing versioned NPDM/firmware metadata")
    for row, expected_npdm, firmware in zip(native_rows, (b"base NPDM", b"update NPDM"), (100, 200)):
        if media_payload(blob, row[1], 3) != expected_npdm or row[2:] != (firmware, 1):
            raise AssertionError("NPDM/firmware belongs to the wrong version")
    snapshots = [SNAPSHOT.unpack_from(catalog, snapshot_off + i * SNAPSHOT.size)
                 for i in range(snapshot_count)]
    strings = catalog[string_off : string_off + string_size]
    actual = [(row[1], row[4], row[9], row[13]) for row in snapshots]
    expected = [
        (0, 10, bytes([0x10]) * 16, bytes([0x30]) * 32),
        (1, 11, bytes([0x11]) * 16, bytes([0x31]) * 32),
        (2, 1, bytes([0x13]) * 16, bytes([0x33]) * 32),
    ]
    if actual != expected:
        raise AssertionError("writer changed snapshot type/version or source Content/Build ID associations")
    display_versions = [strings[row[7] : row[7] + row[8]].decode("utf-8") if row[7] != NO_INDEX else None
                        for row in snapshots]
    if display_versions != ["1.0.0", "1.1.0", None]:
        raise AssertionError("writer changed snapshot display versions")
    if dependency_count != 1:
        raise AssertionError("writer dropped the source snapshot dependency")
    dep_kind, flags, reserved0, content_id, target_identity, reserved1 = struct.unpack_from(
        "<BBH16s32sI", catalog, dependency_off
    )
    if (dep_kind, flags, reserved0, content_id, target_identity, reserved1) != (
        3, 0, 0, bytes(16), bytes([0x42]) * 32, 0
    ):
        raise AssertionError("writer changed the source snapshot dependency")


def media_payload(blob: bytes, index: int, expected_type: int) -> bytes:
    media_off = struct.unpack_from("<Q", blob, 40)[0]
    media_count = struct.unpack_from("<I", blob, media_off + 16)[0]
    if index >= media_count:
        raise AssertionError("metadata refers to a missing Media record")
    media_type, _, _, stored, logical, data_off, _, _ = struct.unpack_from(
        "<HBBQQQ32sI", blob, media_off + 32 + index * 64
    )
    if media_type != expected_type or stored != logical:
        raise AssertionError("metadata Media reference has the wrong type or size")
    return blob[media_off + data_off : media_off + data_off + stored]


def assert_snapshot_metadata_history(blob: bytes) -> None:
    catalog_off, catalog_size = struct.unpack_from("<QQ", blob, 24)
    catalog = blob[catalog_off : catalog_off + catalog_size]
    header = CATALOG_HEADER.unpack_from(catalog)
    app_off, extension_off = header[12], header[17]
    extension_count, string_off, string_size = header[10], header[18], header[19]
    app_metadata_snapshot = struct.unpack_from("<I", catalog, app_off + 68)[0]
    if app_metadata_snapshot != 1:
        raise AssertionError("Catalog selected metadata source is not the expected update snapshot")
    strings = catalog[string_off : string_off + string_size]
    history_ref = None
    for i in range(extension_count):
        type_id, flags, owner_kind, reserved, owner, offset, size = struct.unpack_from(
            "<IIHHIQQ", catalog, extension_off + i * 32
        )
        if type_id == 0x00010004 and owner_kind == 1 and owner == 0:
            history_ref = (offset, size)
            break
    if history_ref is None:
        raise AssertionError("required Snapshot Metadata History extension is missing")
    offset, size = history_ref
    payload = catalog[offset : offset + size]
    magic, version, header_size, record_count, localized_count, opaque_count, reserved = struct.unpack_from(
        "<4sHHIIII", payload
    )
    if (magic, version, header_size, record_count, localized_count, opaque_count, reserved) != (
        b"NXSM", 1, 24, 1, 1, 1, 0
    ):
        raise AssertionError("Snapshot Metadata History has unexpected dimensions")
    row = struct.unpack_from("<12I", payload, 24)
    snapshot, title_off, title_len, publisher_off, publisher_len, icon, nacp, first_local, nlocal, first_opaque, nopaque, row_reserved = row
    decode = lambda off, length: strings[off : off + length].decode("utf-8")
    if (snapshot, decode(title_off, title_len), decode(publisher_off, publisher_len), first_local,
            nlocal, first_opaque, nopaque, row_reserved) != (
        0, "Original native title", "Original publisher", 0, 1, 0, 1, 0
    ):
        raise AssertionError("historical snapshot title, publisher, or ranges were not preserved")
    local_off = 24 + record_count * 48
    lang_off, lang_len, local_title_off, local_title_len, local_icon, local_flags = struct.unpack_from(
        "<6I", payload, local_off
    )
    if (decode(lang_off, lang_len), decode(local_title_off, local_title_len), local_flags) != (
        "ja", "元のタイトル", 0
    ):
        raise AssertionError("historical localized title was not preserved exactly")
    opaque_ref_off = local_off + localized_count * 24
    opaque_index = struct.unpack_from("<I", payload, opaque_ref_off)[0]
    if media_payload(blob, icon, 1) != b"original PNG bytes" or media_payload(blob, local_icon, 1) != b"original PNG bytes":
        raise AssertionError("original snapshot icon bytes were changed")
    if media_payload(blob, nacp, 4) != b"synthetic base NACP":
        raise AssertionError("original snapshot NACP bytes were changed")
    if media_payload(blob, opaque_index, 5) != b"base opaque source metadata":
        raise AssertionError("original opaque native metadata bytes were changed")


def compile_reader(output: Path) -> Path:
    configured = os.environ.get("CXX")
    compiler = shutil.which(configured) if configured else (shutil.which("c++") or shutil.which("g++") or shutil.which("cl"))
    if not compiler:
        raise RuntimeError("C++17 compiler is required for Rust writer interop tests")
    is_msvc = Path(compiler).name.lower() in {"cl", "cl.exe"}
    source = ROOT / "tests" / "conformance" / "writer_output_reader.cpp"
    executable = output / ("writer_output_reader.exe" if is_msvc else "writer_output_reader")
    if is_msvc:
        command = [compiler, "/nologo", "/std:c++17", "/EHsc", "/W4", "/WX", "/O2",
                   f"/I{ROOT / 'dist'}", f"/I{ROOT / 'tests' / 'support'}", str(source),
                   f"/Fo:{output / 'writer_output_reader.obj'}", f"/Fe:{executable}"]
    else:
        dynamic_library = [] if sys.platform in ("darwin", "win32") else ["-ldl"]
        command = [compiler, "-std=c++17", "-O2", "-Wall", "-Wextra", "-Werror", "-pedantic",
                   f"-I{ROOT / 'dist'}", f"-I{ROOT / 'tests' / 'support'}", str(source),
                   *dynamic_library, "-pthread", "-o", str(executable)]
    built = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if built.returncode:
        raise RuntimeError(built.stdout)
    return executable


def generate(cargo: str, directory: Path) -> None:
    subprocess.run([cargo, "run", "--locked", "--quiet", "--manifest-path", str(CARGO_MANIFEST), "--example",
                    "write_conformance_archives", "--", str(directory)], cwd=ROOT, check=True)


def main() -> int:
    validator = load_validator()
    with tempfile.TemporaryDirectory(prefix="nx-rust-writer-") as temp:
        base = Path(temp)
        first, second = base / "first", base / "second"
        first.mkdir()
        second.mkdir()
        cargo = cargo_command()
        generate(cargo, first)
        generate(cargo, second)
        expected = expected_contents()
        add_chunk_size_expected(expected)
        for name, snapshots in expected.items():
            path = first / name
            blob = path.read_bytes()
            stats = validator.validate_archive(blob, verify_payloads=True)
            if name == "global_dedup.nx":
                assert_global_dedup_layout(blob, validator)
            if name == "zstd_only.nx":
                assert_codec_counts(blob, raw=0, zstd=2)
            if name == "zstd_no_readback.nx":
                assert_codec_counts(blob, raw=0, zstd=2)
            if name == "mixed_codecs.nx":
                assert_codec_counts(blob, raw=1, zstd=2)
            if name == "multiple_programs.nx" and (stats["blocks"] != 2 or stats["files"] != 3):
                raise AssertionError("identical content across Programs was not block-deduplicated")
            if name == "snapshot_metadata.nx":
                assert_source_identifiers(blob)
                assert_snapshot_metadata_history(blob)
            actual = parse_file_contents(blob, validator)
            if actual != snapshots:
                raise AssertionError(f"independently extracted logical bytes differ for {name}")
            if blob != (second / name).read_bytes():
                raise AssertionError(f"Rust writer output is not deterministic for {name}")
            print(f"PASS Python validate_archive + exact file-byte comparison + deterministic bytes: {name}: {stats}")
        metadata_stats = validator.validate_archive((first / "snapshot_metadata.nx").read_bytes(), verify_payloads=True)
        if metadata_stats["files"] != 3 or metadata_stats["snapshots"] != 3:
            raise AssertionError("unchanged-path File record reuse or multiple snapshot serialization failed")
        def identities(blob: bytes) -> list[bytes]:
            cat = struct.unpack_from("<Q", blob, 24)[0]
            count = struct.unpack_from("<I", blob, cat + 28)[0]
            table = struct.unpack_from("<Q", blob, cat + 64)[0] + cat
            return [blob[table+i*120+56:table+i*120+88] for i in range(count)]
        original_ids = identities((first / "snapshot_metadata.nx").read_bytes())
        for name, indices, firmware in (
            ("removed_update.nx", [0, 2], [100]),
            ("removed_dlc.nx", [0, 1], [100, 200]),
            ("removed_base.nx", [1, 2], [200]),
        ):
            blob = (first / name).read_bytes()
            assert identities(blob) == [original_ids[i] for i in indices]
            assert [r[2] for r in native_metadata_rows(blob)] == firmware
        assert identities((first / "added_update.nx").read_bytes())[:2] == original_ids[:2]
        assert identities((first / "added_dlc.nx").read_bytes())[:3] == original_ids
        # Native metadata is not part of snapshot identity, but its ownership is preserved.
        assert [r[2] for r in native_metadata_rows((first / "added_update.nx").read_bytes())] == [100, 200, 300]
        assert validator.validate_archive((first / "removed_update.nx").read_bytes())["blocks"] == 2
        reader = compile_reader(base)
        blob = (first / "snapshot_metadata.nx").read_bytes()
        cat_off, cat_size = struct.unpack_from("<QQ", blob, 24)
        header = CATALOG_HEADER.unpack_from(blob, cat_off)
        native_at = None
        for i in range(header[10]):
            type_id, _, _, _, _, at, _ = struct.unpack_from("<IIHHIQQ", blob, cat_off + header[17] + i*32)
            if type_id == 0x00010005:
                native_at = cat_off + at
        assert native_at is not None
        mutations = [(native_at+4, "<H", 2), (native_at+8, "<I", 3),
                     (native_at+16, "<I", 65536), (native_at+32, "<I", 0),
                     (native_at+28, "<I", 2), (native_at+20, "<I", 65535)]
        for i, (at, fmt, value) in enumerate(mutations):
            broken = bytearray(blob)
            struct.pack_into(fmt, broken, at, value)
            broken[120:152] = hashlib.sha256(broken[cat_off:cat_off+cat_size]).digest()
            struct.pack_into("<I", broken, 252, validator.crc32c(broken[:252]))
            broken[4096:4352] = broken[:256]  # keep both header slots identical
            try:
                validator.validate_archive(bytes(broken))
            except validator.InvalidArchive:
                pass
            else:
                raise AssertionError("independent validator accepted malformed native metadata")
            path = base / f"bad_native_{i}.nx"
            path.write_bytes(broken)
            subprocess.run([str(reader), str(path), "reject_open"], check=True)
        print("PASS malformed versioned native metadata rejected independently in Rust-output Python/C++ tests")
        zstd_blob = bytearray((first / "zstd_only.nx").read_bytes())
        first_block = struct.unpack_from("<Q", zstd_blob, 88)[0]
        first_payload = struct.unpack_from("<Q", zstd_blob, first_block)[0]
        zstd_blob[first_payload] ^= 0x40
        try:
            validator.validate_archive(bytes(zstd_blob), verify_payloads=True)
        except validator.InvalidArchive:
            pass
        else:
            raise AssertionError("independent Python validator accepted a corrupted Zstandard payload")
        corrupt_zstd_path = base / "corrupted_zstd_payload.nx"
        corrupt_zstd_path.write_bytes(zstd_blob)
        result = subprocess.run([str(reader), str(corrupt_zstd_path), "reject_verify"], cwd=ROOT, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if result.returncode:
            raise RuntimeError(result.stdout)
        print("PASS corrupted Zstandard payload rejected by independent Python and C++ full verification")
        for name, case in (("empty.nx", "empty"), ("raw_multiblock.nx", "raw"), ("snapshot_metadata.nx", "metadata"),
                           ("dlc_only.nx", "dlc"), ("multiple_programs.nx", "programs"), ("removed_update.nx", "removed_update"),
                           ("removed_dlc.nx", "removed_dlc"), ("removed_base.nx", "removed_base"),
                           ("added_update.nx", "added_update"), ("added_dlc.nx", "added_dlc"),
                           ("global_dedup.nx", "global_dedup"), ("zstd_only.nx", "zstd_only"),
                           ("zstd_no_readback.nx", "zstd_only"),
                           ("mixed_codecs.nx", "mixed_codecs"),
                           ("chunk_64k.nx", "chunk_64k"), ("chunk_128k.nx", "chunk_128k"),
                           ("chunk_256k.nx", "chunk_256k"), ("chunk_512k.nx", "chunk_512k"),
                           ("chunk_1024k.nx", "chunk_1024k")):
            result = subprocess.run([str(reader), str(first / name), case], cwd=ROOT, text=True,
                                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            if result.returncode:
                raise RuntimeError(result.stdout)
            print(result.stdout.strip())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
