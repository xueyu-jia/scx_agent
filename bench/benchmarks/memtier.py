#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Redis + memtier_benchmark wrapper")
    parser.add_argument("--server", default="bench/workloads/bin/redis-server")
    parser.add_argument("--cli", default="bench/workloads/bin/redis-cli")
    parser.add_argument("--memtier", default="bench/workloads/bin/memtier_benchmark")
    parser.add_argument("--port", default="6379")
    parser.add_argument("--clients", default="50")
    parser.add_argument("--threads", default="4")
    parser.add_argument("--requests", default="100000")
    parser.add_argument("--data-size", default="32")
    parser.add_argument("--pipeline", default="1")
    parser.add_argument("--ratio", default="1:10")
    parser.add_argument("--key-pattern", default="R:R")
    parser.add_argument("--key-maximum", default="1000000")
    parser.add_argument("--random-data", action="store_true")
    ns = parser.parse_args(argv)

    out_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    stdout_log = out_dir / "workload_stdout.log"
    stderr_log = out_dir / "workload_stderr.log"
    stdout_parts: list[str] = []
    stderr_parts: list[str] = []
    server: subprocess.Popen[str] | None = None
    started = time.monotonic()
    returncode = 0

    with tempfile.TemporaryDirectory(prefix="scx-memtier-") as temp_dir:
        config_path = Path(temp_dir) / "redis.conf"
        config_path.write_text(
            "\n".join(
                [
                    f"port {ns.port}",
                    "bind 127.0.0.1",
                    "daemonize no",
                    "save \"\"",
                    "appendonly no",
                    f"dir {temp_dir}",
                    "loglevel warning",
                ]
            ),
            encoding="utf-8",
        )
        try:
            server = subprocess.Popen(
                [ns.server, str(config_path)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            _wait_for_redis(ns.cli, ns.port)
            command = [
                ns.memtier,
                "-p",
                ns.port,
                "-c",
                ns.clients,
                "-t",
                ns.threads,
                "-d",
                ns.data_size,
                "--pipeline",
                ns.pipeline,
                "--ratio",
                ns.ratio,
                "--key-pattern",
                ns.key_pattern,
                "--key-maximum",
                ns.key_maximum,
                "-n",
                ns.requests,
                "--hide-histogram",
            ]
            if ns.random_data:
                command.append("--random-data")
            completed = subprocess.run(command, capture_output=True, text=True)
            returncode = completed.returncode
            stdout_parts.append(completed.stdout)
            stderr_parts.append(completed.stderr)
            metrics = {"elapsed_time_sec": time.monotonic() - started}
            metrics.update(parse_metrics(completed.stdout + "\n" + completed.stderr))
            if "ops_per_second" in metrics:
                metrics["throughput"] = metrics["ops_per_second"]
        except FileNotFoundError as exc:
            returncode = 127
            metrics = {"elapsed_time_sec": time.monotonic() - started}
            stderr_parts.append(str(exc))
        except Exception as exc:
            returncode = 1
            metrics = {"elapsed_time_sec": time.monotonic() - started}
            stderr_parts.append(str(exc))
        finally:
            if server is not None:
                _stop_redis(server, ns.cli, ns.port)
                server_out, server_err = server.communicate(timeout=2)
                stdout_parts.append(server_out)
                stderr_parts.append(server_err)

    stdout_log.write_text("\n".join(part for part in stdout_parts if part), encoding="utf-8")
    stderr_log.write_text("\n".join(part for part in stderr_parts if part), encoding="utf-8")
    print(
        json.dumps(
            {
                "metrics": metrics,
                "metadata": {"tool": "memtier", "returncode": returncode},
                "raw": {"stdout_path": str(stdout_log), "stderr_path": str(stderr_log)},
            },
            sort_keys=True,
        )
    )
    return returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    for line in text.splitlines():
        parts = line.strip().split()
        if not parts:
            continue
        if parts[0] == "Totals" and len(parts) >= 8:
            _add_totals(metrics, "", parts)
        elif parts[0] == "Gets" and len(parts) >= 8:
            _add_totals(metrics, "gets_", parts)
        elif parts[0] == "Sets" and len(parts) >= 8:
            _add_totals(metrics, "sets_", parts)
    return metrics


def _add_totals(metrics: dict[str, float], prefix: str, parts: list[str]) -> None:
    fields = (
        ("ops_per_second", 1),
        ("hits_per_second", 2),
        ("misses_per_second", 3),
        ("avg_latency_ms", 4),
        ("p50_latency_ms", 5),
        ("p99_latency_ms", 6),
        ("p999_latency_ms", 7),
        ("bandwidth_kb_sec", 8),
    )
    for name, index in fields:
        if index < len(parts):
            try:
                metrics[f"{prefix}{name}"] = float(parts[index])
            except ValueError:
                pass


def _wait_for_redis(cli: str, port: str) -> None:
    for _attempt in range(40):
        result = subprocess.run(
            [cli, "-p", port, "PING"],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0 and "PONG" in result.stdout:
            return
        time.sleep(0.25)
    raise RuntimeError("redis-server did not become ready")


def _stop_redis(server: subprocess.Popen[str], cli: str, port: str) -> None:
    try:
        subprocess.run([cli, "-p", port, "SHUTDOWN", "NOSAVE"], capture_output=True, text=True)
    except FileNotFoundError:
        pass
    try:
        server.wait(timeout=2)
    except subprocess.TimeoutExpired:
        server.send_signal(signal.SIGTERM)
        try:
            server.wait(timeout=2)
        except subprocess.TimeoutExpired:
            server.kill()


if __name__ == "__main__":
    raise SystemExit(main())
