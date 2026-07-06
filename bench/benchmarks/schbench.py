#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="schbench wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/schbench")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([ns.binary, *args])
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(result.stdout + "\n" + result.stderr))
    emit(result, metrics, tool="schbench")
    return result.returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    for percentile, value in re.findall(r"([0-9.]+)th:\s*([0-9.]+)", text):
        key = percentile_key(percentile)
        if key:
            metrics[key] = float(value)
    rps = re.search(r"\b(?:RPS|requests/sec|Requests/sec):\s*([0-9.]+)", text, re.IGNORECASE)
    if rps:
        metrics["throughput"] = float(rps.group(1))
    return metrics


def percentile_key(percentile: str) -> str | None:
    normalized = percentile.rstrip("0").rstrip(".")
    mapping = {
        "50": "p50_latency_us",
        "90": "p90_latency_us",
        "95": "p95_latency_us",
        "99": "p99_latency_us",
        "99.5": "p995_latency_us",
        "99.9": "p999_latency_us",
    }
    return mapping.get(normalized)


if __name__ == "__main__":
    raise SystemExit(main())
