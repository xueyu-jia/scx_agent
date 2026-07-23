#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
from pathlib import Path
from typing import Any

from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="double_bandwidth memory wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/double_bandwidth")
    parser.add_argument("--buffer-size", default="1073741824")
    parser.add_argument("--threads", default="0")
    parser.add_argument("--duration", default="5")
    parser.add_argument("--read-ratio", default="0.5")
    parser.add_argument("--sequential", action="store_true")
    parser.add_argument("extra_args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)

    command = [
        ns.binary,
        "--buffer-size",
        ns.buffer_size,
        "--threads",
        _threads(ns.threads),
        "--duration",
        ns.duration,
        "--read-ratio",
        ns.read_ratio,
        "--json",
        "--sequential" if ns.sequential else "--random",
    ]
    extra = ns.extra_args[1:] if ns.extra_args and ns.extra_args[0] == "--" else ns.extra_args
    command.extend(extra)
    result = run_command(command)
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(result.stdout + "\n" + result.stderr))
    emit(result, metrics, tool="memory_bandwidth")
    return result.returncode


def parse_metrics(text: str) -> dict[str, Any]:
    stripped = text.strip()
    if stripped:
        for candidate in (stripped, stripped.splitlines()[-1]):
            try:
                data = json.loads(candidate)
            except json.JSONDecodeError:
                continue
            if isinstance(data, dict):
                metrics = dict(data)
                if "total_bandwidth_mbps" in metrics:
                    metrics["bandwidth_mbps"] = metrics["total_bandwidth_mbps"]
                    metrics["throughput"] = metrics["total_bandwidth_mbps"]
                if "test_duration" in metrics:
                    metrics["execution_time_sec"] = metrics["test_duration"]
                return metrics

    metrics: dict[str, Any] = {}
    bandwidth = re.search(r"Total [Bb]andwidth[:\s]+([\d.]+)\s*(MB/s|GB/s)", text)
    if bandwidth:
        value = float(bandwidth.group(1))
        if bandwidth.group(2).upper().startswith("GB"):
            value *= 1024.0
        metrics["bandwidth_mbps"] = value
        metrics["throughput"] = value
    duration = re.search(r"(?:Test )?[Dd]uration[:\s]+([\d.]+)\s*(ms|s|seconds)?", text)
    if duration:
        value = float(duration.group(1))
        unit = duration.group(2) or "s"
        metrics["execution_time_sec"] = value / 1000.0 if unit == "ms" else value
    return metrics


def _threads(value: str) -> str:
    if value != "0":
        return value
    count = os.cpu_count() or 1
    return str(count)


if __name__ == "__main__":
    raise SystemExit(main())
