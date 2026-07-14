from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any


GUEST_OUTPUT_DIR = "/scx_bench_out"
GUEST_PLAN_VERSION = 1
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
    env: dict[str, str]
    vm_settle_seconds: int
    scheduler: SchedulerPlan | None
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
        if self.warmup is not None:
            total += self.warmup.timeout_seconds
        return total

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": GUEST_PLAN_VERSION,
            "workdir": self.workdir,
            "output_dir": self.output_dir,
            "env": dict(self.env),
            "vm_settle_seconds": self.vm_settle_seconds,
            "scheduler": self.scheduler.to_dict() if self.scheduler else None,
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
) -> GuestRunPlan:
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
        env=dict(bench.get("env", {})),
        vm_settle_seconds=int(libvirt.get("vm_settle_seconds", 0)),
        scheduler=scheduler_plan,
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
