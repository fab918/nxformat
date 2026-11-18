"""Copy the reader headers to dist/."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HEADERS = ("nx_reader.hpp", "nx_session.hpp", "nx_disk_source.hpp", "nx_mapped_views.hpp", "nx_native_sha256.hpp", "nx_read_trace.hpp")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="fail unless the distributed header is current")
    args = parser.parse_args()
    for name in HEADERS:
        source = (ROOT / "reader" / name).read_bytes()
        output = ROOT / "dist" / name
        if args.check:
            if not output.is_file() or output.read_bytes() != source:
                print(f"dist/{name} is stale; run python reader/generate_header.py", file=sys.stderr)
                return 1
        else:
            output.parent.mkdir(parents=True, exist_ok=True)
            output.write_bytes(source)
            print(f"generated {output}")
    if args.check:
        print("PASS generated reader headers are current")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
