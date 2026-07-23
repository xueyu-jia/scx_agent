from __future__ import annotations

import csv
from dataclasses import replace
import math
import tempfile
import unittest
from pathlib import Path

from bench.analysis.compare import build_analysis
from bench.analysis.loader import RunMetricSet
from bench.analysis.paired import (
    PairedAnalysisError,
    compare_paired_runs,
    describe,
    write_paired_csv,
)


PROFILE = {
    "primary": [
        {
            "name": "throughput",
            "direction": "higher",
            "regression": "5%",
            "unit": "ops/s",
        }
    ],
    "secondary": ["context_switches"],
}


def run(label: str, run_index: int, **metrics: float) -> RunMetricSet:
    path = Path(f"/{label}/run_{run_index:03d}")
    return RunMetricSet(
        result_dir=path.parent,
        run_dir=path,
        label=label,
        status="PASS",
        run_index=run_index,
        plan="plan",
        machine="small_core",
        suite="batch_suite",
        bench="batch_bench",
        metric_profile="batch_profile",
        metric_profile_config=PROFILE,
        metrics=metrics,
        returncode=0,
        vm_returncode=0,
        bench_returncode=0,
        scheduler_start_returncode=0,
        failure_reason="",
    )


class PairConstructionTest(unittest.TestCase):
    def test_pairs_are_matched_by_run_index(self) -> None:
        paired = compare_paired_runs(
            [run("default", 1, throughput=100), run("default", 2, throughput=200)],
            [run("candidate", 2, throughput=180), run("candidate", 1, throughput=110)],
            "throughput",
            "higher",
            "default",
            "candidate",
        )

        self.assertEqual([row["run_index"] for row in paired["pairs"]], [1, 2])
        self.assertEqual([row["delta"] for row in paired["pairs"]], [10.0, -20.0])
        self.assertEqual(paired["absolute"]["mean"], -5.0)
        self.assertEqual(paired["percent"]["mean"], 0.0)

    def test_missing_pair_is_rejected(self) -> None:
        with self.assertRaisesRegex(PairedAnalysisError, "unpaired runs"):
            compare_paired_runs(
                [run("default", 1, throughput=100)],
                [run("candidate", 2, throughput=90)],
                "throughput",
                "higher",
                "default",
                "candidate",
            )

    def test_duplicate_run_index_is_rejected(self) -> None:
        with self.assertRaisesRegex(PairedAnalysisError, "duplicate default"):
            compare_paired_runs(
                [run("default", 1, throughput=100), run("default", 1, throughput=101)],
                [run("candidate", 1, throughput=90)],
                "throughput",
                "higher",
                "default",
                "candidate",
            )

    def test_missing_metric_is_excluded_from_metric_pairs(self) -> None:
        paired = compare_paired_runs(
            [run("default", 1, throughput=100)],
            [run("candidate", 1)],
            "throughput",
            "higher",
            "default",
            "candidate",
        )

        self.assertEqual(paired["pairs"], [])
        self.assertIsNone(paired["absolute"])

    def test_failed_run_with_metrics_is_excluded_from_pairs(self) -> None:
        failed = replace(
            run("candidate", 2, throughput=1),
            status="SCHEDULER_FAILED",
            failure_reason="scheduler exited during measurement",
        )
        paired = compare_paired_runs(
            [run("default", 1, throughput=100), run("default", 2, throughput=100)],
            [run("candidate", 1, throughput=90), failed],
            "throughput",
            "higher",
            "default",
            "candidate",
        )

        self.assertEqual([row["run_index"] for row in paired["pairs"]], [1])
        self.assertEqual(paired["percent"]["mean"], -10.0)


class PairedStatisticsTest(unittest.TestCase):
    def test_describe_uses_student_t_confidence_interval(self) -> None:
        stats = describe([1.0, 3.0, 5.0])
        margin = 4.302652730 * 2.0 / math.sqrt(3)

        self.assertEqual(stats["n"], 3)
        self.assertEqual(stats["mean"], 3.0)
        self.assertEqual(stats["median"], 3.0)
        self.assertEqual(stats["stdev"], 2.0)
        self.assertAlmostEqual(float(stats["ci95_low"]), 3.0 - margin)
        self.assertAlmostEqual(float(stats["ci95_high"]), 3.0 + margin)


class UnifiedAnalysisTest(unittest.TestCase):
    def test_build_analysis_contains_aggregate_and_paired_results(self) -> None:
        analysis = build_analysis(
            [run("default", 1, throughput=100), run("default", 2, throughput=200)],
            [run("candidate", 1, throughput=110), run("candidate", 2, throughput=180)],
            "default",
            "candidate",
        )

        comparison = next(
            item for item in analysis["comparisons"] if item["metric"] == "throughput"
        )
        self.assertEqual(comparison["baseline"]["mean"], 150.0)
        self.assertEqual(comparison["candidate"]["mean"], 145.0)
        self.assertEqual(comparison["paired"]["absolute"]["mean"], -5.0)
        self.assertEqual(len(comparison["paired"]["pairs"]), 2)

    def test_csv_is_serialized_from_unified_analysis(self) -> None:
        analysis = build_analysis(
            [run("default", 1, throughput=100)],
            [run("candidate", 1, throughput=110)],
            "default",
            "candidate",
        )
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            write_paired_csv(analysis, output)

            with (output / "pairs.csv").open(encoding="utf-8", newline="") as stream:
                pairs = list(csv.DictReader(stream))
            with (output / "summary.csv").open(encoding="utf-8", newline="") as stream:
                summaries = list(csv.DictReader(stream))

        self.assertEqual(pairs[0]["metric"], "throughput")
        self.assertEqual(pairs[0]["run_index"], "1")
        self.assertEqual({row["change"] for row in summaries}, {"absolute", "percent"})


if __name__ == "__main__":
    unittest.main()
