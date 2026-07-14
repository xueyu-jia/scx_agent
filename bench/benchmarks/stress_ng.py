#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="stress-ng wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/stress-ng")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([ns.binary, *args])
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(
        parse_metrics(
            result.stdout + "\n" + result.stderr,
            elapsed_time_sec=result.elapsed_time_sec,
        )
    )
    emit(result, metrics, tool="stress-ng")
    return result.returncode


def parse_metrics(text: str, elapsed_time_sec: float | None = None) -> dict[str, float]:
    records: list[tuple[float, float, float, float]] = []
    for line in text.splitlines():
        if "stress-ng: metrc:" not in line:
            continue
        parts = line.split()
        if len(parts) < 10 or parts[3] != "cpu":
            continue
        try:
            records.append(
                (
                    float(parts[4]),
                    float(parts[5]),
                    float(parts[-2]),
                    float(parts[-1]),
                )
            )
        except ValueError:
            continue

    if records:
        bogo_ops = sum(record[0] for record in records)
        reported_throughput = sum(record[2] for record in records)
        metrics = {
            "bogo_ops": bogo_ops,
            "stress_elapsed_time_sec": max(record[1] for record in records),
            "stress_reported_throughput": reported_throughput,
            "cpu_time_throughput": sum(record[3] for record in records),
        }
        if elapsed_time_sec and elapsed_time_sec > 0:
            metrics["throughput"] = bogo_ops / elapsed_time_sec
        else:
            metrics["throughput"] = reported_throughput
        return metrics

    values = [float(v) for v in re.findall(r"([0-9.]+)\s+bogo ops/s", text)]
    if not values:
        values = [float(v) for v in re.findall(r"([0-9.]+)\s+real time\s+bogo ops/s", text)]
    return {"throughput": sum(values)} if values else {}


if __name__ == "__main__":
    raise SystemExit(main())
