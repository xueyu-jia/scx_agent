#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import signal
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path.cwd()))
from common import (  # noqa: E402
    DRIVER_CPUS,
    REDIS_CPUS,
    REDIS_PORTS,
    RedisCpuError,
    RedisCpuScope,
    content_digest,
    prepare_scope,
    scoped_exec_argv,
    set_current_affinity,
    write_json_atomic,
)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Prepare the Redis CPU contention scenario")
    subparsers = parser.add_subparsers(dest="command", required=True)

    control = subparsers.add_parser("control", help="establish the configured control state")
    _add_common_arguments(control)

    train = subparsers.add_parser("train", help="run Redis, batch, and rolling loadgen")
    _add_common_arguments(train)
    train.add_argument("--redis-server-binary", default="bench/workloads/bin/redis-server")
    train.add_argument("--redis-benchmark-binary", default="bench/workloads/bin/redis-benchmark")
    train.add_argument("--stress-ng-binary", default="bench/workloads/bin/stress-ng")
    train.add_argument("--loadgen", default=str(Path(__file__).with_name("loadgen.py")))
    train.add_argument("--contention", choices=("high", "low"), default="high")
    train.add_argument("--requests", type=int, default=20_000)
    train.add_argument("--clients", type=int, default=64)
    train.add_argument("--timeout-seconds", type=int, default=3_600)

    args = parser.parse_args(argv)
    if args.command == "control":
        return _control(args)
    return _train(args)


def _add_common_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--root", default="/sys/fs/cgroup/scx-bench")
    parser.add_argument("--redis-weight", type=int, default=100)
    parser.add_argument("--batch-weight", type=int, default=100)


def _control(args: argparse.Namespace) -> int:
    scope = RedisCpuScope.from_root(args.root)
    state = prepare_scope(
        scope,
        redis_weight=args.redis_weight,
        batch_weight=args.batch_weight,
    )
    outcome_path = _required_path("SCX_BENCH_TREATMENT_OUTCOME")
    write_json_atomic(
        outcome_path,
        {
            "version": 2,
            "disposition": "proceed",
            "reason": {
                "code": "redis_cpu.control_ready",
                "message": (
                    "Redis CPU control cgroups are at the verified "
                    f"{args.redis_weight}:{args.batch_weight} baseline"
                ),
            },
            "details": {"fixture": "redis-cpu-v1", "scope": state},
        },
    )
    return 0


def _train(args: argparse.Namespace) -> int:
    if args.batch_weight != 100:
        raise RedisCpuError("training batch cpu.weight must remain 100")
    if args.requests < 1 or args.clients < 1 or args.timeout_seconds < 1:
        raise RedisCpuError("requests, clients, and timeout-seconds must be positive")
    for binary in (
        args.redis_server_binary,
        args.redis_benchmark_binary,
        args.stress_ng_binary,
        args.loadgen,
    ):
        if not Path(binary).is_file():
            raise RedisCpuError(f"required scenario executable is missing: {binary}")

    scope = RedisCpuScope.from_root(args.root)
    prepare_scope(
        scope,
        redis_weight=args.redis_weight,
        batch_weight=args.batch_weight,
    )
    set_current_affinity(DRIVER_CPUS)
    output_dir = Path(os.environ.get("SCX_BENCH_OUT", ".")).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    processes: list[subprocess.Popen[bytes]] = []
    try:
        redis_config = _redis_config(args.redis_server_binary, output_dir)
        redis_processes = [
            _start_redis(scope, args.redis_server_binary, port, output_dir, processes)
            for port in REDIS_PORTS
        ]
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
            output_dir / "batch.stdout.log",
            output_dir / "batch.stderr.log",
        )
        processes.append(batch)
        time.sleep(0.25)
        _require_running(processes)

        loadgen_parameters = {
            "benchmark": str(Path(args.redis_benchmark_binary).resolve()),
            "ports": list(REDIS_PORTS),
            "requests": args.requests,
            "clients": args.clients,
            "command": ["GET", "redis_cpu_key"],
            "contention": args.contention,
            "batch_cpu_load": cpu_load,
            "initial_redis_weight": args.redis_weight,
            "initial_batch_weight": args.batch_weight,
            "redis_cpus": list(REDIS_CPUS),
            "driver_cpus": list(DRIVER_CPUS),
        }
        command = [
            sys.executable,
            str(Path(args.loadgen).resolve()),
            "--root",
            str(scope.root),
            "--output-dir",
            str(output_dir),
            "--redis-benchmark-binary",
            str(Path(args.redis_benchmark_binary).resolve()),
            "--redis-pid",
            str(redis_processes[0].pid),
            "--redis-pid",
            str(redis_processes[1].pid),
            "--batch-pid",
            str(batch.pid),
            "--requests",
            str(args.requests),
            "--clients",
            str(args.clients),
            "--run-id",
            uuid.uuid4().hex,
            "--redis-config-digest",
            content_digest(redis_config),
            "--loadgen-parameters-digest",
            content_digest(loadgen_parameters),
        ]
        os.execve(command[0], command, os.environ.copy())
    except BaseException:
        _terminate(processes)
        raise
    return 1


