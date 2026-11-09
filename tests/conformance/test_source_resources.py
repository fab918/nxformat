"""Independent Python/C++ checks for block-backed source preservation."""

import hashlib
from pathlib import Path
import struct
import subprocess
import tempfile

import test_rust_writer as writer
from test_native_conversion import compile_reader

ROOT = Path(__file__).resolve().parents[2]


def main():
    with tempfile.TemporaryDirectory(prefix="nx-source-resources-") as temp:
        out = Path(temp)
        subprocess.run(
            [
                writer.cargo_command(), "run", "--release", "--manifest-path",
                str(ROOT / "packer/Cargo.toml"), "--example",
                "source_resource_conformance", "--", str(out),
            ],
            check=True,
        )
        probe = compile_reader(out, ROOT / "tests/conformance/source_resource_reader.cpp")
        validator = writer.load_validator()
        archive = out / "game.nx"
        original = out / "original.hfs0"
        raw = archive.read_bytes()
        valid = writer.current_header(raw, validator)
        validator.validate_archive(valid, verify_payloads=True)
        subprocess.run([str(probe), str(archive), str(original)], check=True)

        # The resource can still be read when the primary header magic is lost.
        damaged = out / "damaged-primary.nx"
        damaged.write_bytes(b"\0" * 4 + raw[4:])
        subprocess.run([str(probe), str(damaged), str(original)], check=True)

        for case in range(6):
            data = bytearray(valid)
            catalog = struct.unpack_from("<Q", data, 24)[0]
            count = struct.unpack_from("<I", data, catalog + 40)[0]
            table = catalog + struct.unpack_from("<Q", data, catalog + 88)[0]
            extension = next(
                offset for offset in range(table, table + count * 32, 32)
                if struct.unpack_from("<I", data, offset)[0] == 0x10008
            )
            payload = catalog + struct.unpack_from("<Q", data, extension + 16)[0]
            if case == 0:
                struct.pack_into("<I", data, payload + 20, 0xFFFFFFFF)
            elif case == 1:
                struct.pack_into("<I", data, payload + 16, 2)
            elif case == 2:
                struct.pack_into("<I", data, extension + 4, 0)
            elif case == 3:
                struct.pack_into("<I", data, payload + 8, 4097)
            elif case == 4:
                snapshots = catalog + struct.unpack_from("<Q", data, catalog + 64)[0]
                root = struct.unpack_from("<I", data, snapshots + 12)[0]
                struct.pack_into("<I", data, payload + 20, root)
            else:
                for slot in (0, 4096):
                    struct.pack_into("<H", data, slot + 6, 2)

            length = struct.unpack_from("<Q", data, 32)[0]
            digest = hashlib.sha256(data[catalog:catalog + length]).digest()
            for slot in (0, 4096):
                data[slot + 120:slot + 152] = digest
                struct.pack_into("<I", data, slot + 252, validator.crc32c(data[slot:slot + 252]))
            try:
                validator.validate_archive(data, verify_payloads=False)
            except validator.InvalidArchive as error:
                assert error.code not in ("catalog_hash", "manifest_hash", "index_hash"), error
            else:
                raise AssertionError(f"Python accepted invalid resource case {case}")
            path = out / f"invalid-{case}.nx"
            path.write_bytes(data)
            subprocess.run([str(probe), str(path), "reject"], check=True)
        print("PASS Python and C++ reject authenticated invalid source resource structures")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
