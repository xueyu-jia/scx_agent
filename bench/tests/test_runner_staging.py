from __future__ import annotations

from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

from bench.collectors.guest import build_guest_run_plan
from bench.config.parser import RunSpec
from bench.runner import (
    REPO_ROOT,
    _guest_scheduler,
    _guest_run_status,
    _manifest_entry,
    _resolve_host_path,
    _run_libvirt,
    _run_one,
)


class RunnerSchedulerStagingTest(unittest.TestCase):
    def test_guest_scheduler_uses_staged_command_without_mutating_input(self) -> None:
        scheduler = {
            "kind": "scx",
            "command": "guest/scx_test",
            "host_command": "build/scx_test",
            "args": ["--test"],
        }

        guest = _guest_scheduler(scheduler)

        self.assertEqual(guest["command"], "/tmp/scx-bench-scheduler")
        self.assertEqual(guest["args"], ["--test"])
        self.assertEqual(scheduler["command"], "guest/scx_test")

    def test_relative_host_path_is_resolved_from_repository_root(self) -> None:
        self.assertEqual(
            _resolve_host_path("schedule/scx"),
            (REPO_ROOT / Path("schedule/scx")).resolve(),
        )


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
            "bench.runner.write_guest_plan"
        ) as write_guest, patch("bench.runner._run_libvirt") as run_libvirt:
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
        written_plan = write_guest.call_args.args[1]
        self.assertEqual(written_plan.to_dict(), execution_plan)
        self.assertEqual(
            _manifest_entry(spec, scheduler=scheduler)["execution_plan"],
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

    def test_guest_top_level_status_is_authoritative(self) -> None:
        for guest_status in (
            "SCHEDULER_FAILED",
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
            patch("bench.runner._preflight_machine"),
            patch("bench.runner._run_libvirt", return_value=libvirt_result),
            patch("bench.runner._read_guest_result", return_value=guest_result),
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
            "bench.runner._run_libvirt"
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
            patch("bench.runner._prepare_runtime_dir"),
            patch("bench.runner._create_overlay"),
            patch("bench.runner._run_command"),
            patch("bench.runner._apply_host_thread_pinning", return_value={}),
            patch("bench.runner._wait_for_ssh", return_value=("192.0.2.1", 22)),
            patch("bench.runner._scp_to_guest", scp),
            patch("bench.runner._run_guest_command", run_guest),
            patch("bench.runner._cleanup_domain"),
            patch("bench.runner._cleanup_runtime_dir"),
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


if __name__ == "__main__":
    unittest.main()
