#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


BINARIES = {
    "create_threads": "bench/workloads/bin/create_threads",
    "create_processes": "bench/workloads/bin/create_processes",
    "launch_programs": "bench/workloads/bin/launch_programs",
    "create_files": "bench/workloads/bin/create_files",
    "mem_alloc": "bench/workloads/bin/mem_alloc",
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="OSBench wrapper")
    parser.add_argument("test", choices=sorted(BINARIES))
    parser.add_argument("--binary", default=None)
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    binary = ns.binary or BINARIES[ns.test]
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([binary, *args])
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(result.stdout, ns.test))
    emit(result, metrics, tool=f"osbench/{ns.test}")
    return result.returncode


def parse_metrics(text: str, test: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    name_map = {
        "create_threads": "us_per_thread",
        "create_processes": "us_per_process",
        "launch_programs": "us_per_launch",
        "create_files": "us_per_file",
        "mem_alloc": "us_per_alloc",
    }
    key = name_map.get(test, "latency_us")
    match = re.search(r"([0-9.]+)\s*us", text)
    if match:
        metrics[key] = float(match.group(1))
    return metrics


if __name__ == "__main__":
    raise SystemExit(main())
