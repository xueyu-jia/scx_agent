#!/usr/bin/env python3
from __future__ import annotations

import difflib
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import traceback
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


PLAN_VERSION = 2
TREATMENT_OUTCOME_VERSION = 2
MAX_TREATMENT_OUTCOME_BYTES = 64 * 1024
PLAN_KEYS = {
    "version",
    "workdir",
    "output_dir",
    "run_context",
    "env",
    "vm_settle_seconds",
    "scheduler",
    "treatment",
    "post_treatment_settle_seconds",
    "warmup",
    "post_warmup_settle_seconds",
    "measurement",
    "cooldown_seconds",
}
COMMAND_KEYS = {"argv", "timeout_seconds"}
RUN_CONTEXT_KEYS = {"role", "variant", "treatment"}
RUN_ROLES = {"baseline", "candidate", "standalone"}
TREATMENT_KEYS = {"argv", "env", "timeout_seconds"}
TREATMENT_OUTCOME_KEYS = {"version", "disposition", "reason", "details"}
TREATMENT_REASON_KEYS = {"code", "message"}
TREATMENT_DISPOSITIONS = {"proceed", "stop", "unsafe"}
SCHEDULER_KEYS = {
    "argv",
    "env",
    "settle_seconds",
    "startup_grace_seconds",
}
RESERVED_GUEST_ENV = {
    "SCX_BENCH_OUT",
    "SCX_BENCH_ROLE",
    "SCX_BENCH_VARIANT",
    "SCX_BENCH_TREATMENT",
    "SCX_BENCH_TREATMENT_OUTCOME",
    "SCX_BENCH_WORKDIR",
}
PASS = "PASS"
SCHEDULER_FAILED = "SCHEDULER_FAILED"
TREATMENT_FAILED = "TREATMENT_FAILED"
TREATMENT_TIMEOUT = "TREATMENT_TIMEOUT"
TREATMENT_STOPPED = "TREATMENT_STOPPED"
TREATMENT_UNSAFE_STATE = "TREATMENT_UNSAFE_STATE"
WARMUP_FAILED = "WARMUP_FAILED"
WARMUP_TIMEOUT = "WARMUP_TIMEOUT"
BENCH_FAILED = "BENCH_FAILED"
BENCH_TIMEOUT = "BENCH_TIMEOUT"
INTERNAL_ERROR = "INTERNAL_ERROR"


class PlanError(ValueError):
    """Raised when the uploaded guest plan is malformed."""


class ExecutionFailure(RuntimeError):
    def __init__(self, status: str, reason: str) -> None:
        super().__init__(reason)
        self.status = status
        self.reason = reason


@dataclass(frozen=True)
class Command:
    argv: tuple[str, ...]
    timeout_seconds: int

    @classmethod
    def from_value(cls, value: Any, prefix: str) -> "Command":
        mapping = _mapping(value, prefix)
        _validate_keys(mapping, COMMAND_KEYS, prefix)
        return cls(
            argv=_argv(mapping.get("argv"), f"{prefix}.argv"),
            timeout_seconds=_positive_int(
                mapping.get("timeout_seconds"),
                f"{prefix}.timeout_seconds",
            ),
        )


@dataclass(frozen=True)
class RunContext:
    role: str
    variant: str
    treatment: str | None

    @classmethod
    def from_value(cls, value: Any) -> "RunContext":
        mapping = _mapping(value, "run_context")
        _validate_keys(mapping, RUN_CONTEXT_KEYS, "run_context")
        role = _text(mapping.get("role"), "run_context.role")
        if role not in RUN_ROLES:
            raise PlanError(
                f"run_context.role must be one of {sorted(RUN_ROLES)}, got {role!r}"
            )
        return cls(
            role=role,
            variant=_text(mapping.get("variant"), "run_context.variant"),
            treatment=_optional_text(
                mapping.get("treatment"),
                "run_context.treatment",
            ),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "role": self.role,
            "variant": self.variant,
            "treatment": self.treatment,
        }


