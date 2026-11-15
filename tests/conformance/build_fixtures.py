"""Build the fixed, synthetic NX conformance corpus (not a general packer)."""

from __future__ import annotations

import base64
import hashlib
import json
import struct
import zlib
from pathlib import Path
from compression import zstd

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures"
NO_INDEX = 0xFFFFFFFF
ZSTD_A_4096_FRAME = bytes.fromhex("28b52ffd60000f4d00001041410100fbf70116")
JPEG_PIXEL = base64.b64decode(
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABsSFBcUERsXFhceHBsgKEIrKCUlKFE6PTBCYFVlZF9V"
    "XVtqeJmBanGQc1tdhbWGkJ6jq62rZ4C8ybqmx5moq6T/2wBDARweHigjKE4rK06kbl1upKSkpKSk"
    "pKSkpKSkpKSkpKSkpKSkpKSkpKSkpKSkpKSkpKSkpKSkpKSkpKSkpKSkpKT/wAARCAABAAEDASIA"
    "AhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAX/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFAEB"
    "AAAAAAAAAAAAAAAAAAAAA//EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAMAwEAAhEDEQA/AIQBBv/Z"
)

HEADER = struct.Struct("<4sHHIHH13Q32s32s32sQI24sI")
SLOT_B = 4096
DATA_START = 2 * SLOT_B
CHECK_SIZE = 16384
CATALOG_HEADER = struct.Struct("<4sHHQ8I8Q16s")
APPLICATION = struct.Struct("<QBBH14II8s")
PROGRAM = struct.Struct("<QIBBH16s4I16s")
SNAPSHOT = struct.Struct("<IBBHIIQII16sII32s32s")
LOCALIZED = struct.Struct("<6I")
DEPENDENCY = struct.Struct("<BBH16s32sI")
EXTENSION = struct.Struct("<IIHHIQQ")
MEDIA_HEADER = struct.Struct("<4sHHQIHHII")
MEDIA_RECORD = struct.Struct("<HBBQQQ32sI")
MANIFEST_HEADER = struct.Struct("<4sHHQ4I6Q16s")
DIRECTORY = struct.Struct("<4I")
ENTRY = struct.Struct("<QIIBBHI")
FILE = struct.Struct("<QIIBBH32s12s")
BLOCK = struct.Struct("<QIIBBHI")

assert HEADER.size == 256
assert CATALOG_HEADER.size == 128
assert APPLICATION.size == 80
assert PROGRAM.size == 64
assert SNAPSHOT.size == 120
assert LOCALIZED.size == 24
assert DEPENDENCY.size == 56
assert EXTENSION.size == 32
assert MEDIA_HEADER.size == 32
assert MEDIA_RECORD.size == 64
assert MANIFEST_HEADER.size == 96
assert DIRECTORY.size == 16
assert ENTRY.size == 24
assert FILE.size == 64
assert BLOCK.size == 24


def align(value: int, alignment: int) -> int:
    return (value + alignment - 1) & ~(alignment - 1)


def _crc32c_table() -> tuple[int, ...]:
    table = []
    for byte in range(256):
        crc = byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
        table.append(crc)
    return tuple(table)


_CRC32C_TABLE = _crc32c_table()


def crc32c(data: bytes) -> int:
    # Large benchmark fixtures reuse this generator. A byte table avoids eight
    # interpreted bit steps per byte; the independent validator stays bitwise.
    crc = 0xFFFFFFFF
    table = _CRC32C_TABLE
    for byte in data:
        crc = (crc >> 8) ^ table[(crc ^ byte) & 0xFF]
    return crc ^ 0xFFFFFFFF


def sha(data: bytes) -> bytes:
    return hashlib.sha256(data).digest()


def slice_checks(logical: bytes) -> list[int]:
    """NX 0.2 integrity entries: CRC-32C of each 16 KiB logical slice."""
    return [crc32c(logical[i:i + CHECK_SIZE]) for i in range(0, len(logical), CHECK_SIZE)]


def finish_header(archive: bytearray, fields: tuple, catalog: bytes, manifest: bytes,
                  index_bytes: bytes, generation: int = 0) -> None:
    header = bytearray(256)
    HEADER.pack_into(header, 0, b"NXAR", 0, 2, 256, 1, 0, *fields,
                     sha(catalog), sha(manifest), sha(index_bytes), generation, CHECK_SIZE,
                     bytes(24), 0)
    struct.pack_into("<I", header, 252, crc32c(header[:252]))
    archive[:256] = header
    archive[SLOT_B:SLOT_B + 256] = header


class Strings:
    def __init__(self) -> None:
        self.data = bytearray()
        self.offsets: dict[bytes, tuple[int, int]] = {}

    def add(self, value: str | None) -> tuple[int, int]:
        if value is None:
            return NO_INDEX, 0
        raw = value.encode("utf-8")
        if not raw:
            return NO_INDEX, 0
        if raw in self.offsets:
            return self.offsets[raw]
        offset = len(self.data)
        self.data.extend(raw)
        result = (offset, len(raw))
        self.offsets[raw] = result
        return result


def png_pixel(color: tuple[int, int, int, int]) -> bytes:
    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

    scanline = bytes((0, *color))
    zlib_stream = (
        b"\x78\x01"
        + b"\x01"
        + struct.pack("<HH", len(scanline), 0xFFFF - len(scanline))
        + scanline
        + struct.pack(">I", zlib.adler32(scanline) & 0xFFFFFFFF)
    )
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib_stream)
        + chunk(b"IEND", b"")
    )


