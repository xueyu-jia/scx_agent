from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


HARNESS = Path("bench/treatments/tuning_agent_harness.py").resolve()


class TuningAgentHarnessTest(unittest.TestCase):
    def test_activation_statuses_map_to_treatment_outcomes(self) -> None:
        cases = {
            "committed": "ready",
            "no_commit": "no_commit",
            "recovery_required": "recovery_required",
        }
        for activation_status, treatment_status in cases.items():
            with self.subTest(activation_status=activation_status):
                result, outcome = self._run_harness(activation_status)
                self.assertEqual(result.returncode, 0, result.stderr.decode())
                self.assertEqual(outcome["version"], 1)
                self.assertEqual(outcome["status"], treatment_status)
                self.assertEqual(
                    outcome["details"]["activation_response"]["status"],
                    activation_status,
                )

    def test_activation_protocol_errors_fail_without_ready_outcome(self) -> None:
        result, outcome = self._run_harness("rejected")
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(outcome)

    def _run_harness(
        self,
        activation_status: str,
    ) -> tuple[subprocess.CompletedProcess[bytes], dict[str, object] | None]:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_activate = root / "fake_activate.py"
            fake_activate.write_text(
                textwrap.dedent(
                    f"""\
                    #!/usr/bin/env python3
                    import json
                    print(json.dumps({{
                        "version": 1,
                        "request_id": "test",
                        "status": {activation_status!r},
                        "accepted": True,
                    }}))
                    """
                ),
                encoding="utf-8",
            )
            fake_activate.chmod(0o755)
            cgroup_root = root / "cgroup"
            cgroup_root.mkdir()
            outcome_path = root / "outcome.json"
            env = {
                **os.environ,
                "SCX_BENCH_OUT": str(root / "out"),
                "SCX_BENCH_TREATMENT_OUTCOME": str(outcome_path),
                "SCX_TUNING_AGENT_CGROUP_ROOT": str(cgroup_root),
                "SCX_TUNING_AGENT_START_DAEMON": "0",
                "SCX_TUNING_AGENT_ACTIVATE_ARGV": json.dumps(
                    [sys.executable, str(fake_activate)]
                ),
            }
            result = subprocess.run(
                [sys.executable, str(HARNESS)],
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            if outcome_path.exists():
                outcome = json.loads(outcome_path.read_text(encoding="utf-8"))
            else:
                outcome = None
            return result, outcome


if __name__ == "__main__":
    unittest.main()
