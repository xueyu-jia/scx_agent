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
    metrics.update(parse_metrics(result.stdout + "\n" + result.stderr))
    emit(result, metrics, tool="stress-ng")
    return result.returncode


def parse_metrics(text: str) -> dict[str, float]:
    values = [float(v) for v in re.findall(r"([0-9.]+)\s+bogo ops/s", text)]
    if not values:
        values = [float(v) for v in re.findall(r"([0-9.]+)\s+real time\s+bogo ops/s", text)]
    if not values:
        for line in text.splitlines():
            if "stress-ng: metrc:" not in line:
                continue
            parts = line.split()
            if len(parts) >= 10 and parts[3] == "cpu":
                try:
                    values.append(float(parts[-2]))
                except ValueError:
                    pass
    return {"throughput": sum(values)} if values else {}


if __name__ == "__main__":
    raise SystemExit(main())
