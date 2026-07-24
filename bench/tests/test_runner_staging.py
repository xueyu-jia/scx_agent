from __future__ import annotations

from contextlib import redirect_stdout
import hashlib
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import yaml

from bench.core.guest_plan import build_guest_run_plan
from bench.core.config import CONFIG_PART_KEYS, RunSpec
from bench.core.runner import (
    GUEST_SCHEDULER_KCONFIG_PATH,
    GUEST_SCHEDULER_PATH,
    GUEST_SCHEDULER_SUPPORT_DIR,
    REPO_ROOT,
    _guest_scheduler,
    _guest_treatment,
    _guest_run_status,
    _manifest_entry,
    _resolve_host_path,
    _run_libvirt,
    _run_one,
    _stage_scheduler,
)
from bench.scripts.run import _build_pairs, _variant, main as run_main


class RunnerSchedulerStagingTest(unittest.TestCase):
    def test_guest_scheduler_uses_staged_command_without_mutating_input(self) -> None:
        scheduler = {
            "kind": "scx",
            "command": "guest/scx_test",
            "host_command": "build/scx_test",
            "host_kconfig": "build/kernel.config",
            "host_support_files": ["build/scx_agent_classed_mcp"],
            "args": ["--test"],
        }

        guest = _guest_scheduler(scheduler)

        self.assertEqual(guest["command"], GUEST_SCHEDULER_PATH)
        self.assertEqual(guest["args"], ["--test"])
        self.assertNotIn("host_command", guest)
        self.assertNotIn("host_kconfig", guest)
        self.assertNotIn("host_support_files", guest)
        self.assertEqual(scheduler["command"], "guest/scx_test")

    def test_relative_host_path_is_resolved_from_repository_root(self) -> None:
        self.assertEqual(
            _resolve_host_path("schedule/scx_agent_classed"),
            (REPO_ROOT / Path("schedule/scx_agent_classed")).resolve(),
        )

    def test_scheduler_support_files_are_staged_and_hashed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            scheduler = root / "scheduler"
            scheduler.write_bytes(b"scheduler-binary")
            os.chmod(scheduler, 0o755)
            kconfig = root / "kernel.config"
            kconfig.write_bytes(b"CONFIG_SCHED_CLASS_EXT=y\n")
            support = root / "scx_agent_classed_mcp"
            support.write_bytes(b"mcp-provider")

            with patch("bench.core.runner._scp_to_guest") as scp, patch(
                "bench.core.runner._run_command"
            ):
                artifact = _stage_scheduler(
                    {"ssh_user": "root", "ssh_key": "/tmp/test-key"},
                    "192.0.2.1",
                    22,
                    str(scheduler),
                    str(kconfig),
                    [str(support)],
                    [],
                    [],
                )

        self.assertEqual(artifact["guest_path"], GUEST_SCHEDULER_PATH)
        self.assertEqual(
            artifact["sha256"], hashlib.sha256(b"scheduler-binary").hexdigest()
        )
        self.assertEqual(
            artifact["kconfig"],
            {
                "source": str(kconfig),
                "guest_path": GUEST_SCHEDULER_KCONFIG_PATH,
                "sha256": hashlib.sha256(b"CONFIG_SCHED_CLASS_EXT=y\n").hexdigest(),
            },
        )
        self.assertEqual(
            artifact["support_files"][support.name],
            {
                "source": str(support),
                "guest_path": f"{GUEST_SCHEDULER_SUPPORT_DIR}/{support.name}",
                "sha256": hashlib.sha256(b"mcp-provider").hexdigest(),
            },
        )
        self.assertEqual(
            [call.args[4] for call in scp.call_args_list],
            [
                GUEST_SCHEDULER_PATH,
                GUEST_SCHEDULER_KCONFIG_PATH,
                f"{GUEST_SCHEDULER_SUPPORT_DIR}/{support.name}",
            ],
        )

    def test_run_metadata_records_scheduler_artifacts(self) -> None:
        scheduler = {
            "kind": "scx",
            "command": "guest/scx_test",
            "host_command": "build/scx_test",
            "host_support_files": ["build/scx_agent_classed_mcp"],
        }
        scheduler_artifact = {
            "guest_path": GUEST_SCHEDULER_PATH,
            "sha256": "scheduler-sha256",
            "support_files": {
                "scx_agent_classed_mcp": {
                    "guest_path": f"{GUEST_SCHEDULER_SUPPORT_DIR}/scx_agent_classed_mcp",
                    "sha256": "support-sha256",
                }
            },
        }
        completed = {
            "status": None,
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "scheduler_artifact": scheduler_artifact,
        }
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            patch("bench.core.runner._preflight_machine"),
            patch("bench.core.runner._run_libvirt", return_value=completed) as run_libvirt,
            patch("bench.core.runner._read_guest_result", return_value={"status": "PASS"}),
            patch("bench.core.runner.load_bench_metrics", return_value={"metrics": {}}),
            patch("bench.core.runner.load_perf_stat_metrics", return_value={}),
        ):
            result = _run_one(
                RunnerExecutionPlanTest()._spec(),
                Path(temp_dir),
                False,
                "candidate",
                scheduler,
                None,
                30,
                None,
            )

        self.assertEqual(result["scheduler_artifact"], scheduler_artifact)
        self.assertEqual(
            run_libvirt.call_args.kwargs["scheduler_host_support_files"],
            ["build/scx_agent_classed_mcp"],
        )
        self.assertNotIn("host_command", result["execution_plan"]["scheduler"])
        self.assertNotIn("host_support_files", result["execution_plan"]["scheduler"])

    def test_guest_treatment_uses_staged_command_without_mutating_input(self) -> None:
        treatment = {
            "command": "guest/tune",
            "host_command": "bench/integrations/tuning_agent/adapter.py",
            "host_support_files": ["bench/integrations/tuning_agent/mock_llm.py"],
            "args": ["--test"],
        }

        guest = _guest_treatment(treatment)

        self.assertEqual(guest["command"], "/tmp/scx-bench-treatment")
        self.assertEqual(guest["args"], ["--test"])
        self.assertNotIn("host_command", guest)
        self.assertNotIn("host_support_files", guest)
        self.assertEqual(treatment["command"], "guest/tune")


