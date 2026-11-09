"""Run partial_read_benchmark over a matrix and write a CSV.

usage: python benchmarks/run_partial_read_benchmark.py <archives-dir> <out.csv>
         <name>=<benchmark-exe> [<name>=<benchmark-exe> ...]
Each executable is partial_read_benchmark.cpp built against one reader
version (add -DNX_NO_PARTIAL for 1.0.0 readers, whose read() returns a Status). Archives come
from partial_read_archives.py. RAW blocks are never cached, so RAW runs warm only.
"""
from __future__ import annotations

import csv
import subprocess
import sys
from pathlib import Path

FIELDS = ["build", "archive", "mode", "workload", "threads", "state", "reads", "p50_us", "p95_us", "p99_us",
          "p999_us", "max_us", "mib_s", "allocs_per_read", "alloc_bytes_per_read", "peak_heap_bytes",
          "peak_private_bytes", "decompressions", "prefetch_submitted"]


def main() -> int:
    archives, out = Path(sys.argv[1]), Path(sys.argv[2])
    builds = [item.split("=", 1) for item in sys.argv[3:]]
    with out.open("w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(FIELDS)
        for archive in ("raw_64k", "raw_256k", "zstd_64k", "zstd_256k"):
            for state in (("warm",) if archive.startswith("raw") else ("warm", "cold")):
                for workload in ("rand4k", "rand256k", "rand4m", "seq1m"):
                    for threads in (1, 2, 4, 8):
                        for name, exe in builds:
                            modes = ("atomic", "lichii") if name == "old" else ("atomic", "lichii", "partial")
                            for mode in modes:
                                result = subprocess.run([exe, str(archives / f"{archive}.nx"), mode, workload,
                                                         str(threads), state], capture_output=True, text=True)
                                line = next((l for l in result.stdout.splitlines() if l.startswith("RESULT,")), None)
                                if result.returncode or not line:
                                    print(f"FAIL {name} {archive} {mode} {workload} {threads} {state}: {result.stderr.strip()}")
                                    continue
                                row = line.split(",")[1:]
                                writer.writerow([name, *row])
                                handle.flush()
                                print(name, ",".join(row), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
