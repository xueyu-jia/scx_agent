#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


IPC_BINS = {
    "pipe": "bench/workloads/bin/pipe",
    "fifo": "bench/workloads/bin/fifo",
    "socketpair": "bench/workloads/bin/socketpair",
    "tcp": "bench/workloads/bin/tcp",
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="IPC benchmark wrapper")
    parser.add_argument("type", choices=sorted(IPC_BINS))
    parser.add_argument("--binary", default=None)
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    binary = ns.binary or IPC_BINS[ns.type]
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([binary, *args])
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(result.stdout))
    emit(result, metrics, tool=f"ipc_benchmark/{ns.type}")
    return result.returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    match = re.search(r"([0-9.]+)msg/s", text)
    if match:
        metrics["throughput"] = float(match.group(1))
    bw_match = re.search(r"([0-9.]+)MB/s", text)
    if bw_match:
        metrics["bandwidth_mb_per_sec"] = float(bw_match.group(1))
    return metrics


if __name__ == "__main__":
    raise SystemExit(main())
