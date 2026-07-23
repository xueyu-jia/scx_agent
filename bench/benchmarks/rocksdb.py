#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="RocksDB db_bench wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/db_bench")
    parser.add_argument("--db", default=None)
    parser.add_argument("--benchmarks", default="fillrandom,readrandom")
    parser.add_argument("--num", default="500000")
    parser.add_argument("--reads", default=None)
    parser.add_argument("--threads", default=None)
    parser.add_argument("--value-size", default="100")
    parser.add_argument("--compression-type", default="none")
    parser.add_argument("--keep-db", action="store_true")
    parser.add_argument("extra_args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)

    out_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    stdout_log = out_dir / "workload_stdout.log"
    stderr_log = out_dir / "workload_stderr.log"
    db_path = Path(ns.db) if ns.db else Path(tempfile.mkdtemp(prefix="scx-rocksdb-"))

    command = [
        ns.binary,
        f"--db={db_path}",
        "--disable_wal=true",
        "--statistics=false",
        "--histogram=false",
        f"--benchmarks={ns.benchmarks}",
        f"--num={ns.num}",
        f"--value_size={ns.value_size}",
    ]
    if ns.reads is not None:
        command.append(f"--reads={ns.reads}")
    if ns.threads is not None:
        command.append(f"--threads={ns.threads}")
    if ns.compression_type is not None:
        command.append(f"--compression_type={ns.compression_type}")
    extra = ns.extra_args[1:] if ns.extra_args and ns.extra_args[0] == "--" else ns.extra_args
    command.extend(extra)

    started = time.monotonic()
    try:
        if db_path.exists():
            shutil.rmtree(db_path)
        db_path.mkdir(parents=True, exist_ok=True)
        completed = subprocess.run(command, capture_output=True, text=True)
    except FileNotFoundError as exc:
        completed = subprocess.CompletedProcess(command, 127, "", str(exc))
    finally:
        if not ns.keep_db:
            shutil.rmtree(db_path, ignore_errors=True)
    elapsed = time.monotonic() - started

    stdout_log.write_text(completed.stdout, encoding="utf-8")
    stderr_log.write_text(completed.stderr, encoding="utf-8")
    metrics = {"elapsed_time_sec": elapsed}
    metrics.update(parse_metrics(completed.stdout + "\n" + completed.stderr))
    if "overall_ops_per_sec" in metrics:
        metrics["throughput"] = metrics["overall_ops_per_sec"]
    print(_payload(metrics, command, completed.returncode, stdout_log, stderr_log, "rocksdb"))
    return completed.returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    pattern = re.compile(r"([A-Za-z0-9_]+)\s*:\s*([\d.]+)\s*micros/op\s*(\d+)\s*ops/sec")
    for match in pattern.finditer(text):
        operation = match.group(1).lower()
        metrics[f"{operation}_micros_per_op"] = float(match.group(2))
        metrics[f"{operation}_ops_per_sec"] = float(match.group(3))

    throughput_matches = re.findall(r"(\d+)\s+ops/sec", text)
    if throughput_matches:
        metrics["overall_ops_per_sec"] = float(throughput_matches[-1])

    size_match = re.search(r"DB size:\s*([\d.]+)\s*([KMGT]?B)", text)
    if size_match:
        metrics["db_size_bytes"] = _bytes(float(size_match.group(1)), size_match.group(2))

    for label, key in (
        ("Write amplification", "write_amplification"),
        ("Read amplification", "read_amplification"),
    ):
        match = re.search(rf"{re.escape(label)}:\s*([\d.]+)", text)
        if match:
            metrics[key] = float(match.group(1))
    return metrics


def _bytes(value: float, unit: str) -> float:
    multipliers = {"B": 1, "KB": 1024, "MB": 1024**2, "GB": 1024**3, "TB": 1024**4}
    return value * multipliers.get(unit, 1)


def _payload(
    metrics: dict[str, Any],
    command: list[str],
    returncode: int,
    stdout_log: Path,
    stderr_log: Path,
    tool: str,
) -> str:
    return json.dumps(
        {
            "metrics": metrics,
            "metadata": {"tool": tool, "command": command, "returncode": returncode},
            "raw": {"stdout_path": str(stdout_log), "stderr_path": str(stderr_log)},
        },
        sort_keys=True,
    )


if __name__ == "__main__":
    raise SystemExit(main())
