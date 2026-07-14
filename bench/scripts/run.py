#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import sys
import threading
from concurrent.futures import FIRST_COMPLETED, Future, ThreadPoolExecutor, wait
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from bench.analysis.compare import build_analysis
from bench.analysis.loader import load_result_dir
from bench.analysis.report import write_html_report
from bench.base_image import BaseImageManifestError, verify_base_image_manifest
from bench.config.parser import ConfigError, RunSpec, expand_plan, load_config, parse_cpu_list
from bench.runner import run_specs


DEFAULT_EXPERIMENT_ROOT = Path("bench/results/experiments")
LATEST_REPORT_LINK = Path("bench/results/latest_report.html")
_LOG_LOCK = threading.Lock()
_EXECUTION_ORDER_LOCK = threading.Lock()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run a full baseline/candidate benchmark experiment")
    parser.add_argument("--config", default="bench/configs/local.config")
    parser.add_argument("--plan", required=True)
    parser.add_argument("--baseline", required=True, help="baseline scheduler name from config.schedulers")
    parser.add_argument("--candidate", required=True, help="candidate scheduler name from config.schedulers")
    parser.add_argument(
        "--order",
        choices=("alternating", "sequential"),
        default="alternating",
        help="execution order across baseline/candidate",
    )
    parser.add_argument("--output", help="experiment output directory")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument(
        "--progress-interval",
        type=int,
        default=30,
        help="seconds between long-running VM progress messages; use 0 to disable heartbeats",
    )
    parser.add_argument(
        "--parallel",
        help="max comparison pairs to run concurrently; use 'auto' for resource-limited scheduling",
    )
    args = parser.parse_args(argv)

    _log(f"loading config: {args.config}")
    base_image_manifest: dict[str, Any] | None = None
    try:
        config = load_config(args.config)
        if not args.dry_run:
            base_image_manifest = verify_base_image_manifest(
                config["libvirt"]["root_image"],
                REPO_ROOT,
            )
        baseline = _scheduler(config, args.baseline)
        candidate = _scheduler(config, args.candidate)
        specs = expand_plan(config, args.plan)
        parallel = _resolve_parallel(config, args.parallel)
    except ConfigError as exc:
        print(f"config error: {exc}", file=sys.stderr)
        return 2
    except BaseImageManifestError as exc:
        print(f"base image error: {exc}", file=sys.stderr)
        return 2

    if args.baseline == args.candidate:
        print("baseline and candidate must be different schedulers", file=sys.stderr)
        return 2

    experiment_dir = Path(args.output) if args.output else _default_experiment_dir(
        args.baseline,
        args.candidate,
    )
    runs_dir = experiment_dir / "runs"
    analysis_dir = experiment_dir / "analysis"
    runs_dir.mkdir(parents=True, exist_ok=True)
    analysis_dir.mkdir(parents=True, exist_ok=True)

    pairs = _build_pairs(specs, args.baseline, baseline, args.candidate, candidate, args.order)
    total_runs = len(pairs) * 2
    progress = _Progress(total_runs)
    _log(
        "experiment start: "
        f"plan={args.plan} pairs={len(pairs)} total_runs={total_runs} "
        f"baseline={args.baseline} candidate={args.candidate} "
        f"order={args.order} parallel={parallel}"
    )
    _log(f"output: {experiment_dir}")

    metadata = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "config": str(Path(args.config).resolve()),
        "plan": args.plan,
        "baseline": args.baseline,
        "candidate": args.candidate,
        "order": args.order,
        "parallel": parallel,
        "dry_run": args.dry_run,
        "base_image_manifest": base_image_manifest,
        "experiment_dir": str(experiment_dir.resolve()),
        "execution_order": [],
    }
    _write_json(experiment_dir / "metadata.json", metadata)

    execution_order: list[dict[str, Any]] = []
    try:
        _run_pairs(
            pairs,
            config,
            runs_dir,
            args,
            parallel,
            execution_order,
            progress,
        )
    except ConfigError as exc:
        print(f"config error: {exc}", file=sys.stderr)
        return 2

    metadata["execution_order"] = execution_order
    _write_json(experiment_dir / "metadata.json", metadata)

    _log("analysis: loading run results")
    baseline_dir = runs_dir / args.baseline
    candidate_dir = runs_dir / args.candidate
    _log("analysis: building baseline/candidate comparison")
    analysis = build_analysis(
        load_result_dir(baseline_dir, args.baseline),
        load_result_dir(candidate_dir, args.candidate),
        baseline_label=args.baseline,
        candidate_label=args.candidate,
    )
    _write_json(analysis_dir / "analysis.json", analysis)
    _write_json(
        analysis_dir / "metadata.json",
        {
            **metadata,
            "baseline_result_dir": str(baseline_dir.resolve()),
            "candidate_result_dir": str(candidate_dir.resolve()),
        },
    )
    _log("report: writing HTML")
    write_html_report(analysis, analysis_dir / "report.html")
    latest_report = _update_latest_report_link(analysis_dir / "report.html")

    _log("experiment complete")
    _log(f"experiment: {experiment_dir}")
    _log(f"baseline results: {baseline_dir}")
    _log(f"candidate results: {candidate_dir}")
    _log(f"report: {analysis_dir / 'report.html'}")
    _log(f"latest report: {latest_report}")
    return 0


