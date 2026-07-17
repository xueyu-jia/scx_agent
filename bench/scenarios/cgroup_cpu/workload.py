#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path.cwd()))
from common import (  # noqa: E402
    CgroupCpuError,
    CgroupCpuScope,
    cgroup_exec_argv,
    read_cpu_stat,
    read_members,
    scope_state,
    wait_for_empty,
)
from bench.benchmarks.stress_ng import parse_metrics as parse_stress_metrics  # noqa: E402


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Measure CPU service across two existing cgroups")
    parser.add_argument("--root", default="/sys/fs/cgroup/scx-bench")
    parser.add_argument("--stress-ng-binary", default="bench/workloads/bin/stress-ng")
    parser.add_argument("--workers", type=int, default=2)
    parser.add_argument("--duration-seconds", type=int, default=10)
    args = parser.parse_args(argv)
    if args.workers < 1 or args.duration_seconds < 1:
        parser.error("workers and duration-seconds must be positive")

    scope = CgroupCpuScope.from_root(args.root)
    state_before = scope_state(scope)
    if read_members(scope.target) or read_members(scope.neighbor):
        raise CgroupCpuError("held-out measurement requires empty target and neighbor cgroups")

    target_cpu_before = read_cpu_stat(scope.target)["usage_usec"]
    neighbor_cpu_before = read_cpu_stat(scope.neighbor)["usage_usec"]
    command = [
        args.stress_ng_binary,
        "--cpu",
        str(args.workers),
        "--timeout",
        f"{args.duration_seconds}s",
        "--metrics-brief",
    ]
    started = time.monotonic()
    target = subprocess.Popen(
        cgroup_exec_argv(scope.target, command),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    neighbor = subprocess.Popen(
        cgroup_exec_argv(scope.neighbor, command),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    target_stdout, target_stderr = target.communicate()
    neighbor_stdout, neighbor_stderr = neighbor.communicate()
    elapsed = time.monotonic() - started
    wait_for_empty(scope)

    target_cpu = read_cpu_stat(scope.target)["usage_usec"] - target_cpu_before
    neighbor_cpu = read_cpu_stat(scope.neighbor)["usage_usec"] - neighbor_cpu_before
    target_metrics = parse_stress_metrics(
        target_stdout + "\n" + target_stderr,
        elapsed_time_sec=elapsed,
    )
    neighbor_metrics = parse_stress_metrics(
        neighbor_stdout + "\n" + neighbor_stderr,
        elapsed_time_sec=elapsed,
    )
    metrics = build_metrics(
        target_metrics,
        neighbor_metrics,
        target_cpu_usage_usec=target_cpu,
        neighbor_cpu_usage_usec=neighbor_cpu,
        elapsed_time_sec=elapsed,
        target_weight=state_before["target"]["weight"],
        neighbor_weight=state_before["neighbor"]["weight"],
    )

    output_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    raw_paths = _write_raw_logs(
        output_dir,
        target_stdout,
        target_stderr,
        neighbor_stdout,
        neighbor_stderr,
    )
    returncode = target.returncode or neighbor.returncode or 0
    print(
        json.dumps(
            {
                "metrics": metrics,
                "metadata": {
                    "tool": "cgroup-cpu-share",
                    "command": command,
                    "returncode": returncode,
                    "scope": state_before,
                },
                "raw": raw_paths,
            },
            sort_keys=True,
        )
    )
    return returncode


def build_metrics(
    target: dict[str, float],
    neighbor: dict[str, float],
    *,
    target_cpu_usage_usec: int,
    neighbor_cpu_usage_usec: int,
    elapsed_time_sec: float,
    target_weight: int,
    neighbor_weight: int,
) -> dict[str, float]:
    target_throughput = target.get("throughput")
    neighbor_throughput = neighbor.get("throughput")
    if target_throughput is None or neighbor_throughput is None:
        raise CgroupCpuError("stress-ng did not report throughput for both cgroups")
    total_throughput = target_throughput + neighbor_throughput
    total_cpu = target_cpu_usage_usec + neighbor_cpu_usage_usec
    if total_throughput <= 0 or total_cpu <= 0 or elapsed_time_sec <= 0:
        raise CgroupCpuError("held-out workload produced no measurable CPU service")
    return {
        "target_throughput": target_throughput,
        "neighbor_throughput": neighbor_throughput,
        "aggregate_throughput": total_throughput,
        "target_work_share_pct": target_throughput / total_throughput * 100.0,
        "neighbor_work_share_pct": neighbor_throughput / total_throughput * 100.0,
        "target_cpu_share_pct": target_cpu_usage_usec / total_cpu * 100.0,
        "neighbor_cpu_share_pct": neighbor_cpu_usage_usec / total_cpu * 100.0,
        "target_cpu_rate": target_cpu_usage_usec / 1_000_000.0 / elapsed_time_sec,
        "neighbor_cpu_rate": neighbor_cpu_usage_usec / 1_000_000.0 / elapsed_time_sec,
        "target_cpu_weight": float(target_weight),
        "neighbor_cpu_weight": float(neighbor_weight),
        "elapsed_time_sec": elapsed_time_sec,
    }


def _write_raw_logs(
    output_dir: Path,
    target_stdout: str,
    target_stderr: str,
    neighbor_stdout: str,
    neighbor_stderr: str,
) -> dict[str, str]:
    paths = {
        "target_stdout_path": output_dir / "target_stdout.log",
        "target_stderr_path": output_dir / "target_stderr.log",
        "neighbor_stdout_path": output_dir / "neighbor_stdout.log",
        "neighbor_stderr_path": output_dir / "neighbor_stderr.log",
    }
    contents = {
        "target_stdout_path": target_stdout,
        "target_stderr_path": target_stderr,
        "neighbor_stdout_path": neighbor_stdout,
        "neighbor_stderr_path": neighbor_stderr,
    }
    for name, path in paths.items():
        path.write_text(contents[name], encoding="utf-8")
    return {name: str(path) for name, path in paths.items()}


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CgroupCpuError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(125) from exc
