"""Independently parse and validate the generated NX 0.2 fixture corpus."""

from __future__ import annotations

import hashlib
import json
import math
import struct
import sys
from pathlib import Path

from compression import zstd


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures"
NO_INDEX = 0xFFFFFFFF

HEADER = struct.Struct("<4sHHIHH13Q32s32s32sQI24sI")
DATA_START = 8192
HEADER_SLOTS = (0, 4096)
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
STRING_REF = struct.Struct("<II")
BLOCK_REFERENCE = struct.Struct("<I")


def assert_wire_layout(record: struct.Struct, widths: list[int], offsets: list[int], name: str) -> None:
    starts: list[int] = []
    position = 0
    for width in widths:
        starts.append(position)
        position += width
    if position != record.size or starts != offsets:
        raise AssertionError(f"{name} size/offset mismatch: size={record.size}, offsets={starts}")


assert_wire_layout(HEADER, [4, 2, 2, 4, 2, 2, *([8] * 13), 32, 32, 32, 8, 4, 24, 4], [0, 4, 6, 8, 12, 14, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 96, 104, 112, 120, 152, 184, 216, 224, 228, 252], "NxHeader")
assert_wire_layout(CATALOG_HEADER, [4, 2, 2, 8, *([4] * 8), *([8] * 8), 16], [0, 4, 6, 8, 16, 20, 24, 28, 32, 36, 40, 44, 48, 56, 64, 72, 80, 88, 96, 104, 112], "NxCatalogHeader")
assert_wire_layout(APPLICATION, [8, 1, 1, 2, *([4] * 15), 8], [0, 8, 9, 10, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60, 64, 68, 72], "NxApplication")
assert_wire_layout(PROGRAM, [8, 4, 1, 1, 2, 16, *([4] * 4), 16], [0, 8, 12, 13, 14, 16, 32, 36, 40, 44, 48], "NxProgram")
assert_wire_layout(SNAPSHOT, [4, 1, 1, 2, 4, 4, 8, 4, 4, 16, 4, 4, 32, 32], [0, 4, 5, 6, 8, 12, 16, 24, 28, 32, 48, 52, 56, 88], "NxSnapshot")
assert_wire_layout(LOCALIZED, [4] * 6, [0, 4, 8, 12, 16, 20], "NxLocalizedEntry")
assert_wire_layout(DEPENDENCY, [1, 1, 2, 16, 32, 4], [0, 1, 2, 4, 20, 52], "NxDependency")
assert_wire_layout(EXTENSION, [4, 4, 2, 2, 4, 8, 8], [0, 4, 8, 10, 12, 16, 24], "NxExtension")
assert_wire_layout(MEDIA_HEADER, [4, 2, 2, 8, 4, 2, 2, 4, 4], [0, 4, 6, 8, 16, 20, 22, 24, 28], "NxMediaRegionHeader")
assert_wire_layout(MEDIA_RECORD, [2, 1, 1, 8, 8, 8, 32, 4], [0, 2, 3, 4, 12, 20, 28, 60], "NxMediaRecord")
assert_wire_layout(MANIFEST_HEADER, [4, 2, 2, 8, *([4] * 4), *([8] * 6), 16], [0, 4, 6, 8, 16, 20, 24, 28, 32, 40, 48, 56, 64, 72, 80], "NxManifestHeader")
assert_wire_layout(DIRECTORY, [4] * 4, [0, 4, 8, 12], "NxDirectory")
assert_wire_layout(ENTRY, [8, 4, 4, 1, 1, 2, 4], [0, 8, 12, 16, 17, 18, 20], "NxDirectoryEntry")
assert_wire_layout(FILE, [8, 4, 4, 1, 1, 2, 32, 12], [0, 8, 12, 16, 17, 18, 20, 52], "NxFile")
assert_wire_layout(BLOCK, [8, 4, 4, 1, 1, 2, 4], [0, 8, 12, 16, 17, 18, 20], "NxBlock")
assert_wire_layout(STRING_REF, [4, 4], [0, 4], "NxStringRef")
assert_wire_layout(BLOCK_REFERENCE, [4], [0], "NxBlockReference")


class InvalidArchive(Exception):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


def reject(code: str, message: str) -> None:
    raise InvalidArchive(code, message)


def check_snapshot_history_misassociation(data: bytes) -> None:
    mutated = bytearray(data)
    catalog_offset = struct.unpack_from("<Q", mutated, 24)[0]
    catalog_size = struct.unpack_from("<Q", mutated, 32)[0]
    catalog = bytearray(mutated[catalog_offset : catalog_offset + catalog_size])
    extension_count = struct.unpack_from("<I", catalog, 40)[0]
    extension_table = struct.unpack_from("<Q", catalog, 88)[0]
    application_table = struct.unpack_from("<Q", catalog, 48)[0]
    extension_payload = None
    for index in range(extension_count):
        at = extension_table + index * EXTENSION.size
        type_id, _, owner_kind, _, owner_index, payload_offset, _ = EXTENSION.unpack_from(catalog, at)
        if type_id == 0x00010004 and owner_kind == 1 and owner_index == 0:
            extension_payload = payload_offset
            break
    if extension_payload is None:
        raise RuntimeError("metadata-history fixture lacks the required extension")
    selected_snapshot = struct.unpack_from("<I", catalog, application_table + 68)[0]
    struct.pack_into("<I", catalog, extension_payload + 24, selected_snapshot)
    mutated[catalog_offset : catalog_offset + catalog_size] = catalog
    mutated[120:152] = sha(catalog)
    struct.pack_into("<I", mutated, 252, crc32c(mutated[:252]))
    mutated[HEADER_SLOTS[1] : HEADER_SLOTS[1] + 256] = mutated[:256]
    try:
        validate_archive(bytes(mutated))
    except InvalidArchive as exc:
        if exc.code != "catalog":
            raise RuntimeError(f"misassociated metadata history rejected as {exc.code}, expected catalog") from exc
        return
    raise RuntimeError("metadata history naming the selected snapshot was unexpectedly accepted")


