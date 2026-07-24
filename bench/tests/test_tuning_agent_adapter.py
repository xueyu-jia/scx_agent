from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import patch


ADAPTER = Path("bench/integrations/tuning_agent/adapter.py").resolve()


def load_adapter_module() -> object:
    spec = importlib.util.spec_from_file_location("tuning_agent_adapter_test", ADAPTER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class TuningAgentAdapterTest(unittest.TestCase):
    def test_training_readiness_validates_live_process_identity(self) -> None:
        adapter = load_adapter_module()
        identity = adapter._process_identity(os.getpid())
        ready = adapter._validate_training_ready(
            {
                "version": 1,
                "ready": True,
                "workload_digest": "sha256:" + "a" * 64,
                "processes": [identity],
            }
        )

        self.assertTrue(ready["ready"])
        stale = {**identity, "start_time_ticks": identity["start_time_ticks"] + 1}
        with self.assertRaisesRegex(adapter.AdapterError, "identity changed"):
            adapter._validate_training_ready(
                {
                    "version": 1,
                    "ready": True,
                    "workload_digest": "sha256:" + "a" * 64,
                    "processes": [stale],
                }
            )

    def test_training_readiness_rejects_unknown_fields_and_invalid_digest(self) -> None:
        adapter = load_adapter_module()
        identity = adapter._process_identity(os.getpid())
        with self.assertRaisesRegex(adapter.AdapterError, "strict V1"):
            adapter._validate_training_ready(
                {
                    "version": 1,
                    "ready": True,
                    "workload_digest": "sha256:" + "a" * 64,
                    "processes": [identity],
                    "extra": True,
                }
            )
        with self.assertRaisesRegex(adapter.AdapterError, "sha256"):
            adapter._validate_training_ready(
                {
                    "version": 1,
                    "ready": True,
                    "workload_digest": "not-a-digest",
                    "processes": [identity],
                }
            )

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

    def test_generated_config_redacts_api_key_from_bench_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, patch.dict(
            os.environ,
            {"SCX_TUNING_AGENT_LLM_API_KEY": "super-secret-test-key"},
        ):
            root = Path(temp_dir)
            result, outcome = self._run_adapter_at(root, "committed")

            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertIsNotNone(outcome)
            config = (root / "out" / "tuning-agent.toml").read_text(encoding="utf-8")
            self.assertNotIn("super-secret-test-key", config)
            self.assertIn('api_key = "<redacted>"', config)

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
