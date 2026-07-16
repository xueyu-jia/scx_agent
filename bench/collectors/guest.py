from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any


GUEST_OUTPUT_DIR = "/scx_bench_out"
GUEST_PLAN_VERSION = 2
GUEST_EXECUTOR_SOURCE = Path(__file__).with_name("guest_executor.py")
GUEST_EXECUTOR_PATH = "/tmp/scx-bench-guest-executor.py"
GUEST_PLAN_PATH = "/tmp/scx-bench-run-plan.json"
SCHEDULER_STARTUP_GRACE_SECONDS = 1


@dataclass(frozen=True)
class CommandPlan:
    argv: tuple[str, ...]
    timeout_seconds: int

    @classmethod
    def from_config(cls, config: dict[str, Any]) -> "CommandPlan":
        return cls(
            argv=(config["command"], *config.get("args", [])),
            timeout_seconds=int(config["timeout_seconds"]),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "argv": list(self.argv),
            "timeout_seconds": self.timeout_seconds,
        }


@dataclass(frozen=True)
class RunContextPlan:
    role: str
    variant: str
    treatment: str | None

    def to_dict(self) -> dict[str, Any]:
        return {
            "role": self.role,
            "variant": self.variant,
            "treatment": self.treatment,
        }


@dataclass(frozen=True)
class TreatmentPlan:
    argv: tuple[str, ...]
    env: dict[str, str]
    timeout_seconds: int
    allow_no_commit: bool

    @classmethod
    def from_config(cls, config: dict[str, Any]) -> "TreatmentPlan":
        return cls(
            argv=(config["command"], *config.get("args", [])),
            env=dict(config.get("env", {})),
            timeout_seconds=int(config["timeout_seconds"]),
            allow_no_commit=bool(config.get("allow_no_commit", False)),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "argv": list(self.argv),
            "env": dict(self.env),
            "timeout_seconds": self.timeout_seconds,
            "allow_no_commit": self.allow_no_commit,
        }


@dataclass(frozen=True)
class SchedulerPlan:
    argv: tuple[str, ...]
    env: dict[str, str]
    settle_seconds: int
    startup_grace_seconds: int = SCHEDULER_STARTUP_GRACE_SECONDS

    def to_dict(self) -> dict[str, Any]:
        return {
            "argv": list(self.argv),
            "env": dict(self.env),
            "settle_seconds": self.settle_seconds,
            "startup_grace_seconds": self.startup_grace_seconds,
        }


@dataclass(frozen=True)
class GuestRunPlan:
    workdir: str
    output_dir: str
    run_context: RunContextPlan
    env: dict[str, str]
    vm_settle_seconds: int
    scheduler: SchedulerPlan | None
    treatment: TreatmentPlan | None
    post_treatment_settle_seconds: int
    warmup: CommandPlan | None
    post_warmup_settle_seconds: int
    measurement: CommandPlan
    cooldown_seconds: int

    def host_timeout_seconds(self, extra_seconds: int) -> int:
        total = (
            self.vm_settle_seconds
            + self.post_warmup_settle_seconds
            + self.measurement.timeout_seconds
            + self.cooldown_seconds
            + extra_seconds
        )
        if self.scheduler is not None:
            total += self.scheduler.startup_grace_seconds + self.scheduler.settle_seconds
        if self.treatment is not None:
            total += (
                self.treatment.timeout_seconds
                + self.post_treatment_settle_seconds
            )
        if self.warmup is not None:
            total += self.warmup.timeout_seconds
        return total

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": GUEST_PLAN_VERSION,
            "workdir": self.workdir,
            "output_dir": self.output_dir,
            "run_context": self.run_context.to_dict(),
            "env": dict(self.env),
            "vm_settle_seconds": self.vm_settle_seconds,
            "scheduler": self.scheduler.to_dict() if self.scheduler else None,
            "treatment": self.treatment.to_dict() if self.treatment else None,
            "post_treatment_settle_seconds": self.post_treatment_settle_seconds,
            "warmup": self.warmup.to_dict() if self.warmup else None,
            "post_warmup_settle_seconds": self.post_warmup_settle_seconds,
            "measurement": self.measurement.to_dict(),
            "cooldown_seconds": self.cooldown_seconds,
        }


def build_guest_run_plan(
    bench: dict[str, Any],
    scheduler: dict[str, Any],
    libvirt: dict[str, Any],
    output_dir: str = GUEST_OUTPUT_DIR,
    *,
    role: str = "standalone",
    variant: str = "standalone",
    treatment_name: str | None = None,
    treatment: dict[str, Any] | None = None,
) -> GuestRunPlan:
    if (treatment_name is None) != (treatment is None):
        raise ValueError("treatment_name and treatment must be provided together")

    scheduler_plan = None
    if scheduler.get("kind") == "scx":
        scheduler_plan = SchedulerPlan(
            argv=(scheduler["command"], *scheduler.get("args", [])),
            env=dict(scheduler.get("env", {})),
            settle_seconds=int(scheduler.get("settle_seconds", 0)),
        )

    warmup_config = bench.get("warmup")
    return GuestRunPlan(
        workdir=libvirt["workdir"],
        output_dir=output_dir,
        run_context=RunContextPlan(
            role=role,
            variant=variant,
            treatment=treatment_name,
        ),
        env=dict(bench.get("env", {})),
        vm_settle_seconds=int(libvirt.get("vm_settle_seconds", 0)),
        scheduler=scheduler_plan,
        treatment=TreatmentPlan.from_config(treatment) if treatment else None,
        post_treatment_settle_seconds=int(
            treatment.get("post_treatment_settle_seconds", 0)
            if treatment
            else 0
        ),
        warmup=CommandPlan.from_config(warmup_config) if warmup_config else None,
        post_warmup_settle_seconds=int(bench.get("post_warmup_settle_seconds", 0)),
        measurement=CommandPlan.from_config(bench["measurement"]),
        cooldown_seconds=int(bench.get("cooldown_seconds", 0)),
    )


def write_guest_plan(path: Path, plan: GuestRunPlan) -> None:
    path.write_text(
        json.dumps(plan.to_dict(), indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
