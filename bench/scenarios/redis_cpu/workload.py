#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import sys
import time
import uuid
from pathlib import Path
from statistics import median
from types import SimpleNamespace


sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path.cwd()))
from common import (  # noqa: E402
    DRIVER_CPUS,
    METRIC_NAMES,
    REDIS_CPUS,
    REDIS_PORTS,
    RedisCpuError,
    RedisCpuScope,
    content_digest,
    read_members,
    read_weight,
    require_scope,
    set_current_affinity,
    validate_runtime_identity,
    write_json_atomic,
)
from fixture import (  # noqa: E402
    _redis_command,
    _spawn_scoped,
    _terminate,
    _wait_for_redis,
)
from loadgen import _run_round, _runtime_identity  # noqa: E402


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run held-out Redis CPU measurements")
    parser.add_argument("--root", default="/sys/fs/cgroup/scx-bench")
    parser.add_argument("--redis-server-binary", default="bench/workloads/bin/redis-server")
    parser.add_argument("--redis-benchmark-binary", default="bench/workloads/bin/redis-benchmark")
    parser.add_argument("--stress-ng-binary", default="bench/workloads/bin/stress-ng")
    parser.add_argument("--requests", type=int, default=20_000)
    parser.add_argument("--clients", type=int, default=64)
    parser.add_argument("--rounds", type=int, default=5)
    parser.add_argument("--warmup-rounds", type=int, default=2)
    parser.add_argument("--contention", choices=("high", "low"), default="high")
    parser.add_argument("--timeout-seconds", type=int, default=300)
    args = parser.parse_args(argv)
    if min(args.requests, args.clients, args.rounds, args.timeout_seconds) < 1:
        raise RedisCpuError("held-out workload numeric arguments must be positive")
    if args.warmup_rounds < 0:
        raise RedisCpuError("held-out warmup-rounds must be non-negative")

    scope = RedisCpuScope.from_root(args.root)
    require_scope(scope)
    if read_members(scope.redis) or read_members(scope.batch) or read_members(scope.driver):
        raise RedisCpuError("held-out workload requires empty Redis CPU cgroups")
    redis_weight = read_weight(scope.redis)
    if read_weight(scope.batch) != 100:
        raise RedisCpuError("held-out batch cpu.weight must remain 100")
    (scope.driver / "cgroup.procs").write_text(f"{os.getpid()}\n", encoding="utf-8")
    set_current_affinity(DRIVER_CPUS)

    output_dir = Path(os.environ.get("SCX_BENCH_OUT", ".")).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    processes = []
    started = time.monotonic()
    try:
        redis_processes = []
        redis_config = {
            "binary": str(Path(args.redis_server_binary).resolve()),
            "ports": list(REDIS_PORTS),
            "persistence": False,
        }
        for port in REDIS_PORTS:
            directory = output_dir / f"held-out-redis-{port}"
            directory.mkdir(mode=0o700, exist_ok=True)
            command = [
                args.redis_server_binary,
                "--port",
                str(port),
                "--bind",
                "127.0.0.1",
                "--save",
                "",
                "--appendonly",
                "no",
                "--daemonize",
                "no",
                "--protected-mode",
                "no",
                "--dir",
                str(directory),
            ]
            process = _spawn_scoped(
                scope.redis,
                REDIS_CPUS,
                command,
                output_dir / f"held-out-redis-{port}.stdout.log",
                output_dir / f"held-out-redis-{port}.stderr.log",
            )
            processes.append(process)
            redis_processes.append(process)
        for port in REDIS_PORTS:
            _wait_for_redis(port)
            _redis_command(port, b"SET", b"redis_cpu_key", b"x" * 256)

        cpu_load = 100 if args.contention == "high" else 10
        batch_command = [
            args.stress_ng_binary,
            "--cpu",
            "2",
            "--cpu-load",
            str(cpu_load),
            "--timeout",
            f"{args.timeout_seconds}s",
            "--metrics-brief",
        ]
        batch = _spawn_scoped(
            scope.batch,
            REDIS_CPUS,
            batch_command,
            output_dir / "held-out-batch.stdout.log",
            output_dir / "held-out-batch.stderr.log",
        )
        processes.append(batch)
        time.sleep(0.5)
        if any(process.poll() is not None for process in processes):
            raise RedisCpuError("held-out workload process exited during startup")

        loadgen_parameters = {
            "benchmark": str(Path(args.redis_benchmark_binary).resolve()),
            "requests": args.requests,
            "clients": args.clients,
            "contention": args.contention,
            "held_out": True,
        }
        runtime_args = SimpleNamespace(
            redis_pid=[process.pid for process in redis_processes],
            batch_pid=batch.pid,
            run_id=uuid.uuid4().hex,
            redis_config_digest=content_digest(redis_config),
            loadgen_parameters_digest=content_digest(loadgen_parameters),
            requests=args.requests,
            clients=args.clients,
            redis_benchmark_binary=str(Path(args.redis_benchmark_binary).resolve()),
        )
        runtime = _runtime_identity(runtime_args, scope)
        validate_runtime_identity(runtime, scope)
        write_json_atomic(output_dir / "runtime.json", runtime)

        all_samples = []
        total_rounds = args.warmup_rounds + args.rounds
        for sequence in range(1, total_rounds + 1):
            sample = _run_round(
                runtime_args,
                scope,
                validate_runtime_identity(runtime, scope),
                sequence,
                output_dir,
            )
            if sample["quality"] != "valid":
                raise RedisCpuError(f"held-out loadgen round {sequence} was invalid")
            if sequence > args.warmup_rounds:
                all_samples.append(sample)
        metrics = {
            name: float(median(sample["metrics"][name] for sample in all_samples))
            for name in METRIC_NAMES
        }
        metrics["elapsed_time_sec"] = time.monotonic() - started
        print(
            json.dumps(
                {
                    "metrics": metrics,
                    "metadata": {
                        "tool": "redis-cpu-held-out-v1",
                        "rounds": args.rounds,
                        "warmup_rounds": args.warmup_rounds,
                        "contention": args.contention,
                        "redis_cpu_weight": redis_weight,
                        "workload_digest": runtime["workload_digest"],
                    },
                    "raw": {
                        "runtime": str(output_dir / "runtime.json"),
                        "sample_sequences": [sample["sequence"] for sample in all_samples],
                    },
                },
                sort_keys=True,
            )
        )
        return 0
    finally:
        _terminate(processes)
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            if not read_members(scope.redis) and not read_members(scope.batch):
                break
            time.sleep(0.05)
        else:
            raise RedisCpuError("held-out Redis or batch processes did not exit")
        driver_members = read_members(scope.driver)
        if driver_members != (os.getpid(),):
            raise RedisCpuError(f"held-out driver left unexpected processes: {driver_members}")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RedisCpuError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(125) from exc
