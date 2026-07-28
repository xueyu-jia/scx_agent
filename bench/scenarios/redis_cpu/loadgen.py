#!/usr/bin/env python3
from __future__ import annotations

import argparse
from functools import partial
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Any


sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path.cwd()))
from common import (  # noqa: E402
    DRIVER_CPUS,
    METRIC_NAMES,
    REDIS_PORTS,
    STATE_RING_SIZE,
    RedisCpuError,
    RedisCpuScope,
    aggregate_shards,
    content_digest,
    parse_redis_benchmark,
    process_identity,
    read_cpu_pressure_total,
    read_cpu_stat,
    read_weight,
    readiness_processes,
    set_current_affinity,
    validate_runtime_identity,
    write_json_atomic,
)


STOP = False


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Continuously benchmark two Redis shards")
    parser.add_argument("--root", required=True)
    parser.add_argument("--output-dir", required=True)
    parser.add_argument("--redis-benchmark-binary", required=True)
    parser.add_argument("--redis-pid", type=int, action="append", required=True)
    parser.add_argument("--batch-pid", type=int, required=True)
    parser.add_argument("--requests", type=int, required=True)
    parser.add_argument("--clients", type=int, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--redis-config-digest", required=True)
    parser.add_argument("--loadgen-parameters-digest", required=True)
    args = parser.parse_args(argv)
    if len(args.redis_pid) != 2:
        raise RedisCpuError("loadgen requires exactly two Redis PIDs")
    if args.requests < 1 or args.clients < 1:
        raise RedisCpuError("loadgen requests and clients must be positive")

    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    set_current_affinity(DRIVER_CPUS)
    scope = RedisCpuScope.from_root(args.root)
    output_dir = Path(args.output_dir).resolve()
    paths = {
        "runtime": output_dir / "runtime.json",
        "state": output_dir / "loadgen-state.json",
        "ready": output_dir / "ready.json",
    }
    runtime = _runtime_identity(args, scope)
    fingerprint = validate_runtime_identity(runtime, scope)
    write_json_atomic(paths["runtime"], runtime)

    state: dict[str, Any] = {
        "version": 1,
        "run_id": args.run_id,
        "next_sequence": 1,
        "samples": [],
    }
    write_json_atomic(paths["state"], state)
    consecutive_valid = 0
    last_complete_metrics: dict[str, float] | None = None
    while not STOP:
        sample = _run_round(args, scope, fingerprint, state["next_sequence"], output_dir)
        state["next_sequence"] += 1
        state["samples"].append(sample)
        state["samples"] = state["samples"][-STATE_RING_SIZE:]
        write_json_atomic(paths["state"], state)
        if sample["quality"] == "valid":
            consecutive_valid += 1
            last_complete_metrics = sample["metrics"]
        else:
            consecutive_valid = 0
            if not sample["metrics"] and last_complete_metrics is not None:
                sample["metrics"] = last_complete_metrics
                sample["fallback_sequence"] = _last_valid_sequence(state["samples"][:-1])
                write_json_atomic(paths["state"], state)
        if consecutive_valid >= 3 and not paths["ready"].exists():
            write_json_atomic(
                paths["ready"],
                {
                    "version": 1,
                    "ready": True,
                    "workload_digest": runtime["workload_digest"],
                    "processes": readiness_processes(runtime),
                },
            )
    return 0


def _runtime_identity(args: argparse.Namespace, scope: RedisCpuScope) -> dict[str, Any]:
    redis = [process_identity(pid) for pid in args.redis_pid]
    batch = process_identity(args.batch_pid)
    loadgen = process_identity(os.getpid())
    if any(item["affinity"] != [0, 1] for item in redis) or batch["affinity"] != [0, 1]:
        raise RedisCpuError("Redis and batch processes must be pinned to CPUs 0-1")
    if loadgen["affinity"] != list(DRIVER_CPUS):
        raise RedisCpuError(f"loadgen must be pinned to CPUs {list(DRIVER_CPUS)}")
    cgroups = {
        name: {"path": str(path), "inode": path.stat().st_ino}
        for name, path in (
            ("redis", scope.redis),
            ("batch", scope.batch),
            ("driver", scope.driver),
        )
    }
    workload = {
        "redis_config_digest": args.redis_config_digest,
        "loadgen_parameters_digest": args.loadgen_parameters_digest,
        "ports": list(REDIS_PORTS),
        "affinities": {
            "redis": [0, 1],
            "batch": [0, 1],
            "driver": list(DRIVER_CPUS),
        },
    }
    return {
        "version": 1,
        "run_id": args.run_id,
        "scope": str(scope.root),
        "cgroups": cgroups,
        "processes": {"redis": redis, "batch": batch, "loadgen": loadgen},
        "redis": {
            "ports": list(REDIS_PORTS),
            "config_digest": args.redis_config_digest,
        },
        "loadgen": {
            "parameters_digest": args.loadgen_parameters_digest,
            "benchmark_executable": str(Path(args.redis_benchmark_binary).resolve()),
            "requests": args.requests,
            "clients": args.clients,
        },
        "workload_digest": content_digest(workload),
    }


def _run_round(
    args: argparse.Namespace,
    scope: RedisCpuScope,
    expected_fingerprint: str,
    sequence: int,
    output_dir: Path,
) -> dict[str, Any]:
    fingerprint_before = validate_runtime_identity(_read_runtime(output_dir), scope)
    if fingerprint_before != expected_fingerprint:
        raise RedisCpuError("workload fingerprint changed before loadgen round")
    weight_before = read_weight(scope.redis)
    redis_before = read_cpu_stat(scope.redis)["usage_usec"]
    batch_before = read_cpu_stat(scope.batch)["usage_usec"]
    pressure_before = read_cpu_pressure_total(scope.root)
    started_at_ns = time.time_ns()
    monotonic_started_ns = time.monotonic_ns()

    process_specs = _benchmark_process_specs(args)
    processes = [
        subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            preexec_fn=partial(os.sched_setaffinity, 0, {cpu}),
        )
        for cpu, command in process_specs
    ]
    results = []
    for port, process in zip(REDIS_PORTS, processes, strict=True):
        stdout, stderr = process.communicate()
        (output_dir / f"redis-benchmark-{port}.log").write_text(
            stdout + "\n" + stderr,
            encoding="utf-8",
        )
        parsed = None
        error = None
        try:
            parsed = parse_redis_benchmark(stdout + "\n" + stderr)
        except RedisCpuError as exc:
            error = str(exc)
        results.append(
            {
                "port": port,
                "returncode": process.returncode,
                "metrics": parsed,
                "error": error,
            }
        )

    monotonic_ended_ns = time.monotonic_ns()
    ended_at_ns = time.time_ns()
    redis_after = read_cpu_stat(scope.redis)["usage_usec"]
    batch_after = read_cpu_stat(scope.batch)["usage_usec"]
    pressure_after = read_cpu_pressure_total(scope.root)
    weight_after = read_weight(scope.redis)
    fingerprint_after = validate_runtime_identity(_read_runtime(output_dir), scope)
    elapsed_seconds = (monotonic_ended_ns - monotonic_started_ns) / 1_000_000_000.0

    errors = []
    if fingerprint_after != fingerprint_before:
        errors.append("workload fingerprint changed during the loadgen round")
    if weight_after != weight_before:
        errors.append("cpu.weight changed during the loadgen round")
    for result in results:
        if result["returncode"] != 0:
            errors.append(f"redis-benchmark on port {result['port']} exited non-zero")
        if result["metrics"] is None:
            errors.append(f"redis-benchmark on port {result['port']} was not parseable")

    metrics: dict[str, float] = {}
    if not errors:
        metrics = aggregate_shards(
            results[0]["metrics"],
            results[1]["metrics"],
            redis_usage_usec=redis_after - redis_before,
            batch_usage_usec=batch_after - batch_before,
            pressure_usec=pressure_after - pressure_before,
            elapsed_seconds=elapsed_seconds,
            weight=weight_before,
        )
    return {
        "sequence": sequence,
        "started_at_ns": started_at_ns,
        "ended_at_ns": ended_at_ns,
        "monotonic_started_ns": monotonic_started_ns,
        "monotonic_ended_ns": monotonic_ended_ns,
        "quality": "valid" if not errors else "invalid",
        "workload_fingerprint": fingerprint_before,
        "weight_at_start": weight_before,
        "weight_at_end": weight_after,
        "metrics": metrics,
        "shards": results,
        "errors": errors,
    }


