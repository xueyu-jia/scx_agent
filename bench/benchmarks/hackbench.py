#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="hackbench wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/hackbench")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([ns.binary, *args])
    text = result.stdout + "\n" + result.stderr
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    parsed = parse_time(text)
    if parsed is not None:
        metrics["elapsed_time_sec"] = parsed
    throughput = parse_throughput(args, metrics["elapsed_time_sec"])
    if throughput is not None:
        metrics["throughput"] = throughput

    emit(result, metrics, tool="hackbench")
    return result.returncode


def parse_time(text: str) -> float | None:
    match = re.search(r"\bTime:\s*([0-9.]+)", text)
    return float(match.group(1)) if match else None


def parse_throughput(args: list[str], elapsed: float) -> float | None:
    if elapsed <= 0 or not args:
        return None
    try:
        groups = int(args[0])
    except ValueError:
        return None
    loops = 1
    if args and args[-1].isdigit():
        loops = int(args[-1])
    return groups * loops / elapsed


if __name__ == "__main__":
    raise SystemExit(main())
