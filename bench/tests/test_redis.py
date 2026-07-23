from __future__ import annotations

import unittest

from bench.benchmarks.redis import parse_output


SAMPLE = """
====== SET ======
Latency by percentile distribution:
99.805% <= 0.951 milliseconds (cumulative count 1000)
100.000% <= 0.951 milliseconds (cumulative count 1000)
Summary:
  throughput summary: 71428.57 requests per second
  latency summary (msec):
          avg       min       p50       p95       p99       max
        0.095     0.008     0.087     0.199     0.639     0.951
====== GET ======
Latency by percentile distribution:
99.902% <= 0.247 milliseconds (cumulative count 1000)
Summary:
  throughput summary: 142857.14 requests per second
  latency summary (msec):
          avg       min       p50       p95       p99       max
        0.049     0.016     0.047     0.071     0.175     0.247
"""


class RedisOutputTest(unittest.TestCase):
    def test_parses_throughput_and_tail_latency(self) -> None:
        metrics = parse_output(SAMPLE)

        self.assertAlmostEqual(metrics["throughput"], 214285.71)
        self.assertEqual(metrics["set_qps"], 71428.57)
        self.assertEqual(metrics["get_qps"], 142857.14)
        self.assertEqual(metrics["p99_latency_us"], 639.0)
        self.assertEqual(metrics["p999_latency_us"], 951.0)
        self.assertEqual(metrics["set_p50_latency_us"], 87.0)


if __name__ == "__main__":
    unittest.main()