class RunnerVariantTest(unittest.TestCase):
    def test_same_scheduler_can_compare_distinct_treatments(self) -> None:
        scheduler = {"kind": "builtin", "name": "default"}
        control = _variant(
            "baseline",
            "default",
            scheduler,
            "control",
            {"command": "control"},
        )
        tuned = _variant(
            "candidate",
            "default",
            scheduler,
            "agent_tuned",
            {"command": "tune"},
        )
        spec = RunnerExecutionPlanTest()._spec()

        first = _build_pairs([spec], control, tuned, "alternating")[0]
        second_spec = RunSpec(**{**spec.__dict__, "run_index": 2})
        second = _build_pairs([second_spec], control, tuned, "alternating")[0]

        self.assertEqual(control.label, "default__control")
        self.assertEqual(tuned.label, "default__agent_tuned")
        self.assertEqual([variant.role for variant in first.order], ["baseline", "candidate"])
        self.assertEqual([variant.role for variant in second.order], ["candidate", "baseline"])


class RunnerExecutionPlanTest(unittest.TestCase):
    def _spec(self) -> RunSpec:
        return RunSpec(
            plan="warmup-test",
            run_index=1,
            machine_name="small",
            suite_name="suite",
            bench_name="bench",
            metric_profile_name="metrics",
            machine={"memory": "1G", "vcpus": 1, "pin_cpus": "0"},
            suite={},
            bench={
                "host_support_files": ["bench/scenarios/redis_cpu/workload.py"],
                "measurement": {
                    "command": "measure",
                    "args": ["--seconds", "10"],
                    "timeout_seconds": 20,
                },
                "warmup": {
                    "command": "prime",
                    "args": ["--seconds", "5"],
                    "timeout_seconds": 7,
                },
                "post_warmup_settle_seconds": 3,
                "cooldown_seconds": 1,
            },
            metric_profile={},
            libvirt={
                "workdir": "/guest/work",
                "guest_output_dir": "/tmp/output",
                "emulator_cpus": "0",
                "root_image": "/unused.qcow2",
                "network": None,
                "vm_settle_seconds": 2,
                "timeout_extra_seconds": 5,
            },
            executor={},
        )

    def test_dry_run_exposes_and_passes_explicit_warmup_plan(self) -> None:
        spec = self._spec()
        scheduler = {"kind": "builtin"}
        with tempfile.TemporaryDirectory() as temp_dir, patch(
            "bench.core.runner.write_guest_plan"
        ) as write_guest, patch("bench.core.runner._run_libvirt") as run_libvirt:
            result = _run_one(
                spec,
                Path(temp_dir),
                True,
                "dry-run",
                scheduler,
                None,
                30,
                None,
            )

        self.assertEqual(result["status"], "DRY_RUN")
        execution_plan = result["execution_plan"]
        self.assertEqual(execution_plan["warmup"]["argv"], ["prime", "--seconds", "5"])
        self.assertEqual(execution_plan["warmup"]["timeout_seconds"], 7)
        self.assertEqual(execution_plan["post_warmup_settle_seconds"], 3)
        self.assertNotIn("host_support_files", execution_plan)
        self.assertEqual(
            result["spec"]["bench_config"]["host_support_files"],
            ["bench/scenarios/redis_cpu/workload.py"],
        )
        written_plan = write_guest.call_args.args[1]
        self.assertEqual(written_plan.to_dict(), execution_plan)
        self.assertEqual(
            _manifest_entry(spec, label="dry-run", scheduler=scheduler)["execution_plan"],
            execution_plan,
        )
        run_libvirt.assert_not_called()

    def test_host_timeout_includes_every_bounded_phase(self) -> None:
        scheduler = {
            "kind": "scx",
            "command": "scheduler",
            "settle_seconds": 4,
        }
        plan = build_guest_run_plan(self._spec().bench, scheduler, self._spec().libvirt)

        self.assertEqual(
            plan.host_timeout_seconds(extra_seconds=5),
            43,
        )

    def test_dry_run_carries_treatment_identity_and_timeout(self) -> None:
        treatment = {
            "command": "prepare",
            "host_command": "bench/integrations/tuning_agent/adapter.py",
            "host_support_files": ["bench/integrations/tuning_agent/mock_llm.py"],
            "args": ["--mode", "agent"],
            "env": {"MODE": "agent"},
            "timeout_seconds": 12,
            "post_treatment_settle_seconds": 3,
        }
        with tempfile.TemporaryDirectory() as temp_dir:
            result = _run_one(
                self._spec(),
                Path(temp_dir),
                True,
                "scx__agent",
                {"kind": "builtin"},
                None,
                30,
                None,
                "candidate",
                "agent",
                treatment,
            )

        execution_plan = result["execution_plan"]
        self.assertEqual(
            execution_plan["run_context"],
            {
                "role": "candidate",
                "variant": "scx__agent",
                "treatment": "agent",
            },
        )
        self.assertEqual(
            execution_plan["treatment"]["argv"],
            ["/tmp/scx-bench-treatment", "--mode", "agent"],
        )
        self.assertEqual(execution_plan["post_treatment_settle_seconds"], 3)
        self.assertEqual(result["spec"]["treatment_name"], "agent")
        self.assertEqual(
            result["spec"]["treatment_config"]["host_command"],
            "bench/integrations/tuning_agent/adapter.py",
        )
        self.assertNotIn("host_support_files", execution_plan["treatment"])

    def test_guest_top_level_status_is_authoritative(self) -> None:
        for guest_status in (
            "SCHEDULER_FAILED",
            "TREATMENT_FAILED",
            "TREATMENT_TIMEOUT",
            "TREATMENT_STOPPED",
            "TREATMENT_UNSAFE_STATE",
            "WARMUP_FAILED",
            "WARMUP_TIMEOUT",
            "BENCH_FAILED",
            "BENCH_TIMEOUT",
            "INTERNAL_ERROR",
        ):
            with self.subTest(guest_status=guest_status):
                self.assertEqual(
                    _guest_run_status(125, {"status": guest_status}, {}),
                    guest_status,
                )

        self.assertEqual(_guest_run_status(0, {"status": "unknown"}, {}), "FAILED")
        self.assertEqual(_guest_run_status(1, {"status": "PASS"}, {}), "FAILED")

    def test_structured_guest_result_is_preserved_in_metadata(self) -> None:
        guest_result = {
            "status": "WARMUP_TIMEOUT",
            "failure_reason": "warmup timed out",
            "phases": {
                "scheduler": {"status": "SKIPPED"},
                "warmup": {
                    "status": "TIMEOUT",
                    "returncode": 124,
                    "timed_out": True,
                },
                "measurement": {"status": "SKIPPED", "returncode": None},
            },
        }
        libvirt_result = {
            "status": None,
            "returncode": 124,
            "stdout": "",
            "stderr": "",
        }
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            patch("bench.core.runner._preflight_machine"),
            patch("bench.core.runner._run_libvirt", return_value=libvirt_result),
            patch("bench.core.runner._read_guest_result", return_value=guest_result),
        ):
            result = _run_one(
                self._spec(),
                Path(temp_dir),
                False,
                "candidate",
                {"kind": "builtin"},
                None,
                30,
                None,
            )

        self.assertEqual(result["status"], "WARMUP_TIMEOUT")
        self.assertEqual(result["failure_reason"], "warmup timed out")
        self.assertEqual(result["guest_result"], guest_result)
        self.assertNotIn("warmup_status", result)


