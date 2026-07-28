from __future__ import annotations

import os
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import TestCase
from unittest.mock import call, patch

from bench.env import workloads


class WorkloadBuildTest(TestCase):
    def test_stress_ng_uses_a_stable_release(self) -> None:
        self.assertEqual(workloads.WORKLOADS["stress-ng"].ref, "V0.21.04")

    @patch("bench.env.workloads.install")
    @patch("bench.env.workloads.run")
    def test_stress_ng_rebuilds_feature_detection(
        self,
        run_mock,
        install_mock,
    ) -> None:
        with TemporaryDirectory() as temp_dir:
            source = Path(temp_dir) / "stress-ng"
            source.mkdir()
            binary = source / "stress-ng"
            binary.touch()

            workloads.build("stress-ng", source)

            self.assertEqual(
                run_mock.call_args_list,
                [
                    call(["make", "clean"], cwd=source),
                    call(["make", f"-j{os.cpu_count() or 1}"], cwd=source),
                ],
            )
            install_mock.assert_called_once_with(
                binary,
                workloads.BIN / "stress-ng",
            )
