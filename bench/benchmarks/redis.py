#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="redis server + redis-benchmark wrapper")
    parser.add_argument("--server", default="bench/workloads/bin/redis-server")
    parser.add_argument("--benchmark", default="bench/workloads/bin/redis-benchmark")
    parser.add_argument("--requests", default="10000")
    parser.add_argument("--clients", default="50")
    parser.add_argument("--tests", default="get,set")
    ns = parser.parse_args(argv)

    port = free_port()
    with tempfile.TemporaryDirectory(prefix="scx-redis-") as tmp:
        proc = subprocess.Popen(
            [
                ns.server,
                "--port",
                str(port),
                "--save",
                "",
                "--appendonly",
                "no",
                "--dir",
                tmp,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            wait_for_port(port)
            result = run_command(
                [
                    ns.benchmark,
                    "-p",
                    str(port),
                    "-n",
                    ns.requests,
                    "-c",
                    ns.clients,
                    "-t",
                    ns.tests,
                    "--precision",
                    "3",
                ]
            )
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_output(result.stdout))
    emit(result, metrics, tool="redis")
    return result.returncode


def parse_output(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    throughputs: list[float] = []
    p99_values: list[float] = []
    p999_values: list[float] = []
    parts = re.split(
        r"^[ \t\r]*======\s+(.+?)\s+======[ \t\r]*$",
        text,
        flags=re.MULTILINE,
    )
    for index in range(1, len(parts), 2):
        name = parts[index].strip().lower().replace(" ", "_")
        section = parts[index + 1]
        throughput = _number(section, r"throughput summary:\s*([0-9.]+)")
        if throughput is not None:
            metrics[f"{name}_qps"] = throughput
            throughputs.append(throughput)

        summary = re.search(
            r"latency summary \(msec\):.*?\n\s*avg\s+min\s+p50\s+p95\s+p99\s+max\s*\n"
            r"\s*([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)",
            section,
            flags=re.DOTALL,
        )
        if summary:
            for metric, value in zip(
                ("avg", "min", "p50", "p95", "p99", "max"),
                summary.groups(),
                strict=True,
            ):
                metrics[f"{name}_{metric}_latency_us"] = float(value) * 1000.0
            p99_values.append(float(summary.group(5)) * 1000.0)

        percentiles = [
            (float(percentile), float(milliseconds) * 1000.0)
            for percentile, milliseconds in re.findall(
                r"^[ \t]*([0-9.]+)% <= ([0-9.]+) milliseconds",
                section,
                flags=re.MULTILINE,
            )
        ]
        p999 = next((latency for percentile, latency in percentiles if percentile >= 99.9), None)
        if p999 is not None:
            metrics[f"{name}_p999_latency_us"] = p999
            p999_values.append(p999)

    if throughputs:
        metrics["throughput"] = sum(throughputs)
    if p99_values:
        metrics["p99_latency_us"] = max(p99_values)
    if p999_values:
        metrics["p999_latency_us"] = max(p999_values)
    return metrics


def _number(text: str, pattern: str) -> float | None:
    match = re.search(pattern, text, flags=re.IGNORECASE)
    return float(match.group(1)) if match else None


def free_port() -> int:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def wait_for_port(port: int) -> None:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("redis-server did not start")


if __name__ == "__main__":
    raise SystemExit(main())