def make_manifest(
    directories: list[list[tuple[str, int, int]]],
    files: list[dict],
    snapshot_roots: list[int],
    platform: int,
    app_id: int,
    program_ids: list[int],
    snapshot_versions: list[tuple[int, int]],
    snapshot_content_ids: list[bytes] | None = None,
    snapshot_platforms: list[int] | None = None,
    snapshot_app_ids: list[int] | None = None,
) -> tuple[bytes, list[int], list[bytes]]:
    path_pool = bytearray()
    entry_records: list[bytes] = []
    directory_records: list[bytes] = []
    first_entries: list[int] = []
    next_entry = 0
    for children in directories:
        ordered = sorted(children, key=lambda child: child[0].encode("utf-8"))
        first_entries.append(next_entry)
        for name, kind, target in ordered:
            raw_name = name.encode("utf-8")
            name_offset = 0  # Filled after the path pool's absolute section offset is known.
            entry_records.append(ENTRY.pack(name_offset, len(raw_name), target, kind, 0, 0, 0))
            path_pool.extend(raw_name)
            next_entry += 1
        directory_records.append(DIRECTORY.pack(first_entries[-1], len(ordered), 0, 0))

    block_refs: list[int] = []
    file_records: list[bytes] = []
    file_first_refs: list[int] = []
    for item in files:
        file_first_refs.append(len(block_refs))
        block_refs.extend(item["block_ids"])
        file_records.append(
            FILE.pack(
                len(item["bytes"]),
                file_first_refs[-1],
                len(item["block_ids"]),
                item["chunk_log2"],
                0,
                0,
                sha(item["bytes"]),
                bytes(12),
            )
        )

    cursor = MANIFEST_HEADER.size
    directory_offset = align(cursor, 8)
    cursor = directory_offset + len(directory_records) * DIRECTORY.size
    file_offset = align(cursor, 8) if file_records else 0
    if file_records:
        cursor = file_offset + len(file_records) * FILE.size
    entry_offset = align(cursor, 8) if entry_records else 0
    if entry_records:
        cursor = entry_offset + len(entry_records) * ENTRY.size
    reference_offset = align(cursor, 4) if block_refs else 0
    if block_refs:
        cursor = reference_offset + len(block_refs) * 4
    path_offset = align(cursor, 8)
    cursor = path_offset + len(path_pool)
    manifest = bytearray(cursor)
    for index, record in enumerate(directory_records):
        start = directory_offset + index * DIRECTORY.size
        manifest[start : start + DIRECTORY.size] = record
    for index, record in enumerate(file_records):
        start = file_offset + index * FILE.size
        manifest[start : start + FILE.size] = record
    name_position = 0
    for index, record in enumerate(entry_records):
        fields = list(ENTRY.unpack(record))
        fields[0] = path_offset + name_position
        manifest[entry_offset + index * ENTRY.size : entry_offset + (index + 1) * ENTRY.size] = ENTRY.pack(*fields)
        name_position += fields[1]
    for index, block_id in enumerate(block_refs):
        struct.pack_into("<I", manifest, reference_offset + index * 4, block_id)
    manifest[path_offset : path_offset + len(path_pool)] = path_pool
    MANIFEST_HEADER.pack_into(
        manifest,
        0,
        b"NXMF",
        1,
        MANIFEST_HEADER.size,
        len(manifest),
        len(directories),
        len(files),
        len(entry_records),
        len(block_refs),
        directory_offset,
        file_offset,
        entry_offset,
        reference_offset,
        path_offset,
        len(path_pool),
        bytes(16),
    )

    snapshot_content_ids = snapshot_content_ids if snapshot_content_ids is not None else [bytes(16)] * len(snapshot_roots)
    if len(snapshot_content_ids) != len(snapshot_roots):
        raise ValueError("snapshot content ID count must match snapshot count")
    snapshot_platforms = snapshot_platforms or [platform] * len(snapshot_roots)
    snapshot_app_ids = snapshot_app_ids or [app_id] * len(snapshot_roots)
    logical_sizes: list[int] = []
    identities: list[bytes] = []
    for snapshot_i, root in enumerate(snapshot_roots):
        seen: set[tuple[int, int]] = set()
        nodes: list[tuple[bytes, int, int | None]] = []

        def walk(directory_id: int, prefix: bytes) -> int:
            key = (1, directory_id)
            if key in seen:
                raise ValueError("fixture tree reuses a node within one snapshot")
            seen.add(key)
            nodes.append((prefix, 1, None))
            first, count, _, _ = DIRECTORY.unpack_from(manifest, directory_offset + directory_id * DIRECTORY.size)
            total = 0
            for entry_i in range(first, first + count):
                name_off, name_len, target, kind, _, _, _ = ENTRY.unpack_from(manifest, entry_offset + entry_i * ENTRY.size)
                name = bytes(manifest[name_off : name_off + name_len])
                full_path = name if not prefix else prefix + b"/" + name
                if kind == 1:
                    total += walk(target, full_path)
                else:
                    file_key = (2, target)
                    if file_key in seen:
                        raise ValueError("fixture file is repeated within one snapshot")
                    seen.add(file_key)
                    size = len(files[target]["bytes"])
                    nodes.append((full_path, 2, target))
                    total += size
            return total

        logical_sizes.append(walk(root, b""))
        nodes.sort(key=lambda node: node[0])
        platform_value = snapshot_platforms[snapshot_i]
        app_id_value = snapshot_app_ids[snapshot_i]
        program_id = program_ids[snapshot_i]
        canonical = bytearray(b"NX-SNAPSHOT-ID\0")
        snapshot_type, numeric_version = snapshot_versions[snapshot_i]
        content_id = snapshot_content_ids[snapshot_i]
        if len(content_id) != 16:
            raise ValueError("snapshot content ID must be exactly 16 bytes")
        canonical.extend(struct.pack("<H B Q Q B I 16s I", 3, platform_value, app_id_value, program_id, snapshot_type, numeric_version, content_id, len(nodes)))
        for path, kind, file_id in nodes:
            canonical.extend(struct.pack("<B I", kind, len(path)))
            canonical.extend(path)
            if kind == 2:
                content = files[file_id]["bytes"]
                canonical.extend(struct.pack("<Q", len(content)))
                canonical.extend(sha(content))
        identities.append(sha(bytes(canonical)))
    return bytes(manifest), logical_sizes, identities


def make_catalog(
    app_id: int,
    platform: int,
    title: str | None,
    publisher: str | None,
    programs: list[dict],
    snapshots: list[dict],
    logical_sizes: list[int],
    identities: list[bytes],
    localized: list[dict],
    default_icon: int,
    metadata_history: list[dict] | None = None,
    extra_root: int | None = None,
) -> bytes:
    metadata_history = metadata_history or []
    strings = Strings()
    app_title = strings.add(title)
    publisher_ref = strings.add(publisher)
    local_records: list[bytes] = []
    for item in localized:
        lang = strings.add(item["language"])
        local_title = strings.add(item.get("title"))
        local_records.append(LOCALIZED.pack(*lang, *local_title, item.get("icon", NO_INDEX), 0))
    snap_version_refs = [strings.add(item.get("display")) for item in snapshots]
    history_records: list[bytes] = []
    history_localized: list[bytes] = []
    history_opaque: list[int] = []
    previous_history_snapshot = -1
    for item in metadata_history:
        snapshot_index = item["snapshot"]
        if snapshot_index <= previous_history_snapshot:
            raise ValueError("snapshot metadata history must be sorted by snapshot index")
        previous_history_snapshot = snapshot_index
        title_ref = strings.add(item.get("title"))
        publisher_history_ref = strings.add(item.get("publisher"))
        local_first = len(history_localized)
        prior_language = ""
        for local in item.get("localized", []):
            if local["language"] <= prior_language:
                raise ValueError("snapshot metadata history languages must be sorted")
            prior_language = local["language"]
            lang_ref = strings.add(local["language"])
            local_title_ref = strings.add(local.get("title"))
            history_localized.append(LOCALIZED.pack(*lang_ref, *local_title_ref, local.get("icon", NO_INDEX), 0))
        opaque_first = len(history_opaque)
        history_opaque.extend(item.get("opaque", []))
        history_records.append(struct.pack(
            "<12I", snapshot_index, *title_ref, *publisher_history_ref,
            item.get("icon", NO_INDEX), item.get("nacp", NO_INDEX),
            local_first, len(item.get("localized", [])), opaque_first,
            len(item.get("opaque", [])), 0,
        ))
    history_payload = b""
    if history_records:
        history_payload = (
            struct.pack("<4sHHIIII", b"NXSM", 1, 24, len(history_records), len(history_localized), len(history_opaque), 0)
            + b"".join(history_records)
            + b"".join(history_localized)
            + b"".join(struct.pack("<I", index) for index in history_opaque)
        )

    application_offset = CATALOG_HEADER.size
    catalog_cursor = application_offset + APPLICATION.size
    program_offset = align(catalog_cursor, 8) if programs else 0
    if programs:
        catalog_cursor = program_offset + len(programs) * PROGRAM.size
    snapshot_offset = align(catalog_cursor, 8)
    localized_offset = align(snapshot_offset + len(snapshots) * SNAPSHOT.size, 8) if localized else 0
    cursor = localized_offset + len(local_records) * LOCALIZED.size if localized else snapshot_offset + len(snapshots) * SNAPSHOT.size
    extension_count = int(bool(history_payload)) + int(extra_root is not None)
    extension_offset = align(cursor, 8) if extension_count else 0
    if extension_count:
        cursor = extension_offset + extension_count * EXTENSION.size
    cursor = align(cursor, 8)
    string_offset = cursor
    string_size = len(strings.data)
    cursor = string_offset + string_size
    history_payload_offset = align(cursor, 8) if history_payload else 0
    if history_payload:
        cursor = history_payload_offset + len(history_payload)

    catalog = bytearray(cursor)
    for index, item in enumerate(programs):
        flags = 1 if item.get("content_id") else 0
        rows = [i for i, snap in enumerate(snapshots) if snap["program"] == index]
        PROGRAM.pack_into(
            catalog,
            program_offset + index * PROGRAM.size,
            item["id"],
            item["application"],
            item.get("kind", 0),
            flags,
            0,
            item.get("content_id", bytes(16)),
            rows[0],
            len(rows),
            item.get("npdm", NO_INDEX),
            0,
            bytes(16),
        )

    dependency_cursor = 0
    for index, item in enumerate(snapshots):
        display = snap_version_refs[index]
        SNAPSHOT.pack_into(
            catalog,
            snapshot_offset + index * SNAPSHOT.size,
            item.get("program", NO_INDEX),
            item.get("type", 0),
            1 if item.get("build_id") else 0,
            0,
            item.get("version", 0),
            item["root"],
            logical_sizes[index],
            display[0],
            display[1],
            item.get("content_id", bytes(16)),
            dependency_cursor,
            0,
            identities[index],
            item.get("build_id", bytes(32)),
        )

    if localized:
        for i, record in enumerate(local_records):
            start = localized_offset + i * LOCALIZED.size
            catalog[start : start + LOCALIZED.size] = record

    app_program_rows = [i for i, item in enumerate(programs) if item["application"] == 0]
    app_snap_rows = list(range(len(snapshots)))
    app_flags = 0
    if any(item.get("type", 0) == 0 for item in snapshots):
        app_flags |= 1
    if any(item.get("type", 0) == 2 for item in snapshots):
        app_flags |= 2
    has_display_metadata = title is not None or publisher is not None or bool(localized) or default_icon != NO_INDEX
    metadata_candidates = [i for i, item in enumerate(snapshots) if item.get("type", 0) != 2]
    metadata_snapshot = max(metadata_candidates, key=lambda i: (snapshots[i].get("version", 0), i)) if has_display_metadata and metadata_candidates else NO_INDEX
    APPLICATION.pack_into(
        catalog,
        application_offset,
        app_id,
        platform,
        app_flags,
        0,
        app_program_rows[0] if app_program_rows else NO_INDEX,
        len(app_program_rows),
        app_snap_rows[0],
        len(app_snap_rows),
        *app_title,
        *publisher_ref,
        default_icon,
        0 if localized else 0,
        len(localized),
        NO_INDEX,
        0,
        0,
        metadata_snapshot,
        bytes(8),
    )
    catalog[string_offset : string_offset + string_size] = strings.data
    if history_payload:
        EXTENSION.pack_into(catalog, extension_offset, 0x00010004, 1, 1, 0, 0, history_payload_offset, len(history_payload))
        catalog[history_payload_offset : history_payload_offset + len(history_payload)] = history_payload
    if extra_root is not None:
        extra_offset = align(len(catalog), 8)
        catalog.extend(bytes(extra_offset - len(catalog)))
        payload = struct.pack("<4sHHII", b"NXEX", 1, 16, extra_root, 0)
        EXTENSION.pack_into(catalog, extension_offset + (EXTENSION.size if history_payload else 0),
                            0x00010009, 0, 0, 0, NO_INDEX, extra_offset, len(payload))
        catalog.extend(payload)
    CATALOG_HEADER.pack_into(
        catalog,
        0,
        b"NXCT",
        1,
        CATALOG_HEADER.size,
        len(catalog),
        0,
        1,
        len(programs),
        len(snapshots),
        len(localized),
        0,
        extension_count,
        0,
        application_offset,
        program_offset,
        snapshot_offset,
        localized_offset,
        0,
        extension_offset,
        string_offset,
        string_size,
        bytes(16),
    )
    return bytes(catalog)


