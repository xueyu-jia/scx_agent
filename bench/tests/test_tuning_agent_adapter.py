from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


ADAPTER = Path("bench/integrations/tuning_agent/adapter.py").resolve()


class TuningAgentAdapterTest(unittest.TestCase):
    def test_activation_statuses_map_to_treatment_outcomes(self) -> None:
        cases = (
            ("committed", "stop", "proceed", "tuning_agent.committed"),
            ("no_commit", "proceed", "proceed", "tuning_agent.no_commit_baseline"),
            ("no_commit", "stop", "stop", "tuning_agent.no_commit"),
            (
                "recovery_required",
                "proceed",
                "unsafe",
                "tuning_agent.recovery_required",
            ),
        )
        for activation_status, policy, disposition, reason_code in cases:
            with self.subTest(activation_status=activation_status, policy=policy):
                result, outcome = self._run_adapter(
                    activation_status,
                    no_commit_disposition=policy,
                )
                self.assertEqual(result.returncode, 0, result.stderr.decode())
                self.assertEqual(outcome["version"], 2)
                self.assertEqual(outcome["disposition"], disposition)
                self.assertEqual(outcome["reason"]["code"], reason_code)
                self.assertEqual(
                    outcome["details"]["activation_response"]["status"],
                    activation_status,
                )
                self.assertEqual(outcome["details"]["cgroup_lifecycle"], "treatment")

    def test_activation_protocol_errors_fail_without_ready_outcome(self) -> None:
        result, outcome = self._run_adapter("rejected")
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(outcome)

    def test_fixed_cgroup_scope_can_be_preserved_for_later_measurement(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            scope = root / "cgroup" / "scx-bench" / "target"
            result, outcome = self._run_adapter_at(
                root,
                "committed",
                fixed_cgroup=scope,
                preserve=True,
            )

            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertIsNotNone(outcome)
            self.assertTrue(scope.is_dir())
            self.assertEqual(outcome["details"]["cgroup_path"], str(scope))
            self.assertEqual(outcome["details"]["cgroup_lifecycle"], "vm")

    def _run_adapter(
        self,
        activation_status: str,
        *,
        no_commit_disposition: str = "stop",
    ) -> tuple[subprocess.CompletedProcess[bytes], dict[str, object] | None]:
        with tempfile.TemporaryDirectory() as temp_dir:
            return self._run_adapter_at(
                Path(temp_dir),
                activation_status,
                no_commit_disposition=no_commit_disposition,
            )

    def _run_adapter_at(
        self,
        root: Path,
        activation_status: str,
        *,
        fixed_cgroup: Path | None = None,
        preserve: bool = False,
        no_commit_disposition: str = "stop",
    ) -> tuple[subprocess.CompletedProcess[bytes], dict[str, object] | None]:
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
            "SCX_TUNING_AGENT_ACTIVATE_ARGV": json.dumps([sys.executable, str(fake_activate)]),
        }
        if fixed_cgroup is not None:
            env["SCX_TUNING_AGENT_CGROUP_PATH"] = str(fixed_cgroup)
        if preserve:
            env["SCX_TUNING_AGENT_PRESERVE_CGROUP"] = "true"
        result = subprocess.run(
            [
                sys.executable,
                str(ADAPTER),
                "--no-commit-disposition",
                no_commit_disposition,
            ],
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
