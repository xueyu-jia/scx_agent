from __future__ import annotations

import math
import statistics
from dataclasses import dataclass
from typing import Any

from .loader import RunMetricSet


VALID_STATUSES = {"PASS"}


@dataclass(frozen=True)
class MetricSpec:
    name: str
    role: str
    direction: str | None
    regression: str | None
    unit: str
    chart: str


def build_analysis(
    baseline_runs: list[RunMetricSet],
    candidate_runs: list[RunMetricSet],
    baseline_label: str = "baseline",
    candidate_label: str = "candidate",
) -> dict[str, Any]:
    comparisons = []
    for key in sorted(_comparison_keys(baseline_runs, candidate_runs)):
        baseline_group = _select_group(baseline_runs, key)
        candidate_group = _select_group(candidate_runs, key)
        profile = _first_profile(baseline_group, candidate_group)

        for metric_spec in _metric_specs(profile):
            comparisons.append(
                _build_comparison(key, metric_spec, baseline_group, candidate_group)
            )

    return {
        "baseline_label": baseline_label,
        "candidate_label": candidate_label,
        "summary": _summary(comparisons, baseline_runs, candidate_runs),
        "comparisons": comparisons,
        "invalid_runs": _invalid_runs(baseline_runs, candidate_runs),
    }


def _comparison_keys(
    baseline_runs: list[RunMetricSet],
    candidate_runs: list[RunMetricSet],
) -> set[tuple[str, str, str, str]]:
    return {
        (run.machine, run.suite, run.bench, run.metric_profile)
        for run in [*baseline_runs, *candidate_runs]
    }


def _select_group(
    runs: list[RunMetricSet],
    key: tuple[str, str, str, str],
) -> list[RunMetricSet]:
    machine, suite, bench, profile = key
    return [
        run
        for run in runs
        if (run.machine, run.suite, run.bench, run.metric_profile)
        == (machine, suite, bench, profile)
    ]


def _first_profile(
    baseline_group: list[RunMetricSet],
    candidate_group: list[RunMetricSet],
) -> dict[str, Any]:
    for run in [*baseline_group, *candidate_group]:
        if run.metric_profile_config:
            return run.metric_profile_config
    return {}


def _metric_specs(profile: dict[str, Any]) -> list[MetricSpec]:
    specs: list[MetricSpec] = []
    for metric in profile.get("primary", []):
        if isinstance(metric, dict):
            specs.append(
                MetricSpec(
                    name=str(metric["name"]),
                    role="primary",
                    direction=metric.get("direction"),
                    regression=metric.get("regression"),
                    unit=str(metric.get("unit", "")),
                    chart=str(metric.get("chart", _default_chart(metric["name"]))),
                )
            )

    for metric in profile.get("secondary", []):
        if isinstance(metric, str):
            specs.append(
                MetricSpec(
                    name=metric,
                    role="secondary",
                    direction=None,
                    regression=None,
                    unit="",
                    chart=_default_chart(metric),
                )
            )
    return specs


def _build_comparison(
    key: tuple[str, str, str, str],
    metric_spec: MetricSpec,
    baseline_group: list[RunMetricSet],
    candidate_group: list[RunMetricSet],
) -> dict[str, Any]:
    machine, suite, bench, profile = key
    baseline_values = _values(baseline_group, metric_spec.name)
    candidate_values = _values(candidate_group, metric_spec.name)
    baseline_stats = _stats(baseline_values)
    candidate_stats = _stats(candidate_values)
    delta_pct = _delta_pct(baseline_stats.get("mean"), candidate_stats.get("mean"))

    return {
        "machine": machine,
        "suite": suite,
        "bench": bench,
        "metric_profile": profile,
        "metric": metric_spec.name,
        "role": metric_spec.role,
        "direction": metric_spec.direction,
        "unit": metric_spec.unit,
        "chart": metric_spec.chart,
        "baseline": baseline_stats,
        "candidate": candidate_stats,
        "delta_pct": delta_pct,
        "verdict": _verdict(metric_spec, baseline_values, candidate_values, delta_pct),
    }


def _values(runs: list[RunMetricSet], metric: str) -> list[float]:
    values: list[float] = []
    for run in runs:
        if run.status not in VALID_STATUSES:
            continue
        value = run.metrics.get(metric)
        if isinstance(value, bool):
            continue
        if isinstance(value, (int, float)) and math.isfinite(float(value)):
            values.append(float(value))
    return values


def _stats(values: list[float]) -> dict[str, Any]:
    if not values:
        return {"values": [], "count": 0}
    return {
        "values": values,
        "count": len(values),
        "mean": statistics.fmean(values),
        "median": statistics.median(values),
        "stdev": statistics.stdev(values) if len(values) > 1 else 0.0,
        "min": min(values),
        "max": max(values),
    }


def _delta_pct(baseline_mean: Any, candidate_mean: Any) -> float | None:
    if not isinstance(baseline_mean, (int, float)):
        return None
    if not isinstance(candidate_mean, (int, float)):
        return None
    if baseline_mean == 0:
        return None
    return (candidate_mean - baseline_mean) / abs(baseline_mean) * 100.0


def _verdict(
    metric_spec: MetricSpec,
    baseline_values: list[float],
    candidate_values: list[float],
    delta_pct: float | None,
) -> str:
    if not baseline_values or not candidate_values:
        return "missing"
    if metric_spec.direction not in {"higher", "lower"} or delta_pct is None:
        return "informational"

    threshold = abs(_parse_percent(metric_spec.regression) or 0.0)
    if metric_spec.direction == "higher":
        if delta_pct <= -threshold:
            return "regression"
        if delta_pct >= threshold:
            return "improvement"
    else:
        if delta_pct >= threshold:
            return "regression"
        if delta_pct <= -threshold:
            return "improvement"
    return "no_change"


def _parse_percent(value: str | None) -> float | None:
    if not value:
        return None
    text = value.strip()
    if text.endswith("%"):
        text = text[:-1]
    try:
        return float(text)
    except ValueError:
        return None


def _summary(
    comparisons: list[dict[str, Any]],
    baseline_runs: list[RunMetricSet],
    candidate_runs: list[RunMetricSet],
) -> dict[str, Any]:
    primary = [item for item in comparisons if item["role"] == "primary"]
    return {
        "primary_total": len(primary),
        "primary_improvements": _count(primary, "improvement"),
        "primary_regressions": _count(primary, "regression"),
        "primary_no_change": _count(primary, "no_change"),
        "primary_missing": _count(primary, "missing"),
        "invalid_runs": len(_invalid_runs(baseline_runs, candidate_runs)),
    }


def _invalid_runs(
    baseline_runs: list[RunMetricSet],
    candidate_runs: list[RunMetricSet],
) -> list[dict[str, str]]:
    invalid = []
    for run in [*baseline_runs, *candidate_runs]:
        if run.status in VALID_STATUSES:
            continue
        invalid.append(
            {
                "label": run.label,
                "status": run.status,
                "machine": run.machine,
                "suite": run.suite,
                "bench": run.bench,
                "path": str(run.run_dir),
            }
        )
    return invalid


def _count(comparisons: list[dict[str, Any]], verdict: str) -> int:
    return sum(1 for item in comparisons if item["verdict"] == verdict)


def _default_chart(metric_name: str) -> str:
    lower = metric_name.lower()
    if "latency" in lower or "wait" in lower:
        return "latency_bar"
    return "delta_bar"