def make_media(payloads: list[tuple[int, bytes]]) -> tuple[bytes, list[int]]:
    if not payloads:
        return b"", []
    unique: list[tuple[int, bytes]] = []
    indexes: list[int] = []
    seen: dict[tuple[int, bytes], int] = {}
    for media_type, payload in payloads:
        key = (media_type, payload)
        if key not in seen:
            seen[key] = len(unique)
            unique.append(key)
        indexes.append(seen[key])
    payloads = unique
    payload_start = align(MEDIA_HEADER.size + len(payloads) * MEDIA_RECORD.size, 16)
    cursor = payload_start
    records: list[bytes] = []
    offsets: list[int] = []
    blobs = bytearray()
    for media_type, payload in payloads:
        cursor = align(cursor, 16)
        if len(blobs) < cursor - payload_start:
            blobs.extend(bytes(cursor - payload_start - len(blobs)))
        offsets.append(cursor)
        records.append(MEDIA_RECORD.pack(media_type, 0, 0, len(payload), len(payload), cursor, sha(payload), 0))
        blobs.extend(payload)
        cursor += len(payload)
    total_size = cursor
    media = bytearray(total_size)
    MEDIA_HEADER.pack_into(media, 0, b"NXMD", 1, MEDIA_HEADER.size, total_size, len(payloads), MEDIA_RECORD.size, 0, MEDIA_HEADER.size, payload_start)
    for i, record in enumerate(records):
        start = MEDIA_HEADER.size + i * MEDIA_RECORD.size
        media[start : start + MEDIA_RECORD.size] = record
    media[payload_start:] = blobs
    return bytes(media), indexes


def assemble(
    *,
    app_id: int = 0x0100000000000001,
    platform: int = 1,
    title: str | None = "Synthetic NX Fixture",
    publisher: str | None = "Fixture Publisher",
    program_ids: list[int] | None = None,
    directories: list[list[tuple[str, int, int]]],
    file_bytes: list[bytes],
    snapshots: list[dict],
    chunk_log2: int = 16,
    compress: bool = False,
    localized: list[dict] | None = None,
    media_payloads: list[tuple[int, bytes]] | None = None,
    metadata_history: list[dict] | None = None,
    orphan_blocks: list[bytes] | None = None,
    block_codecs: list[int] | None = None,
    extra_root: int | None = None,
) -> bytes:
    if program_ids is None:
        program_ids = [0x0100000000000001]
    localized = localized or []
    media_payloads = media_payloads or []
    metadata_history = metadata_history or []
    files: list[dict] = []
    blocks: list[dict] = []
    block_by_bytes: dict[bytes, int] = {}
    for contents in file_bytes:
        block_ids: list[int] = []
        chunk_size = 1 << chunk_log2
        for at in range(0, len(contents), chunk_size):
            chunk = contents[at : at + chunk_size]
            block_id = block_by_bytes.get(chunk)
            if block_id is None:
                codec = block_codecs[len(blocks)] if block_codecs is not None else (1 if compress else 0)
                if codec not in (0, 1):
                    raise ValueError("fixture block codec must be RAW (0) or Zstandard (1)")
                stored = (ZSTD_A_4096_FRAME if chunk == b"A" * 4096 else zstd.compress(chunk)) if codec else chunk
                block_id = len(blocks)
                block_by_bytes[chunk] = block_id
                blocks.append({"logical": chunk, "stored": stored, "codec": codec})
            block_ids.append(block_id)
        files.append({"bytes": contents, "block_ids": block_ids, "chunk_log2": chunk_log2})

    for orphan in orphan_blocks or []:
        if not orphan:
            raise ValueError("orphan physical blocks must be nonempty")
        blocks.append({"logical": orphan, "stored": orphan, "codec": 0})
    if block_codecs is not None and len(block_codecs) != len(blocks) - len(orphan_blocks or []):
        raise ValueError("fixture block codec count must match unique file blocks")

    roots = [item["root"] for item in snapshots]
    manifest, logical_sizes, identities = make_manifest(
        directories,
        files,
        roots,
        platform,
        app_id,
        [0xFFFFFFFFFFFFFFFF if item.get("program", NO_INDEX) == NO_INDEX else program_ids[item["program"]] for item in snapshots],
        [(item.get("type", 0), item.get("version", 0)) for item in snapshots],
        snapshot_content_ids=[item.get("content_id", bytes(16)) for item in snapshots],
    )
    programs = [{"id": pid, "application": 0} for pid in program_ids]
    media, media_indexes = make_media(media_payloads)
    localized = [
        {**item, "icon": media_indexes[item["icon"]] if item.get("icon", NO_INDEX) != NO_INDEX else NO_INDEX}
        for item in localized
    ]
    mapped_history: list[dict] = []
    for item in metadata_history:
        mapped = dict(item)
        for field in ("icon", "nacp"):
            if mapped.get(field, NO_INDEX) != NO_INDEX:
                mapped[field] = media_indexes[mapped[field]]
        mapped["opaque"] = [media_indexes[index] for index in mapped.get("opaque", [])]
        mapped["localized"] = [
            {**local, "icon": media_indexes[local["icon"]] if local.get("icon", NO_INDEX) != NO_INDEX else NO_INDEX}
            for local in mapped.get("localized", [])
        ]
        mapped_history.append(mapped)
    catalog = make_catalog(app_id, platform, title, publisher, programs, snapshots, logical_sizes, identities,
                           localized, 0 if media_payloads else NO_INDEX, mapped_history, extra_root)

    archive = bytearray(DATA_START)
    data_offset = 0
    block_records: list[tuple] = []
    checks: list[int] = []
    if blocks:
        data_offset = DATA_START
        for block in blocks:
            block_at = align(len(archive), 16)
            archive.extend(bytes(block_at - len(archive)))
            block_records.append((block_at, len(block["stored"]), len(block["logical"]), block["codec"], 0, 0,
                                  len(checks)))
            checks.extend(slice_checks(block["logical"]))
            archive.extend(block["stored"])
        data_size = len(archive) - data_offset
    else:
        data_size = 0

    media_offset = 0
    if media:
        media_offset = align(len(archive), 16)
        archive.extend(bytes(media_offset - len(archive)))
        archive.extend(media)
    catalog_offset = align(len(archive), 16)
    archive.extend(bytes(catalog_offset - len(archive)))
    archive.extend(catalog)
    manifest_offset = align(len(archive), 16)
    archive.extend(bytes(manifest_offset - len(archive)))
    archive.extend(manifest)
    index_bytes = b""
    if blocks:
        block_index_offset = align(len(archive), 16)
        archive.extend(bytes(block_index_offset - len(archive)))
        for record in block_records:
            archive.extend(BLOCK.pack(*record))
        integrity_offset = align(len(archive), 16)
        archive.extend(bytes(integrity_offset - len(archive)))
        integrity = b"".join(struct.pack("<I", c) for c in checks)
        archive.extend(integrity)
        index_bytes = b"".join(BLOCK.pack(*r) for r in block_records) + integrity
    else:
        block_index_offset = integrity_offset = 0

    integrity_size = len(checks) * 4
    header_fields = (
        len(archive),
        catalog_offset,
        len(catalog),
        media_offset,
        len(media),
        data_offset,
        data_size,
        manifest_offset,
        len(manifest),
        block_index_offset,
        len(blocks),
        integrity_offset,
        integrity_size,
    )
    finish_header(archive, header_fields, catalog, manifest, index_bytes)
    return bytes(archive)


