"""Build the fixtures twice and check both runs give the same bytes."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures"
BUILDER = ROOT / "tests" / "conformance" / "build_fixtures.py"


def hashes() -> dict[str, bytes]:
    return {path.name: path.read_bytes() for path in sorted(FIXTURES.glob("*.nx"))}


def main() -> int:
    generated: list[dict[str, bytes]] = []
    for _ in range(2):
        result = subprocess.run([sys.executable, str(BUILDER)], cwd=ROOT, text=True, capture_output=True)
        if result.returncode:
            print(result.stdout + result.stderr, file=sys.stderr)
            return result.returncode
        generated.append(hashes())
    if generated[0] != generated[1]:
        names = sorted(set(generated[0]) | set(generated[1]))
        changed = [name for name in names if generated[0].get(name) != generated[1].get(name)]
        print("fixture generation is not byte-reproducible: " + ", ".join(changed), file=sys.stderr)
        return 1
    print(f"PASS byte-for-byte fixture reproducibility: {len(generated[0])} archives")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