@dataclass(frozen=True)
class _SchedulerRun:
    name: str
    config: dict[str, Any]


@dataclass(frozen=True)
class _ComparisonPair:
    index: int
    spec: RunSpec
    order: tuple[_SchedulerRun, _SchedulerRun]


@dataclass(frozen=True)
class _CoreGroup:
    package_id: int
    core_id: int
    siblings: tuple[int, ...]


@dataclass(frozen=True)
class _Allocation:
    pin_cpus: str
    reserved_cpus: str
    memory_bytes: int
    placement: dict[str, Any]


class _HostResourcePool:
    def __init__(
        self,
        core_groups: list[_CoreGroup],
        memory_bytes: int,
        smt_policy: str,
    ) -> None:
        self._free_cores = core_groups
        self._memory_free = memory_bytes
        self._smt_policy = smt_policy

    @classmethod
    def from_config(cls, config: dict[str, Any], dry_run: bool) -> "_HostResourcePool":
        executor = config.get("executor", {})
        if executor.get("cpu_source", "configured") != "isolated":
            raise ConfigError("pin_cpus: auto requires executor.cpu_source: isolated")

        smt_policy = executor.get("smt_policy", "use_all_siblings")
        target_cpus = _target_isolated_cpus(executor, require_isolated=not dry_run)
        core_groups = _read_isolated_core_groups(target_cpus)
        if not core_groups:
            raise ConfigError("no complete isolated physical core sibling groups are available")

        guard_bytes = int(executor.get("memory_guard_gb", 0)) * 1024**3
        memory_bytes = max(0, _read_mem_available_bytes() - guard_bytes)
        return cls(core_groups, memory_bytes, smt_policy)

    def allocate(self, spec: RunSpec) -> _Allocation | None:
        memory_bytes = _parse_memory_bytes(spec.machine["memory"])
        if memory_bytes > self._memory_free:
            return None

        selected: list[_CoreGroup] = []
        logical_count = 0
        for group in self._free_cores:
            selected.append(group)
            logical_count += len(group.siblings)
            if logical_count >= spec.machine["vcpus"]:
                break

        if logical_count < spec.machine["vcpus"]:
            return None
        if logical_count != spec.machine["vcpus"]:
            raise ConfigError(
                f"machines.{spec.machine_name}.vcpus must match whole SMT sibling groups; "
                f"requested {spec.machine['vcpus']}, next allocation would provide {logical_count}"
            )

        selected_set = set(selected)
        self._free_cores = [group for group in self._free_cores if group not in selected_set]
        self._memory_free -= memory_bytes

        pin_cpus = sorted(cpu for group in selected for cpu in group.siblings)
        placement = {
            "cpu_source": "isolated",
            "smt_policy": self._smt_policy,
            "pin_cpus": _format_cpu_list(pin_cpus),
            "reserved_cpus": _format_cpu_list(pin_cpus),
            "memory": spec.machine["memory"],
            "physical_cores": [
                {
                    "package_id": group.package_id,
                    "core_id": group.core_id,
                    "siblings": list(group.siblings),
                }
                for group in selected
            ],
        }
        return _Allocation(
            pin_cpus=placement["pin_cpus"],
            reserved_cpus=placement["reserved_cpus"],
            memory_bytes=memory_bytes,
            placement=placement,
        )

    def release(self, allocation: _Allocation) -> None:
        cores = [
            _CoreGroup(
                package_id=core["package_id"],
                core_id=core["core_id"],
                siblings=tuple(core["siblings"]),
            )
            for core in allocation.placement["physical_cores"]
        ]
        self._free_cores.extend(cores)
        self._free_cores.sort(key=lambda group: (group.package_id, group.core_id, group.siblings))
        self._memory_free += allocation.memory_bytes