def append_generation(base: bytes, generation: int = 1, gap: int = 48) -> bytearray:
    """Simulate an in-place edit: copy the tail after `gap` free bytes and
    commit `generation` into its slot. Data blocks are not moved."""
    archive = bytearray(base)
    slot_a = HEADER.unpack_from(archive, 0)
    slot_b = HEADER.unpack_from(archive, SLOT_B)
    values = list(slot_b if slot_b[22] > slot_a[22] else slot_a)
    fields = values[6:19]
    tail_start = fields[3] if fields[4] else fields[1]
    tail = bytes(archive[tail_start:fields[0]])
    new_start = align(len(archive) + gap, 16)
    archive.extend(b"\xa5" * (new_start - len(archive)))  # free space: arbitrary bytes
    archive.extend(tail)
    delta = new_start - tail_start
    for i in (1, 3, 7, 9, 11):
        if fields[i]:
            fields[i] += delta
    fields[0] += delta
    header = bytearray(256)
    HEADER.pack_into(header, 0, *values[:6], *fields, *values[19:22], generation, CHECK_SIZE, bytes(24), 0)
    struct.pack_into("<I", header, 252, crc32c(header[:252]))
    slot = SLOT_B if generation % 2 else 0
    archive[slot:slot + 256] = header
    return archive


def one_snapshot(*, version: int = 0, program: int = 0, root: int = 0, kind: int = 0) -> list[dict]:
    return [{"program": program, "type": kind, "version": version, "root": root}]


def same_id_cross_platform_archive() -> bytes:
    app_id = 0x0100000000000001
    program_id = 0x0100000000000001
    manifest, logical_sizes, identities = make_manifest(
        directories=[[]], files=[], snapshot_roots=[0, 0], platform=0, app_id=app_id,
        program_ids=[program_id, program_id], snapshot_versions=[(0, 0), (0, 0)],
        snapshot_platforms=[0, 2], snapshot_app_ids=[app_id, app_id],
    )
    catalog = bytearray(656)
    for i, platform in enumerate((0, 2)):
        APPLICATION.pack_into(catalog, 128 + i * 80, app_id, platform, 1, 0,
                              i, 1, i, 1, NO_INDEX, 0, NO_INDEX, 0,
                              NO_INDEX, 0, 0, NO_INDEX, 0, 0, NO_INDEX, bytes(8))
        PROGRAM.pack_into(catalog, 288 + i * 64, program_id, i, 0, 0, 0,
                          bytes(16), i, 1, NO_INDEX, 0, bytes(16))
        SNAPSHOT.pack_into(catalog, 416 + i * 120, i, 0, 0, 0, 0, 0,
                           logical_sizes[i], NO_INDEX, 0, bytes(16), 0, 0,
                           identities[i], bytes(32))
    CATALOG_HEADER.pack_into(catalog, 0, b"NXCT", 1, 128, len(catalog), 0,
                             2, 2, 2, 0, 0, 0, 0,
                             128, 288, 416, 0, 0, 0, 656, 0, bytes(16))
    archive = bytearray(DATA_START)
    catalog_offset = len(archive)
    archive.extend(catalog)
    manifest_offset = align(len(archive), 16)
    archive.extend(bytes(manifest_offset - len(archive)))
    archive.extend(manifest)
    fields = (len(archive), catalog_offset, len(catalog), 0, 0, 0, 0, manifest_offset,
              len(manifest), 0, 0, 0, 0)
    finish_header(archive, fields, bytes(catalog), manifest, b"")
    return bytes(archive)


def write_fixture(name: str, data: bytes, outcome: str, reason: str, characteristics: list[str], expected_error: str | None = None, mirror: bool = True) -> dict:
    # Header mutations are made on slot A; mirror them so slot B never
    # masks the case being tested.
    if mirror and len(data) >= DATA_START and data[:4] == b"NXAR":
        data = data[:SLOT_B] + data[:256] + data[SLOT_B + 256:]
    path = FIXTURES / name
    path.write_bytes(data)
    return {
        "file": name,
        "expected": outcome,
        "reason": reason,
        "size": len(data),
        "sha256": sha(data).hex(),
        "characteristics": characteristics,
        "expected_error": expected_error,
    }


