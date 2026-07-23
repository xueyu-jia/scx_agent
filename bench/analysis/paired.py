from __future__ import annotations

import csv
import math
import statistics
from pathlib import Path
from typing import Any, Iterable, Sequence

from .loader import RunMetricSet

PAIR_FIELDS = (
    "baseline",
    "candidate",
    "machine",
    "suite",
    "bench",
    "metric_profile",
    "run_index",
    "metric",
    "direction",
    "baseline_value",
    "candidate_value",
    "delta",
    "delta_pct",
)

SUMMARY_FIELDS = (
    "baseline",
    "candidate",
    "machine",
    "suite",
    "bench",
    "metric_profile",
    "metric",
    "direction",
    "change",
    "n",
    "mean",
    "median",
    "stdev",
    "min",
    "max",
    "ci95_low",
    "ci95_high",
)

# Two-sided 95% Student-t critical values for 1..30 degrees of freedom.
_T_CRITICAL_975 = (
    12.706204736,
    4.302652730,
    3.182446305,
    2.776445105,
    2.570581836,
    2.446911851,
    2.364624252,
    2.306004135,
    2.262157163,
    2.228138852,
    2.200985160,
    2.178812830,
    2.160368656,
    2.144786688,
    2.131449546,
    2.119905299,
    2.109815578,
    2.100922040,
    2.093024054,
    2.085963447,
    2.079613845,
    2.073873068,
    2.068657610,
    2.063898562,
    2.059538553,
    2.055529439,
    2.051830516,
    2.048407142,
    2.045229642,
    2.042272456,
)


class PairedAnalysisError(ValueError):
    pass


def compare_paired_runs(
    baseline_runs: Sequence[RunMetricSet],
    candidate_runs: Sequence[RunMetricSet],
    metric: str,
    direction: str | None,
    baseline_label: str,
    candidate_label: str,
) -> dict[str, Any]:
    if not baseline_runs or not candidate_runs:
        return {"pairs": [], "absolute": None, "percent": None}

    baseline = _index_runs(baseline_runs, baseline_label)
    candidate = _index_runs(candidate_runs, candidate_label)
    if baseline.keys() != candidate.keys():
        raise PairedAnalysisError(
            f"unpaired runs for {metric}: "
            f"missing {baseline_label}={sorted(candidate.keys() - baseline.keys())}, "
            f"missing {candidate_label}={sorted(baseline.keys() - candidate.keys())}"
        )

    pairs = []
    for run_index in sorted(baseline):
        baseline_run = baseline[run_index]
        candidate_run = candidate[run_index]
        if baseline_run.status != "PASS" or candidate_run.status != "PASS":
            continue
        baseline_value = _number(baseline_run.metrics.get(metric))
        candidate_value = _number(candidate_run.metrics.get(metric))
        if baseline_value is None or candidate_value is None:
            continue
        delta = candidate_value - baseline_value
        identity = baseline_run
        pairs.append(
            {
                "baseline": baseline_label,
                "candidate": candidate_label,
                "machine": identity.machine,
                "suite": identity.suite,
                "bench": identity.bench,
                "metric_profile": identity.metric_profile,
                "run_index": run_index,
                "metric": metric,
                "direction": direction or "informational",
                "baseline_value": baseline_value,
                "candidate_value": candidate_value,
                "delta": delta,
                "delta_pct": (
                    delta / abs(baseline_value) * 100.0
                    if baseline_value != 0
                    else None
                ),
            }
        )

    return {
        "pairs": pairs,
        "absolute": describe([pair["delta"] for pair in pairs]),
        "percent": describe(
            [pair["delta_pct"] for pair in pairs if pair["delta_pct"] is not None]
        ),
    }


def summary_rows(analysis: dict[str, Any]) -> list[dict[str, Any]]:
    rows = []
    for comparison in analysis.get("comparisons", []):
        paired = comparison.get("paired", {})
        pairs = paired.get("pairs", [])
        if not pairs:
            continue
        identity = {field: pairs[0][field] for field in SUMMARY_FIELDS[:8]}
        for change in ("absolute", "percent"):
            stats = paired.get(change)
            if stats:
                rows.append({**identity, "change": change, **stats})
    return rows


def pair_rows(analysis: dict[str, Any]) -> list[dict[str, Any]]:
    return [
        pair
        for comparison in analysis.get("comparisons", [])
        for pair in comparison.get("paired", {}).get("pairs", [])
    ]


def write_paired_csv(analysis: dict[str, Any], output: str | Path) -> None:
    directory = Path(output)
    directory.mkdir(parents=True, exist_ok=True)
    _write_csv(directory / "pairs.csv", PAIR_FIELDS, pair_rows(analysis))
    _write_csv(directory / "summary.csv", SUMMARY_FIELDS, summary_rows(analysis))


def describe(values: Sequence[float]) -> dict[str, float | int | None] | None:
    if not values:
        return None
    sample = [float(value) for value in values]
    mean = statistics.fmean(sample)
    result: dict[str, float | int | None] = {
        "n": len(sample),
        "mean": mean,
        "median": statistics.median(sample),
        "stdev": None,
        "min": min(sample),
        "max": max(sample),
        "ci95_low": None,
        "ci95_high": None,
    }
    if len(sample) > 1:
        stdev = statistics.stdev(sample)
        margin = _t_critical_975(len(sample) - 1) * stdev / math.sqrt(len(sample))
        result.update(
            stdev=stdev,
            ci95_low=mean - margin,
            ci95_high=mean + margin,
        )
    return result


def _index_runs(
    runs: Sequence[RunMetricSet], label: str
) -> dict[int, RunMetricSet]:
    indexed = {}
    for run in runs:
        if run.run_index < 1:
            raise PairedAnalysisError(f"{run.run_dir}: invalid run_index")
        if run.run_index in indexed:
            raise PairedAnalysisError(f"duplicate {label} run_index {run.run_index}")
        indexed[run.run_index] = run
    return indexed


def _number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    return number if math.isfinite(number) else None


def _write_csv(
    path: Path, fields: Iterable[str], rows: Iterable[dict[str, Any]]
) -> None:
    with path.open("w", encoding="utf-8", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=list(fields), extrasaction="ignore")
        writer.writeheader()
        writer.writerows(rows)


def _t_critical_975(degrees_of_freedom: int) -> float:
    if degrees_of_freedom <= len(_T_CRITICAL_975):
        return _T_CRITICAL_975[degrees_of_freedom - 1]
    z = statistics.NormalDist().inv_cdf(0.975)
    v = float(degrees_of_freedom)
    return z + (z**3 + z) / (4 * v) + (5 * z**5 + 16 * z**3 + 3 * z) / (96 * v**2)
