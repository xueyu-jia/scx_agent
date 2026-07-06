from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class RunMetricSet:
    result_dir: Path
    run_dir: Path
    label: str
    status: str
    plan: str
    machine: str
    suite: str
    bench: str
    metric_profile: str
    metric_profile_config: dict[str, Any]
    metrics: dict[str, Any]


def load_result_dir(path: str | Path, label: str) -> list[RunMetricSet]:
    root = Path(path)
    runs: list[RunMetricSet] = []
    for result_path in sorted(root.glob("*/result.json")):
        run = _load_one(root, result_path.parent, result_path, label)
        if run is not None:
            runs.append(run)
    return runs


def _load_one(
    result_dir: Path,
    run_dir: Path,
    result_path: Path,
    label: str,
) -> RunMetricSet | None:
    result = _read_json(result_path)
    bench_metrics = _read_json(run_dir / "bench_metrics.json")
    spec = result.get("spec", {})
    if not isinstance(spec, dict):
        return None

    metrics = bench_metrics.get("metrics", {})
    return RunMetricSet(
        result_dir=result_dir,
        run_dir=run_dir,
        label=label,
        status=str(result.get("status", "UNKNOWN")),
        plan=str(spec.get("plan", "")),
        machine=str(spec.get("machine", "")),
        suite=str(spec.get("suite", "")),
        bench=str(spec.get("bench", "")),
        metric_profile=str(spec.get("metric_profile", "")),
        metric_profile_config=_as_dict(spec.get("metric_profile_config", {})),
        metrics=_as_dict(metrics),
    )


def _read_json(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}
    return data if isinstance(data, dict) else {}


def _as_dict(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}
