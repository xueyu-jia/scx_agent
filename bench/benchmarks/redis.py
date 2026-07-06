#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import os
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
                    "--csv",
                ]
            )
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_csv(result.stdout))
    emit(result, metrics, tool="redis")
    return result.returncode


def parse_csv(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    values = []
    for row in csv.reader(text.splitlines()):
        if len(row) < 2:
            continue
        name = row[0].strip().lower().replace(" ", "_")
        try:
            value = float(row[1])
        except ValueError:
            continue
        metrics[f"{name}_qps"] = value
        values.append(value)
    if values:
        metrics["throughput"] = sum(values)
    return metrics


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
