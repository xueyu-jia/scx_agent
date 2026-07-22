#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="sysbench fileio wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/sysbench")
    parser.add_argument("--file-total-size", default="1G")
    parser.add_argument("--file-test-mode", default="rndrw")
    parser.add_argument("--threads", default="4")
    parser.add_argument("--time", default="30")
    parser.add_argument("--keep-files", action="store_true")
    ns = parser.parse_args(argv)
    binary = Path(ns.binary)
    if not binary.is_absolute():
        binary = Path.cwd() / binary

    out_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    stdout_log = out_dir / "workload_stdout.log"
    stderr_log = out_dir / "workload_stderr.log"
    stdout_parts: list[str] = []
    stderr_parts: list[str] = []
    started = time.monotonic()
    returncode = 0

    with tempfile.TemporaryDirectory(prefix="scx-sysbench-fileio-") as temp_dir:
        workdir = Path(temp_dir)
        base = [
            str(binary),
            "fileio",
            f"--file-total-size={ns.file_total_size}",
            f"--file-test-mode={ns.file_test_mode}",
            f"--threads={ns.threads}",
            "--file-fsync-freq=0",
            "--file-fsync-all=off",
            "--file-fsync-end=off",
        ]
        try:
            prepare = subprocess.run(base + ["prepare"], cwd=workdir, capture_output=True, text=True)
            stdout_parts.append(prepare.stdout)
            stderr_parts.append(prepare.stderr)
            if prepare.returncode != 0:
                returncode = prepare.returncode
                metrics: dict[str, Any] = {"elapsed_time_sec": time.monotonic() - started}
            else:
                run = subprocess.run(
                    base + [f"--time={ns.time}", "run"],
                    cwd=workdir,
                    capture_output=True,
                    text=True,
                )
                returncode = run.returncode
                stdout_parts.append(run.stdout)
                stderr_parts.append(run.stderr)
                metrics = {"elapsed_time_sec": time.monotonic() - started}
                metrics.update(parse_metrics(run.stdout + "\n" + run.stderr))
                if "throughput_mb_per_sec" in metrics:
                    metrics["throughput"] = metrics["throughput_mb_per_sec"]
            if not ns.keep_files:
                cleanup = subprocess.run(base + ["cleanup"], cwd=workdir, capture_output=True, text=True)
                stdout_parts.append(cleanup.stdout)
                stderr_parts.append(cleanup.stderr)
        except FileNotFoundError as exc:
            returncode = 127
            metrics = {"elapsed_time_sec": time.monotonic() - started}
            stderr_parts.append(str(exc))

    stdout_log.write_text("\n".join(part for part in stdout_parts if part), encoding="utf-8")
    stderr_log.write_text("\n".join(part for part in stderr_parts if part), encoding="utf-8")
    print(
        json.dumps(
            {
                "metrics": metrics,
                "metadata": {"tool": "sysbench_fileio", "returncode": returncode},
                "raw": {"stdout_path": str(stdout_log), "stderr_path": str(stderr_log)},
            },
            sort_keys=True,
        )
    )
    return returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    patterns = (
        (r"reads/s:\s*([\d.]+)", "reads_per_sec"),
        (r"writes/s:\s*([\d.]+)", "writes_per_sec"),
        (r"fsyncs/s:\s*([\d.]+)", "fsyncs_per_sec"),
        (r"read, MiB/s:\s*([\d.]+)", "read_mib_per_sec"),
        (r"written, MiB/s:\s*([\d.]+)", "written_mib_per_sec"),
        (r"events/s \(eps\):\s*([\d.]+)", "events_per_sec"),
        (r"avg:\s*([\d.]+)", "avg_latency_ms"),
        (r"95th percentile:\s*([\d.]+)", "p95_latency_ms"),
    )
    for pattern, key in patterns:
        match = re.search(pattern, text)
        if match:
            metrics[key] = float(match.group(1))
    if "read_mib_per_sec" in metrics or "written_mib_per_sec" in metrics:
        metrics["throughput_mb_per_sec"] = metrics.get("read_mib_per_sec", 0.0) + metrics.get(
            "written_mib_per_sec",
            0.0,
        )
    return metrics


if __name__ == "__main__":
    raise SystemExit(main())
