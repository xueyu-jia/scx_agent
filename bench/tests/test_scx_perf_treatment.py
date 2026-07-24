from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from bench.integrations.tuning_agent import scx_perf_treatment as treatment


class TreatmentParsingTest(unittest.TestCase):
    def test_repeated_targets_are_combined(self) -> None:
        with patch.dict(os.environ, {}, clear=True):
            args = treatment._parser().parse_args(
                [
                    "--mode",
                    "control",
                    "--target",
                    "redis-server=latency",
                    "--target",
                    "hackbench=batch",
                ]
            )
            options = treatment._options(args, Path("/tmp/run/treatment"))

        self.assertEqual(
            options.targets,
            (
                treatment.Target("redis-server", "latency"),
                treatment.Target("hackbench", "batch"),
            ),
        )
        self.assertEqual(options.evidence_dir, Path("/tmp/run/real_llm"))

    def test_single_target_uses_supervisor_target_environment(self) -> None:
        with patch.dict(
            os.environ,
            {"SCX_REAL_LLM_TARGET_COMM": "redis-alt"},
            clear=True,
        ):
            args = treatment._parser().parse_args(["--mode", "classify"])
            options = treatment._options(args, Path("/tmp/run/treatment"))

        self.assertEqual(options.targets, (treatment.Target("redis-alt", "latency"),))

    def test_duplicate_target_is_rejected(self) -> None:
        args = treatment._parser().parse_args(
            ["--mode", "classify", "--target", "worker=batch", "--target", "worker=batch"]
        )
        with self.assertRaises(treatment.TreatmentError):
            treatment._options(args, Path("/tmp/run/treatment"))


class ControlTreatmentTest(unittest.TestCase):
    def test_control_runs_targets_and_writes_proceed_outcome(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outcome = root / "treatment" / "outcome.json"
            with patch.dict(
                os.environ,
                {
                    "SCX_BENCH_OUT": str(outcome.parent),
                    "SCX_BENCH_TREATMENT_OUTCOME": str(outcome),
                },
                clear=True,
            ):
                returncode = treatment.main(
                    [
                        "--mode",
                        "control",
                        "--duration-seconds",
                        "0.05",
                        "--comm",
                        "scx-treat-b",
                    ]
                )

            self.assertEqual(returncode, 0)
            value = json.loads(outcome.read_text(encoding="utf-8"))
            self.assertEqual(value["version"], 2)
            self.assertEqual(value["disposition"], "proceed")
            self.assertEqual(value["details"]["discovery"]["process_count"], 1)


class ClassificationAdmissionTest(unittest.TestCase):
    def test_scheduler_model_must_match_the_treatment_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            options = treatment.Options(
                mode="classify",
                duration_seconds=1.0,
                targets=(treatment.Target("schbench", "latency"),),
                classify_timeout_seconds=1.0,
                io_timeout_seconds=0.1,
                control_socket=root / "control.sock",
                evidence_dir=root / "evidence",
            )
            with (
                patch.dict(
                    os.environ,
                    {"SCX_PERF_EXPECTED_MODEL": "planned-model"},
                    clear=True,
                ),
                patch.object(
                    treatment,
                    "_wait_for_json",
                    return_value={"model": "unexpected-model"},
                ),
                self.assertRaisesRegex(treatment.TreatmentError, "planned-model"),
            ):
                treatment._run_classification(options)

    def test_committed_episode_and_rule_transition_are_accepted(self) -> None:
        target = treatment.Target("schbench", "latency")
        treatment._verify_rule(self._snapshot(target, learned=False), target, learned=False)
        treatment._verify_rule(self._snapshot(target, learned=True), target, learned=True)
        treatment._verify_episode(
            [
                {
                    "event": "episode_started",
                    "episode_id": 7,
                    "data": {
                        "activation": {"evidence": {"unknown_comms": [target.comm]}}
                    },
                }
            ],
            7,
            {
                "phase": "committed",
                "data": {"decision": {"verdict": "improved"}},
            },
            target,
        )

    def test_non_committed_episode_is_rejected(self) -> None:
        target = treatment.Target("schbench", "latency")
        with self.assertRaisesRegex(treatment.TreatmentError, "did not commit"):
            treatment._verify_episode(
                [
                    {
                        "event": "episode_started",
                        "episode_id": 7,
                        "data": {
                            "activation": {"evidence": {"unknown_comms": [target.comm]}}
                        },
                    }
                ],
                7,
                {"phase": "clean", "data": {}},
                target,
            )

    def test_extra_unknown_comm_episode_is_rejected(self) -> None:
        records = [
            {"event": "episode_started", "episode_id": 1},
            {"event": "episode_finished", "episode_id": 1},
            {"event": "episode_started", "episode_id": 2},
            {"event": "episode_finished", "episode_id": 2},
        ]
        with self.assertRaisesRegex(treatment.TreatmentError, "2 tuning episodes"):
            treatment._require_episode_count(records, 1)

    def test_failure_and_budget_overrun_write_unsafe(self) -> None:
        for failure, expected in (
            (treatment.TreatmentError("LLM verification failed"), "LLM verification failed"),
            (None, "exceeded the fixed treatment budget"),
        ):
            with self.subTest(expected=expected), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                outcome = root / "treatment" / "outcome.json"

                def classify(_options: treatment.Options) -> dict[str, object]:
                    if failure is not None:
                        raise failure
                    treatment.time.sleep(0.02)
                    return {}

                with (
                    patch.dict(
                        os.environ,
                        {
                            "SCX_BENCH_OUT": str(outcome.parent),
                            "SCX_BENCH_TREATMENT_OUTCOME": str(outcome),
                        },
                        clear=True,
                    ),
                    patch.object(treatment, "_run_classification", side_effect=classify),
                ):
                    returncode = treatment.main(
                        ["--mode", "classify", "--duration-seconds", "0.01"]
                    )

                self.assertEqual(returncode, 0)
                value = json.loads(outcome.read_text(encoding="utf-8"))
                self.assertEqual(value["disposition"], "unsafe")
                self.assertIn(expected, value["reason"]["message"])

    @staticmethod
    def _snapshot(target: treatment.Target, *, learned: bool) -> dict[str, object]:
        rule: dict[str, object] = {
            "comm": target.comm,
            "class": target.rule_class if learned else "batch",
            "source": "learned" if learned else "default",
            "consistent": True,
        }
        if learned:
            rule.update(
                {
                    "active_class": target.rule_class,
                    "persisted_class": target.rule_class,
                }
            )
        return {"revision": 3 if learned else 0, "rules_seq": 8 if learned else 2, "rules": [rule]}


if __name__ == "__main__":
    unittest.main()
