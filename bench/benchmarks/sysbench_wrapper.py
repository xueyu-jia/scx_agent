#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="sysbench wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/sysbench")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([ns.binary, *args])
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(result.stdout + "\n" + result.stderr))
    emit(result, metrics, tool="sysbench")
    return result.returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    eps_match = re.search(r"events\s+per\s+second:\s*([0-9.]+)", text)
    if eps_match:
        metrics["throughput"] = float(eps_match.group(1))
    latency_match = re.search(r"avg:\s*([0-9.]+)ms", text)
    if latency_match:
        metrics["avg_latency_ms"] = float(latency_match.group(1))
    p95_match = re.search(r"95th\s+percentile:\s*([0-9.]+)ms", text)
    if p95_match:
        metrics["p95_latency_ms"] = float(p95_match.group(1))
    return metrics


if __name__ == "__main__":
    raise SystemExit(main())
