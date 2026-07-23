#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
from pathlib import Path


DEFAULT_EVENTS = "task-clock,context-switches,cpu-migrations"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run a benchmark under perf stat without changing its output"
    )
    parser.add_argument("--perf", default="bench/workloads/bin/perf")
    parser.add_argument("--events", default=DEFAULT_EVENTS)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a benchmark command is required")

    output = Path(os.environ.get("SCX_BENCH_OUT", ".")) / "perf_stat.csv"
    perf_command = [
        args.perf,
        "stat",
        "-x,",
        "-o",
        str(output),
        "-e",
        args.events,
        "--",
        *command,
    ]
    os.execvp(perf_command[0], perf_command)
    return 127


if __name__ == "__main__":
    raise SystemExit(main())
