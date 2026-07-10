#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="BenchmarkMutex wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/BenchmarkMutex")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args
    default_args = ["--benchmark_color=false", "--benchmark_min_time=1s"]
    if not args:
        args = default_args

    result = run_command([ns.binary, *args])
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(result.stdout))
    emit(result, metrics, tool="BenchmarkMutex")
    return result.returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    for line in text.splitlines():
        simple = re.sub(r"<[^>]+>", "_", line)
        parts = simple.split()
        if len(parts) >= 4 and "benchmark" in parts[0].lower():
            try:
                metrics[f"{parts[0]}_ns"] = float(parts[1])
            except ValueError:
                pass
    return metrics


if __name__ == "__main__":
    raise SystemExit(main())
