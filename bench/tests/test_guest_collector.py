from __future__ import annotations

from contextlib import redirect_stderr
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from bench.collectors.guest import build_guest_run_plan, write_guest_plan
from bench.collectors.guest_executor import (
    Command,
    GuestExecutor,
    Plan,
    PlanError,
    Scheduler,
)


class LightweightGuestExecutor(GuestExecutor):
    """Keep lifecycle tests independent of host snapshot permissions."""

    def _snapshot(self, phase: str) -> None:
        destination = self.output_dir / "snapshots" / phase
        destination.mkdir(parents=True, exist_ok=True)
        (destination / "proc_stat.txt").write_text(phase, encoding="utf-8")

    def _write_dmesg_diff(self) -> None:
        return


def python_command(source: str, timeout_seconds: int = 5) -> Command:
    return Command((sys.executable, "-c", source), timeout_seconds)


def scheduler(source: str, startup_grace_seconds: int = 0) -> Scheduler:
    return Scheduler(
        argv=(sys.executable, "-c", source),
        env={},
        settle_seconds=0,
        startup_grace_seconds=startup_grace_seconds,
    )


class GuestPlanTest(unittest.TestCase):
    def test_plan_round_trip_and_host_timeout(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            host_plan = build_guest_run_plan(
                {
                    "env": {"MODE": "test"},
                    "warmup": {
                        "command": "prime",
                        "args": ["--seconds", "5"],
                        "timeout_seconds": 7,
                    },
                    "post_warmup_settle_seconds": 3,
                    "measurement": {
                        "command": "measure",
                        "args": ["--seconds", "10"],
                        "timeout_seconds": 20,
                    },
                    "cooldown_seconds": 1,
                },
                {
                    "kind": "scx",
                    "command": "scheduler",
                    "args": ["--test"],
                    "env": {"SCHED_MODE": "test"},
                    "settle_seconds": 4,
                },
                {
                    "workdir": str(root),
                    "vm_settle_seconds": 2,
                },
                output_dir=str(root / "output"),
            )
            path = root / "guest_plan.json"

            write_guest_plan(path, host_plan)
            guest_plan = Plan.load(path)

            self.assertEqual(guest_plan.warmup.argv, ("prime", "--seconds", "5"))
            self.assertEqual(
                guest_plan.measurement.argv,
                ("measure", "--seconds", "10"),
            )
            self.assertEqual(guest_plan.scheduler.argv, ("scheduler", "--test"))
            self.assertEqual(guest_plan.env, {"MODE": "test"})
            self.assertEqual(host_plan.host_timeout_seconds(extra_seconds=5), 43)
            self.assertEqual(json.loads(path.read_text()), host_plan.to_dict())

    def test_plan_rejects_legacy_and_unknown_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            host_plan = build_guest_run_plan(
                {
                    "measurement": {
                        "command": "true",
                        "timeout_seconds": 1,
                    }
                },
                {"kind": "builtin"},
                {"workdir": str(root)},
                output_dir=str(root / "output"),
            ).to_dict()
            host_plan["warmup_seconds"] = 1
            path = root / "guest_plan.json"
            path.write_text(json.dumps(host_plan), encoding="utf-8")

            with self.assertRaisesRegex(PlanError, "unsupported keys"):
                Plan.load(path)

    def test_plan_paths_must_be_absolute(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            host_plan = build_guest_run_plan(
                {
                    "measurement": {
                        "command": "true",
                        "timeout_seconds": 1,
                    }
                },
                {"kind": "builtin"},
                {"workdir": "relative/workdir"},
                output_dir=str(root / "output"),
            )
            path = root / "guest_plan.json"
            write_guest_plan(path, host_plan)

            with self.assertRaisesRegex(PlanError, "must be an absolute path"):
                Plan.load(path)


class GuestExecutorTest(unittest.TestCase):
    def _plan(
        self,
        root: Path,
        measurement: Command,
        *,
        warmup: Command | None = None,
        scheduler_plan: Scheduler | None = None,
        output_dir: Path | None = None,
    ) -> Plan:
        return Plan(
            workdir=root,
            output_dir=output_dir or root / "output",
            env={},
            vm_settle_seconds=0,
            scheduler=scheduler_plan,
            warmup=warmup,
            post_warmup_settle_seconds=0,
            measurement=measurement,
            cooldown_seconds=0,
        )

    def _run(self, plan: Plan) -> tuple[int, dict[str, object]]:
        returncode = LightweightGuestExecutor(plan).run()
        result = json.loads(
            (plan.output_dir / "guest_result.json").read_text(encoding="utf-8")
        )
        return returncode, result

    def test_warmup_artifacts_are_isolated_from_measurement(self) -> None:
        warmup_source = """
import os
from pathlib import Path
out = Path(os.environ["SCX_BENCH_OUT"])
(out / "ready").write_text("ready")
(out / "perf_stat.csv").write_text("warmup-only")
print("warmup", end="")
"""
        measurement_source = """
import os
from pathlib import Path
out = Path(os.environ["SCX_BENCH_OUT"])
assert (out / "warmup" / "ready").is_file()
assert not (out / "ready").exists()
print("measurement", end="")
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(
                self._plan(
                    root,
                    python_command(measurement_source),
                    warmup=python_command(warmup_source),
                )
            )
            output = root / "output"

            self.assertEqual(returncode, 0)
            self.assertEqual(result["status"], "PASS")
            self.assertEqual(result["phases"]["warmup"]["status"], "PASS")
            self.assertEqual(result["phases"]["measurement"]["status"], "PASS")
            self.assertEqual((output / "warmup" / "stdout.log").read_text(), "warmup")
            self.assertEqual((output / "stdout.log").read_text(), "measurement")
            self.assertTrue((output / "warmup" / "perf_stat.csv").exists())
            self.assertFalse((output / "perf_stat.csv").exists())

    def test_warmup_failure_blocks_measurement_and_snapshots(self) -> None:
        measurement = python_command(
            'from pathlib import Path; import os; '
            'Path(os.environ["SCX_BENCH_OUT"], "measurement").write_text("ran")'
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(
                self._plan(
                    root,
                    measurement,
                    warmup=python_command('print("broken", end=""); raise SystemExit(7)'),
                )
            )
            output = root / "output"

            self.assertEqual(returncode, 7)
            self.assertEqual(result["status"], "WARMUP_FAILED")
            self.assertEqual(result["phases"]["warmup"]["returncode"], 7)
            self.assertEqual(result["phases"]["measurement"]["status"], "SKIPPED")
            self.assertFalse((output / "measurement").exists())
            self.assertFalse((output / "snapshots" / "before").exists())
            self.assertEqual((output / "warmup" / "stdout.log").read_text(), "broken")

    def test_warmup_timeout_cleans_its_process_group(self) -> None:
        source = """
import signal
import time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
time.sleep(30)
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(
                self._plan(
                    root,
                    python_command("pass"),
                    warmup=python_command(source, timeout_seconds=1),
                )
            )
            phase = result["phases"]["warmup"]

            self.assertEqual(returncode, 124)
            self.assertEqual(result["status"], "WARMUP_TIMEOUT")
            self.assertTrue(phase["timed_out"])
            self.assertTrue(phase["process_group_clean"])
            self.assertEqual(result["phases"]["measurement"]["status"], "SKIPPED")

    def test_leaked_warmup_child_is_cleaned_and_fails_run(self) -> None:
        source = """
import subprocess
import sys
subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(
                self._plan(
                    root,
                    python_command("pass"),
                    warmup=python_command(source),
                )
            )
            phase = result["phases"]["warmup"]

            self.assertEqual(returncode, 125)
            self.assertEqual(result["status"], "WARMUP_FAILED")
            self.assertTrue(phase["leaked_pids"])
            self.assertTrue(phase["process_group_clean"])
            self.assertIn("left processes in its process group", phase["error"])

    def test_measurement_timeout_is_reported_after_both_snapshots(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(
                self._plan(
                    root,
                    python_command("import time; time.sleep(30)", timeout_seconds=1),
                )
            )
            output = root / "output"

            self.assertEqual(returncode, 124)
            self.assertEqual(result["status"], "BENCH_TIMEOUT")
            self.assertTrue(result["phases"]["measurement"]["timed_out"])
            self.assertTrue((output / "snapshots" / "before").exists())
            self.assertTrue((output / "snapshots" / "after").exists())

    def test_scheduler_exit_during_warmup_blocks_measurement(self) -> None:
        measurement = python_command(
            'from pathlib import Path; import os; '
            'Path(os.environ["SCX_BENCH_OUT"], "measurement").write_text("ran")'
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(
                self._plan(
                    root,
                    measurement,
                    warmup=python_command("import time; time.sleep(0.4)"),
                    scheduler_plan=scheduler("import time; time.sleep(0.2)"),
                )
            )
            scheduler_phase = result["phases"]["scheduler"]

            self.assertEqual(returncode, 125)
            self.assertEqual(result["status"], "SCHEDULER_FAILED")
            self.assertEqual(result["phases"]["warmup"]["status"], "PASS")
            self.assertEqual(scheduler_phase["start_returncode"], 0)
            self.assertEqual(scheduler_phase["exit_returncode"], 0)
            self.assertEqual(scheduler_phase["failure_context"], "warmup")
            self.assertIsNone(scheduler_phase["alive_before_measurement"])
            self.assertFalse((root / "output" / "measurement").exists())

    def test_scheduler_exit_during_measurement_invalidates_run(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(
                self._plan(
                    root,
                    python_command("import time; time.sleep(0.4)"),
                    scheduler_plan=scheduler("import time; time.sleep(0.2)"),
                )
            )
            scheduler_phase = result["phases"]["scheduler"]

            self.assertEqual(returncode, 125)
            self.assertEqual(result["status"], "SCHEDULER_FAILED")
            self.assertEqual(result["phases"]["measurement"]["status"], "PASS")
            self.assertEqual(scheduler_phase["start_returncode"], 0)
            self.assertTrue(scheduler_phase["alive_before_measurement"])
            self.assertFalse(scheduler_phase["alive_after_measurement"])
            self.assertEqual(scheduler_phase["failure_context"], "measurement")

    def test_scheduler_start_returncode_describes_spawn_not_lifetime(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(
                self._plan(
                    root,
                    python_command("pass"),
                    scheduler_plan=scheduler(
                        "raise SystemExit(6)",
                        startup_grace_seconds=1,
                    ),
                )
            )
            scheduler_phase = result["phases"]["scheduler"]

            self.assertEqual(returncode, 125)
            self.assertEqual(result["status"], "SCHEDULER_FAILED")
            self.assertEqual(scheduler_phase["start_returncode"], 0)
            self.assertEqual(scheduler_phase["exit_returncode"], 6)

    def test_result_is_atomically_replaced(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            returncode, result = self._run(self._plan(root, python_command("pass")))
            output = root / "output"

            self.assertEqual(returncode, 0)
            self.assertEqual(result["status"], "PASS")
            self.assertFalse((output / "guest_result.json.tmp").exists())

    def test_result_write_failure_does_not_escape_run(self) -> None:
        class BrokenResultExecutor(LightweightGuestExecutor):
            def _write_result(self) -> None:
                raise OSError("disk full")

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            executor = BrokenResultExecutor(self._plan(root, python_command("pass")))

            with redirect_stderr(io.StringIO()):
                returncode = executor.run()

            self.assertEqual(returncode, 125)
            self.assertEqual(executor.status, "INTERNAL_ERROR")
            self.assertIn("failed to write guest_result.json", executor.failure_reason)

    def test_snapshot_mount_failure_is_best_effort(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            executor = GuestExecutor(self._plan(root, python_command("pass")))

            with patch("subprocess.run", side_effect=FileNotFoundError("mount")):
                executor._copy_sched_ext(root / "sched_ext")

            self.assertEqual(executor.snapshot_errors, ["mount debugfs: mount"])


if __name__ == "__main__":
    unittest.main()
