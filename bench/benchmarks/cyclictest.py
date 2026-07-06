#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="cyclictest wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/cyclictest")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([ns.binary, *args])
    text = result.stdout + "\n" + result.stderr
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(text))
    emit(result, metrics, tool="cyclictest")
    return result.returncode


def parse_metrics(text: str) -> dict[str, float]:
    mins: list[float] = []
    avgs: list[float] = []
    maxes: list[float] = []
    for line in text.splitlines():
        match = re.search(
            r"Min:\s*([0-9.]+).*?Act:\s*([0-9.]+).*?Avg:\s*([0-9.]+).*?Max:\s*([0-9.]+)",
            line,
        )
        if not match:
            continue
        mins.append(float(match.group(1)))
        avgs.append(float(match.group(3)))
        maxes.append(float(match.group(4)))

    metrics: dict[str, float] = {}
    if mins:
        metrics["min_latency_us"] = min(mins)
    if avgs:
        metrics["avg_latency_us"] = sum(avgs) / len(avgs)
    if maxes:
        metrics["max_latency_us"] = max(maxes)
        metrics["p99_latency_us"] = max(maxes)
        metrics["p999_latency_us"] = max(maxes)
    return metrics


if __name__ == "__main__":
    raise SystemExit(main())
