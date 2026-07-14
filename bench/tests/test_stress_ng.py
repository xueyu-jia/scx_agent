from __future__ import annotations

import unittest

from bench.benchmarks.stress_ng import parse_metrics


STRESS_NG_OUTPUT = """
stress-ng: metrc: [581] stressor       bogo ops real time  usr time  sys time   bogo ops/s     bogo ops/s
stress-ng: metrc: [581]                           (secs)    (secs)    (secs)   (real time) (usr+sys time)
stress-ng: metrc: [581] cpu              106764     33.53     60.00      0.00      3183.67        1779.43
"""


class StressNgMetricsTest(unittest.TestCase):
    def test_reported_rates_are_preserved(self) -> None:
        metrics = parse_metrics(STRESS_NG_OUTPUT)

        self.assertEqual(metrics["bogo_ops"], 106764.0)
        self.assertEqual(metrics["stress_elapsed_time_sec"], 33.53)
        self.assertEqual(metrics["stress_reported_throughput"], 3183.67)
        self.assertEqual(metrics["cpu_time_throughput"], 1779.43)
        self.assertEqual(metrics["throughput"], 3183.67)

    def test_wrapper_monotonic_time_drives_wall_throughput(self) -> None:
        metrics = parse_metrics(STRESS_NG_OUTPUT, elapsed_time_sec=30.144816276)

        self.assertAlmostEqual(metrics["throughput"], 3541.70, places=2)


if __name__ == "__main__":
    unittest.main()
