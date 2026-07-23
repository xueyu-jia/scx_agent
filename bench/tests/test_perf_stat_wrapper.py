from __future__ import annotations

import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from bench.benchmarks import perf_stat_wrapper


class PerfStatWrapperTest(unittest.TestCase):
    def test_execs_perf_without_changing_the_workload_command(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.dict(os.environ, {"SCX_BENCH_OUT": temporary}, clear=True),
                patch.object(perf_stat_wrapper.os, "execvp") as execvp,
            ):
                returncode = perf_stat_wrapper.main(
                    ["--perf", "/tmp/perf", "--", "python3", "workload.py", "--x"]
                )

        command = [
            "/tmp/perf",
            "stat",
            "-x,",
            "-o",
            str(Path(temporary) / "perf_stat.csv"),
            "-e",
            perf_stat_wrapper.DEFAULT_EVENTS,
            "--",
            "python3",
            "workload.py",
            "--x",
        ]
        execvp.assert_called_once_with("/tmp/perf", command)
        self.assertEqual(returncode, 127)

    def test_requires_a_workload_command(self) -> None:
        with self.assertRaises(SystemExit) as error:
            perf_stat_wrapper.main([])
        self.assertEqual(error.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
