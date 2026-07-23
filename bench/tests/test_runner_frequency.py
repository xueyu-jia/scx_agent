from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from bench.core.runner import _check_fixed_frequency


class FixedFrequencyPreflightTest(unittest.TestCase):
    def _root(self, directory: str, *, no_turbo: str = "1") -> Path:
        root = Path(directory)
        cpufreq = root / "cpu2" / "cpufreq"
        cpufreq.mkdir(parents=True)
        (cpufreq / "scaling_min_freq").write_text("2500000\n", encoding="utf-8")
        (cpufreq / "scaling_max_freq").write_text("2500000\n", encoding="utf-8")
        (cpufreq / "scaling_governor").write_text("performance\n", encoding="utf-8")
        intel_pstate = root / "intel_pstate"
        intel_pstate.mkdir()
        (intel_pstate / "no_turbo").write_text(f"{no_turbo}\n", encoding="utf-8")
        return root

    def test_exact_non_turbo_frequency_passes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self._root(directory)

            errors = _check_fixed_frequency(
                [2],
                {
                    "fixed": True,
                    "governor": "performance",
                    "target_khz": 2_500_000,
                    "turbo": False,
                },
                root,
            )

        self.assertEqual(errors, [])

    def test_target_and_turbo_mismatches_fail(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = self._root(directory, no_turbo="0")

            errors = _check_fixed_frequency(
                [2],
                {"fixed": True, "target_khz": 2_600_000, "turbo": False},
                root,
            )

        self.assertTrue(any("expected 2600000 kHz" in error for error in errors))
        self.assertIn("CPU turbo is enabled, expected disabled", errors)


if __name__ == "__main__":
    unittest.main()
