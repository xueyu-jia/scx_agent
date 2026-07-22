#!/usr/bin/env python3
from __future__ import annotations

import argparse
import shutil
import tempfile
from pathlib import Path

from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Linux kernel config build wrapper")
    parser.add_argument("--source", default="/home/bob/linux-6.18")
    parser.add_argument("--config", default="tinyconfig")
    parser.add_argument("--target", default="bzImage")
    parser.add_argument("--jobs", default="0")
    parser.add_argument("--clean-between", action="store_true")
    parser.add_argument("--keep-output", action="store_true")
    ns = parser.parse_args(argv)

    source = Path(ns.source)
    if not source.exists():
        result = run_command(["false"])
        emit(result, {"elapsed_time_sec": result.elapsed_time_sec}, tool="linux-build-config")
        return 2

    build_dir = Path(tempfile.mkdtemp(prefix="scx-linux-build-"))
    jobs = ns.jobs if ns.jobs != "0" else str(__import__("os").cpu_count() or 1)
    try:
        if ns.clean_between:
            clean = run_command(["make", "-C", str(source), f"O={build_dir}", "clean"])
            if clean.returncode != 0:
                emit(clean, {"elapsed_time_sec": clean.elapsed_time_sec}, tool="linux-build-config")
                return clean.returncode

        setup = run_command(["make", "-C", str(source), f"O={build_dir}", ns.config])
        if setup.returncode != 0:
            emit(setup, {"elapsed_time_sec": setup.elapsed_time_sec}, tool="linux-build-config")
            return setup.returncode

        result = run_command(["make", "-C", str(source), f"O={build_dir}", f"-j{jobs}", ns.target])
        metrics = {"elapsed_time_sec": result.elapsed_time_sec}
        if result.elapsed_time_sec > 0:
            metrics["throughput"] = 1.0 / result.elapsed_time_sec
        emit(result, metrics, tool="linux-build-config")
        return result.returncode
    finally:
        if not ns.keep_output:
            shutil.rmtree(build_dir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
