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
    baseline_run_summary = _run_summary(baseline_group, metric_spec.name)
    candidate_run_summary = _run_summary(candidate_group, metric_spec.name)
    run_status = _run_status(baseline_run_summary, candidate_run_summary)
    baseline_stats = _stats(baseline_values, baseline_run_summary)
    candidate_stats = _stats(candidate_values, candidate_run_summary)
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
        "run_status": run_status,
        "failure_reason": _failure_reason(baseline_run_summary, candidate_run_summary),
        "delta_pct": delta_pct,
        "verdict": _verdict(
            metric_spec,
            baseline_values,
            candidate_values,
            delta_pct,
            run_status,
        ),
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


def _run_summary(runs: list[RunMetricSet], metric: str) -> dict[str, Any]:
    passed = [run for run in runs if run.status in VALID_STATUSES]
    failed = [run for run in runs if run.status not in VALID_STATUSES]
    metric_missing = [
        run
        for run in passed
        if not _is_number(run.metrics.get(metric))
    ]
    return {
        "total": len(runs),
        "passed": len(passed),
        "failed": len(failed),
        "metric_missing": len(metric_missing),
        "failed_runs": [
            {
                "label": run.label,
                "status": run.status,
                "path": str(run.run_dir),
                "reason": run.failure_reason,
            }
            for run in failed
        ],
    }


def _run_status(
    baseline: dict[str, Any],
    candidate: dict[str, Any],
) -> str:
    if _all_failed(baseline) or _all_failed(candidate):
        return "failed"
    if baseline["failed"] > 0 or candidate["failed"] > 0:
        return "partial_failed"
    return "ok"


def _all_failed(summary: dict[str, Any]) -> bool:
    return summary["total"] > 0 and summary["passed"] == 0


def _failure_reason(
    baseline: dict[str, Any],
    candidate: dict[str, Any],
) -> str:
    reasons = []
    for side, summary in (("baseline", baseline), ("candidate", candidate)):
        failed_runs = summary.get("failed_runs", [])
        if not failed_runs:
            continue
        first = failed_runs[0]
        reason = first.get("reason") or first.get("status") or "failed"
        reasons.append(f"{side}: {reason}")
    return "; ".join(reasons)


def _stats(values: list[float], run_summary: dict[str, Any]) -> dict[str, Any]:
    base = {
        "values": values,
        "count": len(values),
        "total_runs": run_summary["total"],
        "passed_runs": run_summary["passed"],
        "failed_runs": run_summary["failed"],
        "metric_missing_runs": run_summary["metric_missing"],
    }
    if not values:
        return base
    return {
        **base,
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
    run_status: str,
) -> str:
    if run_status == "failed":
        return "failed"
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
        "primary_failed": sum(1 for item in primary if item["run_status"] == "failed"),
        "primary_partial_failed": sum(
            1 for item in primary if item["run_status"] == "partial_failed"
        ),
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
                "reason": run.failure_reason,
                "path": str(run.run_dir),
            }
        )
    return invalid


def _count(comparisons: list[dict[str, Any]], verdict: str) -> int:
    return sum(1 for item in comparisons if item["verdict"] == verdict)


def _is_number(value: Any) -> bool:
    if isinstance(value, bool):
        return False
    return isinstance(value, (int, float)) and math.isfinite(float(value))


def _default_chart(metric_name: str) -> str:
    lower = metric_name.lower()
    if "latency" in lower or "wait" in lower:
        return "latency_bar"
    return "delta_bar"