@dataclass(frozen=True)
class Treatment:
    argv: tuple[str, ...]
    env: dict[str, str]
    timeout_seconds: int

    @classmethod
    def from_value(cls, value: Any) -> "Treatment":
        mapping = _mapping(value, "treatment")
        _validate_keys(mapping, TREATMENT_KEYS, "treatment")
        env = _environment(mapping.get("env", {}), "treatment.env")
        _reject_reserved_environment(env, "treatment.env")
        return cls(
            argv=_argv(mapping.get("argv"), "treatment.argv"),
            env=env,
            timeout_seconds=_positive_int(
                mapping.get("timeout_seconds"),
                "treatment.timeout_seconds",
            ),
        )

    def command(self) -> Command:
        return Command(self.argv, self.timeout_seconds)


@dataclass(frozen=True)
class TreatmentOutcome:
    disposition: str
    reason: dict[str, str]
    details: dict[str, Any]

    @classmethod
    def load(cls, path: Path) -> "TreatmentOutcome":
        try:
            size = path.stat().st_size
        except OSError as exc:
            raise PlanError(f"cannot stat treatment outcome {path}: {exc}") from exc
        if size > MAX_TREATMENT_OUTCOME_BYTES:
            raise PlanError(
                "treatment outcome exceeds "
                f"{MAX_TREATMENT_OUTCOME_BYTES} bytes"
            )
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as exc:
            raise PlanError(f"cannot read treatment outcome {path}: {exc}") from exc

        mapping = _mapping(value, "treatment outcome")
        _validate_keys(mapping, TREATMENT_OUTCOME_KEYS, "treatment outcome")
        version = mapping.get("version")
        if isinstance(version, bool) or version != TREATMENT_OUTCOME_VERSION:
            raise PlanError(
                "treatment outcome.version must be "
                f"{TREATMENT_OUTCOME_VERSION}, got {version!r}"
            )
        disposition = _text(
            mapping.get("disposition"),
            "treatment outcome.disposition",
        )
        if disposition not in TREATMENT_DISPOSITIONS:
            raise PlanError(
                "treatment outcome.disposition must be one of "
                f"{sorted(TREATMENT_DISPOSITIONS)}, got {disposition!r}"
            )
        reason = _mapping(mapping.get("reason"), "treatment outcome.reason")
        _validate_keys(reason, TREATMENT_REASON_KEYS, "treatment outcome.reason")
        normalized_reason = {
            "code": _text(reason.get("code"), "treatment outcome.reason.code"),
            "message": _text(
                reason.get("message"),
                "treatment outcome.reason.message",
            ),
        }
        details = mapping.get("details")
        if not isinstance(details, dict):
            raise PlanError("treatment outcome.details must be a mapping")
        return cls(
            disposition=disposition,
            reason=normalized_reason,
            details=details,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": TREATMENT_OUTCOME_VERSION,
            "disposition": self.disposition,
            "reason": dict(self.reason),
            "details": self.details,
        }


@dataclass(frozen=True)
class Scheduler:
    argv: tuple[str, ...]
    env: dict[str, str]
    settle_seconds: int
    startup_grace_seconds: int

    @classmethod
    def from_value(cls, value: Any) -> "Scheduler":
        mapping = _mapping(value, "scheduler")
        _validate_keys(mapping, SCHEDULER_KEYS, "scheduler")
        env = _environment(mapping.get("env", {}), "scheduler.env")
        _reject_reserved_environment(env, "scheduler.env")
        return cls(
            argv=_argv(mapping.get("argv"), "scheduler.argv"),
            env=env,
            settle_seconds=_non_negative_int(
                mapping.get("settle_seconds"),
                "scheduler.settle_seconds",
            ),
            startup_grace_seconds=_non_negative_int(
                mapping.get("startup_grace_seconds"),
                "scheduler.startup_grace_seconds",
            ),
        )