def _benchmark_command(args: argparse.Namespace, port: int) -> list[str]:
    return [
        args.redis_benchmark_binary,
        "-h",
        "127.0.0.1",
        "-p",
        str(port),
        "-n",
        str(args.requests),
        "-c",
        str(args.clients),
        "--precision",
        "3",
        "GET",
        "redis_cpu_key",
    ]


def _benchmark_process_specs(
    args: argparse.Namespace,
) -> list[tuple[int, list[str]]]:
    if len(DRIVER_CPUS) != len(REDIS_PORTS):
        raise RedisCpuError("each Redis shard requires one dedicated driver CPU")
    return [
        (cpu, _benchmark_command(args, port))
        for cpu, port in zip(DRIVER_CPUS, REDIS_PORTS, strict=True)
    ]


def _read_runtime(output_dir: Path) -> dict[str, Any]:
    try:
        value = json.loads((output_dir / "runtime.json").read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise RedisCpuError(f"runtime identity became unreadable: {exc}") from exc
    if not isinstance(value, dict):
        raise RedisCpuError("runtime identity must be an object")
    return value


def _last_valid_sequence(samples: list[dict[str, Any]]) -> int | None:
    for sample in reversed(samples):
        if sample.get("quality") == "valid" and set(sample.get("metrics", {})) == set(METRIC_NAMES):
            return int(sample["sequence"])
    return None


def _stop(_signum: int, _frame: object) -> None:
    global STOP
    STOP = True


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RedisCpuError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(125) from exc
