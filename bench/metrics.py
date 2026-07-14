from __future__ import annotations

import csv
import json
from pathlib import Path
from typing import Any


def load_bench_metrics(stdout_path: Path) -> dict[str, Any]:
    if not stdout_path.exists():
        return {"metrics": {}, "metadata": {}, "parse_status": "missing_stdout"}

    text = stdout_path.read_text(encoding="utf-8", errors="replace").strip()
    if not text:
        return {"metrics": {}, "metadata": {}, "parse_status": "empty_stdout"}

    try:
        parsed = json.loads(text)
    except json.JSONDecodeError as exc:
        return {
            "metrics": {},
            "metadata": {},
            "parse_status": "non_json_stdout",
            "parse_error": str(exc),
        }

    if not isinstance(parsed, dict):
        return {
            "metrics": {},
            "metadata": {},
            "parse_status": "json_not_object",
        }

    if "metrics" in parsed:
        metrics = parsed.get("metrics")
        return {
            "metrics": metrics if isinstance(metrics, dict) else {},
            "metadata": parsed.get("metadata", {}) if isinstance(parsed.get("metadata", {}), dict) else {},
            "raw": parsed.get("raw", {}),
            "parse_status": "ok",
        }

    return {
        "metrics": parsed,
        "metadata": {},
        "parse_status": "legacy_json_object",
    }


def load_perf_stat_metrics(path: Path) -> dict[str, float]:
    if not path.exists():
        return {}

    names = {
        "context-switches": "context_switches",
        "cpu-migrations": "migrations",
    }
    metrics: dict[str, float] = {}
    with path.open(encoding="utf-8", errors="replace", newline="") as stream:
        for fields in csv.reader(stream):
            if len(fields) < 3:
                continue
            name = names.get(fields[2].strip())
            if not name:
                continue
            try:
                metrics[name] = float(fields[0].strip())
            except ValueError:
                continue
    return metrics