def main() -> None:
    FIXTURES.mkdir(parents=True, exist_ok=True)
    entries: list[dict] = []

    minimal = assemble(
        directories=[[]],
        file_bytes=[],
        snapshots=one_snapshot(),
    )
    entries.append(write_fixture("minimal_valid.nx", minimal, "accept", "empty complete base snapshot", ["1 app", "1 program", "1 snapshot", "1 empty root", "0 files", "0 blocks"]))

    raw = assemble(
        directories=[[('data.bin', 2, 0)]],
        file_bytes=[b"NX!?"],
        snapshots=one_snapshot(),
    )
    entries.append(write_fixture("raw_only.nx", raw, "accept", "one RAW data block", ["file size=4", "chunk log2=16", "one RAW block", "no ZSTD blocks"]))

    nested = assemble(
        directories=[[('folder', 1, 1), ('root.txt', 2, 0)], [('child.bin', 2, 1)]],
        file_bytes=[b"root", b"nested child"],
        snapshots=one_snapshot(),
    )
    entries.append(write_fixture("nested_vfs.nx", nested, "accept", "root and child directory entries with independently addressable files", ["root has one directory and one file", "folder contains child.bin", "two files and two RAW blocks"]))

    zstd_file = b"A" * 4096
    compressed = assemble(
        directories=[[('compressed.bin', 2, 0)]],
        file_bytes=[zstd_file],
        snapshots=[{"program": 0, "type": 0, "version": 0, "root": 0, "build_id": bytes.fromhex("00112233445566778899aabbccddeeff102132435465768798a9bacbdcedfe0f")}],
        compress=True,
    )
    entries.append(write_fixture("zstd_single.nx", compressed, "accept", "one independent Zstandard frame and full snapshot Build ID", ["file size=4096", "one ZSTD block", "32-byte snapshot Build ID"]))

    multiblock = bytes((i * 31 + 7) & 0xFF for i in range(65537))
    multi = assemble(
        directories=[[('large.bin', 2, 0)]],
        file_bytes=[multiblock],
        snapshots=one_snapshot(),
    )
    entries.append(write_fixture("multi_block_file.nx", multi, "accept", "one file crosses a fixed chunk boundary", ["file size=65537", "block logical sizes=65536,1", "two RAW blocks"]))

    mixed_bytes = bytes((i * 31 + 7) & 0xFF for i in range(256 * 1024)) + b"Z" * (256 * 1024)
    mixed = assemble(
        directories=[[('mixed.bin', 2, 0)]],
        file_bytes=[mixed_bytes],
        snapshots=one_snapshot(),
        chunk_log2=18,
        block_codecs=[0, 1],
    )
    entries.append(write_fixture("mixed_raw_zstd.nx", mixed, "accept", "a 256 KiB RAW block followed by a 256 KiB independent Zstandard block", ["chunk log2=18", "block 0 RAW and 256 KiB", "block 1 Zstandard and 256 KiB", "single-block slices and a cross-codec read"]))

    shared = assemble(
        directories=[[('a.bin', 2, 0), ('b.bin', 2, 1)]],
        file_bytes=[b"same", b"same"],
        snapshots=one_snapshot(),
    )
    entries.append(write_fixture("shared_blocks.nx", shared, "accept", "identical files share one block", ["2 file records", "1 unique block", "both references target block 0"]))

    shared_with_orphan = assemble(
        directories=[[('a.bin', 2, 0), ('b.bin', 2, 1)]],
        file_bytes=[b"shared", b"shared"],
        snapshots=one_snapshot(),
        orphan_blocks=[b"unreferenced physical block"],
    )
    entries.append(write_fixture("shared_with_orphan_block.nx", shared_with_orphan, "accept", "two files share block 0 and block 1 is physically present but unreferenced", ["2 file records reference block 0", "block 1 has no Manifest reference", "full verification covers both physical blocks"]))
    corrupted_orphan = bytearray(shared_with_orphan)
    orphan_index = struct.unpack_from("<Q", corrupted_orphan, 88)[0] + 24
    orphan_payload = struct.unpack_from("<Q", corrupted_orphan, orphan_index)[0]
    corrupted_orphan[orphan_payload] ^= 1
    entries.append(write_fixture("corrupted_orphan_block.nx", bytes(corrupted_orphan), "reject_full_verify", "unreferenced physical block differs from its integrity digest", ["shared referenced block remains valid", "unreferenced block 1 payload byte changed", "index and integrity table remain valid"], "block_hash"))

    shared_snaps = assemble(
        directories=[[('shared.bin', 2, 0)]],
        file_bytes=[b"shared across snapshots"],
        snapshots=[{"program": 0, "type": 0, "version": 0, "root": 0}, {"program": 0, "type": 1, "version": 1, "root": 0}],
    )
    entries.append(write_fixture("shared_snapshots.nx", shared_snaps, "accept", "two versioned complete roots share a tree, file and blocks", ["2 snapshots with different versions", "same root directory", "distinct version-scoped identities", "1 unique block"]))

    deleted = assemble(
        directories=[[('keep.bin', 2, 0), ('remove.bin', 2, 1)], [('keep.bin', 2, 0)]],
        file_bytes=[b"keep", b"gone"],
        snapshots=[{"program": 0, "type": 0, "version": 0, "root": 0}, {"program": 0, "type": 1, "version": 1, "root": 1}],
    )
    entries.append(write_fixture("deleted_between_versions.nx", deleted, "accept", "later full tree omits a deleted file", ["2 snapshots", "version 0 contains keep.bin and remove.bin", "version 1 contains keep.bin only"]))

    multi_program = assemble(
        program_ids=[0x0100000000000001, 0x0100000000000002],
        directories=[[('one.bin', 2, 0)], [('two.bin', 2, 1)]],
        file_bytes=[b"program one", b"program two"],
        snapshots=[{"program": 0, "type": 0, "version": 0, "root": 0}, {"program": 1, "type": 0, "version": 0, "root": 1}],
    )
    entries.append(write_fixture("multiple_programs.nx", multi_program, "accept", "one app has two independent programs", ["1 app", "2 sorted program IDs", "2 snapshots", "2 roots"]))

    metadata_tie = assemble(
        program_ids=[0x0100000000000001, 0x0100000000000002],
        directories=[[]],
        file_bytes=[],
        snapshots=[{"program": 0, "type": 0, "version": 5, "root": 0}, {"program": 1, "type": 0, "version": 5, "root": 0}],
        title="Current source title",
        publisher="Current source publisher",
        localized=[{"language": "fr", "title": "Titre courant", "icon": 0}],
        media_payloads=[(1, png_pixel((15, 25, 35, 255))), (1, png_pixel((45, 55, 65, 255))), (4, b"synthetic native NACP for snapshot zero")],
        metadata_history=[{
            "snapshot": 0,
            "title": "Original source title",
            "publisher": "Original source publisher",
            "icon": 1,
            "nacp": 2,
            "localized": [
                {"language": "ja", "title": "元の作品", "icon": 1},
                {"language": "zh-hans", "title": "原始标题", "icon": 1},
            ],
        }],
    )
    entries.append(write_fixture("metadata_snapshot_tie.nx", metadata_tie, "accept", "equal-version metadata source uses greatest Catalog snapshot index while the other snapshot retains its own native metadata", ["two executable programs", "both version=5", "metadata_snapshot_index=1", "required per-snapshot metadata history extension", "historical languages=ja,zh-hans", "historical original PNG shared by default and localized icon refs", "historical NACP media reference"]))

    icons = assemble(
        directories=[[]],
        file_bytes=[],
        snapshots=one_snapshot(),
        title="Example",
        localized=[{"language": "ja", "title": "例", "icon": 1}],
        media_payloads=[(1, png_pixel((10, 20, 30, 255))), (1, png_pixel((40, 50, 60, 255)))],
    )
    entries.append(write_fixture("localized_metadata_icons.nx", icons, "accept", "localized title and separately addressable icon", ["default title=Example", "localized language=ja", "two independent synthetic PNG media payloads"]))

    switch2 = assemble(
        platform=2,
        title="Synthetic Switch 2 Title",
        localized=[{"language": "fr", "title": "Titre synthétique", "icon": 0}, {"language": "ja", "title": "架空の作品", "icon": 0}],
        media_payloads=[(1, png_pixel((12, 34, 56, 255)))],
        directories=[[]],
        file_bytes=[],
        snapshots=one_snapshot(),
    )
    entries.append(write_fixture("switch2_localized_icons.nx", switch2, "accept", "synthetic Switch 2 catalog with multilingual labels and one shared original PNG icon", ["platform=2", "localized languages=fr,ja", "one icon media record shared by both languages"]))

    jpeg_icon = assemble(
        title="Synthetic JPEG icon",
        directories=[[]],
        file_bytes=[],
        snapshots=one_snapshot(),
        media_payloads=[(2, JPEG_PIXEL)],
    )
    entries.append(write_fixture("jpeg_icon.nx", jpeg_icon, "accept", "on-demand JPEG icon retains its exact 285-byte synthetic JPEG payload", ["media type=2 JPEG", "default application icon", "exact original SHA-256 is asserted by the C++ reader test"]))

    cross_platform = same_id_cross_platform_archive()
    entries.append(write_fixture("same_application_id_cross_platform.nx", cross_platform, "accept", "application uniqueness is scoped by platform plus Application ID", ["same numeric Application ID on platforms 0 and 2", "two independent programs and snapshots", "different platform-scoped snapshot identities"]))

    dlc = assemble(
        directories=[[], []],
        file_bytes=[],
        snapshots=[
            {"program": 0, "type": 0, "version": 0, "root": 0},
            {"program": NO_INDEX, "type": 2, "version": 1, "root": 1},
        ],
    )
    entries.append(write_fixture("dlc_without_program.nx", dlc, "accept", "DLC complete tree has no executable Program ID", ["1 base executable program", "DLC program_index=0xffffffff", "distinct snapshot identity scope"]))

    dlc_with_program = assemble(
        directories=[[]], file_bytes=[],
        snapshots=[
            {"program": 0, "type": 0, "version": 0, "root": 0},
            {"program": 0, "type": 2, "version": 1, "root": 0},
        ],
    )
    entries.append(write_fixture("dlc_with_program.nx", dlc_with_program, "accept", "DLC may name a real source-provided executable Program ID", ["base and DLC share program 0", "DLC Program ID is not fabricated"]))

    distinct_dlc = assemble(
        directories=[[]], file_bytes=[],
        snapshots=[
            {"program": 0, "type": 0, "version": 0, "root": 0},
            {"program": NO_INDEX, "type": 2, "version": 1, "root": 0, "content_id": bytes([1]) + bytes(15)},
            {"program": NO_INDEX, "type": 2, "version": 1, "root": 0, "content_id": bytes([2]) + bytes(15)},
        ],
    )
    entries.append(write_fixture("distinct_dlc_same_tree.nx", distinct_dlc, "accept", "two DLCs with equal trees/versions but different source content IDs have distinct identities", ["two DLC content IDs", "same empty root", "same numeric version", "distinct snapshot identities"]))

    dlc_only = assemble(
        program_ids=[],
        title=None,
        publisher=None,
        directories=[[]],
        file_bytes=[],
        snapshots=[{"program": NO_INDEX, "type": 2, "version": 7, "root": 0}],
    )
    entries.append(write_fixture("dlc_only_application.nx", dlc_only, "accept", "DLC-only application has no executable Program rows", ["application program_count=0", "first_program_index=0xffffffff", "one unassociated DLC snapshot"]))

    empty_file = assemble(
        directories=[[('empty.bin', 2, 0)]],
        file_bytes=[b""],
        snapshots=one_snapshot(),
    )
    entries.append(write_fixture("empty_files.nx", empty_file, "accept", "zero-byte file has no block references", ["file size=0", "block count=0", "SHA-256 of empty bytes"]))

    corrupted_manifest = bytearray(raw)
    manifest_offset = struct.unpack_from("<Q", corrupted_manifest, 72)[0]
    corrupted_manifest[manifest_offset] ^= 1
    entries.append(write_fixture("corrupted_manifest.nx", bytes(corrupted_manifest), "reject", "Manifest digest mismatch", ["Manifest byte changed", "stored manifest SHA-256 unchanged"], "manifest_hash"))

    corrupted_hash = bytearray(raw)
    integrity_offset = struct.unpack_from("<Q", corrupted_hash, 104)[0]
    corrupted_hash[integrity_offset] ^= 1
    entries.append(write_fixture("corrupted_integrity_table.nx", bytes(corrupted_hash), "reject", "Integrity-table checksum mismatch", ["integrity row 0 changed", "header index_sha256 unchanged"], "index_hash"))

    corrupted_index = bytearray(raw)
    hot_index_offset = struct.unpack_from("<Q", corrupted_index, 88)[0]
    corrupted_index[hot_index_offset] ^= 1
    entries.append(write_fixture("corrupted_hot_index.nx", bytes(corrupted_index), "reject", "Hot-index checksum mismatch", ["one block descriptor byte changed", "header index_sha256 unchanged"], "index_hash"))

    corrupted_index_digest = bytearray(raw)
    corrupted_index_digest[184] ^= 1
    struct.pack_into("<I", corrupted_index_digest, 252, crc32c(corrupted_index_digest[:252]))
    entries.append(write_fixture("corrupted_index_sha256.nx", bytes(corrupted_index_digest), "reject", "Header index_sha256 does not match index and integrity bytes", ["index_sha256 byte changed", "header CRC-32C recalculated"], "index_hash"))

    corrupted_payload = bytearray(raw)
    payload_offset = struct.unpack_from("<Q", corrupted_payload, 56)[0]
    corrupted_payload[payload_offset] ^= 1
    entries.append(write_fixture("corrupted_block_payload.nx", bytes(corrupted_payload), "reject_full_verify", "RAW block differs from the original integrity digest", ["index and integrity table remain valid", "one data payload byte changed"], "block_hash"))

    def refresh_catalog(blob: bytearray) -> None:
        offset = struct.unpack_from("<Q", blob, 24)[0]
        size = struct.unpack_from("<Q", blob, 32)[0]
        blob[120:152] = sha(bytes(blob[offset : offset + size]))
        struct.pack_into("<I", blob, 252, crc32c(blob[:252]))

    bad_metadata_snapshot = bytearray(minimal)
    catalog_offset = struct.unpack_from("<Q", bad_metadata_snapshot, 24)[0]
    application_table = struct.unpack_from("<Q", bad_metadata_snapshot, catalog_offset + 48)[0]
    struct.pack_into("<I", bad_metadata_snapshot, catalog_offset + application_table + 68, 77)
    refresh_catalog(bad_metadata_snapshot)
    entries.append(write_fixture("invalid_metadata_snapshot.nx", bytes(bad_metadata_snapshot), "reject", "Application metadata source points outside its snapshot range", ["Catalog digest and header CRC recalculated", "metadata_snapshot_index out of range"], "catalog"))

    source_without_metadata = bytearray(assemble(
        directories=[[]], file_bytes=[], snapshots=one_snapshot(), title=None, publisher=None,
    ))
    source_catalog = struct.unpack_from("<Q", source_without_metadata, 24)[0]
    source_app_table = struct.unpack_from("<Q", source_without_metadata, source_catalog + 48)[0]
    # Forge an in-range metadata source while leaving all native metadata absent.
    struct.pack_into("<I", source_without_metadata, source_catalog + source_app_table + 68, 0)
    refresh_catalog(source_without_metadata)
    entries.append(write_fixture("invalid_empty_metadata_source.nx", bytes(source_without_metadata), "reject",
                                 "Metadata source pointer exists without any native metadata",
                                 ["no native metadata", "forged metadata source index 0", "Catalog hash refreshed"], "catalog"))

    invalid_history_type = bytearray(metadata_tie)
    history_catalog = struct.unpack_from("<Q", invalid_history_type, 24)[0]
    history_extension_table = struct.unpack_from("<Q", invalid_history_type, history_catalog + 88)[0]
    history_payload = struct.unpack_from("<Q", invalid_history_type, history_catalog + history_extension_table + 16)[0]
    # History row 0: icon_media_index at byte 20. Media index 2 is NACP,
    # not an icon. Refresh hashes so structural validation is actually reached.
    struct.pack_into("<I", invalid_history_type, history_catalog + history_payload + 24 + 20, 2)
    refresh_catalog(invalid_history_type)
    entries.append(write_fixture("invalid_history_media_type.nx", bytes(invalid_history_type), "reject",
                                 "Historical icon reference targets an NACP Media record",
                                 ["Snapshot Metadata History", "icon/NACP type conflict", "Catalog hash refreshed"], "catalog"))

    bad_empty_title = bytearray(minimal)
    catalog_offset = struct.unpack_from("<Q", bad_empty_title, 24)[0]
    application_table = struct.unpack_from("<Q", bad_empty_title, catalog_offset + 48)[0]
    struct.pack_into("<II", bad_empty_title, catalog_offset + application_table + 28, 0, 0)
    struct.pack_into("<I", bad_empty_title, catalog_offset + application_table + 68, 0)
    refresh_catalog(bad_empty_title)
    entries.append(write_fixture("invalid_present_empty_title.nx", bytes(bad_empty_title), "reject", "Present empty title is forbidden", ["offset 0, length 0", "Catalog hash refreshed"], "string"))

    bad_language = bytearray(icons)
    catalog_offset = struct.unpack_from("<Q", bad_language, 24)[0]
    localized_table = struct.unpack_from("<Q", bad_language, catalog_offset + 72)[0]
    string_pool = struct.unpack_from("<Q", bad_language, catalog_offset + 96)[0]
    language_offset, language_length = struct.unpack_from("<II", bad_language, catalog_offset + localized_table)
    assert language_length == 2
    begin = catalog_offset + string_pool + language_offset
    bad_language[begin : begin + 2] = b"j-"
    refresh_catalog(bad_language)
    entries.append(write_fixture("invalid_language_subtag.nx", bytes(bad_language), "reject", "Empty locale subtag is forbidden", ["language j-", "Catalog hash refreshed"], "catalog"))

    bad_shared_media = bytearray(icons)
    cat = struct.unpack_from("<Q", bad_shared_media, 24)[0]
    program_table = struct.unpack_from("<Q", bad_shared_media, cat + 56)[0]
    struct.pack_into("<I", bad_shared_media, cat + program_table + 40, 0)
    refresh_catalog(bad_shared_media)
    entries.append(write_fixture("invalid_shared_media_types.nx", bytes(bad_shared_media), "reject", "one Media record is referenced as both icon and NPDM", ["Catalog and header digests refreshed", "incompatible Media owners"], "catalog"))

    duplicate_identity = bytearray(distinct_dlc)
    cat = struct.unpack_from("<Q", duplicate_identity, 24)[0]
    snapshot_table = struct.unpack_from("<Q", duplicate_identity, cat + 64)[0]
    first = cat + snapshot_table + SNAPSHOT.size + 56
    second = first + SNAPSHOT.size
    duplicate_identity[second:second + 32] = duplicate_identity[first:first + 32]
    refresh_catalog(duplicate_identity)
    entries.append(write_fixture("duplicate_snapshot_identity.nx", bytes(duplicate_identity), "reject", "two distinct DLC rows claim the same snapshot identity", ["content IDs remain distinct", "Catalog and header digests refreshed"], "catalog"))

    bad_snapshot_program = bytearray(minimal)
    catalog_offset = struct.unpack_from("<Q", bad_snapshot_program, 24)[0]
    snapshot_table = struct.unpack_from("<Q", bad_snapshot_program, catalog_offset + 64)[0]
    struct.pack_into("<I", bad_snapshot_program, catalog_offset + snapshot_table, NO_INDEX)
    refresh_catalog(bad_snapshot_program)
    entries.append(write_fixture("invalid_snapshot_program.nx", bytes(bad_snapshot_program), "reject", "Non-DLC snapshot has no executable program association", ["Catalog digest and header CRC recalculated", "base snapshot program_index is absent"], "catalog"))

    bad_build_id = bytearray(minimal)
    catalog_offset = struct.unpack_from("<Q", bad_build_id, 24)[0]
    snapshot_table = struct.unpack_from("<Q", bad_build_id, catalog_offset + 64)[0]
    bad_build_id[catalog_offset + snapshot_table + 5] = 1
    refresh_catalog(bad_build_id)
    entries.append(write_fixture("invalid_build_id_flag.nx", bytes(bad_build_id), "reject", "Snapshot Build ID presence flag disagrees with its 32-byte value", ["Catalog digest and header CRC recalculated", "Build ID presence set but value all zero"], "catalog"))

    bad_program_application = bytearray(multi_program)
    catalog_offset = struct.unpack_from("<Q", bad_program_application, 24)[0]
    program_table = struct.unpack_from("<Q", bad_program_application, catalog_offset + 56)[0]
    struct.pack_into("<I", bad_program_application, catalog_offset + program_table + PROGRAM.size + 8, 1)
    refresh_catalog(bad_program_application)
    entries.append(write_fixture("invalid_program_application.nx", bytes(bad_program_application), "reject", "Program points outside application table", ["Catalog digest and header CRC recalculated", "second program application_index=1"], "catalog"))

    duplicate_app_key = bytearray(cross_platform)
    catalog_offset = struct.unpack_from("<Q", duplicate_app_key, 24)[0]
    application_table = struct.unpack_from("<Q", duplicate_app_key, catalog_offset + 48)[0]
    duplicate_app_key[catalog_offset + application_table + APPLICATION.size + 8] = 0
    refresh_catalog(duplicate_app_key)
    entries.append(write_fixture("duplicate_application_key.nx", bytes(duplicate_app_key), "reject", "Two application records have the same platform and Application ID", ["Catalog digest and header CRC recalculated", "second platform changed from 2 to 0"], "catalog"))

    wrong_metadata_owner = bytearray(cross_platform)
    catalog_offset = struct.unpack_from("<Q", wrong_metadata_owner, 24)[0]
    application_table = struct.unpack_from("<Q", wrong_metadata_owner, catalog_offset + 48)[0]
    struct.pack_into("<I", wrong_metadata_owner, catalog_offset + application_table + APPLICATION.size + 68, 0)
    refresh_catalog(wrong_metadata_owner)
    entries.append(write_fixture("invalid_metadata_snapshot_owner.nx", bytes(wrong_metadata_owner), "reject", "Application metadata pointer names another application's snapshot", ["Catalog digest and header CRC recalculated", "second application metadata_snapshot_index=0"], "catalog"))

    wrong_program_snapshot = bytearray(cross_platform)
    catalog_offset = struct.unpack_from("<Q", wrong_program_snapshot, 24)[0]
    program_table = struct.unpack_from("<Q", wrong_program_snapshot, catalog_offset + 56)[0]
    struct.pack_into("<I", wrong_program_snapshot, catalog_offset + program_table + PROGRAM.size + 32, 0)
    refresh_catalog(wrong_program_snapshot)
    entries.append(write_fixture("invalid_program_snapshot_range.nx", bytes(wrong_program_snapshot), "reject", "Program snapshot range points to another program", ["Catalog digest and header CRC recalculated", "second program first_snapshot_index=0"], "catalog"))

    invalid_offsets = bytearray(minimal)
    struct.pack_into("<Q", invalid_offsets, 24, 0xFFFFFFFFFFFFF000)
    struct.pack_into("<I", invalid_offsets, 252, crc32c(invalid_offsets[:252]))
    entries.append(write_fixture("invalid_offsets.nx", bytes(invalid_offsets), "reject", "Catalog out of bounds", ["catalog offset beyond archive", "header CRC-32C recalculated"], "bounds"))

    truncated = minimal[:-1]
    entries.append(write_fixture("truncated_archive.nx", truncated, "reject", "declared and actual archive lengths differ", ["one final byte removed", "header archive size unchanged"], "length"))

    invalid_path = bytearray(
        assemble(
            directories=[[('foobar', 2, 0)]],
            file_bytes=[b"path"],
            snapshots=one_snapshot(),
        )
    )
    manifest_offset = struct.unpack_from("<Q", invalid_path, 72)[0]
    entry_offset = struct.unpack_from("<Q", invalid_path, manifest_offset + 48)[0]
    path_offset = struct.unpack_from("<Q", invalid_path, manifest_offset + 64)[0]
    name_offset = struct.unpack_from("<Q", invalid_path, manifest_offset + entry_offset)[0]
    invalid_path[manifest_offset + name_offset : manifest_offset + name_offset + 6] = b"../bad"
    manifest_size = struct.unpack_from("<Q", invalid_path, 80)[0]
    invalid_path[152:184] = sha(bytes(invalid_path[manifest_offset : manifest_offset + manifest_size]))
    struct.pack_into("<I", invalid_path, 252, crc32c(invalid_path[:252]))
    entries.append(write_fixture("invalid_path.nx", bytes(invalid_path), "reject", "unsafe path component", ["component=../bad", "Manifest digest and header CRC recalculated"], "path"))

    # Hostile directory graphs (compare NUP issue #27): the builder refuses
    # to make them, so each one patches one Entry of a valid archive and
    # recomputes the Manifest digest and header CRC.
    def retarget(blob: bytes, entry_index: int, kind: int, target: int) -> bytes:
        out = bytearray(blob)
        manifest_at = struct.unpack_from("<Q", out, 72)[0]
        manifest_len = struct.unpack_from("<Q", out, 80)[0]
        entries_at = struct.unpack_from("<Q", out, manifest_at + 48)[0]
        record = manifest_at + entries_at + entry_index * ENTRY.size
        fields = list(ENTRY.unpack_from(out, record))
        fields[2], fields[3] = target, kind
        ENTRY.pack_into(out, record, *fields)
        out[152:184] = sha(bytes(out[manifest_at : manifest_at + manifest_len]))
        struct.pack_into("<I", out, 252, crc32c(out[:252]))
        return bytes(out)

    # folder/child.bin becomes a directory Entry pointing back at the root.
    entries.append(write_fixture("cyclic_directory.nx", retarget(nested, 2, 1, 0), "reject",
                                 "a directory Entry points back at the snapshot root",
                                 ["folder/child.bin retargeted to directory 0", "Manifest digest and header CRC recalculated"], "manifest"))
    two_dirs = assemble(
        directories=[[('a', 1, 1), ('b', 1, 2)], [('x.bin', 2, 0)], [('x.bin', 2, 1)]],
        file_bytes=[b"same", b"same"],
        snapshots=one_snapshot(),
    )
    entries.append(write_fixture("repeated_directory.nx", retarget(two_dirs, 1, 1, 1), "reject",
                                 "two Entries of one snapshot reach the same directory",
                                 ["a/ and b/ both target directory 1", "Manifest digest and header CRC recalculated"], "manifest"))
    two_files = assemble(
        directories=[[('a.bin', 2, 0), ('b.bin', 2, 1)]],
        file_bytes=[b"same", b"same"],
        snapshots=one_snapshot(),
    )
    # Same bytes, so the snapshot identity and logical size stay valid: only
    # the repeated File record is wrong.
    entries.append(write_fixture("repeated_file.nx", retarget(two_files, 1, 2, 0), "reject",
                                 "one File record appears at two paths of one snapshot",
                                 ["a.bin and b.bin both target file 0", "identity and logical size unchanged", "Manifest digest and header CRC recalculated"], "manifest"))

    unknown_feature = bytearray(minimal)
    struct.pack_into("<H", unknown_feature, 12, 3)
    struct.pack_into("<I", unknown_feature, 252, crc32c(unknown_feature[:252]))
    entries.append(write_fixture("unknown_mandatory_feature.nx", bytes(unknown_feature), "reject", "unknown required feature bit", ["unknown required feature bit 1 set", "header CRC-32C recalculated"], "feature"))

    appended = append_generation(icons)
    entries.append(write_fixture("appended_generation.nx", bytes(appended), "accept", "in-place edit: generation 1 tail after free space", ["slot B generation 1", "slot A generation 0 still valid", "stale tail and arbitrary free bytes before the new tail", "Media region moved with the tail"], mirror=False))

    second = append_generation(bytes(appended), generation=2)
    entries.append(write_fixture("appended_generation_2.nx", bytes(second), "accept", "two in-place edits alternate slots", ["slot A generation 2", "slot B generation 1"], mirror=False))

    interrupted = bytearray(appended)
    interrupted[SLOT_B + 40] ^= 1  # torn generation-1 slot: CRC fails
    interrupted.extend(b"\x5a" * 100)  # residue of the interrupted edit
    entries.append(write_fixture("interrupted_commit.nx", bytes(interrupted), "accept", "torn newer slot falls back to generation 0; trailing residue ignored", ["slot B CRC invalid", "100 bytes after archive_size"], mirror=False))

    one_slot = bytearray(raw)
    one_slot[0] ^= 1
    entries.append(write_fixture("damaged_slot_a.nx", bytes(one_slot), "accept", "slot A damaged, identical slot B used", ["slot A magic changed", "slot B intact"], mirror=False))

    for page in (0, 1):
        lost = bytearray(second)
        start = page * SLOT_B
        lost[start:start + SLOT_B] = bytes([0xA5]) * SLOT_B
        entries.append(write_fixture(f"lost_header_page_{page}.nx", bytes(lost), "accept",
                                     "the other generation survives complete header-page damage",
                                     [f"page {page} entirely overwritten"], mirror=False))
    for count in (1, 2):
        padding = bytearray(raw)
        padding[256] = 1
        if count == 2:
            padding[SLOT_B + 256] = 1
        entries.append(write_fixture(f"bad_header_page_padding_{count}.nx", bytes(padding),
                                     "accept" if count == 1 else "reject",
                                     "page padding is checked independently",
                                     [f"bad padding in {count} pages"],
                                     None if count == 1 else "unsupported", mirror=False))
    legacy = bytearray(minimal)
    struct.pack_into("<H", legacy, 12, 0)
    struct.pack_into("<I", legacy, 252, crc32c(legacy[:252]))
    legacy[2048:2304] = legacy[:256]
    legacy[SLOT_B:SLOT_B + 256] = bytes(256)
    entries.append(write_fixture("legacy_shared_header_page.nx", bytes(legacy), "reject",
                                 "earlier draft used the shared header page",
                                 ["layout flag absent", "second slot at 2048"], "unsupported", mirror=False))

    entries.append(write_fixture("truncated_header_area.nx", raw[:DATA_START - 1], "reject",
                                 "file ends inside the second header page",
                                 ["8191-byte file"], "length", mirror=False))
    unknown_optional = bytearray(raw)
    struct.pack_into("<H", unknown_optional, 14, 1)
    struct.pack_into("<I", unknown_optional, 252, crc32c(unknown_optional[:252]))
    entries.append(write_fixture("unknown_optional_header_feature.nx", bytes(unknown_optional), "reject",
                                 "header optional feature bits are reserved",
                                 ["optional header bit 0 set", "valid header CRCs"], "feature"))
    max_generation = bytearray(shared_snaps)
    struct.pack_into("<Q", max_generation, 216, (1 << 64) - 1)
    struct.pack_into("<I", max_generation, 252, crc32c(max_generation[:252]))
    entries.append(write_fixture("max_generation.nx", bytes(max_generation), "accept",
                                 "maximum generation remains readable",
                                 ["both header generations UINT64_MAX"]))
    conflict = bytearray(raw)
    struct.pack_into("<Q", conflict, SLOT_B + 16, struct.unpack_from("<Q", conflict, 16)[0] + 16)
    struct.pack_into("<I", conflict, SLOT_B + 252, crc32c(conflict[SLOT_B:SLOT_B + 252]))
    entries.append(write_fixture("slot_generation_conflict.nx", bytes(conflict), "reject", "both slots valid with generation 0 but different bytes", ["slot B archive_size changed", "both CRCs valid"], "format", mirror=False))

    both_bad = bytearray(raw)
    both_bad[0] ^= 1
    both_bad[SLOT_B] ^= 1
    entries.append(write_fixture("damaged_both_slots.nx", bytes(both_bad), "reject", "no valid header slot", ["slot A and slot B magic changed"], "unsupported", mirror=False))

    index = {"format": "NX 0.2 synthetic conformance fixtures", "generator": "tests/conformance/build_fixtures.py", "fixtures": entries}
    (FIXTURES / "fixture_index.json").write_text(json.dumps(index, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"generated {len(entries)} fixture archives in {FIXTURES}")


if __name__ == "__main__":
    main()