@dataclass(frozen=True)
class Plan:
    workdir: Path
    output_dir: Path
    run_context: RunContext
    env: dict[str, str]
    vm_settle_seconds: int
    scheduler: Scheduler | None
    treatment: Treatment | None
    post_treatment_settle_seconds: int
    warmup: Command | None
    post_warmup_settle_seconds: int
    measurement: Command
    cooldown_seconds: int

    @classmethod
    def load(cls, path: Path) -> "Plan":
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise PlanError(f"cannot read guest plan {path}: {exc}") from exc

        mapping = _mapping(value, "plan")
        _validate_keys(mapping, PLAN_KEYS, "plan")
        version = mapping.get("version")
        if isinstance(version, bool) or version != PLAN_VERSION:
            raise PlanError(
                f"plan.version must be {PLAN_VERSION}, got {version!r}"
            )

        scheduler_value = mapping.get("scheduler")
        treatment_value = mapping.get("treatment")
        warmup_value = mapping.get("warmup")
        run_context = RunContext.from_value(mapping.get("run_context"))
        env = _environment(mapping.get("env", {}), "plan.env")
        _reject_reserved_environment(env, "plan.env")
        treatment = (
            Treatment.from_value(treatment_value)
            if treatment_value is not None
            else None
        )
        if (run_context.treatment is None) != (treatment is None):
            raise PlanError(
                "run_context.treatment and plan.treatment must either both be set or both be null"
            )
        post_treatment_settle_seconds = _non_negative_int(
            mapping.get("post_treatment_settle_seconds"),
            "plan.post_treatment_settle_seconds",
        )
        if treatment is None and post_treatment_settle_seconds != 0:
            raise PlanError(
                "plan.post_treatment_settle_seconds must be zero without a treatment"
            )
        return cls(
            workdir=_absolute_path(mapping.get("workdir"), "plan.workdir"),
            output_dir=_absolute_path(mapping.get("output_dir"), "plan.output_dir"),
            run_context=run_context,
            env=env,
            vm_settle_seconds=_non_negative_int(
                mapping.get("vm_settle_seconds"),
                "plan.vm_settle_seconds",
            ),
            scheduler=Scheduler.from_value(scheduler_value)
            if scheduler_value is not None
            else None,
            treatment=treatment,
            post_treatment_settle_seconds=post_treatment_settle_seconds,
            warmup=Command.from_value(warmup_value, "warmup")
            if warmup_value is not None
            else None,
            post_warmup_settle_seconds=_non_negative_int(
                mapping.get("post_warmup_settle_seconds"),
                "plan.post_warmup_settle_seconds",
            ),
            measurement=Command.from_value(mapping.get("measurement"), "measurement"),
            cooldown_seconds=_non_negative_int(
                mapping.get("cooldown_seconds"),
                "plan.cooldown_seconds",
            ),
        )


@dataclass
class PhaseResult:
    status: str = "SKIPPED"
    returncode: int | None = None
    timed_out: bool = False
    process_group_clean: bool = True
    leaked_pids: list[int] = field(default_factory=list)
    error: str = ""
    started_at: str | None = None
    finished_at: str | None = None
    duration_seconds: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status,
            "returncode": self.returncode,
            "timed_out": self.timed_out,
            "process_group_clean": self.process_group_clean,
            "leaked_pids": list(self.leaked_pids),
            "error": self.error,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "duration_seconds": self.duration_seconds,
        }