def _redis_config(binary: str, output_dir: Path) -> dict[str, object]:
    return {
        "binary": str(Path(binary).resolve()),
        "ports": list(REDIS_PORTS),
        "bind": "127.0.0.1",
        "save": "",
        "appendonly": "no",
        "daemonize": "no",
        "directories": [str(output_dir / f"redis-{port}") for port in REDIS_PORTS],
    }


def _start_redis(
    scope: RedisCpuScope,
    binary: str,
    port: int,
    output_dir: Path,
    processes: list[subprocess.Popen[bytes]],
) -> subprocess.Popen[bytes]:
    directory = output_dir / f"redis-{port}"
    directory.mkdir(mode=0o700, exist_ok=True)
    command = [
        binary,
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
        output_dir / f"redis-{port}.stdout.log",
        output_dir / f"redis-{port}.stderr.log",
    )
    processes.append(process)
    return process


def _spawn_scoped(
    cgroup: Path,
    cpus: tuple[int, ...],
    command: list[str],
    stdout_path: Path,
    stderr_path: Path,
) -> subprocess.Popen[bytes]:
    stdout = stdout_path.open("ab")
    stderr = stderr_path.open("ab")
    try:
        return subprocess.Popen(
            scoped_exec_argv(cgroup, cpus, command),
            stdout=stdout,
            stderr=stderr,
        )
    finally:
        stdout.close()
        stderr.close()


def _wait_for_redis(port: int) -> None:
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        try:
            if _redis_command(port, b"PING") == b"PONG":
                return
        except OSError:
            pass
        time.sleep(0.05)
    raise RedisCpuError(f"Redis shard on port {port} did not become ready")


def _redis_command(port: int, *parts: bytes) -> bytes:
    payload = f"*{len(parts)}\r\n".encode("ascii") + b"".join(
        f"${len(part)}\r\n".encode("ascii") + part + b"\r\n" for part in parts
    )
    with socket.create_connection(("127.0.0.1", port), timeout=0.5) as connection:
        connection.sendall(payload)
        response = connection.recv(4096)
    if response.startswith(b"+"):
        return response[1:].split(b"\r\n", 1)[0]
    if response.startswith(b"$"):
        rows = response.split(b"\r\n")
        return rows[1] if len(rows) > 1 else b""
    if response.startswith(b"-"):
        raise RedisCpuError(response.decode("utf-8", errors="replace"))
    return response


def _require_running(processes: list[subprocess.Popen[bytes]]) -> None:
    for process in processes:
        returncode = process.poll()
        if returncode is not None:
            raise RedisCpuError(f"fixture child {process.pid} exited with {returncode}")


def _terminate(processes: list[subprocess.Popen[bytes]]) -> None:
    for process in reversed(processes):
        if process.poll() is None:
            try:
                process.send_signal(signal.SIGTERM)
            except ProcessLookupError:
                pass
    for process in reversed(processes):
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def _required_path(name: str) -> Path:
    value = os.environ.get(name)
    if not value:
        raise RedisCpuError(f"{name} is required")
    return Path(value)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RedisCpuError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(125) from exc