def _run_pairs(
    pairs: list[_ComparisonPair],
    config: dict[str, Any],
    runs_dir: Path,
    args: argparse.Namespace,
    parallel: int | str,
    execution_order: list[dict[str, Any]],
    progress: "_Progress",
) -> None:
    if not pairs:
        return

    auto_pinning = any(pair.spec.machine.get("pin_cpus") == "auto" for pair in pairs)
    max_parallel = len(pairs) if parallel == "auto" else int(parallel)
    if max_parallel > 1 and not auto_pinning:
        raise ConfigError("parallel execution requires machines.*.pin_cpus: auto")
    if auto_pinning:
        pool: _HostResourcePool | None = _HostResourcePool.from_config(config, args.dry_run)
    else:
        pool = None

    if max_parallel == 1:
        for pair in pairs:
            allocation = _allocate_pair(pool, pair)
            if pool is not None and allocation is None:
                raise ConfigError(
                    f"insufficient isolated CPU or memory resources for pair {pair.index}: "
                    f"machine={pair.spec.machine_name} vcpus={pair.spec.machine['vcpus']} "
                    f"memory={pair.spec.machine['memory']}"
                )
            try:
                _run_pair(pair, allocation, runs_dir, args, execution_order, progress)
            finally:
                if pool is not None and allocation is not None:
                    pool.release(allocation)
        return

    _run_pairs_parallel(
        pairs,
        pool,
        max_parallel,
        runs_dir,
        args,
        execution_order,
        progress,
    )


def _run_pairs_parallel(
    pairs: list[_ComparisonPair],
    pool: _HostResourcePool | None,
    max_parallel: int,
    runs_dir: Path,
    args: argparse.Namespace,
    execution_order: list[dict[str, Any]],
    progress: "_Progress",
) -> None:
    pending = list(pairs)
    active: dict[Future[None], _Allocation | None] = {}

    with ThreadPoolExecutor(max_workers=max_parallel) as executor:
        while pending or active:
            launched = False
            while pending and len(active) < max_parallel:
                pair, allocation = _pop_allocatable_pair(pending, pool)
                if pair is None:
                    break
                future = executor.submit(
                    _run_pair,
                    pair,
                    allocation,
                    runs_dir,
                    args,
                    execution_order,
                    progress,
                )
                active[future] = allocation
                launched = True

            if active:
                if launched and pending and len(active) < max_parallel:
                    continue
                done, _ = wait(active, return_when=FIRST_COMPLETED)
                for future in done:
                    allocation = active.pop(future)
                    try:
                        future.result()
                    finally:
                        if pool is not None and allocation is not None:
                            pool.release(allocation)
                continue

            blocked = pending[0]
            raise ConfigError(
                f"insufficient isolated CPU or memory resources for pair {blocked.index}: "
                f"machine={blocked.spec.machine_name} vcpus={blocked.spec.machine['vcpus']} "
                f"memory={blocked.spec.machine['memory']}"
            )


def _pop_allocatable_pair(
    pending: list[_ComparisonPair],
    pool: _HostResourcePool | None,
) -> tuple[_ComparisonPair | None, _Allocation | None]:
    if pool is None:
        return pending.pop(0), None

    for index, pair in enumerate(pending):
        allocation = _allocate_pair(pool, pair)
        if allocation is not None:
            return pending.pop(index), allocation
    return None, None


def _run_pair(
    pair: _ComparisonPair,
    allocation: _Allocation | None,
    runs_dir: Path,
    args: argparse.Namespace,
    execution_order: list[dict[str, Any]],
    progress: "_Progress",
) -> None:
    spec = _place_spec(pair.spec, allocation)
    placement = allocation.placement if allocation is not None else _configured_placement(spec)
    _log(
        f"pair {pair.index} start: {_spec_label(spec)} "
        f"pin_cpus={placement.get('pin_cpus')}"
    )
    for scheduler in pair.order:
        _append_execution_order(
            execution_order,
            {
                "pair": pair.index,
                "scheduler": scheduler.name,
                "run_index": spec.run_index,
                "machine": spec.machine_name,
                "suite": spec.suite_name,
                "bench": spec.bench_name,
                "placement": placement,
            },
        )
        run_specs(
            [spec],
            output_dir=runs_dir / scheduler.name,
            dry_run=args.dry_run,
            label=scheduler.name,
            scheduler=scheduler.config,
            config_path=args.config,
            progress_callback=progress.callback,
            progress_interval=args.progress_interval,
            placement=placement,
        )
    _log(f"pair {pair.index} done: {_spec_label(spec)}")