class GuestExecutor:
    def __init__(self, plan: Plan) -> None:
        self.plan = plan
        self.output_dir = plan.output_dir
        self.output_ready = False
        self.scheduler_process: subprocess.Popen[bytes] | None = None
        self.scheduler_result: dict[str, Any] = {
            "status": "SKIPPED" if plan.scheduler is None else "PENDING",
            "start_returncode": None,
            "exit_returncode": None,
            "stop_returncode": None,
            "process_group_clean": True,
            "failure_context": None,
            "alive_before_measurement": None,
            "alive_after_measurement": None,
            "started_at": None,
            "finished_at": None,
        }
        self.treatment_result = PhaseResult()
        self.treatment_outcome: TreatmentOutcome | None = None
        self.warmup_result = PhaseResult()
        self.measurement_result = PhaseResult()
        self.snapshot_errors: list[str] = []
        self.status = INTERNAL_ERROR
        self.failure_reason = "guest executor did not complete"
        self.started_at = _utc_now()
        self.finished_at: str | None = None

    def run(self) -> int:
        try:
            self._prepare_output()
            self._sleep(self.plan.vm_settle_seconds)
            self._start_scheduler()
            self._run_treatment()

            if self.plan.warmup is not None:
                self.warmup_result = self._run_command(
                    self.plan.warmup,
                    output_dir=self.output_dir / "warmup",
                    stdout_path=self.output_dir / "warmup" / "stdout.log",
                    stderr_path=self.output_dir / "warmup" / "stderr.log",
                )
                self._check_scheduler("warmup")
                self._require_phase(
                    self.warmup_result,
                    failed_status=WARMUP_FAILED,
                    timeout_status=WARMUP_TIMEOUT,
                    phase_name="warmup",
                )
            else:
                self._check_scheduler("pre-measurement")

            self._sleep(self.plan.post_warmup_settle_seconds)
            self._check_scheduler(
                "post-warmup settle",
                "alive_before_measurement",
            )
            self._snapshot("before")
            try:
                self.measurement_result = self._run_command(
                    self.plan.measurement,
                    output_dir=self.output_dir,
                    stdout_path=self.output_dir / "stdout.log",
                    stderr_path=self.output_dir / "stderr.log",
                )
            finally:
                self._snapshot("after")

            self._check_scheduler("measurement", "alive_after_measurement")
            self._require_phase(
                self.measurement_result,
                failed_status=BENCH_FAILED,
                timeout_status=BENCH_TIMEOUT,
                phase_name="measurement",
            )
            self._sleep(self.plan.cooldown_seconds)
            self.status = PASS
            self.failure_reason = ""
        except ExecutionFailure as exc:
            self.status = exc.status
            self.failure_reason = exc.reason
        except Exception as exc:  # pragma: no cover - defensive artifact path
            self.status = INTERNAL_ERROR
            self.failure_reason = f"{type(exc).__name__}: {exc}"
            self._write_internal_error(traceback.format_exc())
        finally:
            self._finalize()
        return self._exit_code()

    def _prepare_output(self) -> None:
        self.output_dir = self.output_dir.resolve(strict=False)
        workdir = self.plan.workdir.resolve(strict=False)
        if (
            self.output_dir == Path("/")
            or self.output_dir == workdir
            or self.output_dir in workdir.parents
        ):
            raise PlanError(
                "plan.output_dir must not resolve to / or an ancestor of plan.workdir"
            )
        if self.output_dir.exists():
            shutil.rmtree(self.output_dir)
        self.output_dir.mkdir(parents=True)
        self.output_ready = True
        (self.output_dir / "stdout.log").touch()
        (self.output_dir / "stderr.log").touch()
        for name, enabled in (
            ("treatment", self.plan.treatment is not None),
            ("warmup", self.plan.warmup is not None),
        ):
            if not enabled:
                continue
            phase_dir = self.output_dir / name
            phase_dir.mkdir()
            (phase_dir / "stdout.log").touch()
            (phase_dir / "stderr.log").touch()

    def _run_treatment(self) -> None:
        treatment = self.plan.treatment
        if treatment is None:
            return

        output_dir = self.output_dir / "treatment"
        outcome_path = output_dir / "outcome.json"
        self.treatment_result = self._run_command(
            treatment.command(),
            output_dir=output_dir,
            stdout_path=output_dir / "stdout.log",
            stderr_path=output_dir / "stderr.log",
            extra_env=treatment.env,
            treatment_outcome_path=outcome_path,
            include_run_context=True,
        )
        self._check_scheduler("treatment")
        self._require_phase(
            self.treatment_result,
            failed_status=TREATMENT_FAILED,
            timeout_status=TREATMENT_TIMEOUT,
            phase_name="treatment",
        )

        try:
            self.treatment_outcome = TreatmentOutcome.load(outcome_path)
        except PlanError as exc:
            self.treatment_result.status = "FAILED"
            self.treatment_result.error = str(exc)
            raise ExecutionFailure(
                TREATMENT_FAILED,
                f"treatment outcome is invalid: {exc}",
            ) from exc

        disposition = self.treatment_outcome.disposition
        if disposition == "unsafe":
            self.treatment_result.status = "UNSAFE"
            self.treatment_result.error = self.treatment_outcome.reason["message"]
            raise ExecutionFailure(
                TREATMENT_UNSAFE_STATE,
                self.treatment_result.error,
            )
        if disposition == "stop":
            self.treatment_result.status = "STOPPED"
            self.treatment_result.error = self.treatment_outcome.reason["message"]
            raise ExecutionFailure(
                TREATMENT_STOPPED,
                self.treatment_result.error,
            )

        self.treatment_result.status = "PROCEEDED"
        self._sleep(self.plan.post_treatment_settle_seconds)
        self._check_scheduler("post-treatment settle")

    def _start_scheduler(self) -> None:
        scheduler = self.plan.scheduler
        if scheduler is None:
            return

        env = self._phase_env(self.output_dir)
        env.update(scheduler.env)
        env["SCX_BENCH_OUT"] = str(self.output_dir)
        stdout_path = self.output_dir / "scheduler_stdout.log"
        stderr_path = self.output_dir / "scheduler_stderr.log"
        self.scheduler_result["started_at"] = _utc_now()
        try:
            with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
                self.scheduler_process = subprocess.Popen(
                    scheduler.argv,
                    cwd=self.plan.workdir,
                    env=env,
                    stdout=stdout,
                    stderr=stderr,
                    start_new_session=True,
                )
        except OSError as exc:
            self.scheduler_result.update(
                status="FAILED",
                start_returncode=127,
                finished_at=_utc_now(),
            )
            raise ExecutionFailure(
                SCHEDULER_FAILED,
                f"scheduler failed to start: {exc}",
            ) from exc

        self.scheduler_result.update(status="RUNNING", start_returncode=0)
        self._sleep(scheduler.startup_grace_seconds)
        self._check_scheduler("startup")
        self._sleep(scheduler.settle_seconds)
        self._check_scheduler("settle")

    def _check_scheduler(self, context: str, result_key: str | None = None) -> None:
        if self.plan.scheduler is None:
            return
        process = self.scheduler_process
        alive = process is not None and process.poll() is None
        if result_key is not None:
            self.scheduler_result[result_key] = alive
        if alive:
            return

        returncode = process.returncode if process is not None else None
        self.scheduler_result.update(
            status="FAILED",
            exit_returncode=returncode,
            failure_context=context,
            finished_at=_utc_now(),
        )
        raise ExecutionFailure(
            SCHEDULER_FAILED,
            f"scheduler exited during {context} with returncode {returncode}",
        )

    def _stop_scheduler(self) -> str | None:
        process = self.scheduler_process
        if process is None:
            return None

        failure_reason = None
        if process.poll() is None:
            clean = _terminate_process_group(process.pid)
            self.scheduler_result["process_group_clean"] = clean
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                clean = _terminate_process_group(process.pid, term_wait_seconds=0)
                self.scheduler_result["process_group_clean"] = clean
                process.wait()
            self.scheduler_result["stop_returncode"] = process.returncode
            if not clean:
                self.scheduler_result["status"] = "FAILED"
                failure_reason = "scheduler process group could not be cleaned"
            elif self.scheduler_result["status"] == "RUNNING":
                self.scheduler_result["status"] = "PASS"
        else:
            self.scheduler_result["exit_returncode"] = process.returncode
            clean = _terminate_process_group(process.pid)
            self.scheduler_result["process_group_clean"] = clean
            if self.scheduler_result["status"] == "RUNNING":
                self.scheduler_result["status"] = "FAILED"
                failure_reason = (
                    "scheduler exited before cleanup with returncode "
                    f"{process.returncode}"
                )
            if not clean:
                cleanup_reason = "scheduler process group could not be cleaned"
                failure_reason = (
                    f"{failure_reason}; {cleanup_reason}"
                    if failure_reason
                    else cleanup_reason
                )
        self.scheduler_result["finished_at"] = _utc_now()
        self.scheduler_process = None
        return failure_reason

    def _run_command(
        self,
        command: Command,
        *,
        output_dir: Path,
        stdout_path: Path,
        stderr_path: Path,
        extra_env: dict[str, str] | None = None,
        treatment_outcome_path: Path | None = None,
        include_run_context: bool = False,
    ) -> PhaseResult:
        result = PhaseResult(status="RUNNING", started_at=_utc_now())
        started = time.monotonic()
        output_dir.mkdir(parents=True, exist_ok=True)
        env = self._phase_env(
            output_dir,
            extra_env,
            include_run_context=include_run_context,
        )
        if treatment_outcome_path is not None:
            env["SCX_BENCH_TREATMENT_OUTCOME"] = str(treatment_outcome_path)

        try:
            with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
                process = subprocess.Popen(
                    command.argv,
                    cwd=self.plan.workdir,
                    env=env,
                    stdout=stdout,
                    stderr=stderr,
                    start_new_session=True,
                )
        except OSError as exc:
            result.status = "FAILED"
            result.returncode = 127
            result.error = str(exc)
            result.finished_at = _utc_now()
            result.duration_seconds = time.monotonic() - started
            return result

        try:
            process.wait(timeout=command.timeout_seconds)
            result.returncode = process.returncode
        except subprocess.TimeoutExpired:
            result.timed_out = True
            result.returncode = 124
            result.process_group_clean = _terminate_process_group(process.pid)
            process.wait()

        active_members = _active_group_members(process.pid)
        if active_members:
            result.leaked_pids = active_members
            result.process_group_clean = _terminate_process_group(process.pid)

        if result.timed_out:
            result.status = "TIMEOUT"
        elif result.returncode != 0:
            result.status = "FAILED"
        elif result.leaked_pids or not result.process_group_clean:
            result.status = "FAILED"
            pids = ", ".join(str(pid) for pid in result.leaked_pids)
            result.error = f"command left processes in its process group: {pids}"
        else:
            result.status = PASS

        result.finished_at = _utc_now()
        result.duration_seconds = time.monotonic() - started
        return result

    def _require_phase(
        self,
        result: PhaseResult,
        *,
        failed_status: str,
        timeout_status: str,
        phase_name: str,
    ) -> None:
        if result.status == PASS:
            return
        if result.timed_out:
            reason = f"{phase_name} timed out"
            if not result.process_group_clean:
                reason += " and its process group could not be cleaned"
            raise ExecutionFailure(
                timeout_status,
                reason,
            )
        detail = result.error or f"returncode {result.returncode}"
        raise ExecutionFailure(failed_status, f"{phase_name} failed: {detail}")

    def _phase_env(
        self,
        output_dir: Path,
        extra_env: dict[str, str] | None = None,
        *,
        include_run_context: bool = False,
    ) -> dict[str, str]:
        env = {
            **os.environ,
            **self.plan.env,
        }
        if extra_env:
            env.update(extra_env)
        for name in RESERVED_GUEST_ENV:
            env.pop(name, None)
        env["SCX_BENCH_OUT"] = str(output_dir)
        env["SCX_BENCH_WORKDIR"] = str(self.plan.workdir)
        if include_run_context:
            env.update(
                {
                    "SCX_BENCH_ROLE": self.plan.run_context.role,
                    "SCX_BENCH_VARIANT": self.plan.run_context.variant,
                    "SCX_BENCH_TREATMENT": self.plan.run_context.treatment or "",
                }
            )
        return env

    def _snapshot(self, phase: str) -> None:
        destination = self.output_dir / "snapshots" / phase
        destination.mkdir(parents=True, exist_ok=True)
        sources = {
            "/proc/stat": "proc_stat.txt",
            "/proc/schedstat": "proc_schedstat.txt",
            "/proc/interrupts": "proc_interrupts.txt",
            "/proc/pressure/cpu": "psi_cpu.txt",
            "/proc/pressure/io": "psi_io.txt",
            "/proc/pressure/memory": "psi_memory.txt",
        }
        for source, name in sources.items():
            self._copy_snapshot_file(Path(source), destination / name)

        self._run_snapshot_command(
            ["dmesg"],
            destination / "dmesg.txt",
            destination / "dmesg.err",
        )
        self._copy_sched_ext(destination / "sched_ext")

    def _copy_snapshot_file(self, source: Path, destination: Path) -> None:
        if not source.exists():
            return
        try:
            shutil.copyfile(source, destination)
        except OSError as exc:
            self.snapshot_errors.append(f"{source}: {exc}")
            destination.with_suffix(destination.suffix + ".err").write_text(
                str(exc),
                encoding="utf-8",
            )

    def _run_snapshot_command(
        self,
        argv: list[str],
        stdout_path: Path,
        stderr_path: Path,
    ) -> None:
        try:
            with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
                subprocess.run(argv, stdout=stdout, stderr=stderr, check=False)
        except OSError as exc:
            self.snapshot_errors.append(f"{' '.join(argv)}: {exc}")
            stderr_path.write_text(str(exc), encoding="utf-8")

    def _copy_sched_ext(self, destination: Path) -> None:
        try:
            subprocess.run(
                ["mount", "-t", "debugfs", "none", "/sys/kernel/debug"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        except OSError as exc:
            self.snapshot_errors.append(f"mount debugfs: {exc}")
            return
        source = Path("/sys/kernel/debug/sched_ext")
        if not source.is_dir():
            return

        try:
            files = [path for path in source.rglob("*") if path.is_file()]
        except OSError as exc:
            self.snapshot_errors.append(f"{source}: {exc}")
            return

        for path in files:
            relative = path.relative_to(source)
            if len(relative.parts) > 3:
                continue
            target = destination / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            self._copy_snapshot_file(path, target)

    def _write_dmesg_diff(self) -> None:
        before = self.output_dir / "snapshots" / "before" / "dmesg.txt"
        after = self.output_dir / "snapshots" / "after" / "dmesg.txt"
        if not before.exists() or not after.exists():
            return
        try:
            delta = self.output_dir / "snapshots" / "delta" / "dmesg.diff"
            delta.parent.mkdir(parents=True, exist_ok=True)
            before_lines = before.read_text(
                encoding="utf-8",
                errors="replace",
            ).splitlines(True)
            after_lines = after.read_text(
                encoding="utf-8",
                errors="replace",
            ).splitlines(True)
            delta.write_text(
                "".join(
                    difflib.unified_diff(
                        before_lines,
                        after_lines,
                        fromfile="before/dmesg.txt",
                        tofile="after/dmesg.txt",
                    )
                ),
                encoding="utf-8",
            )
        except OSError as exc:
            self.snapshot_errors.append(f"dmesg diff: {exc}")

    def _finalize(self) -> None:
        try:
            scheduler_failure = self._stop_scheduler()
        except Exception as exc:  # pragma: no cover - defensive cleanup path
            scheduler_failure = f"scheduler cleanup failed: {type(exc).__name__}: {exc}"
            self._write_internal_error(traceback.format_exc())

        if scheduler_failure is not None:
            if self.status == PASS:
                self.status = SCHEDULER_FAILED
                self.failure_reason = scheduler_failure
            elif scheduler_failure not in self.failure_reason:
                self.failure_reason = f"{self.failure_reason}; {scheduler_failure}"

        self._write_dmesg_diff()
        self.finished_at = _utc_now()
        try:
            self._write_result()
        except Exception as exc:  # pragma: no cover - no artifact destination
            previous_reason = self.failure_reason
            self.status = INTERNAL_ERROR
            self.failure_reason = (
                f"failed to write guest_result.json: {type(exc).__name__}: {exc}; "
                f"prior result: {previous_reason}"
            )
            self._write_internal_error(traceback.format_exc())
            print(self.failure_reason, file=sys.stderr)

    def _write_internal_error(self, value: str) -> None:
        if not self.output_ready:
            return
        try:
            self.output_dir.mkdir(parents=True, exist_ok=True)
            (self.output_dir / "internal_error.log").write_text(value, encoding="utf-8")
        except OSError:
            pass

    def _write_result(self) -> None:
        if not self.output_ready:
            raise OSError("guest output directory was not prepared")
        result = {
            "version": PLAN_VERSION,
            "status": self.status,
            "failure_reason": self.failure_reason,
            "run_context": self.plan.run_context.to_dict(),
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "phases": {
                "scheduler": self.scheduler_result,
                "treatment": {
                    **self.treatment_result.to_dict(),
                    "outcome": (
                        self.treatment_outcome.to_dict()
                        if self.treatment_outcome is not None
                        else None
                    ),
                },
                "warmup": self.warmup_result.to_dict(),
                "measurement": self.measurement_result.to_dict(),
            },
            "snapshot_errors": list(self.snapshot_errors),
            "timing": {
                "vm_settle_seconds": self.plan.vm_settle_seconds,
                "post_treatment_settle_seconds": (
                    self.plan.post_treatment_settle_seconds
                ),
                "post_warmup_settle_seconds": self.plan.post_warmup_settle_seconds,
                "cooldown_seconds": self.plan.cooldown_seconds,
            },
        }
        self.output_dir.mkdir(parents=True, exist_ok=True)
        destination = self.output_dir / "guest_result.json"
        temporary = destination.with_suffix(".json.tmp")
        temporary.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        os.replace(temporary, destination)

    def _exit_code(self) -> int:
        if self.status == PASS:
            return 0
        if self.status in {TREATMENT_TIMEOUT, WARMUP_TIMEOUT, BENCH_TIMEOUT}:
            return 124
        if self.status == TREATMENT_FAILED:
            phase = self.treatment_result
        elif self.status == WARMUP_FAILED:
            phase = self.warmup_result
        else:
            phase = self.measurement_result
        if phase.returncode is not None and 1 <= phase.returncode <= 255:
            return phase.returncode
        return 125

    @staticmethod
    def _sleep(seconds: int) -> None:
        if seconds > 0:
            time.sleep(seconds)


def _active_group_members(pgid: int) -> list[int]:
    members: list[int] = []
    for stat_path in Path("/proc").glob("[0-9]*/stat"):
        try:
            line = stat_path.read_text(encoding="utf-8")
            fields = line.rsplit(") ", 1)[1].split()
            state = fields[0]
            process_group = int(fields[2])
            pid = int(stat_path.parent.name)
        except (IndexError, OSError, ValueError):
            continue
        if process_group == pgid and state not in {"Z", "X"}:
            members.append(pid)
    return sorted(members)


def _terminate_process_group(
    pgid: int,
    *,
    term_wait_seconds: float = 1.0,
    kill_wait_seconds: float = 1.0,
) -> bool:
    if not _active_group_members(pgid):
        return True

    _signal_process_group(pgid, signal.SIGTERM)
    if _wait_for_empty_group(pgid, term_wait_seconds):
        return True

    _signal_process_group(pgid, signal.SIGKILL)
    return _wait_for_empty_group(pgid, kill_wait_seconds)


def _signal_process_group(pgid: int, value: signal.Signals) -> None:
    try:
        os.killpg(pgid, value)
    except ProcessLookupError:
        pass


def _wait_for_empty_group(pgid: int, timeout_seconds: float) -> bool:
    deadline = time.monotonic() + timeout_seconds
    while _active_group_members(pgid):
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.05)
    return True


def _mapping(value: Any, prefix: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise PlanError(f"{prefix} must be an object")
    return value


def _validate_keys(mapping: dict[str, Any], expected: set[str], prefix: str) -> None:
    missing = expected - set(mapping)
    if missing:
        raise PlanError(f"{prefix} is missing keys: {sorted(missing)}")
    unknown = set(mapping) - expected
    if unknown:
        raise PlanError(f"{prefix} has unsupported keys: {sorted(unknown)}")


def _argv(value: Any, prefix: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not value:
        raise PlanError(f"{prefix} must be a non-empty string array")
    if any(not isinstance(item, str) or not item for item in value):
        raise PlanError(f"{prefix} must contain non-empty strings")
    return tuple(value)


def _environment(value: Any, prefix: str) -> dict[str, str]:
    mapping = _mapping(value, prefix)
    if any(
        not isinstance(key, str) or not isinstance(item, str)
        for key, item in mapping.items()
    ):
        raise PlanError(f"{prefix} must contain string:string entries")
    return dict(mapping)


def _reject_reserved_environment(value: dict[str, str], prefix: str) -> None:
    reserved = sorted(set(value) & RESERVED_GUEST_ENV)
    if reserved:
        raise PlanError(f"{prefix} contains reserved variables: {reserved}")


def _text(value: Any, prefix: str) -> str:
    if (
        not isinstance(value, str)
        or not value
        or len(value) > 256
        or value.strip() != value
        or any(ord(character) < 32 or ord(character) == 127 for character in value)
    ):
        raise PlanError(f"{prefix} must be a valid non-empty string")
    return value


def _optional_text(value: Any, prefix: str) -> str | None:
    if value is None:
        return None
    return _text(value, prefix)


def _string(value: Any, prefix: str) -> str:
    if not isinstance(value, str) or not value:
        raise PlanError(f"{prefix} must be a non-empty string")
    return value


def _absolute_path(value: Any, prefix: str) -> Path:
    path = Path(_string(value, prefix))
    if not path.is_absolute():
        raise PlanError(f"{prefix} must be an absolute path")
    return path


def _positive_int(value: Any, prefix: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise PlanError(f"{prefix} must be a positive integer")
    return value


def _non_negative_int(value: Any, prefix: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise PlanError(f"{prefix} must be a non-negative integer")
    return value


def _utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    if len(arguments) != 1:
        print("usage: guest_executor.py PLAN.json", file=sys.stderr)
        return 2
    try:
        plan = Plan.load(Path(arguments[0]))
    except PlanError as exc:
        print(f"guest plan error: {exc}", file=sys.stderr)
        return 125
    return GuestExecutor(plan).run()


if __name__ == "__main__":
    raise SystemExit(main())