class RunnerGuestTransferTest(unittest.TestCase):
    def test_dry_run_does_not_enter_libvirt_execution(self) -> None:
        spec = RunSpec(
            plan="test",
            run_index=1,
            machine_name="small",
            suite_name="suite",
            bench_name="bench",
            metric_profile_name="metrics",
            machine={"memory": "1G", "vcpus": 1, "pin_cpus": "0"},
            suite={},
            bench={
                "measurement": {
                    "command": "true",
                    "timeout_seconds": 1,
                }
            },
            metric_profile={},
            libvirt={
                "workdir": "/guest/work",
                "guest_output_dir": "/tmp/output",
                "emulator_cpus": "0",
                "root_image": "/unused.qcow2",
                "network": None,
            },
            executor={},
        )
        with tempfile.TemporaryDirectory() as temp_dir, patch(
            "bench.core.runner._run_libvirt"
        ) as run_libvirt:
            result = _run_one(
                spec,
                Path(temp_dir),
                True,
                "dry-run",
                {"kind": "builtin"},
                None,
                30,
                None,
            )

        self.assertEqual(result["status"], "DRY_RUN")
        run_libvirt.assert_not_called()

    def test_executor_upload_failure_prevents_guest_execution(self) -> None:
        spec = RunSpec(
            plan="test",
            run_index=1,
            machine_name="small",
            suite_name="suite",
            bench_name="bench",
            metric_profile_name="metrics",
            machine={"memory": "1G", "vcpus": 1, "pin_cpus": "0"},
            suite={},
            bench={
                "measurement": {
                    "command": "true",
                    "timeout_seconds": 1,
                }
            },
            metric_profile={},
            libvirt={"destroy_on_exit": True},
            executor={},
        )
        scp = Mock(side_effect=RuntimeError("executor upload failed"))
        run_guest = Mock()
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            patch("bench.core.runner._prepare_runtime_dir"),
            patch("bench.core.runner._create_overlay"),
            patch("bench.core.runner._run_command"),
            patch("bench.core.runner._apply_host_thread_pinning", return_value={}),
            patch("bench.core.runner._wait_for_ssh", return_value=("192.0.2.1", 22)),
            patch("bench.core.runner._scp_to_guest", scp),
            patch("bench.core.runner._run_guest_command", run_guest),
            patch("bench.core.runner._cleanup_domain"),
            patch("bench.core.runner._cleanup_runtime_dir"),
        ):
            root = Path(temp_dir)
            result = _run_libvirt(
                spec,
                root,
                root / "runtime",
                "test-domain",
                root / "domain.xml",
                root / "disk.qcow2",
                root / "guest_plan.json",
                "/tmp/output",
                scheduler_host_command=None,
                scheduler_host_kconfig=None,
                timeout=1,
                boot_timeout=1,
                progress_interval=0,
                heartbeat=lambda _elapsed: None,
            )

        self.assertEqual(result["status"], "LIBVIRT_FAILED")
        self.assertIn("executor upload failed", result["stderr"])
        scp.assert_called_once()
        run_guest.assert_not_called()


