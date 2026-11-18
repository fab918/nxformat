"""Write Zstd decoder test cases to a folder."""

from __future__ import annotations

import json
import struct
import sys
from pathlib import Path

from compression import zstd


def main() -> int:
    destination = Path(sys.argv[1])
    destination.mkdir(parents=True, exist_ok=True)
    valid_data = b"A" * 4096
    valid = zstd.compress(valid_data)
    valid_info = zstd.get_frame_info(valid)
    descriptor = valid[4]
    if valid_info.decompressed_size != len(valid_data) or (descriptor >> 6) & 0x03 != 1 or not descriptor & 0x20:
        raise RuntimeError("valid frame does not use the expected two-byte single-segment content size")
    wrong_size = bytearray(valid)
    wrong_size[5:7] = (len(valid_data) - 1 - 256).to_bytes(2, "little")
    wrong_size = bytes(wrong_size)
    if zstd.get_frame_info(wrong_size).decompressed_size != len(valid_data) - 1:
        raise RuntimeError("wrong-size test frame did not declare the altered logical size")
    dictionary = zstd.ZstdDict(b"abcdefg0123456789" * 100, is_raw=True)
    dictionary_data = b"abcdefg0123456789" * 256
    dictionary_frame = zstd.compress(dictionary_data, zstd_dict=dictionary)
    large_data = b"0123456789abcdef" * 56000
    single_segment = zstd.compress(
        large_data,
        options={
            zstd.CompressionParameter.window_log: 21,
            zstd.CompressionParameter.content_size_flag: 1,
        },
    )
    # Rewrite only the frame header to request a 2 MiB window while preserving
    # the same independently decodable compressed blocks and known content size.
    large_window = single_segment[:4] + b"\xc0\x58" + struct.pack("<Q", len(large_data)) + single_segment[9:]
    cases = {
        "valid.zst": valid,
        "truncated.zst": valid[:-1],
        "wrong_size.zst": wrong_size,
        "concatenated.zst": valid + valid,
        "trailing.zst": valid + b"trailing",
        "skippable.zst": bytes.fromhex("502a4d18") + struct.pack("<I", 3) + b"abc",
        "dictionary.zst": dictionary_frame,
        "large_window.zst": large_window,
    }
    for name, frame in cases.items():
        (destination / name).write_bytes(frame)
    metadata = {
        "zstandard_version": list(zstd.zstd_version_info),
        "sizes": {name: len(frame) for name, frame in cases.items()},
        "large_window_frame_content_size": zstd.get_frame_info(large_window).decompressed_size,
        "large_window_logical_size": len(large_data),
        "dictionary_frame_content_size": zstd.get_frame_info(dictionary_frame).decompressed_size,
    }
    (destination / "metadata.json").write_text(json.dumps(metadata, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    if zstd.get_frame_info(large_window).decompressed_size != len(large_data):
        raise RuntimeError("large-window frame lacks its declared content size")
    descriptor = large_window[4]
    if descriptor & 0x20 or descriptor & 0x18:
        raise RuntimeError("large-window test frame does not use the expected standard frame header")
    window_descriptor = large_window[5]
    exponent = window_descriptor >> 3
    base = 1 << (10 + exponent)
    declared_window = base + (base >> 3) * (window_descriptor & 7)
    if declared_window <= 1024 * 1024:
        raise RuntimeError("large-window frame does not declare a window above the NX limit")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
