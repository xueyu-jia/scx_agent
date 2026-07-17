from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from bench.metrics import load_perf_stat_metrics


class PerfStatMetricsTest(unittest.TestCase):
    def test_parses_and_aggregates_hybrid_hardware_events(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "perf.csv"
            path.write_text(
                "10,,context-switches,1000,100.00,,\n"
                "2,,cpu-migrations,1000,100.00,,\n"
                "1000,,cpu_core/cycles/,1000,100.00,,\n"
                "500,,cpu_atom/cycles/,1000,95.00,,\n"
                "750,,instructions,1000,100.00,,\n",
                encoding="utf-8",
            )

            metrics = load_perf_stat_metrics(path)

        self.assertEqual(metrics["context_switches"], 10.0)
        self.assertEqual(metrics["migrations"], 2.0)
        self.assertEqual(metrics["cycles"], 1500.0)
        self.assertEqual(metrics["instructions"], 750.0)
        self.assertEqual(metrics["perf_hardware_running_pct_min"], 95.0)
        self.assertEqual(metrics["perf_hardware_events_valid"], 1.0)

    def test_marks_zero_and_unsupported_hardware_events_invalid(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "perf.csv"
            path.write_text(
                "0,,cpu_atom/cycles/,1000,100.00,,\n"
                "<not supported>,,cache-misses,1000,100.00,,\n",
                encoding="utf-8",
            )

            metrics = load_perf_stat_metrics(path)

        self.assertEqual(metrics["cycles"], 0.0)
        self.assertEqual(metrics["perf_hardware_invalid_events"], 2.0)
        self.assertEqual(metrics["perf_hardware_events_valid"], 0.0)


if __name__ == "__main__":
    unittest.main()
