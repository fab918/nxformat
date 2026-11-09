"""Independent Python/C++ checks for block-backed source preservation."""

import hashlib
import json
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile

import test_rust_writer as writer
from test_native_conversion import compile_reader

ROOT = Path(__file__).resolve().parents[2]


def main():
    with tempfile.TemporaryDirectory(prefix="nx-source-resources-") as temp:
        out = Path(temp)
        cargo = writer.cargo_command()
        manifest = ["--manifest-path", str(ROOT / "packer/Cargo.toml"), "--locked"]
        subprocess.run(
            [cargo, "build", "--release", *manifest, "--example",
             "source_resource_conformance", "--bin", "nxconvert"],
            check=True,
        )
        metadata = json.loads(subprocess.check_output(
            [cargo, "metadata", *manifest, "--no-deps", "--format-version", "1"]
        ))
        release = Path(metadata["target_directory"]) / "release"
        suffix = ".exe" if sys.platform == "win32" else ""
        generator = release / "examples" / f"source_resource_conformance{suffix}"
        subprocess.run([str(generator), str(out)], check=True)
        nxconvert = release / f"nxconvert{suffix}"

        def cli(*args, check=True):
            result = subprocess.run(
                [str(nxconvert), *map(str, args)],
                capture_output=True, text=True,
            )
            if check and result.returncode:
                raise AssertionError(f"{args}: {result.stderr}")
            return result

        assert cli("--version").stdout.strip() == f"nxconvert {metadata['packages'][0]['version']}"
        assert "--omit-firmware" in cli("--help").stdout

        source = out / "source.dxci"
        patch = out / "patch.dxci"
        archive = out / "game.nx"
        original = out / "original.hfs0"
        cli("pack", source, archive)
        # The recipe write is not published as a second generation: both
        # header pages hold the one selected generation.
        packed = archive.read_bytes()
        assert packed[:4096] == packed[4096:8192], "pack published more than one generation"
        rebuilt = out / "rebuilt"
        rebuilt.mkdir()
        cli("rebuild", archive, rebuilt)
        assert (rebuilt / source.name).read_bytes() == source.read_bytes()
        default_contents = cli("contents", archive).stdout.splitlines()
        assert any(line.startswith("@RESOURCE\t") for line in default_contents)
        assert any(line.startswith("@RECIPE\t") for line in default_contents)

        kept = out / "no-recipe.nx"
        omitted = out / "no-firmware.nx"
        cli("pack", source, kept, "--no-recipe", "--no-compress")
        cli("pack", source, omitted, "--no-recipe", "--omit-firmware", "--no-compress")
        kept_contents = cli("contents", kept).stdout.splitlines()
        omitted_contents = cli("contents", omitted).stdout.splitlines()
        assert any(line.startswith("@RESOURCE\t") for line in kept_contents)
        assert not any(line.startswith(("@RESOURCE\t", "@RECIPE\t")) for line in omitted_contents)
        assert [line for line in kept_contents if line.startswith("@SNAPSHOT\t")] == [
            line for line in omitted_contents if line.startswith("@SNAPSHOT\t")
        ]
        assert omitted.stat().st_size < kept.stat().st_size
        error = cli("rebuild", omitted, rebuilt, check=False)
        assert error.returncode != 0 and "no such rebuild recipe" in error.stderr
        cli("verify", omitted)

        rejected = out / "rejected.nx"
        error = cli("pack", source, rejected, "--omit-firmware", check=False)
        assert error.returncode != 0 and "requires --no-recipe" in error.stderr
        assert not rejected.exists()
        error = cli("contents", archive, "--omit-firmware", check=False)
        assert error.returncode != 0 and "native pack/add only" in error.stderr

        updated_with_firmware = out / "updated-with-firmware.nx"
        shutil.copyfile(archive, updated_with_firmware)
        cli("add", updated_with_firmware, patch)
        updated_default = cli("contents", updated_with_firmware).stdout.splitlines()
        assert len([line for line in updated_default if line.startswith("@RESOURCE\t")]) == 2
        cli("check-recipe", updated_with_firmware)

        # The same firmware twice is kept once: on add, and within one pack.
        same = out / "patch-same.dxci"
        added_same = out / "added-same.nx"
        shutil.copyfile(archive, added_same)
        cli("add", added_same, same)
        lines = cli("contents", added_same).stdout.splitlines()
        assert len([line for line in lines if line.startswith("@RESOURCE	")]) == 1
        assert len([line for line in lines if line.startswith("@RECIPE	")]) == 2
        cli("check-recipe", added_same)
        cli("verify", added_same)
        packed_same = out / "packed-same.nx"
        cli("pack", source, same, packed_same)
        lines = cli("contents", packed_same).stdout.splitlines()
        assert len([line for line in lines if line.startswith("@RESOURCE	")]) == 1
        cli("check-recipe", packed_same)
        cli("verify", packed_same)
        both = out / "both"
        both.mkdir()
        cli("rebuild", packed_same, both)
        assert (both / source.name).read_bytes() == source.read_bytes()
        assert (both / same.name).read_bytes() == same.read_bytes()

        updated = out / "updated.nx"
        shutil.copyfile(archive, updated)
        cli("add", updated, patch, "--omit-firmware", "--no-recipe")
        updated_contents = cli("contents", updated).stdout.splitlines()
        assert [line for line in updated_contents if line.startswith("@RESOURCE\t")] == [
            line for line in default_contents if line.startswith("@RESOURCE\t")
        ]
        assert len([line for line in updated_contents if line.startswith("@SNAPSHOT\t")]) == 2
        cli("check-recipe", updated)
        cli("verify", updated)
        print("PASS CLI firmware defaults, omission, retained resources on add and exact rebuild")

        probe = compile_reader(out, ROOT / "tests/conformance/source_resource_reader.cpp")
        validator = writer.load_validator()
        assert struct.unpack_from("<H", writer.current_header(omitted.read_bytes(), validator), 6)[0] == 2
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
