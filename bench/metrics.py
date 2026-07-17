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


def _perf_event_name(raw_name: str) -> tuple[str | None, bool]:
    name = raw_name.strip()
    aliases = {
        "context-switches": ("context_switches", False),
        "cpu-migrations": ("migrations", False),
        "task-clock": ("task_clock_msec", False),
        "page-faults": ("page_faults", False),
        "cycles": ("cycles", True),
        "instructions": ("instructions", True),
        "cache-misses": ("cache_misses", True),
        "LLC-load-misses": ("llc_load_misses", True),
        "dTLB-load-misses": ("dtlb_load_misses", True),
    }
    if name in aliases:
        return aliases[name]

    # Hybrid Intel PMUs qualify generic event names as cpu_core/event/ or
    # cpu_atom/event/. Aggregate either spelling into one stable metric.
    if name.endswith("/") and "/" in name:
        qualified = name.rsplit("/", 2)[-2]
        if qualified in aliases:
            metric, _ = aliases[qualified]
            return metric, True
    return None, False


def load_perf_stat_metrics(path: Path) -> dict[str, float]:
    if not path.exists():
        return {}

    metrics: dict[str, float] = {}
    hardware_seen = False
    invalid_hardware = 0
    running_percentages: list[float] = []
    with path.open(encoding="utf-8", errors="replace", newline="") as stream:
        for fields in csv.reader(stream):
            if len(fields) < 3:
                continue
            name, hardware = _perf_event_name(fields[2])
            if not name:
                continue
            try:
                value = float(fields[0].strip())
            except ValueError:
                if hardware:
                    hardware_seen = True
                    invalid_hardware += 1
                continue
            metrics[name] = metrics.get(name, 0.0) + value
            if hardware:
                hardware_seen = True
                if value <= 0:
                    invalid_hardware += 1
                if len(fields) >= 5:
                    try:
                        running_percentages.append(float(fields[4].strip()))
                    except ValueError:
                        pass

    if hardware_seen:
        running_min = min(running_percentages, default=0.0)
        metrics["perf_hardware_invalid_events"] = float(invalid_hardware)
        metrics["perf_hardware_running_pct_min"] = running_min
        metrics["perf_hardware_events_valid"] = float(
            invalid_hardware == 0 and running_min >= 90.0
        )
    return metrics