def _allocate_pair(pool: _HostResourcePool | None, pair: _ComparisonPair) -> _Allocation | None:
    if pool is None:
        return None
    allocation = pool.allocate(pair.spec)
    if allocation is not None:
        _log(
            f"pair {pair.index} allocated: machine={pair.spec.machine_name} "
            f"pin_cpus={allocation.pin_cpus}"
        )
    return allocation


def _place_spec(spec: RunSpec, allocation: _Allocation | None) -> RunSpec:
    if allocation is None:
        return spec
    machine = dict(spec.machine)
    machine["pin_cpus"] = allocation.pin_cpus
    return replace(spec, machine=machine)


def _configured_placement(spec: RunSpec) -> dict[str, Any]:
    return {
        "cpu_source": "configured",
        "pin_cpus": spec.machine["pin_cpus"],
        "reserved_cpus": spec.machine["pin_cpus"],
        "memory": spec.machine["memory"],
    }


def _build_pairs(
    specs: list[RunSpec],
    baseline_name: str,
    baseline: dict[str, Any],
    candidate_name: str,
    candidate: dict[str, Any],
    order: str,
) -> list[_ComparisonPair]:
    pairs: list[_ComparisonPair] = []
    for index, spec in enumerate(specs, start=1):
        baseline_run = _SchedulerRun(baseline_name, baseline)
        candidate_run = _SchedulerRun(candidate_name, candidate)
        if order == "alternating" and spec.run_index % 2 == 0:
            scheduler_order = (candidate_run, baseline_run)
        else:
            scheduler_order = (baseline_run, candidate_run)
        pairs.append(_ComparisonPair(index=index, spec=spec, order=scheduler_order))
    return pairs


def _resolve_parallel(config: dict[str, Any], value: str | None) -> int | str:
    raw = value if value is not None else config.get("executor", {}).get("parallel", 1)
    if raw == "auto":
        return "auto"
    if isinstance(raw, int):
        if raw < 1:
            raise ConfigError("parallel must be 'auto' or a positive integer")
        return raw
    if isinstance(raw, str) and raw.isdigit() and int(raw) >= 1:
        return int(raw)
    raise ConfigError("parallel must be 'auto' or a positive integer")


def _scheduler(config: dict[str, Any], name: str) -> dict[str, Any]:
    schedulers = config["schedulers"]
    if name not in schedulers:
        raise ConfigError(f"unknown scheduler: {name}")
    scheduler = dict(schedulers[name])
    scheduler["name"] = name
    return scheduler


def _default_experiment_dir(baseline: str, candidate: str) -> Path:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_EXPERIMENT_ROOT / f"{timestamp}__{_safe(baseline)}_vs_{_safe(candidate)}"


def _safe(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in ("-", "_") else "_" for ch in value)


def _target_isolated_cpus(executor: dict[str, Any], require_isolated: bool) -> list[int]:
    configured = executor.get("isolated_cpus")
    if configured:
        target = parse_cpu_list(configured)
    else:
        target = _read_sys_cpu_list(Path("/sys/devices/system/cpu/isolated"))
        if not target:
            raise ConfigError("executor.isolated_cpus is required when no isolated CPUs are active")

    if require_isolated:
        isolated = set(_read_sys_cpu_list(Path("/sys/devices/system/cpu/isolated")))
        missing = sorted(set(target) - isolated)
        if missing:
            raise ConfigError(
                "configured executor.isolated_cpus are not isolated on this host: "
                f"{_format_cpu_list(missing)}"
            )
    return target


def _read_isolated_core_groups(target_cpus: list[int]) -> list[_CoreGroup]:
    target = set(target_cpus)
    groups: dict[tuple[int, ...], _CoreGroup] = {}
    for cpu in sorted(target):
        cpu_path = Path(f"/sys/devices/system/cpu/cpu{cpu}")
        if not cpu_path.exists():
            raise ConfigError(f"configured CPU does not exist on host: {cpu}")
        siblings = tuple(_read_thread_siblings(cpu))
        if not set(siblings).issubset(target):
            raise ConfigError(
                f"CPU {cpu} has SMT sibling(s) outside executor.isolated_cpus: "
                f"{_format_cpu_list(sorted(set(siblings) - target))}"
            )
        groups[siblings] = _CoreGroup(
            package_id=_read_topology_int(cpu, "physical_package_id"),
            core_id=_read_topology_int(cpu, "core_id"),
            siblings=siblings,
        )
    return sorted(groups.values(), key=lambda group: (group.package_id, group.core_id, group.siblings))


