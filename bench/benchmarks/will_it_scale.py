#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="will-it-scale wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/will-it-scale")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([ns.binary, *args])
    text = result.stdout + "\n" + result.stderr
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    parsed = parse_csv_metrics(text)
    if parsed:
        metrics.update(parsed)
    else:
        throughput = parse_throughput(text)
        if throughput is not None:
            metrics["throughput"] = throughput
    emit(result, metrics, tool="will-it-scale")
    return result.returncode


def parse_throughput(text: str) -> float | None:
    csv_metrics = parse_csv_metrics(text)
    if csv_metrics:
        return csv_metrics["throughput"]

    named = re.search(r"(?:throughput|ops/sec|per sec|per_second)\D+([0-9.]+)", text, re.I)
    if named:
        return float(named.group(1))

    numeric_rows: list[list[float]] = []
    for line in text.splitlines():
        values = [float(v) for v in re.findall(r"[-+]?(?:\d+\.\d+|\d+)", line)]
        if len(values) >= 2:
            numeric_rows.append(values)
    if numeric_rows:
        return numeric_rows[-1][-1]
    return None


def parse_csv_metrics(text: str) -> dict[str, float]:
    rows = []
    for row in csv.DictReader(line for line in text.splitlines() if "," in line):
        try:
            rows.append(
                {
                    "tasks": float(row["tasks"]),
                    "processes": float(row["processes"]),
                    "threads": float(row["threads"]),
                    "linear": float(row["linear"]),
                }
            )
        except (KeyError, TypeError, ValueError):
            continue
    rows = [row for row in rows if row["tasks"] > 0]
    if not rows:
        return {}
    final = rows[-1]
    max_processes = max(row["processes"] for row in rows)
    max_threads = max(row["threads"] for row in rows)
    return {
        "throughput": max(max_processes, max_threads),
        "processes_throughput": final["processes"],
        "threads_throughput": final["threads"],
        "max_processes_throughput": max_processes,
        "max_threads_throughput": max_threads,
        "linear_target": final["linear"],
    }


if __name__ == "__main__":
    raise SystemExit(main())
