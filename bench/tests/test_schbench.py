from __future__ import annotations

import unittest

from bench.benchmarks.schbench import parse_metrics


SCHBENCH_OUTPUT = """
Wakeup Latencies percentiles (usec) runtime 30 (s) (100 total samples)
  50.0th: 5
  90.0th: 7
  99.0th: 11
  99.9th: 19
Request Latencies percentiles (usec) runtime 30 (s) (100 total samples)
  50.0th: 2014
  90.0th: 2040
  99.0th: 2200
  99.9th: 2600
RPS percentiles (requests) runtime 30 (s) (31 total samples)
  50.0th: 490
  90.0th: 495
current rps: 493.97
Wakeup Latencies percentiles (usec) runtime 30 (s) (101 total samples)
  50.0th: 6
  90.0th: 8
  99.0th: 12
  99.9th: 20
Request Latencies percentiles (usec) runtime 30 (s) (101 total samples)
  50.0th: 2018
  90.0th: 2050
  99.0th: 2250
  99.9th: 2700
RPS percentiles (requests) runtime 30 (s) (31 total samples)
  50.0th: 491
  90.0th: 496
average rps: 492.80
"""


class SchbenchMetricsTest(unittest.TestCase):
    def test_request_percentiles_are_not_overwritten_by_rps(self) -> None:
        metrics = parse_metrics(SCHBENCH_OUTPUT)

        self.assertEqual(metrics["p50_latency_us"], 2018.0)
        self.assertEqual(metrics["p90_latency_us"], 2050.0)
        self.assertEqual(metrics["p99_latency_us"], 2250.0)
        self.assertEqual(metrics["p999_latency_us"], 2700.0)
        self.assertEqual(metrics["request_p99_latency_us"], 2250.0)

    def test_wakeup_percentiles_are_reported_separately(self) -> None:
        metrics = parse_metrics(SCHBENCH_OUTPUT)

        self.assertEqual(metrics["wakeup_p50_latency_us"], 6.0)
        self.assertEqual(metrics["wakeup_p99_latency_us"], 12.0)
        self.assertEqual(metrics["wakeup_p999_latency_us"], 20.0)

    def test_final_average_rps_is_preferred(self) -> None:
        self.assertEqual(parse_metrics(SCHBENCH_OUTPUT)["throughput"], 492.8)

    def test_final_request_count_is_reported(self) -> None:
        self.assertEqual(parse_metrics(SCHBENCH_OUTPUT)["request_count"], 101.0)


if __name__ == "__main__":
    unittest.main()
