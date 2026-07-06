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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run a workload and emit normalized JSON")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)

    command = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        emit({"metrics": {}, "metadata": {"error": "missing command"}})
        return 2

    out_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    raw_stdout = out_dir / "workload_stdout.log"
    raw_stderr = out_dir / "workload_stderr.log"

    started = time.monotonic()
    try:
        completed = subprocess.run(command, check=False, capture_output=True, text=True)
        returncode: int | None = completed.returncode
        stdout = completed.stdout
        stderr = completed.stderr
        error = None
    except FileNotFoundError as exc:
        returncode = 127
        stdout = ""
        stderr = str(exc)
        error = "command_not_found"
    finished = time.monotonic()

    raw_stdout.write_text(stdout, encoding="utf-8")
    raw_stderr.write_text(stderr, encoding="utf-8")

    payload: dict[str, Any] = {
        "metrics": {
            "elapsed_time_sec": finished - started,
        },
        "metadata": {
            "wrapper": "generic",
            "command": command,
            "returncode": returncode,
        },
        "raw": {
            "stdout_path": str(raw_stdout),
            "stderr_path": str(raw_stderr),
        },
    }
    if error:
        payload["metadata"]["error"] = error

    emit(payload)
    return returncode if returncode is not None else 1


def emit(payload: dict[str, Any]) -> None:
    print(json.dumps(payload, sort_keys=True))


if __name__ == "__main__":
    raise SystemExit(main())
