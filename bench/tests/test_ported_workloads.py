from __future__ import annotations

import unittest

from bench.benchmarks.memtier import parse_metrics as parse_memtier
from bench.benchmarks.memory_bandwidth import parse_metrics as parse_memory_bandwidth
from bench.benchmarks.nginx_wrk2 import parse_metrics as parse_wrk2
from bench.benchmarks.rocksdb import parse_metrics as parse_rocksdb
from bench.benchmarks.sysbench_fileio import parse_metrics as parse_sysbench_fileio


class PortedWorkloadParserTest(unittest.TestCase):
    def test_rocksdb_metrics(self) -> None:
        metrics = parse_rocksdb(
            """
fillrandom   :       5.2 micros/op 192000 ops/sec; 10.1 MB/s
readrandom   :       3.1 micros/op 322000 ops/sec; 11.2 MB/s
DB size: 10 MB
"""
        )

        self.assertEqual(metrics["fillrandom_ops_per_sec"], 192000.0)
        self.assertEqual(metrics["readrandom_micros_per_op"], 3.1)
        self.assertEqual(metrics["overall_ops_per_sec"], 322000.0)
        self.assertEqual(metrics["db_size_bytes"], 10 * 1024 * 1024)

    def test_memory_bandwidth_json_metrics(self) -> None:
        metrics = parse_memory_bandwidth(
            '{"total_bandwidth_mbps": 12345.5, "test_duration": 5.0}'
        )

        self.assertEqual(metrics["bandwidth_mbps"], 12345.5)
        self.assertEqual(metrics["throughput"], 12345.5)
        self.assertEqual(metrics["execution_time_sec"], 5.0)

    def test_memtier_metrics(self) -> None:
        metrics = parse_memtier(
            """
Totals      100000.00 90000.00 10000.00 0.50 0.40 1.20 2.50 4096.00
Gets         90000.00 90000.00     0.00 0.40 0.30 1.00 2.00 2048.00
Sets         10000.00     0.00 10000.00 0.70 0.60 1.80 3.20 2048.00
"""
        )

        self.assertEqual(metrics["ops_per_second"], 100000.0)
        self.assertEqual(metrics["gets_p99_latency_ms"], 1.0)
        self.assertEqual(metrics["sets_ops_per_second"], 10000.0)

    def test_wrk2_metrics(self) -> None:
        metrics = parse_wrk2(
            """
Latency Distribution
  50.000%    1.10ms
  99.000%    8.20ms
Requests/sec: 1234.56
Transfer/sec: 1.50MB
10000 requests in 10.00s
"""
        )

        self.assertEqual(metrics["requests_per_second"], 1234.56)
        self.assertEqual(metrics["p99_latency_ms"], 8.2)
        self.assertEqual(metrics["transfer_bytes_per_sec"], 1.5 * 1024 * 1024)
        self.assertEqual(metrics["total_requests"], 10000.0)

    def test_sysbench_fileio_metrics(self) -> None:
        metrics = parse_sysbench_fileio(
            """
reads/s:                      1000.00
writes/s:                     500.00
read, MiB/s:                  10.50
written, MiB/s:               5.25
events/s (eps):               1500.00
avg:                          0.15
95th percentile:              1.20
"""
        )

        self.assertEqual(metrics["reads_per_sec"], 1000.0)
        self.assertEqual(metrics["writes_per_sec"], 500.0)
        self.assertEqual(metrics["throughput_mb_per_sec"], 15.75)
        self.assertEqual(metrics["p95_latency_ms"], 1.2)


if __name__ == "__main__":
    unittest.main()