def crc32c(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


def sha(data: bytes) -> bytes:
    return hashlib.sha256(data).digest()


def checked_range(offset: int, size: int, limit: int, code: str = "bounds") -> tuple[int, int]:
    if offset < 0 or size < 0 or offset > limit or size > limit - offset:
        reject(code, f"range [{offset}, {offset}+{size}) exceeds {limit}")
    return offset, offset + size


def table_range(offset: int, count: int, item_size: int, total: int, *, alignment: int, optional: bool = False) -> tuple[int, int] | None:
    if count == 0:
        if offset != 0 and optional:
            reject("structure", "empty optional table has nonzero offset")
        return None
    if offset == 0 or offset % alignment:
        reject("structure", "table offset is zero or misaligned")
    size = count * item_size
    if size > total:
        reject("bounds", "table byte count exceeds region")
    return checked_range(offset, size, total)


def decode_string(pool: bytes, offset: int, length: int, *, required: bool = False, max_length: int = 4096) -> bytes | None:
    if offset == NO_INDEX:
        if length != 0 or required:
            reject("string", "invalid absent string reference")
        return None
    if length == 0:
        reject("string", "present string must not be empty")
    if length > max_length or offset > len(pool) or length > len(pool) - offset:
        reject("bounds", "string reference out of bounds or over limit")
    raw = pool[offset : offset + length]
    if b"\x00" in raw:
        reject("string", "NUL in string")
    try:
        raw.decode("utf-8", "strict")
    except UnicodeDecodeError:
        reject("string", "invalid UTF-8")
    return raw


def valid_language_tag(raw: bytes) -> bool:
    return 0 < len(raw) <= 63 and all(
        1 <= len(part) <= 8 and all(c in b"abcdefghijklmnopqrstuvwxyz0123456789" for c in part)
        for part in raw.split(b"-")
    )


def parse_media(data: bytes, offset: int, size: int, *, verify_payloads: bool) -> list[dict]:
    if size == 0:
        if offset != 0:
            reject("structure", "media references without a Media region")
        return []
    checked_range(offset, size, len(data))
    if offset % 16 or size < MEDIA_HEADER.size or size > 4 * 1024**3:
        reject("structure", "invalid Media region alignment or size")
    magic, version, header_size, total_size, count, record_size, reserved, record_offset, payload_offset = MEDIA_HEADER.unpack_from(data, offset)
    if (magic, version, header_size, total_size, record_size, reserved, record_offset) != (b"NXMD", 1, 32, size, 64, 0, 32):
        reject("structure", "invalid Media header")
    if count == 0 or count > 65535 or payload_offset != ((32 + count * 64 + 15) & ~15):
        reject("structure", "invalid Media record count or payload start")
    records_end = 32 + count * 64
    checked_range(offset + 32, count * MEDIA_RECORD.size, offset + size)
    if any(data[offset + records_end : offset + payload_offset]):
        reject("padding", "nonzero Media table padding")
    records: list[dict] = []
    ranges: list[tuple[int, int]] = []
    payload_cursor = payload_offset
    for index in range(count):
        media_type, codec, flags, stored, logical, data_offset, digest, reserved_record = MEDIA_RECORD.unpack_from(data, offset + record_offset + index * MEDIA_RECORD.size)
        if media_type not in (1, 2, 3, 4, 5) or codec != 0 or flags != 0 or reserved_record != 0:
            reject("structure", "invalid Media record values")
        if stored == 0 or stored > 64 * 1024 * 1024 or logical != stored or data_offset % 16:
            reject("resource", "Media size or alignment outside limit")
        payload_cursor = (payload_cursor + 15) & ~15
        if data_offset != payload_cursor:
            reject("structure", "Media payload is not at its minimum aligned offset")
        lo, hi = checked_range(data_offset, stored, size)
        if lo < payload_offset:
            reject("bounds", "Media payload overlaps record table")
        if verify_payloads:
            payload = bytes(data[offset + lo : offset + hi])
            if sha(payload) != digest:
                reject("media_hash", "Media payload hash mismatch")
        ranges.append((lo, hi))
        records.append({"type": media_type, "offset": data_offset, "size": stored, "hash": digest})
        payload_cursor = hi
    ranges.sort()
    prior = payload_offset
    for lo, hi in ranges:
        if lo < prior:
            reject("overlap", "Media payloads overlap")
        if any(data[offset + prior : offset + lo]):
            reject("padding", "nonzero Media payload padding")
        prior = hi
    if prior != size:
        reject("structure", "Media region has unaccounted trailing bytes")
    if payload_cursor != size:
        reject("structure", "Media payload order/size is invalid")
    return records


def select_header(data: bytes) -> tuple:
    """Pick the valid header slot with the highest generation (SPEC §1/§3)."""
    if len(data) < DATA_START:
        reject("length", "file shorter than the header area")
    valid = []
    for slot in HEADER_SLOTS:
        raw = data[slot : slot + 256]
        values = HEADER.unpack_from(raw)
        if values[0] != b"NXAR" or crc32c(raw[:252]) != values[25] or any(data[slot+256:slot+4096]):
            continue
        valid.append((values[22], raw, values))
    if not valid:
        reject("unsupported", "no header slot has a valid magic and CRC-32C")
    if len(valid) == 2 and valid[0][0] == valid[1][0] and valid[0][1] != valid[1][1]:
        reject("format", "both header slots are valid with the same generation but differ")
    return max(valid, key=lambda item: item[0])[2]


def validate_archive(data: bytes, *, verify_payloads: bool = True) -> dict:
    values = select_header(data)
    magic, major, minor, header_size, required, optional = values[:6]
    q = values[6:19]
    catalog_hash, manifest_hash, index_hash, generation, check_size, reserved, _ = values[19:26]
    if major != 0 or minor not in (2, 3) or header_size != 256:
        reject("version", "unsupported header identity/version")
    if required != 1:
        reject("feature", "unsupported header layout or required feature")
    if optional:
        reject("feature", "unknown optional feature in 0.2")
    if reserved != bytes(24) or check_size != CHECK_SIZE:
        reject("header", "reserved bytes or check size invalid")
    (
        archive_size,
        catalog_offset,
        catalog_size,
        media_offset,
        media_size,
        data_offset,
        data_size,
        manifest_offset,
        manifest_size,
        block_index_offset,
        block_count,
        integrity_offset,
        integrity_size,
    ) = q
    if archive_size > len(data):
        reject("length", "declared archive size exceeds the physical file")
    if archive_size > 0x7FFFFFFFFFFFFFFF or catalog_size > 16 * 1024 * 1024:
        reject("resource", "archive/Catalog size outside limit")
    if block_count > 2**32 or integrity_size % 4:
        reject("structure", "invalid block count or integrity size")
    if block_count == 0:
        if block_index_offset or integrity_offset or data_offset or data_size or integrity_size:
            reject("structure", "empty data/index fields disagree")
    elif data_offset != DATA_START or not block_index_offset or not integrity_offset or not data_size or not integrity_size:
        reject("structure", "nonempty block table lacks a Data/index section")
    checked_range(catalog_offset, catalog_size, archive_size)
    if sha(data[catalog_offset : catalog_offset + catalog_size]) != catalog_hash:
        reject("catalog_hash", "Catalog digest mismatch")
    if manifest_size < MANIFEST_HEADER.size or manifest_size > 4 * 1024**3:
        reject("resource", "Manifest size outside limit")
    if media_size == 0 and media_offset:
        reject("structure", "absent media section has nonzero offset")

    # Tail: Media?, Catalog, Manifest, [block index, integrity], packed.
    tail = []
    if media_size:
        tail.append((media_offset, media_size, "media"))
    tail.append((catalog_offset, catalog_size, "catalog"))
    tail.append((manifest_offset, manifest_size, "manifest"))
    if block_count:
        tail.append((block_index_offset, block_count * BLOCK.size, "block-index"))
        tail.append((integrity_offset, integrity_size, "integrity"))
    data_end = data_offset + data_size if block_count else DATA_START
    first_start, _, first_name = tail[0]
    if first_start % 16 or first_start < ((data_end + 15) & ~15):
        reject("structure", f"{first_name} does not start after the Data region on a 16-byte boundary")
    prior_end = first_start
    for start, size, name in tail:
        if start != ((prior_end + 15) & ~15) and name != first_name:
            reject("structure", f"{name} is not at its packed tail offset")
        checked_range(start, size, archive_size)
        if any(data[prior_end:start]):
            reject("padding", f"nonzero tail padding before {name}")
        prior_end = start + size
    if prior_end != archive_size:
        reject("length", "tail does not end at archive_size")

    index_bytes = block_count * BLOCK.size
    checked_range(block_index_offset, index_bytes, len(data))
    checked_range(integrity_offset, integrity_size, len(data))
    computed_index_hash = sha(data[block_index_offset : block_index_offset + index_bytes] + data[integrity_offset : integrity_offset + integrity_size])
    if computed_index_hash != index_hash:
        reject("index_hash", "hot-index or integrity-table digest mismatch")

    catalog = data[catalog_offset : catalog_offset + catalog_size]
    if len(catalog) < CATALOG_HEADER.size:
        reject("catalog", "short Catalog")
    ch = CATALOG_HEADER.unpack_from(catalog)
    c_magic, c_version, c_header_size, c_total = ch[:4]
    c_counts = ch[4:12]
    c_offsets = ch[12:20]
    c_reserved = ch[20]
    cflags, app_count, program_count, snapshot_count, local_count, dep_count, ext_count, c_reserved0 = c_counts
    app_off, program_off, snapshot_off, local_off, dep_off, ext_off, string_off, string_size = c_offsets
    if (c_magic, c_version, c_header_size, c_total, cflags, c_reserved0, c_reserved) != (b"NXCT", 1, 128, catalog_size, 0, 0, bytes(16)):
        reject("catalog", "invalid Catalog header")
    if not (1 <= app_count <= 4096 and program_count <= 16384 and 1 <= snapshot_count <= 65536 and local_count <= 16384 and dep_count <= 32768 and ext_count <= 4096):
        reject("resource", "Catalog count outside limit")
    arrays = [
        table_range(app_off, app_count, APPLICATION.size, catalog_size, alignment=8),
        table_range(program_off, program_count, PROGRAM.size, catalog_size, alignment=8),
        table_range(snapshot_off, snapshot_count, SNAPSHOT.size, catalog_size, alignment=8),
        table_range(local_off, local_count, LOCALIZED.size, catalog_size, alignment=8, optional=True),
        table_range(dep_off, dep_count, DEPENDENCY.size, catalog_size, alignment=8, optional=True),
        table_range(ext_off, ext_count, EXTENSION.size, catalog_size, alignment=8, optional=True),
    ]
    if string_off % 8:
        reject("catalog", "string pool is misaligned")
    string_range = checked_range(string_off, string_size, catalog_size)
    occupied = [(0, 128)] + [r for r in arrays if r is not None] + [string_range]
    occupied.sort()
    for (_, end), (start, _) in zip(occupied, occupied[1:]):
        if start < end:
            reject("catalog", "Catalog tables/string pool overlap")
        if any(catalog[end:start]):
            reject("padding", "nonzero Catalog padding")

    catalog_cursor = CATALOG_HEADER.size
    for offset, count, item_size in (
        (app_off, app_count, APPLICATION.size),
        (program_off, program_count, PROGRAM.size),
        (snapshot_off, snapshot_count, SNAPSHOT.size),
        (local_off, local_count, LOCALIZED.size),
        (dep_off, dep_count, DEPENDENCY.size),
        (ext_off, ext_count, EXTENSION.size),
    ):
        if count:
            catalog_cursor = (catalog_cursor + 7) & ~7
            if offset != catalog_cursor:
                reject("catalog", "Catalog table is not at its minimum aligned offset")
            catalog_cursor += count * item_size
        elif offset:
            reject("catalog", "empty Catalog table has nonzero offset")
    catalog_cursor = (catalog_cursor + 7) & ~7
    if string_off != catalog_cursor:
        reject("catalog", "string pool is not at its minimum aligned offset")
    catalog_cursor += string_size

    pool = bytes(catalog[string_off : string_off + string_size])
    apps = [APPLICATION.unpack_from(catalog, app_off + i * APPLICATION.size) for i in range(app_count)]
    programs = [PROGRAM.unpack_from(catalog, program_off + i * PROGRAM.size) for i in range(program_count)]
    snapshots = [SNAPSHOT.unpack_from(catalog, snapshot_off + i * SNAPSHOT.size) for i in range(snapshot_count)]
    localized = [LOCALIZED.unpack_from(catalog, local_off + i * LOCALIZED.size) for i in range(local_count)] if local_count else []
    program_snapshots: list[list[int]] = [[] for _ in range(program_count)]
    for snapshot_i, snapshot in enumerate(snapshots):
        if snapshot[0] != NO_INDEX and snapshot[0] < program_count:
            program_snapshots[snapshot[0]].append(snapshot_i)

    expected_media_refs: list[tuple[int, tuple[int, ...]]] = []
    opaque_ranges: list[tuple[int, int]] = []
    app_program_cursor = app_snapshot_cursor = localized_cursor = 0
    app_keys: list[tuple[int, int]] = []
    for app_i, row in enumerate(apps):
        app_id, platform, flags, reserved0 = row[:4]
        ints = row[4:18]
        (first_program, nprogram, first_snapshot, nsnapshot, title_off, title_len, pub_off, pub_len,
         icon, first_local, nlocal, nacp, opaque_first, opaque_count) = ints
        metadata_snapshot = row[18]
        if not app_id or platform not in (0, 1, 2) or flags & ~3 or reserved0 or row[19] != bytes(8):
            reject("catalog", "invalid application record")
        key = (platform, app_id)
        if app_i and key <= app_keys[-1]:
            reject("catalog", "(platform, application ID) pairs not strictly sorted")
        app_keys.append(key)
        if nprogram:
            if first_program != app_program_cursor or first_program + nprogram > program_count:
                reject("catalog", "invalid application program range")
            app_program_cursor += nprogram
        elif first_program != NO_INDEX:
            reject("catalog", "invalid application program range")
        if first_snapshot != app_snapshot_cursor or nsnapshot == 0 or first_snapshot + nsnapshot > snapshot_count:
            reject("catalog", "invalid application snapshot range")
        app_snapshot_cursor += nsnapshot
        title = decode_string(pool, title_off, title_len)
        publisher = decode_string(pool, pub_off, pub_len)
        if icon != NO_INDEX:
            expected_media_refs.append((icon, (1, 2)))
        if nacp != NO_INDEX:
            expected_media_refs.append((nacp, (4,)))
        if first_local != localized_cursor or first_local + nlocal > local_count:
            reject("catalog", "invalid localized-entry range")
        localized_cursor += nlocal
        has_native_metadata = (title is not None or publisher is not None or icon != NO_INDEX or
                               nacp != NO_INDEX or bool(nlocal) or bool(opaque_count))
        if metadata_snapshot == NO_INDEX:
            if has_native_metadata:
                reject("catalog", "native metadata exists without a metadata source snapshot")
        elif not first_snapshot <= metadata_snapshot < first_snapshot + nsnapshot:
            reject("catalog", "metadata source snapshot is outside the application range")
        elif not has_native_metadata:
            reject("catalog", "metadata source snapshot has no native metadata")
        if opaque_count:
            expected_media_refs.extend((index, (5,)) for index in range(opaque_first, opaque_first + opaque_count))
            opaque_ranges.append((opaque_first, opaque_first + opaque_count))
        elif opaque_first not in (0, NO_INDEX):
            reject("catalog", "empty opaque-media range has invalid start")
    if app_program_cursor != program_count or app_snapshot_cursor != snapshot_count or localized_cursor != local_count:
        reject("catalog", "Catalog ranges do not cover all rows")

    prior_program_key: tuple[int, int] | None = None
    for prog_i, row in enumerate(programs):
        pid, app_index, kind, flags, reserved0, content_id, first_snapshot, nsnapshot, npdm, reserved1, reserved2 = row
        if not pid or app_index >= app_count or kind not in (0, 1, 2) or flags & ~1 or reserved0 or reserved1 or reserved2 != bytes(16):
            reject("catalog", "invalid program record")
        if bool(flags & 1) != (content_id != bytes(16)):
            reject("catalog", "program optional ID flag/value mismatch")
        key = (app_index, pid)
        if prior_program_key is not None and key <= prior_program_key:
            reject("catalog", "program records are not sorted")
        prior_program_key = key
        if nsnapshot == 0 or first_snapshot + nsnapshot > snapshot_count:
            reject("catalog", "invalid program snapshot range")
        associated = program_snapshots[prog_i]
        if associated != list(range(first_snapshot, first_snapshot + nsnapshot)):
            reject("catalog", "program snapshot range does not exactly cover associated snapshots")
        for snapshot_i in range(first_snapshot, first_snapshot + nsnapshot):
            if snapshots[snapshot_i][0] != prog_i:
                reject("catalog", "program snapshot range points to another program")
        if npdm != NO_INDEX:
            if nsnapshot != 1:
                reject("catalog", "legacy Program NPDM is ambiguous across snapshots")
            expected_media_refs.append((npdm, (3,)))
    if app_program_cursor != program_count:
        reject("catalog", "application program ranges do not cover table")

    for app_i, row in enumerate(apps):
        ints = row[4:18]
        first_program, nprogram = ints[0], ints[1]
        for program_i in range(first_program, first_program + nprogram):
            if programs[program_i][1] != app_i:
                reject("catalog", "program application range mismatch")
    for app in apps:
        first_program, n_program = app[4], app[5]
        first_snapshot, n_snapshot = app[6], app[7]
        program_ids_in_app = set(range(first_program, first_program + n_program)) if n_program else set()
        seen_unassociated_dlc = False
        previous_program = -1
        for snapshot_i in range(first_snapshot, first_snapshot + n_snapshot):
            program_index = snapshots[snapshot_i][0]
            kind = snapshots[snapshot_i][1]
            if program_index == NO_INDEX:
                if kind != 2:
                    reject("catalog", "only DLC may lack an executable program association")
                seen_unassociated_dlc = True
            elif program_index not in program_ids_in_app or seen_unassociated_dlc or program_index < previous_program:
                reject("catalog", "application snapshot range points outside its programs")
            else:
                previous_program = program_index

    local_cursor = 0
    last_language_by_app: dict[int, bytes] = {}
    for app_i, row in enumerate(apps):
        first, count = row[4 + 9], row[4 + 10]
        for local_i in range(first, first + count):
            lang_off, lang_len, title_off, title_len, icon, flags = localized[local_i]
            lang = decode_string(pool, lang_off, lang_len, required=True, max_length=63)
            if not valid_language_tag(lang) or flags:
                reject("catalog", "invalid localized language/flags")
            previous = last_language_by_app.get(app_i)
            if previous is not None and lang <= previous:
                reject("catalog", "localized languages not sorted")
            last_language_by_app[app_i] = lang
            decode_string(pool, title_off, title_len)
            if title_off == NO_INDEX and icon == NO_INDEX:
                reject("catalog", "localized entry has no title or icon")
            if icon != NO_INDEX:
                expected_media_refs.append((icon, (1, 2)))

    deps = [DEPENDENCY.unpack_from(catalog, dep_off + i * DEPENDENCY.size) for i in range(dep_count)] if dep_count else []
    dep_cursor = 0
    snapshot_owner = [-1] * snapshot_count
    for app_i, app in enumerate(apps):
        first_snapshot, n_snapshot = app[6], app[7]
        for snapshot_i in range(first_snapshot, first_snapshot + n_snapshot):
            snapshot_owner[snapshot_i] = app_i
    prior_snapshot_key: tuple[int, int, int, int, bytes, bytes] | None = None
    identities_seen: set[bytes] = set()
    for snapshot_i, row in enumerate(snapshots):
        program_index, kind, flags, reserved0, numeric, root, logical_size, display_off, display_len, content_id, first_dep, ndeps, identity, build_id = row
        if flags & 2 and (kind != 2 or program_index != NO_INDEX or
                          content_id[:8] == bytes(8) or content_id[8:] != bytes(8)):
            reject("catalog", "add-on title ID needs an unassociated DLC")
        if (kind not in (0, 1, 2, 3, 4) or flags & ~3 or reserved0 or
                (program_index == NO_INDEX and kind != 2) or
                (program_index != NO_INDEX and program_index >= program_count) or
                bool(flags & 1) != (build_id != bytes(32))):
            reject("catalog", "invalid snapshot record")
        if identity == bytes(32) or identity in identities_seen:
            reject("catalog", "zero or duplicate snapshot identity")
        identities_seen.add(identity)
        sort_program = program_index if program_index != NO_INDEX else program_count
        snapshot_key = (snapshot_owner[snapshot_i], sort_program, kind, numeric, content_id, build_id)
        if prior_snapshot_key is not None and snapshot_key <= prior_snapshot_key:
            reject("catalog", "snapshot rows are not sorted")
        prior_snapshot_key = snapshot_key
        if first_dep != dep_cursor or first_dep + ndeps > dep_count:
            reject("catalog", "snapshot dependency range invalid")
        dep_cursor += ndeps
        decode_string(pool, display_off, display_len, max_length=256)
    for app_i, app in enumerate(apps):
        metadata_snapshot = app[18]
        if metadata_snapshot != NO_INDEX and snapshots[metadata_snapshot][1] == 2:
            reject("catalog", "DLC cannot supply application display metadata")
    if dep_cursor != dep_count:
        reject("catalog", "snapshot ranges do not cover dependencies")
    for row in deps:
        dep_kind, flags, reserved0, content_id, target_identity, reserved1 = row
        if dep_kind not in (1, 2, 3) or flags & ~1 or reserved0 or reserved1 or target_identity == bytes(32):
            reject("catalog", "invalid dependency record")
        if bool(flags & 1) == (content_id == bytes(16)):
            reject("catalog", "dependency content ID flag/value mismatch")

    extensions = [EXTENSION.unpack_from(catalog, ext_off + i * EXTENSION.size) for i in range(ext_count)] if ext_count else []
    extension_ranges: list[tuple[int, int]] = []
    extension_keys: set[tuple[int, int, int]] = set()
    metadata_history: dict[int, list[int]] = {}
    source_resources: list[tuple[int, int]] = []
    for type_id, flags, owner_kind, reserved0, owner_index, payload_off, payload_size in extensions:
        if flags & ~1 or owner_kind not in (0, 1, 2, 3) or reserved0:
            reject("catalog", "invalid extension descriptor")
        if (owner_kind == 0 and owner_index != NO_INDEX) or (owner_kind == 1 and owner_index >= app_count) or (owner_kind == 2 and owner_index >= program_count) or (owner_kind == 3 and owner_index >= snapshot_count):
            reject("catalog", "invalid extension owner")
        key = (type_id, owner_kind, owner_index)
        if key in extension_keys:
            reject("catalog", "duplicate extension key")
        extension_keys.add(key)
        lo, hi = checked_range(payload_off, payload_size, len(catalog))
        if lo < string_off + string_size:
            reject("catalog", "extension payload overlaps tables or strings")
        extension_ranges.append((lo, hi))
        catalog_cursor = (catalog_cursor + 7) & ~7
        if lo != catalog_cursor:
            reject("catalog", "extension payload is not at its minimum aligned offset")
        catalog_cursor = hi
        if type_id in (0x00010001, 0x00010002, 0x00010003) and (owner_kind != 1 or apps[owner_index][18] == NO_INDEX):
            reject("catalog", "application discovery extension has no exact metadata source")
        if type_id == 0x00010001:
            if owner_kind != 1 or payload_size != 4:
                reject("catalog", "invalid required-firmware extension")
        elif type_id == 0x00010002:
            if owner_kind != 1 or payload_size < 3:
                reject("catalog", "invalid supported-languages extension")
            extension_data = catalog[lo:hi]
            language_count = struct.unpack_from("<H", extension_data)[0]
            if not 1 <= language_count <= 64:
                reject("catalog", "language extension count outside limit")
            position = 2
            previous_language = b""
            for _ in range(language_count):
                if position >= len(extension_data):
                    reject("catalog", "truncated language extension")
                language_length = extension_data[position]
                position += 1
                if not 1 <= language_length <= 63 or language_length > len(extension_data) - position:
                    reject("catalog", "invalid language extension tag length")
                language = extension_data[position : position + language_length]
                position += language_length
                if not valid_language_tag(language) or language <= previous_language:
                    reject("catalog", "invalid or unsorted language extension tag")
                previous_language = language
            if position != len(extension_data):
                reject("catalog", "extra bytes in language extension")
        elif type_id == 0x00010003:
            if owner_kind != 1 or payload_size < 4:
                reject("catalog", "invalid region extension")
            extension_data = catalog[lo:hi]
            region_count = struct.unpack_from("<H", extension_data)[0]
            if not 1 <= region_count <= 249 or len(extension_data) != 2 + region_count * 2:
                reject("catalog", "region extension count/size mismatch")
            region_codes = [extension_data[2 + i * 2 : 4 + i * 2] for i in range(region_count)]
            if any(any(byte not in b"ABCDEFGHIJKLMNOPQRSTUVWXYZ" for byte in value) for value in region_codes) or region_codes != sorted(set(region_codes)):
                reject("catalog", "invalid or unsorted region code")
        elif type_id == 0x00010004:
            if owner_kind != 1 or flags != 1 or payload_size < 24:
                reject("catalog", "invalid Snapshot Metadata History descriptor")
            payload = catalog[lo:hi]
            magic, version, header_size, record_count, history_local_count, opaque_ref_count, reserved = struct.unpack_from("<4sHHIIII", payload)
            if magic != b"NXSM" or version != 1 or header_size != 24 or reserved or not record_count or len(payload) != 24 + record_count * 48 + history_local_count * 24 + opaque_ref_count * 4:
                reject("catalog", "invalid Snapshot Metadata History header or payload size")
            app = apps[owner_index]
            selected_metadata_snapshot = app[18]
            if selected_metadata_snapshot == NO_INDEX or app[7] < 2 or record_count > app[7] - 1 or history_local_count > 16384:
                reject("catalog", "Snapshot Metadata History count or selected source is invalid")
            record_base = 24
            history_local_base = record_base + record_count * 48
            opaque_base = history_local_base + history_local_count * 24
            local_cursor = opaque_cursor = 0
            prior_history_snapshot = -1
            history_indices: list[int] = []
            for record_i in range(record_count):
                row = struct.unpack_from("<12I", payload, record_base + record_i * 48)
                (snapshot_index, title_off, title_len, publisher_off, publisher_len, icon_index, nacp_index,
                 first_local, local_count, first_opaque, opaque_count, row_reserved) = row
                if (row_reserved or snapshot_index < app[6] or snapshot_index >= app[6] + app[7] or
                        snapshot_index == selected_metadata_snapshot or snapshot_index <= prior_history_snapshot or
                        snapshot_owner[snapshot_index] != owner_index or first_local != local_cursor or
                        local_count > history_local_count - local_cursor or first_opaque != opaque_cursor or
                        opaque_count > opaque_ref_count - opaque_cursor):
                    reject("catalog", "snapshot history row is outside its owner or has invalid ranges/order")
                title = decode_string(pool, title_off, title_len)
                publisher_value = decode_string(pool, publisher_off, publisher_len)
                if title is None and publisher_value is None and icon_index == NO_INDEX and nacp_index == NO_INDEX and not local_count and not opaque_count:
                    reject("catalog", "empty snapshot metadata history row")
                if icon_index != NO_INDEX:
                    expected_media_refs.append((icon_index, (1, 2)))
                if nacp_index != NO_INDEX:
                    expected_media_refs.append((nacp_index, (4,)))
                prior_language = b""
                for local_i in range(first_local, first_local + local_count):
                    lang_off, lang_len, local_title_off, local_title_len, local_icon, local_flags = LOCALIZED.unpack_from(payload, history_local_base + local_i * LOCALIZED.size)
                    language = decode_string(pool, lang_off, lang_len, required=True, max_length=63)
                    local_title = decode_string(pool, local_title_off, local_title_len)
                    if not valid_language_tag(language) or local_flags or (local_title is None and local_icon == NO_INDEX) or (local_i > first_local and language <= prior_language):
                        reject("catalog", "invalid historical localized metadata row")
                    prior_language = language
                    if local_icon != NO_INDEX:
                        expected_media_refs.append((local_icon, (1, 2)))
                for opaque_i in range(first_opaque, first_opaque + opaque_count):
                    opaque_index = struct.unpack_from("<I", payload, opaque_base + opaque_i * 4)[0]
                    expected_media_refs.append((opaque_index, (5,)))
                history_indices.append(snapshot_index)
                prior_history_snapshot = snapshot_index
                local_cursor += local_count
                opaque_cursor += opaque_count
            if local_cursor != history_local_count or opaque_cursor != opaque_ref_count:
                reject("catalog", "snapshot metadata ranges do not cover payload arrays")
            metadata_history[owner_index] = history_indices
        elif type_id == 0x00010005:
            if owner_kind != 1 or flags != 1 or payload_size < 16:
                reject("catalog", "invalid Snapshot Native Metadata descriptor")
            payload = catalog[lo:hi]
            magic, version, header_size, count, reserved = struct.unpack_from("<4sHHII", payload)
            if magic != b"NXNM" or version != 1 or header_size != 16 or reserved or not count or count > apps[owner_index][7] or payload_size != 16 + count * 16:
                reject("catalog", "invalid Snapshot Native Metadata header")
            previous = -1
            for i in range(count):
                si, npdm, firmware, row_flags = struct.unpack_from("<4I", payload, 16 + i * 16)
                if si >= snapshot_count or snapshot_owner[si] != owner_index or si <= previous or row_flags & ~1 or (not row_flags & 1 and firmware) or (npdm == NO_INDEX and not row_flags & 1):
                    reject("catalog", "invalid Snapshot Native Metadata row")
                if npdm != NO_INDEX:
                    program = snapshots[si][0]
                    if program == NO_INDEX or programs[program][8] != NO_INDEX:
                        reject("catalog", "ambiguous snapshot NPDM")
                    expected_media_refs.append((npdm, (3,)))
                previous = si
        elif type_id == 0x00010008:
            if owner_kind != 1 or flags != 1 or payload_size < 16:
                reject("catalog", "invalid source resource descriptor")
            payload = catalog[lo:hi]
            magic, version, header_size, count, reserved = struct.unpack_from("<4sHHII", payload)
            if (magic, version, header_size, reserved) != (b"NXSR", 1, 16, 0) or not 1 <= count <= 4096 or payload_size != 16 + count * 8:
                reject("catalog", "invalid source resource header")
            prior = None
            for i in range(count):
                item = struct.unpack_from("<II", payload, 16 + i * 8)
                if item[0] > 1 or (prior is not None and prior >= item):
                    reject("catalog", "invalid or unsorted source resource")
                source_resources.append(item)
                prior = item
        elif type_id == 0x00010006:
            # Optional rebuild recipes: an older reader may skip them.
            if owner_kind != 1 or flags != 0 or payload_size < 16:
                reject("catalog", "invalid rebuild recipe descriptor")
            payload = catalog[lo:hi]
            magic, version, header_size, count, reserved = struct.unpack_from("<4sHHII", payload)
            if magic != b"NXRC" or version != 1 or header_size != 16 or reserved or not count or count > 4096:
                reject("catalog", "invalid rebuild recipe header")
            at, previous = 16, b""
            for _ in range(count):
                if at + 8 > payload_size:
                    reject("catalog", "rebuild recipe is truncated")
                (length,) = struct.unpack_from("<Q", payload, at)
                start, end = at + 8, at + 8 + length
                if length < 104 or end > payload_size:
                    reject("catalog", "rebuild recipe is truncated")
                recipe = payload[start:end]
                if recipe[:8] != b"NXRECIPE" or sha(recipe[:-32]) != recipe[-32:]:
                    reject("catalog", "rebuild recipe checksum mismatch")
                if recipe[-32:] <= previous:
                    reject("catalog", "rebuild recipes are unsorted or duplicated")
                previous = recipe[-32:]
                padded = (end + 7) & ~7
                if padded > payload_size or any(payload[end:padded]):
                    reject("catalog", "invalid rebuild recipe padding")
                at = padded
            if at != payload_size:
                reject("catalog", "rebuild recipe extension has trailing bytes")
        if type_id not in (0x00010001, 0x00010002, 0x00010003, 0x00010004, 0x00010005, 0x00010008) and flags & 1:
            reject("feature", "unknown required Catalog extension")
    if len(source_resources) > 4096 or (minor == 3) != bool(source_resources):
        reject("catalog", "format version disagrees with source resources")
    if catalog_cursor != catalog_size:
        reject("catalog", "Catalog has unaccounted trailing bytes")
    for app_i, history_indices in metadata_history.items():
        if apps[app_i][18] == NO_INDEX:
            reject("catalog", "snapshot history has no selected Catalog metadata source")
        # The archive records the selected source, not whether selection was
        # automatic or user-directed. Any metadata-bearing non-DLC snapshot
        # in this application is therefore valid. Writers choose the highest
        # version (then greatest Catalog index) by default; repack callers may
        # explicitly select another eligible source.

    media_records = parse_media(data, media_offset, media_size, verify_payloads=verify_payloads)
    reference_counts: dict[int, int] = {}
    for index, expected_types in expected_media_refs:
        if index >= len(media_records) or media_records[index]["type"] not in expected_types:
            reject("catalog", "media index/type mismatch")
        reference_counts[index] = reference_counts.get(index, 0) + 1
    if any(reference_counts.get(i, 0) == 0 for i in range(len(media_records))):
        reject("catalog", "unreferenced Media record")
    for index, record in enumerate(media_records):
        if record["type"] not in (1, 2) and reference_counts.get(index, 0) != 1:
            reject("catalog", "non-icon Media record is not referenced exactly once")
    opaque_ranges.sort()
    for (_, prior_end), (next_start, _) in zip(opaque_ranges, opaque_ranges[1:]):
        if next_start < prior_end:
            reject("catalog", "opaque media ranges overlap")

    manifest = data[manifest_offset : manifest_offset + manifest_size]
    if sha(manifest) != manifest_hash:
        reject("manifest_hash", "Manifest digest mismatch")
    if len(manifest) < MANIFEST_HEADER.size:
        reject("manifest", "short Manifest")
    mh = MANIFEST_HEADER.unpack_from(manifest)
    m_magic, m_version, m_header_size, m_total = mh[:4]
    m_counts, m_offsets, m_reserved = mh[4:8], mh[8:14], mh[14]
    directory_count, file_count, entry_count, reference_count = m_counts
    directory_off, file_off, entry_off, reference_off, path_off, path_size = m_offsets
    if (m_magic, m_version, m_header_size, m_total, m_reserved) != (b"NXMF", 1, 96, manifest_size, bytes(16)):
        reject("manifest", "invalid Manifest header")
    if not (1 <= directory_count <= 16_777_216 and file_count <= 33_554_432 and entry_count <= 50_331_648 and path_size <= 2 * 1024**3):
        reject("resource", "Manifest count outside limit")
    m_ranges = [
        table_range(directory_off, directory_count, DIRECTORY.size, manifest_size, alignment=8),
        table_range(file_off, file_count, FILE.size, manifest_size, alignment=8, optional=True),
        table_range(entry_off, entry_count, ENTRY.size, manifest_size, alignment=8, optional=True),
        table_range(reference_off, reference_count, 4, manifest_size, alignment=4, optional=True),
    ]
    path_range = checked_range(path_off, path_size, manifest_size)
    occupied = [(0, 96)] + [r for r in m_ranges if r is not None] + [path_range]
    occupied.sort()
    for (_, end), (start, _) in zip(occupied, occupied[1:]):
        if start < end:
            reject("manifest", "Manifest tables/path pool overlap")
        if any(manifest[end:start]):
            reject("padding", "nonzero Manifest table padding")
    manifest_cursor = MANIFEST_HEADER.size
    for offset, count, item_size, alignment in (
        (directory_off, directory_count, DIRECTORY.size, 8),
        (file_off, file_count, FILE.size, 8),
        (entry_off, entry_count, ENTRY.size, 8),
        (reference_off, reference_count, 4, 4),
    ):
        if count:
            manifest_cursor = (manifest_cursor + alignment - 1) & ~(alignment - 1)
            if offset != manifest_cursor:
                reject("manifest", "Manifest table is not at its minimum aligned offset")
            manifest_cursor += count * item_size
        elif offset:
            reject("manifest", "empty Manifest table has nonzero offset")
    manifest_cursor = (manifest_cursor + 7) & ~7
    if path_off != manifest_cursor:
        reject("manifest", "Path Pool is not at its minimum aligned offset")
    manifest_cursor += path_size
    if manifest_cursor != manifest_size:
        reject("manifest", "Manifest has unaccounted trailing bytes")

    directories = [DIRECTORY.unpack_from(manifest, directory_off + i * DIRECTORY.size) for i in range(directory_count)]
    entries: list[dict] = []
    entry_cursor = 0
    for dir_i, (first, count, flags, reserved0) in enumerate(directories):
        if first != entry_cursor or first + count > entry_count or flags or reserved0:
            reject("manifest", "invalid directory entry range or flags")
        previous: bytes | None = None
        for entry_i in range(first, first + count):
            name_off, name_len, target, kind, flags, reserved0, reserved1 = ENTRY.unpack_from(manifest, entry_off + entry_i * ENTRY.size)
            if kind not in (1, 2) or flags or reserved0 or reserved1 or not 1 <= name_len <= 4096:
                reject("path", "invalid directory entry")
            if name_off < path_off or name_off > path_off + path_size or name_len > path_off + path_size - name_off:
                reject("bounds", "path component out of path pool")
            name = bytes(manifest[name_off : name_off + name_len])
            try:
                name.decode("utf-8", "strict")
            except UnicodeDecodeError:
                reject("path", "invalid path UTF-8")
            if b"\x00" in name or b"/" in name or b"\\" in name or name in (b".", b".."):
                reject("path", "unsafe path component")
            if previous is not None and name <= previous:
                reject("path", "directory names are duplicate or unsorted")
            previous = name
            if (kind == 1 and target >= directory_count) or (kind == 2 and target >= file_count):
                reject("manifest", "directory target out of bounds")
            entries.append({"name": name, "kind": kind, "target": target})
        entry_cursor += count
    if entry_cursor != entry_count:
        reject("manifest", "directory ranges do not cover entries")

    block_refs: list[int] = []
    for i in range(reference_count):
        block_refs.append(struct.unpack_from("<I", manifest, reference_off + i * 4)[0])
    files = []
    ref_cursor = 0
    for file_i in range(file_count):
        size, first, count, chunk_log2, flags, reserved0, digest, reserved1 = FILE.unpack_from(manifest, file_off + file_i * FILE.size)
        if size > 0x7FFFFFFFFFFFFFFF or chunk_log2 not in (16, 17, 18, 19, 20) or flags or reserved0 or reserved1 != bytes(12):
            reject("manifest", "invalid file record")
        expected = math.ceil(size / (1 << chunk_log2)) if size else 0
        if count != expected or first != ref_cursor or first + count > reference_count:
            reject("manifest", "file block-reference range/count mismatch")
        refs = block_refs[first : first + count]
        if any(block_id >= block_count for block_id in refs):
            reject("manifest", "block reference out of range")
        ref_cursor += count
        files.append({"size": size, "digest": digest, "chunk_log2": chunk_log2, "refs": refs})
    if ref_cursor != reference_count:
        reject("manifest", "files do not cover block-reference table")

    block_records = []
    block_contents: list[bytes | None] = []
    if block_count:
        index_bytes = block_count * BLOCK.size
        checked_range(block_index_offset, index_bytes, len(data))
        checked_range(integrity_offset, integrity_size, len(data))
        last_end = data_offset
        next_check = 0
        for block_i in range(block_count):
            record = BLOCK.unpack_from(data, block_index_offset + block_i * BLOCK.size)
            block_at, stored, logical, codec, flags, reserved0, first_check = record
            if flags or reserved0 or codec not in (0, 1) or not 1 <= logical <= 1024 * 1024 or not 1 <= stored <= 1024 * 1024 + 65536:
                reject("block", "invalid hot block record")
            if first_check != next_check:
                reject("block", "block first_check is not the running slice count")
            slices = (logical + CHECK_SIZE - 1) // CHECK_SIZE
            next_check += slices
            if next_check * 4 > integrity_size:
                reject("structure", "integrity table shorter than the block slices")
            if block_at % 16 or block_at < ((last_end + 15) & ~15):
                reject("block", "block offsets overlap or misalign")
            checked_range(block_at, stored, data_offset + data_size)
            payload = bytes(data[block_at : block_at + stored])
            if codec == 0:
                if stored != logical:
                    reject("block", "RAW block stored and logical sizes differ")
                decoded = payload if verify_payloads else None
            else:
                decoded = None
                if verify_payloads:
                    try:
                        decoder = zstd.ZstdDecompressor(options={zstd.DecompressionParameter.window_log_max: 20})
                        decoded = decoder.decompress(payload, max_length=logical + 1)
                    except zstd.ZstdError as exc:
                        reject("zstd", str(exc))
                    if len(decoded) != logical or not decoder.eof or decoder.unused_data:
                        reject("zstd", "frame output size, end marker, or trailing bytes invalid")
            if verify_payloads and decoded is not None:
                for k in range(slices):
                    expected = struct.unpack_from("<I", data, integrity_offset + (first_check + k) * 4)[0]
                    if crc32c(decoded[k * CHECK_SIZE : (k + 1) * CHECK_SIZE]) != expected:
                        reject("block_hash", "block slice CRC-32C mismatch")
            block_records.append(record)
            block_contents.append(decoded)
            last_end = block_at + stored
        if last_end != data_offset + data_size:
            reject("length", "Data region does not end at the last block byte")
        if next_check * 4 != integrity_size:
            reject("structure", "integrity size disagrees with the block slices")
    for file in files:
        chunk_size = 1 << file["chunk_log2"]
        for chunk_index, block_id in enumerate(file["refs"]):
            expected_size = min(chunk_size, file["size"] - chunk_index * chunk_size)
            if block_records[block_id][2] != expected_size:
                reject("manifest", "block logical size disagrees with file chunk")

    for file in files:
        if verify_payloads:
            chunks = [block_contents[block_id] for block_id in file["refs"]]
            if any(chunk is None for chunk in chunks):
                reject("block", "full verification has an undecoded block")
            content = b"".join(chunk for chunk in chunks if chunk is not None)
            content = content[: file["size"]]
            if len(content) != file["size"]:
                reject("manifest", "file block references do not cover logical size")
            if sha(content) != file["digest"]:
                reject("file_hash", "file content digest mismatch")

    resource_roots = set()
    child_roots = {e["target"] for e in entries if e["kind"] == 1}
    snapshot_roots = {row[5] for row in snapshots}
    for kind, root in source_resources:
        if root >= directory_count or root in resource_roots or root in child_roots or root in snapshot_roots:
            reject("manifest", "invalid source resource root")
        resource_roots.add(root)
        first, count, _, _ = directories[root]
        members = entries[first:first+count]
        if not members or any(e["kind"] != 2 for e in members) or len({e["target"] for e in members}) != len(members):
            reject("manifest", "source resource is not a flat unique file set")
        if kind == 1 and (count != 1 or members[0]["name"] != b"partition.hfs0"):
            reject("manifest", "system update resource lacks partition.hfs0")

    for snapshot_i, row in enumerate(snapshots):
        program_index, kind, _, _, _, root, logical_size, _, _, _, _, _, expected_identity, _ = row
        if root >= directory_count:
            reject("manifest", "snapshot root out of bounds")
        app_index = snapshot_owner[snapshot_i]
        program_id = 0xFFFFFFFFFFFFFFFF if program_index == NO_INDEX else programs[program_index][0]
        platform = apps[app_index][1]
        app_id = apps[app_index][0]
        seen: set[tuple[int, int]] = set()
        nodes: list[tuple[bytes, int, int | None]] = []

        def walk(dir_id: int, prefix: bytes, depth: int) -> int:
            if depth > 256:
                reject("resource", "directory depth exceeds limit")
            key = (1, dir_id)
            if key in seen:
                reject("manifest", "directory cycle or repeated directory in snapshot")
            seen.add(key)
            nodes.append((prefix, 1, None))
            first, count, _, _ = directories[dir_id]
            total = 0
            for entry_i in range(first, first + count):
                entry = entries[entry_i]
                full_path = entry["name"] if not prefix else prefix + b"/" + entry["name"]
                if entry["kind"] == 1:
                    total += walk(entry["target"], full_path, depth + 1)
                else:
                    file_id = entry["target"]
                    key_file = (2, file_id)
                    if key_file in seen:
                        reject("manifest", "file is repeated in snapshot")
                    seen.add(key_file)
                    nodes.append((full_path, 2, file_id))
                    total += files[file_id]["size"]
            return total

        total = walk(root, b"", 0)
        if total != logical_size:
            reject("manifest", "snapshot logical size mismatch")
        nodes.sort(key=lambda node: node[0])
        canonical = bytearray(b"NX-SNAPSHOT-ID\0")
        canonical.extend(struct.pack("<H B Q Q B I 16s I", 3, platform, app_id, program_id, kind, row[4], row[9], len(nodes)))
        for path, node_kind, file_id in nodes:
            canonical.extend(struct.pack("<B I", node_kind, len(path)))
            canonical.extend(path)
            if node_kind == 2:
                canonical.extend(struct.pack("<Q", files[file_id]["size"]))
                canonical.extend(files[file_id]["digest"])
        if sha(canonical) != expected_identity:
            reject("snapshot_identity", "snapshot identity mismatch")

    for app_i, app in enumerate(apps):
        flags = app[2]
        first_snapshot, n_snapshot = app[4 + 2], app[4 + 3]
        types = [snapshots[i][1] for i in range(first_snapshot, first_snapshot + n_snapshot)]
        if bool(flags & 1) != (0 in types) or bool(flags & 2) != (2 in types):
            reject("catalog", "application base/DLC flags disagree")

    return {"apps": app_count, "programs": program_count, "snapshots": snapshot_count, "files": file_count, "blocks": block_count}


def main() -> int:
    index_path = FIXTURES / "fixture_index.json"
    if not index_path.exists():
        print("missing fixture_index.json; run build_fixtures.py first", file=sys.stderr)
        return 2
    index = json.loads(index_path.read_text(encoding="utf-8"))
    failures: list[str] = []
    for item in index["fixtures"]:
        path = FIXTURES / item["file"]
        if not path.is_file():
            failures.append(f"{item['file']}: missing")
            continue
        data = path.read_bytes()
        actual_sha = sha(data).hex()
        if len(data) != item["size"] or actual_sha != item["sha256"]:
            failures.append(f"{item['file']}: fixture index size/hash mismatch")
            continue
        try:
            if item["expected"] == "reject_full_verify":
                structural = validate_archive(data, verify_payloads=False)
                if structural["blocks"] < 1:
                    failures.append(f"{item['file']}: expected at least one structurally valid block")
                    continue
                try:
                    validate_archive(data, verify_payloads=True)
                except InvalidArchive as exc:
                    if exc.code != item.get("expected_error"):
                        failures.append(f"{item['file']}: rejected as {exc.code}, expected {item.get('expected_error')}")
                else:
                    failures.append(f"{item['file']}: full verification unexpectedly accepted")
            elif item["expected"] == "reject":
                try:
                    validate_archive(data)
                except InvalidArchive as exc:
                    if exc.code != item.get("expected_error"):
                        failures.append(f"{item['file']}: rejected as {exc.code}, expected {item.get('expected_error')}")
                else:
                    failures.append(f"{item['file']}: invalid fixture unexpectedly accepted")
            else:
                stats = validate_archive(data)
                print(f"PASS {item['file']}: {stats}")
        except InvalidArchive as exc:
            failures.append(f"{item['file']}: {exc.code}: {exc}")

    try:
        history_fixture = next(item for item in index["fixtures"] if item["file"] == "metadata_snapshot_tie.nx")
        check_snapshot_history_misassociation((FIXTURES / history_fixture["file"]).read_bytes())
        print("PASS metadata-history rejection: row cannot name the selected source snapshot")
    except (InvalidArchive, RuntimeError, StopIteration) as exc:
        failures.append(f"metadata history negative check: {exc}")

    if failures:
        for failure in failures:
            print(f"FAIL {failure}", file=sys.stderr)
        return 1
    print(f"PASS {len(index['fixtures'])} fixtures validated; {sum(i['expected'] != 'accept' for i in index['fixtures'])} rejected at expected validation level")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
