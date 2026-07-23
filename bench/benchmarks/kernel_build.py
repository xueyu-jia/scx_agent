#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import shutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Linux kernel build wrapper")
    parser.add_argument("--source", required=True)
    parser.add_argument("--config", help="kernel config copied into the output tree")
    parser.add_argument("--target", default="bzImage")
    parser.add_argument("--jobs", default="0")
    parser.add_argument("--keep-output", action="store_true")
    ns = parser.parse_args(argv)

    out_root = Path(os.environ.get("SCX_BENCH_OUT", "."))
    build_dir = out_root / "kernel-build"
    if build_dir.exists() and not ns.keep_output:
        shutil.rmtree(build_dir)
    build_dir.mkdir(parents=True, exist_ok=True)

    jobs = int(ns.jobs)
    if jobs <= 0:
        jobs = os.cpu_count() or 1

    source = Path(ns.source)
    if not source.is_dir():
        raise SystemExit(f"kernel source does not exist: {source}")
    if ns.config:
        kernel_config = Path(ns.config)
        if not kernel_config.is_file():
            raise SystemExit(f"kernel config does not exist: {kernel_config}")
        shutil.copy2(kernel_config, build_dir / ".config")
    setup = run_command(["make", "-C", str(source), f"O={build_dir}", "olddefconfig"])
    if setup.returncode != 0:
        emit(setup, {"elapsed_time_sec": setup.elapsed_time_sec}, tool="kernel-build")
        return setup.returncode

    result = run_command(
        ["make", "-C", str(source), f"O={build_dir}", f"-j{jobs}", ns.target]
    )
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    if result.elapsed_time_sec > 0:
        metrics["throughput"] = 1.0 / result.elapsed_time_sec
    emit(result, metrics, tool="kernel-build")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