def _read_thread_siblings(cpu: int) -> list[int]:
    path = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")
    if not path.exists():
        return [cpu]
    return parse_cpu_list(path.read_text(encoding="utf-8").strip())


def _read_topology_int(cpu: int, name: str) -> int:
    path = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/{name}")
    if not path.exists():
        return 0
    text = path.read_text(encoding="utf-8").strip()
    return int(text) if text else 0


def _read_sys_cpu_list(path: Path) -> list[int]:
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8").strip()
    if not text or text == "(null)":
        return []
    return parse_cpu_list(text)


def _read_mem_available_bytes() -> int:
    path = Path("/proc/meminfo")
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith("MemAvailable:"):
            parts = line.split()
            return int(parts[1]) * 1024
    raise ConfigError("/proc/meminfo does not contain MemAvailable")


def _parse_memory_bytes(value: str) -> int:
    text = value.strip().upper()
    units = {
        "K": 1024,
        "M": 1024**2,
        "G": 1024**3,
        "T": 1024**4,
    }
    if text[-1:] in units:
        number = text[:-1]
        multiplier = units[text[-1]]
    else:
        number = text
        multiplier = 1
    if not number.isdigit():
        raise ConfigError(f"invalid memory size: {value}")
    return int(number) * multiplier


def _format_cpu_list(cpus: list[int]) -> str:
    if not cpus:
        return ""
    values = sorted(cpus)
    ranges: list[str] = []
    start = prev = values[0]
    for cpu in values[1:]:
        if cpu == prev + 1:
            prev = cpu
            continue
        ranges.append(f"{start}-{prev}" if start != prev else str(start))
        start = prev = cpu
    ranges.append(f"{start}-{prev}" if start != prev else str(start))
    return ",".join(ranges)


def _append_execution_order(
    execution_order: list[dict[str, Any]],
    entry: dict[str, Any],
) -> None:
    with _EXECUTION_ORDER_LOCK:
        execution_order.append(entry)


def _write_json(path: Path, data: Any) -> None:
    path.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")


def _update_latest_report_link(report_path: Path) -> Path:
    link = LATEST_REPORT_LINK
    link.parent.mkdir(parents=True, exist_ok=True)
    if link.exists() or link.is_symlink():
        link.unlink()
    link.symlink_to(Path.cwd() / report_path)
    return link


class _Progress:
    def __init__(self, total: int) -> None:
        self.total = total
        self.started = 0
        self.done = 0
        self.active: dict[tuple[Any, ...], int] = {}
        self._lock = threading.Lock()

    def callback(self, event: str, payload: dict[str, Any]) -> None:
        spec = payload["spec"]
        label = payload["label"]
        key = _progress_key(label, spec)
        with self._lock:
            if event == "start":
                self.started += 1
                current = self.started
                self.active[key] = current
                _log(f"run {current}/{self.total} start: scheduler={label} {_spec_label(spec)}")
            elif event == "heartbeat":
                current = self.active.get(key, self.started)
                elapsed = int(payload["elapsed_seconds"])
                _log(
                    f"run {current}/{self.total} running: "
                    f"scheduler={label} elapsed={elapsed}s {_spec_label(spec)}"
                )
            elif event == "end":
                current = self.active.pop(key, self.done + 1)
                self.done += 1
                result = payload["result"]
                status = result.get("status", "UNKNOWN")
                duration = result.get("duration_seconds")
                metric_count = len(result.get("bench_metrics", {}))
                duration_text = f"{duration:.1f}s" if isinstance(duration, (int, float)) else "unknown"
                _log(
                    f"run {current}/{self.total} done: scheduler={label} "
                    f"status={status} duration={duration_text} metrics={metric_count} {_spec_label(spec)}"
                )


def _progress_key(label: str, spec: Any) -> tuple[Any, ...]:
    return (
        label,
        spec.run_index,
        spec.machine_name,
        spec.suite_name,
        spec.bench_name,
    )


def _spec_label(spec: Any) -> str:
    return (
        f"run_index={spec.run_index} machine={spec.machine_name} "
        f"suite={spec.suite_name} bench={spec.bench_name}"
    )


def _log(message: str) -> None:
    timestamp = datetime.now().strftime("%H:%M:%S")
    with _LOG_LOCK:
        print(f"[{timestamp}] {message}", flush=True)


if __name__ == "__main__":
    raise SystemExit(main())
