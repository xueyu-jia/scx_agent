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
    prepare_scope,
    scope_state,
)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Prepare a two-cgroup CPU contention fixture")
    subparsers = parser.add_subparsers(dest="command", required=True)

    control = subparsers.add_parser("control", help="establish baseline scope state")
    _add_scope_arguments(control)

    train = subparsers.add_parser("train", help="run target and neighbor CPU workloads")
    _add_scope_arguments(train)
    train.add_argument("--stress-ng-binary", default="bench/workloads/bin/stress-ng")
    train.add_argument("--workers", type=int, default=2)
    train.add_argument("--timeout-seconds", type=int, default=3_600)

    args = parser.parse_args(argv)
    if args.command == "control":
        return _control(args)
    return _train(args)


def _add_scope_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--root", default="/sys/fs/cgroup/scx-bench")
    parser.add_argument("--target-weight", type=int, default=10)
    parser.add_argument("--neighbor-weight", type=int, default=100)


def _control(args: argparse.Namespace) -> int:
    scope = CgroupCpuScope.from_root(args.root)
    state = prepare_scope(
        scope,
        target_weight=args.target_weight,
        neighbor_weight=args.neighbor_weight,
    )
    outcome_path = _required_path("SCX_BENCH_TREATMENT_OUTCOME")
    _write_json(
        outcome_path,
        {
            "version": 2,
            "disposition": "proceed",
            "reason": {
                "code": "cgroup_cpu.baseline_ready",
                "message": "baseline cgroup state established and verified",
            },
            "details": {"fixture": "cgroup-cpu-v1", "scope": state},
        },
    )
    return 0


def _train(args: argparse.Namespace) -> int:
    if args.workers < 1:
        raise CgroupCpuError("workers must be positive")
    if args.timeout_seconds < 1:
        raise CgroupCpuError("timeout-seconds must be positive")

    scope = CgroupCpuScope.from_root(args.root)
    prepare_scope(
        scope,
        target_weight=args.target_weight,
        neighbor_weight=args.neighbor_weight,
    )
    command = [
        args.stress_ng_binary,
        "--cpu",
        str(args.workers),
        "--timeout",
        f"{args.timeout_seconds}s",
        "--metrics-brief",
    ]
    target = subprocess.Popen(cgroup_exec_argv(scope.target, command))
    neighbor = subprocess.Popen(cgroup_exec_argv(scope.neighbor, command))
    _write_fixture_state(scope, target.pid, neighbor.pid)

    processes = (target, neighbor)
    while True:
        for process in processes:
            returncode = process.poll()
            if returncode is not None:
                for peer in processes:
                    if peer.poll() is None:
                        peer.terminate()
                for peer in processes:
                    try:
                        peer.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        peer.kill()
                        peer.wait()
                return returncode or 1
        time.sleep(0.1)


def _write_fixture_state(scope: CgroupCpuScope, target_pid: int, neighbor_pid: int) -> None:
    output_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    _write_json(
        output_dir / "cgroup_cpu_fixture.json",
        {
            "version": 1,
            "scope": scope_state(scope),
            "launcher_pids": {"target": target_pid, "neighbor": neighbor_pid},
        },
    )


def _required_path(name: str) -> Path:
    value = os.environ.get(name)
    if not value:
        raise CgroupCpuError(f"{name} is required")
    return Path(value)


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CgroupCpuError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(125) from exc
