from __future__ import annotations

import json
import os
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class CommandResult:
    command: list[str]
    returncode: int
    stdout: str
    stderr: str
    elapsed_time_sec: float
    raw_stdout: Path
    raw_stderr: Path


def run_command(command: list[str]) -> CommandResult:
    out_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    raw_stdout = out_dir / "workload_stdout.log"
    raw_stderr = out_dir / "workload_stderr.log"

    started = time.monotonic()
    try:
        completed = subprocess.run(command, check=False, capture_output=True, text=True)
        returncode = completed.returncode
        stdout = completed.stdout
        stderr = completed.stderr
    except FileNotFoundError as exc:
        returncode = 127
        stdout = ""
        stderr = str(exc)
    elapsed = time.monotonic() - started

    raw_stdout.write_text(stdout, encoding="utf-8")
    raw_stderr.write_text(stderr, encoding="utf-8")
    return CommandResult(command, returncode, stdout, stderr, elapsed, raw_stdout, raw_stderr)


def emit(result: CommandResult, metrics: dict[str, Any], tool: str) -> None:
    print(
        json.dumps(
            {
                "metrics": metrics,
                "metadata": {
                    "tool": tool,
                    "command": result.command,
                    "returncode": result.returncode,
                },
                "raw": {
                    "stdout_path": str(result.raw_stdout),
                    "stderr_path": str(result.raw_stderr),
                },
            },
            sort_keys=True,
        )
    )
