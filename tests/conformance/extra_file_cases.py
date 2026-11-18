"""Archives with an extra-file tree, used by the reader tests."""

from __future__ import annotations

import struct
from pathlib import Path

from build_fixtures import assemble, crc32c, sha


def build_extra_cases(destination: Path) -> list[tuple[Path, str]]:
    cases = []

    def emit(name, data, expected):
        path = destination / f"{name}.nx"
        path.write_bytes(data)
        cases.append((path, expected))

    def image(directories=None, extra_root=1):
        return assemble(
            directories=directories if directories is not None else [
                [("game.bin", 2, 0)],
                [("notes", 1, 2), ("readme.txt", 2, 1)],
                [("cheat.txt", 2, 2)],
            ],
            file_bytes=[b"GAME", b"README", b"CHEAT"],
            snapshots=[{"root": 0, "program": 0, "type": 0, "version": 1}],
            extra_root=extra_root,
        )

    valid = image()
    emit("extra_files", valid, "accept")
    emit("extra_missing_root", image(extra_root=99), "reject_mount")
    emit("extra_snapshot_root", image(extra_root=0), "reject_mount")
    game = [("game.bin", 2, 0)]
    for name, directories in (
        ("empty_root", [game, []]),
        ("shared_file", [game, [("copy.bin", 2, 0)]]),
        ("repeated_file", [game, [("a", 2, 1), ("b", 2, 1)]]),
        ("cycle", [game, [("loop", 1, 1)]]),
        ("repeated_directory", [game, [("a", 1, 2), ("b", 1, 2)], [("x", 2, 1)]]),
        ("shared_directory", [[("game.bin", 2, 0), ("shared", 1, 2)],
                              [("copy", 1, 2)], [("x", 2, 1)]]),
        ("snapshot_contains_extra", [[("extra", 1, 1), ("game.bin", 2, 0)], [("x", 2, 1)]]),
        ("extra_contains_snapshot", [game, [("game", 1, 0)]]),
        ("orphan_references_extra", [game, [("x", 2, 1)], [("extra", 1, 1)]]),
        ("too_deep", [game] + [[("d", 1, i + 1)] for i in range(1, 258)] + [[("x", 2, 1)]]),
    ):
        emit(f"extra_{name}", image(directories), "reject_mount")

    catalog, size = struct.unpack_from("<QQ", valid, 24)
    extension = catalog + struct.unpack_from("<Q", valid, catalog + 88)[0]
    payload = catalog + struct.unpack_from("<Q", valid, extension + 16)[0]
    snapshot = catalog + struct.unpack_from("<Q", valid, catalog + 64)[0]
    for name, offset, fmt, value, expected in (
        ("magic", payload, "4s", b"BAD!", "reject_open"),
        ("version", payload + 4, "H", 2, "reject_open"),
        ("size", payload + 6, "H", 15, "reject_open"),
        ("absent_root", payload + 8, "I", 0xffffffff, "reject_open"),
        ("reserved", payload + 12, "I", 1, "reject_open"),
        ("required", extension + 4, "I", 1, "reject_open"),
        ("owner", extension + 8, "H", 1, "reject_open"),
        ("owner_index", extension + 12, "I", 0, "reject_open"),
        ("invalid_snapshot_root", snapshot + 12, "I", 0xffffffff, "reject_mount"),
    ):
        data = bytearray(valid)
        struct.pack_into("<" + fmt, data, offset, value)
        data[120:152] = sha(data[catalog:catalog + size])
        struct.pack_into("<I", data, 252, crc32c(data[:252]))
        data[4096:4352] = data[:256]
        emit(f"extra_{name}", data, expected)

    # Integrity of an extra is checked just like any other file's payload.
    data = bytearray(valid)
    index = struct.unpack_from("<Q", data, 88)[0]
    extra_block = struct.unpack_from("<Q", data, index + 24)[0]
    data[extra_block] ^= 1
    emit("extra_corrupt_payload", data, "reject_verify")
    return cases
