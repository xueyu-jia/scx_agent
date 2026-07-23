#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

repo_root = Path(os.environ.get("SCX_BENCH_WORKDIR", Path(__file__).resolve().parents[2]))
sys.path.insert(0, str(repo_root))
from bench.benchmarks.schbench import (
    parse_metrics as parse_schbench_metrics,
    parse_section_metrics,
)
from bench.benchmarks.stress_ng import parse_metrics as parse_stress_metrics


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run schbench over a saturated batch workload")
    parser.add_argument("--schbench-binary", default="bench/workloads/bin/schbench")
    parser.add_argument("--stress-ng-binary", default="bench/workloads/bin/stress-ng")
    parser.add_argument("--batch-workers", type=int, default=2)
    parser.add_argument("--batch-seconds", type=int, default=12)
    parser.add_argument("--batch-warmup-seconds", type=float, default=1.0)
    parser.add_argument("schbench_args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    schbench_args = (
        ns.schbench_args[1:]
        if ns.schbench_args[:1] == ["--"]
        else ns.schbench_args
    )

    if ns.batch_workers < 1 or ns.batch_seconds < 1 or ns.batch_warmup_seconds < 0:
        parser.error("batch workers/seconds must be positive and warmup must be non-negative")

    batch_command = [
        ns.stress_ng_binary,
        "--cpu",
        str(ns.batch_workers),
        "--timeout",
        f"{ns.batch_seconds}s",
        "--metrics-brief",
    ]
    latency_command = [ns.schbench_binary, *schbench_args]

    batch = subprocess.Popen(
        batch_command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    time.sleep(ns.batch_warmup_seconds)

    started = time.monotonic()
    try:
        latency = subprocess.run(
            latency_command,
            check=False,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError as error:
        latency = subprocess.CompletedProcess(latency_command, 127, "", str(error))
    latency_elapsed = time.monotonic() - started

    try:
        batch_stdout, batch_stderr = batch.communicate(timeout=ns.batch_seconds + 5)
    except subprocess.TimeoutExpired:
        batch.kill()
        batch_stdout, batch_stderr = batch.communicate()
    batch_returncode = batch.returncode if batch.returncode is not None else 125

    out_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    raw_stdout = out_dir / "workload_stdout.log"
    raw_stderr = out_dir / "workload_stderr.log"
    raw_stdout.write_text(
        "[schbench]\n" + latency.stdout + "\n[stress-ng]\n" + batch_stdout,
        encoding="utf-8",
    )
    raw_stderr.write_text(
        "[schbench]\n" + latency.stderr + "\n[stress-ng]\n" + batch_stderr,
        encoding="utf-8",
    )

    metrics = {"elapsed_time_sec": latency_elapsed}
    latency_text = latency.stdout + "\n" + latency.stderr
    metrics.update(parse_schbench_metrics(latency_text))
    wakeup_metrics = parse_section_metrics(
        latency_text,
        "Wakeup Latencies percentiles",
        "Request Latencies percentiles",
        "wakeup",
    )
    request_metrics = parse_section_metrics(
        latency_text,
        "Request Latencies percentiles",
        "RPS percentiles",
        "request",
    )
    metrics.update(wakeup_metrics)
    metrics.update(request_metrics)

    # The generic parser sees the later RPS percentile section too. Keep the
    # legacy unprefixed latency fields, but make them unambiguously requests.
    metrics.update(
        {
            key.removeprefix("request_"): value
            for key, value in request_metrics.items()
        }
    )
    batch_metrics = parse_stress_metrics(batch_stdout + "\n" + batch_stderr)
    if "throughput" in batch_metrics:
        metrics["batch_throughput"] = batch_metrics["throughput"]

    returncode = latency.returncode or batch_returncode
    print(
        json.dumps(
            {
                "metrics": metrics,
                "metadata": {
                    "tool": "mixed-class",
                    "latency_command": latency_command,
                    "batch_command": batch_command,
                    "latency_returncode": latency.returncode,
                    "batch_returncode": batch_returncode,
                    "returncode": returncode,
                },
                "raw": {
                    "stdout_path": str(raw_stdout),
                    "stderr_path": str(raw_stderr),
                },
            },
            sort_keys=True,
        )
    )
    return returncode
if __name__ == "__main__":
    raise SystemExit(main())
