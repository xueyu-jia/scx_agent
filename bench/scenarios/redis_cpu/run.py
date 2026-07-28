#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

from bench.core.config import ConfigError, load_config_data
from bench.integrations.tuning_agent.llm_preflight import (
    LlmPreflightError,
    preflight_protocol,
    settings_from_config,
)


DEFAULT_CONFIG = Path("bench/configs/local_config")
DEFAULT_ROOT = Path("bench/results/redis_cpu")
BASELINE_TREATMENT = "redis_cpu_control"
CANDIDATE_TREATMENT = "redis_cpu_agent"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run the real-LLM Redis CPU experiment")
    parser.add_argument("--config", default=str(DEFAULT_CONFIG))
    parser.add_argument("--plan", default="redis_cpu_demo")
    parser.add_argument("--scheduler", default="default")
    parser.add_argument("--output")
    parser.add_argument("--progress-interval", type=int, default=15)
    args = parser.parse_args(argv)

    config_path = Path(args.config).expanduser().resolve()
    if not _preflight_llm(config_path):
        return 2

    output = (
        Path(args.output).expanduser().resolve()
        if args.output
        else _default_output().resolve()
    )
    command = [
        sys.executable,
        "-m",
        "bench.scripts.run",
        "--config",
        str(config_path),
        "--plan",
        args.plan,
        "--baseline",
        args.scheduler,
        "--baseline-treatment",
        BASELINE_TREATMENT,
        "--candidate",
        args.scheduler,
        "--candidate-treatment",
        CANDIDATE_TREATMENT,
        "--order",
        "alternating",
        "--parallel",
        "1",
        "--progress-interval",
        str(args.progress_interval),
        "--output",
        str(output),
    ]
    print(f"redis cpu experiment: {' '.join(command)}", flush=True)
    try:
        completed = subprocess.run(command, check=False)
    except OSError as error:
        print(f"error: failed to start runner: {error}", file=sys.stderr)
        return 2
    if completed.returncode != 0:
        return completed.returncode

    try:
        _validate_outputs(output, args.scheduler)
    except (OSError, ValueError) as error:
        print(f"error: incomplete experiment output: {error}", file=sys.stderr)
        return 2

    print(f"experiment: {output}")
    print(f"analysis: {output / 'analysis' / 'analysis.json'}")
    print(f"report: {output / 'analysis' / 'report.html'}")
    return 0


def _preflight_llm(config_path: Path) -> bool:
    try:
        settings = settings_from_config(
            load_config_data(config_path),
            treatment_names=[CANDIDATE_TREATMENT],
        )
        for item in settings:
            preflight_protocol(item)
            print(f"LLM preflight passed ({item.model} @ {item.base_url})", flush=True)
    except (ConfigError, LlmPreflightError, OSError, ValueError) as error:
        print(f"error: LLM preflight failed: {error}", file=sys.stderr)
        return False
    return True


def _validate_outputs(output: Path, scheduler: str) -> None:
    _read_json_object(output / "analysis" / "analysis.json")
    report = output / "analysis" / "report.html"
    if not report.is_file() or report.stat().st_size == 0:
        raise ValueError(f"report is missing or empty: {report}")

    for treatment in (BASELINE_TREATMENT, CANDIDATE_TREATMENT):
        result_paths = sorted(
            (output / "runs" / f"{scheduler}__{treatment}").glob("*/result.json")
        )
        if not result_paths:
            raise ValueError(f"result files are missing for {treatment}")
        for path in result_paths:
            _read_json_object(path)


def _read_json_object(path: Path) -> dict[str, object]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return value


def _default_output() -> Path:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_ROOT / timestamp


if __name__ == "__main__":
    raise SystemExit(main())
