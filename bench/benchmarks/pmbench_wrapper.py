#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="pmbench wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/pmbench")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([ns.binary, *args])
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(result.stdout + "\n" + result.stderr))
    emit(result, metrics, tool="pmbench")
    return result.returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    match = re.search(r"Net average page latency.*?([0-9.]+)\s*us", text)
    if match:
        metrics["avg_page_latency_us"] = float(match.group(1))
    clk_match = re.search(r"Net average page latency.*?([0-9.]+)\s*clks", text)
    if clk_match:
        metrics["avg_page_latency_clocks"] = float(clk_match.group(1))
    return metrics


if __name__ == "__main__":
    raise SystemExit(main())
