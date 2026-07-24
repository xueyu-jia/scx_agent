from __future__ import annotations

import io
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

    def test_target_is_required(self) -> None:
        with patch("sys.stderr", new=io.StringIO()), self.assertRaises(SystemExit):
            treatment._parser().parse_args(["--mode", "classify"])

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
                        "--target",
                        "scx-treat-b=batch",
                    ]
                )

            self.assertEqual(returncode, 0)
            value = json.loads(outcome.read_text(encoding="utf-8"))
            self.assertEqual(value["version"], 2)
            self.assertEqual(value["disposition"], "proceed")
            self.assertEqual(value["details"]["discovery"]["process_count"], 1)


class GroupProcessTest(unittest.TestCase):
    def test_group_barrier_publishes_all_comms(self) -> None:
        for iteration in range(50):
            prefix = f"g{os.getpid():x}{iteration:02x}"
            targets = (
                treatment.Target(f"{prefix}a", "latency"),
                treatment.Target(f"{prefix}b", "batch"),
            )
            processes = treatment._start_target_group(targets, 2.0)
            try:
                for target, process in processes:
                    observed = (
                        Path(f"/proc/{process.pid}/comm")
                        .read_text(encoding="utf-8")
                        .strip()
                    )
                    self.assertEqual(observed, target.comm)
            finally:
                for _target, process in processes:
                    treatment._terminate(process)


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

    def test_committed_group_episode_is_accepted(self) -> None:
        targets = (
            treatment.Target("schbench", "latency"),
            treatment.Target("stress-ng", "batch"),
        )
        audit = self._group_audit(targets)

        mutation_count = treatment._verify_group_episode(
            audit,
            7,
            {
                "phase": "committed",
                "data": {"decision": {"verdict": "improved"}},
            },
            targets,
        )

        self.assertEqual(mutation_count, 2)

    def test_split_group_activation_is_rejected(self) -> None:
        targets = (
            treatment.Target("schbench", "latency"),
            treatment.Target("stress-ng", "batch"),
        )
        audit = self._group_audit(targets)
        audit[0]["data"]["activation"]["evidence"]["unknown_comms"] = ["schbench"]

        with self.assertRaisesRegex(treatment.TreatmentError, "group activation contained"):
            treatment._verify_group_episode(
                audit,
                7,
                {
                    "phase": "committed",
                    "data": {"decision": {"verdict": "improved"}},
                },
                targets,
            )

    def test_partial_group_mutation_is_rejected(self) -> None:
        targets = (
            treatment.Target("schbench", "latency"),
            treatment.Target("stress-ng", "batch"),
        )
        audit = self._group_audit(targets)
        audit = [
            record
            for record in audit
            if record.get("data", {}).get("call_id") not in {"mutate-2"}
        ]

        with self.assertRaisesRegex(treatment.TreatmentError, "mutations differ"):
            treatment._verify_group_episode(
                audit,
                7,
                {
                    "phase": "committed",
                    "data": {"decision": {"verdict": "improved"}},
                },
                targets,
            )

    def test_extra_episode_is_rejected(self) -> None:
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
                        [
                            "--mode",
                            "classify",
                            "--duration-seconds",
                            "0.01",
                            "--target",
                            "schbench=latency",
                        ]
                    )

                self.assertEqual(returncode, 0)
                value = json.loads(outcome.read_text(encoding="utf-8"))
                self.assertEqual(value["disposition"], "unsafe")
                self.assertIn(expected, value["reason"]["message"])

    @staticmethod
    def _group_audit(targets: tuple[treatment.Target, ...]) -> list[dict[str, object]]:
        episode_id = 7
        change_ids = [f"change-{index}" for index in range(1, len(targets) + 1)]
        records: list[dict[str, object]] = [
            {
                "event": "episode_started",
                "episode_id": episode_id,
                "data": {
                    "activation": {
                        "evidence": {
                            "unknown_comms": sorted(target.comm for target in targets)
                        }
                    }
                },
            },
            {
                "event": "agent_command",
                "episode_id": episode_id,
                "data": {
                    "call_id": "begin",
                    "tool": "begin_experiment",
                    "arguments": {
                        "evaluation_contract": {
                            "measurement": {
                                "specification": {
                                    "targets": [
                                        {"comm": target.comm, "class": target.rule_class}
                                        for target in targets
                                    ]
                                }
                            }
                        }
                    },
                },
            },
        ]
        for index, (target, change_id) in enumerate(zip(targets, change_ids), start=1):
            call_id = f"mutate-{index}"
            records.extend(
                [
                    {
                        "event": "agent_command",
                        "episode_id": episode_id,
                        "data": {
                            "call_id": call_id,
                            "tool": "experiment-dynamic",
                            "arguments": {
                                "arguments": {
                                    "comm": target.comm,
                                    "class": target.rule_class,
                                }
                            },
                        },
                    },
                    {
                        "event": "agent_command_result",
                        "episode_id": episode_id,
                        "data": {
                            "call_id": call_id,
                            "ok": True,
                            "content": {
                                "change": {
                                    "capability_id": "mcp/scx-agent-classed/rule.upsert.v1",
                                    "change_id": change_id,
                                }
                            },
                        },
                    },
                ]
            )
        records.extend(
            [
                {
                    "event": "agent_command",
                    "episode_id": episode_id,
                    "data": {
                        "call_id": "commit",
                        "tool": "request_commit",
                        "arguments": {"change_ids": change_ids},
                    },
                },
                {
                    "event": "agent_command_result",
                    "episode_id": episode_id,
                    "data": {
                        "call_id": "commit",
                        "ok": True,
                        "content": {
                            "committed": True,
                            "finalized_changes": [
                                {"change_id": change_id} for change_id in change_ids
                            ],
                        },
                    },
                },
            ]
        )
        return records


if __name__ == "__main__":
    unittest.main()