class RunScriptTreatmentIntegrationTest(unittest.TestCase):
    def test_dry_run_compares_treatments_on_the_same_scheduler(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            kernel_source = root / "linux"
            (kernel_source / "tools" / "perf").mkdir(parents=True)
            config_path = root / "config"
            output = root / "results"
            config = {
                "libvirt": {
                    "root_image": str(root / "base.qcow2"),
                    "kernel_source": str(kernel_source),
                    "ssh_user": "root",
                    "ssh_key": str(root / "key"),
                    "workdir": "/guest/work",
                    "guest_output_dir": "/scx_bench_out",
                    "emulator_cpus": "0",
                    "network": None,
                },
                "schedulers": {"default": {"kind": "builtin"}},
                "treatments": {
                    "control": {
                        "command": "control",
                        "timeout_seconds": 10,
                    },
                    "agent": {
                        "command": "agent",
                        "timeout_seconds": 20,
                    },
                },
                "plans": {
                    "test": {
                        "runs": 1,
                        "matrix": [{"machine": "small", "suites": ["suite"]}],
                    }
                },
                "machines": {
                    "small": {
                        "vcpus": 1,
                        "memory": "1G",
                        "pin_cpus": "0",
                        "exclusive": True,
                        "frequency": {"fixed": True},
                    }
                },
                "suites": {
                    "suite": {
                        "benches": ["bench"],
                        "metric_profile": "metrics",
                    }
                },
                "metric_profiles": {
                    "metrics": {
                        "primary": [
                            {
                                "name": "throughput",
                                "direction": "higher",
                            }
                        ]
                    }
                },
                "benches": {
                    "bench": {
                        "measurement": {
                            "command": "measure",
                            "timeout_seconds": 30,
                        }
                    }
                },
            }
            config_path.mkdir()
            for part_name, keys in CONFIG_PART_KEYS:
                part = {key: config[key] for key in keys if key in config}
                (config_path / part_name).write_text(
                    yaml.safe_dump(part, sort_keys=False),
                    encoding="utf-8",
                )

            with (
                patch(
                    "bench.scripts.run._update_latest_report_link",
                    return_value=output / "report.html",
                ),
                redirect_stdout(io.StringIO()),
            ):
                returncode = run_main(
                    [
                        "--config",
                        str(config_path),
                        "--plan",
                        "test",
                        "--baseline",
                        "default",
                        "--candidate",
                        "default",
                        "--baseline-treatment",
                        "control",
                        "--candidate-treatment",
                        "agent",
                        "--output",
                        str(output),
                        "--dry-run",
                        "--parallel",
                        "1",
                    ]
                )

            self.assertEqual(returncode, 0)
            metadata = yaml.safe_load((output / "metadata.json").read_text())
            self.assertEqual(metadata["baseline"], "default__control")
            self.assertEqual(metadata["candidate"], "default__agent")
            self.assertTrue((output / "runs" / "default__control").is_dir())
            self.assertTrue((output / "runs" / "default__agent").is_dir())

    def test_standalone_mode_reuses_run_entrypoint(self) -> None:
        spec = RunnerExecutionPlanTest()._spec()
        config = {"schedulers": {"default": {"kind": "builtin"}}}
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            patch("bench.scripts.run.load_config", return_value=config),
            patch("bench.scripts.run.expand_plan", return_value=[spec]),
            patch("bench.scripts.run.run_specs") as run_specs,
            patch("bench.scripts.run._result_statuses", return_value=["DRY_RUN"]),
            redirect_stdout(io.StringIO()),
        ):
            returncode = run_main(
                [
                    "--config",
                    str(Path(temp_dir) / "config.yaml"),
                    "--plan",
                    "warmup-test",
                    "--scheduler",
                    "default",
                    "--output",
                    str(Path(temp_dir) / "results"),
                    "--dry-run",
                ]
            )

        self.assertEqual(returncode, 0)
        self.assertEqual(run_specs.call_count, 1)
        self.assertEqual(run_specs.call_args.kwargs["label"], "default")


if __name__ == "__main__":
    unittest.main()
