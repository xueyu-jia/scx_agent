from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_SCOPE_ROOT = Path("/sys/fs/cgroup/scx-bench")
REDIS_CPUS = (0, 1)
DRIVER_CPUS = (2,)
REDIS_PORTS = (16_379, 16_380)
MIN_CPU_WEIGHT = 1
MAX_CPU_WEIGHT = 10_000
STATE_RING_SIZE = 32
METRIC_NAMES = (
    "redis_p50_latency_us",
    "redis_p95_latency_us",
    "redis_p99_latency_us",
    "redis_qps",
    "redis_cpu_rate",
    "batch_cpu_rate",
    "redis_cpu_share_pct",
    "batch_cpu_share_pct",
    "target_cpu_weight",
    "cpu_pressure_some_pct",
)


class RedisCpuError(RuntimeError):
    pass


@dataclass(frozen=True)
class RedisCpuScope:
    root: Path
    redis: Path
    batch: Path
    driver: Path

    @classmethod
    def from_root(cls, root: str | Path = DEFAULT_SCOPE_ROOT) -> "RedisCpuScope":
        canonical = Path(root).resolve(strict=False)
        return cls(
            root=canonical,
            redis=canonical / "redis",
            batch=canonical / "batch",
            driver=canonical / "driver",
        )


def prepare_scope(
    scope: RedisCpuScope,
    *,
    redis_weight: int = 100,
    batch_weight: int = 100,
    validate_cpus: bool = True,
) -> dict[str, Any]:
    _validate_weight(redis_weight)
    _validate_weight(batch_weight)
    if validate_cpus:
        require_cpu_topology()
    parent = scope.root.parent
    if not parent.is_dir():
        raise RedisCpuError(f"cgroup parent does not exist: {parent}")
    _enable_controller(parent, "cpu")
    try:
        scope.root.mkdir(mode=0o755, exist_ok=True)
    except OSError as exc:
        raise RedisCpuError(f"failed to create cgroup root {scope.root}: {exc}") from exc
    if read_members(scope.root):
        raise RedisCpuError(f"cgroup root must not contain processes: {scope.root}")
    _enable_controller(scope.root, "cpu")
    existed = {path: path.exists() for path in (scope.redis, scope.batch, scope.driver)}
    for path in (scope.redis, scope.batch, scope.driver):
        try:
            path.mkdir(mode=0o755, exist_ok=True)
        except OSError as exc:
            raise RedisCpuError(f"failed to create cgroup {path}: {exc}") from exc
    require_scope(scope)
    for path in (scope.redis, scope.batch):
        if not existed[path] and read_weight(path) != 100:
            raise RedisCpuError(f"new cgroup {path} did not receive Linux cpu.weight default 100")
    write_weight(scope.redis, redis_weight)
    write_weight(scope.batch, batch_weight)
    if read_weight(scope.redis) != redis_weight or read_weight(scope.batch) != batch_weight:
        raise RedisCpuError("cgroup cpu.weight defaults could not be established")
    return scope_state(scope)


def require_scope(scope: RedisCpuScope) -> None:
    for path in (scope.root, scope.redis, scope.batch, scope.driver):
        if not path.is_dir():
            raise RedisCpuError(f"required cgroup does not exist: {path}")
    for path in (scope.redis, scope.batch, scope.driver):
        for name in ("cgroup.procs", "cpu.stat", "cpu.weight", "cpu.max"):
            resource = path / name
            if not resource.is_file():
                raise RedisCpuError(f"required cgroup resource does not exist: {resource}")
        cpu_max = _read_text(path / "cpu.max").split()
        if not cpu_max or cpu_max[0] != "max":
            raise RedisCpuError(f"cgroup {path} must not have a CPU quota")


def require_cpu_topology() -> None:
    available = os.sched_getaffinity(0)
    required = set((*REDIS_CPUS, *DRIVER_CPUS))
    if not required.issubset(available):
        raise RedisCpuError(
            f"redis CPU scenario requires CPUs {sorted(required)}, available={sorted(available)}"
        )


def set_current_affinity(cpus: tuple[int, ...]) -> None:
    try:
        os.sched_setaffinity(0, set(cpus))
    except OSError as exc:
        raise RedisCpuError(f"failed to set CPU affinity to {list(cpus)}: {exc}") from exc
    if tuple(sorted(os.sched_getaffinity(0))) != tuple(sorted(cpus)):
        raise RedisCpuError(f"CPU affinity readback does not match {list(cpus)}")


def scoped_exec_argv(path: Path, cpus: tuple[int, ...], argv: list[str]) -> list[str]:
    if not argv or not argv[0]:
        raise RedisCpuError("scoped exec requires a non-empty argv")
    return [
        sys.executable,
        str(Path(__file__).resolve()),
        "_exec",
        str(path),
        ",".join(str(cpu) for cpu in cpus),
        "--",
        *argv,
    ]


def read_weight(path: Path) -> int:
    value = _read_text(path / "cpu.weight")
    try:
        weight = int(value)
    except ValueError as exc:
        raise RedisCpuError(f"invalid cpu.weight in {path}: {value!r}") from exc
    _validate_weight(weight)
    return weight


def write_weight(path: Path, value: int) -> None:
    _validate_weight(value)
    resource = path / "cpu.weight"
    try:
        resource.write_text(f"{value}\n", encoding="utf-8")
    except OSError as exc:
        raise RedisCpuError(f"failed to write {resource}: {exc}") from exc
    if read_weight(path) != value:
        raise RedisCpuError(f"cpu.weight readback mismatch for {path}")


def read_cpu_stat(path: Path) -> dict[str, int]:
    values: dict[str, int] = {}
    for line in _read_text(path / "cpu.stat").splitlines():
        fields = line.split()
        if len(fields) != 2:
            continue
        try:
            values[fields[0]] = int(fields[1])
        except ValueError as exc:
            raise RedisCpuError(f"invalid cpu.stat row in {path}: {line!r}") from exc
    if "usage_usec" not in values:
        raise RedisCpuError(f"cpu.stat in {path} does not contain usage_usec")
    return values


def read_cpu_pressure_total(path: Path) -> int:
    resource = path / "cpu.pressure"
    for line in _read_text(resource).splitlines():
        fields = line.split()
        if fields and fields[0] == "some":
            for field in fields[1:]:
                if field.startswith("total="):
                    try:
                        return int(field.split("=", 1)[1])
                    except ValueError as exc:
                        raise RedisCpuError(f"invalid CPU pressure total in {resource}") from exc
    raise RedisCpuError(f"CPU pressure some total is unavailable in {resource}")


def read_members(path: Path) -> tuple[int, ...]:
    resource = path / "cgroup.procs"
    if not resource.exists():
        return ()
    members = []
    for line in _read_text(resource).splitlines():
        try:
            pid = int(line)
        except ValueError as exc:
            raise RedisCpuError(f"invalid PID in {resource}: {line!r}") from exc
        if pid > 0:
            members.append(pid)
    return tuple(sorted(set(members)))


def scope_state(scope: RedisCpuScope) -> dict[str, Any]:
    require_scope(scope)
    return {
        "root": str(scope.root),
        "redis": _group_state(scope.redis),
        "batch": _group_state(scope.batch),
        "driver": _group_state(scope.driver),
    }


def process_identity(pid: int) -> dict[str, Any]:
    proc = Path("/proc") / str(pid)
    try:
        stat_text = (proc / "stat").read_text(encoding="utf-8")
        _, tail = stat_text.rsplit(")", 1)
        start_time = int(tail.split()[19])
        executable = os.readlink(proc / "exe")
        affinity = sorted(os.sched_getaffinity(pid))
    except (OSError, ValueError, IndexError) as exc:
        raise RedisCpuError(f"failed to identify process {pid}: {exc}") from exc
    _require_thread_affinity(pid, affinity)
    return {
        "pid": pid,
        "start_time_ticks": start_time,
        "executable": executable,
        "affinity": affinity,
    }


def validate_runtime_identity(runtime: dict[str, Any], scope: RedisCpuScope) -> str:
    if not isinstance(runtime, dict) or runtime.get("version") != 1:
        raise RedisCpuError("runtime identity must be a V1 object")
    if runtime.get("scope") != str(scope.root):
        raise RedisCpuError("runtime identity scope does not match the configured cgroup")
    workload_digest = runtime.get("workload_digest")
    if not _is_digest(workload_digest):
        raise RedisCpuError("runtime workload digest is invalid")
    groups = runtime.get("cgroups")
    if not isinstance(groups, dict) or set(groups) != {"redis", "batch", "driver"}:
        raise RedisCpuError("runtime cgroup identity is invalid")
    for name, path in (("redis", scope.redis), ("batch", scope.batch), ("driver", scope.driver)):
        identity = groups.get(name)
        if not isinstance(identity, dict) or identity.get("path") != str(path):
            raise RedisCpuError(f"runtime {name} cgroup path changed")
        if identity.get("inode") != path.stat().st_ino:
            raise RedisCpuError(f"runtime {name} cgroup inode changed")

    processes = runtime.get("processes")
    if not isinstance(processes, dict) or set(processes) != {"redis", "batch", "loadgen"}:
        raise RedisCpuError("runtime process identity is invalid")
    redis_processes = processes.get("redis")
    if not isinstance(redis_processes, list) or len(redis_processes) != 2:
        raise RedisCpuError("runtime requires two Redis process identities")
    stable = [*redis_processes, processes.get("batch"), processes.get("loadgen")]
    for expected in stable:
        if not isinstance(expected, dict):
            raise RedisCpuError("runtime process identity entry is invalid")
        pid = expected.get("pid")
        if isinstance(pid, bool) or not isinstance(pid, int) or pid < 1:
            raise RedisCpuError("runtime process PID is invalid")
        observed = process_identity(pid)
        if observed != expected:
            raise RedisCpuError(f"runtime process identity changed for PID {pid}")

    redis_members = set(read_members(scope.redis))
    redis_pids = {item["pid"] for item in redis_processes}
    if redis_members != redis_pids:
        raise RedisCpuError("Redis cgroup membership drifted")
    if processes["batch"]["pid"] not in read_members(scope.batch):
        raise RedisCpuError("batch parent left its cgroup")
    batch_parent = processes["batch"]["pid"]
    batch_workers = set(read_members(scope.batch)) - {batch_parent}
    if len(batch_workers) != 2 or any(
        not _is_descendant(worker, batch_parent) for worker in batch_workers
    ):
        raise RedisCpuError("batch cgroup must contain exactly two stress-ng worker descendants")
    if processes["loadgen"]["pid"] not in read_members(scope.driver):
        raise RedisCpuError("loadgen parent left its cgroup")
    loadgen_config = runtime.get("loadgen")
    if not isinstance(loadgen_config, dict):
        raise RedisCpuError("runtime loadgen identity is invalid")
    benchmark = loadgen_config.get("benchmark_executable")
    if not isinstance(benchmark, str) or not Path(benchmark).is_absolute():
        raise RedisCpuError("runtime benchmark executable is invalid")
    loadgen_parent = processes["loadgen"]["pid"]
    benchmark_processes = set(read_members(scope.driver)) - {loadgen_parent}
    for pid in benchmark_processes:
        try:
            identity = process_identity(pid)
        except RedisCpuError:
            if pid not in read_members(scope.driver):
                continue
            raise
        if (
            not _is_descendant(pid, loadgen_parent)
            or identity["executable"] != benchmark
            or identity["affinity"] != list(DRIVER_CPUS)
        ):
            raise RedisCpuError("driver cgroup contains an unexpected process")

    fingerprint_payload = {
        "workload_digest": workload_digest,
        "cgroups": groups,
        "processes": processes,
        "redis": runtime.get("redis"),
        "loadgen": runtime.get("loadgen"),
    }
    return content_digest(fingerprint_payload)


def readiness_processes(runtime: dict[str, Any]) -> list[dict[str, Any]]:
    processes = runtime["processes"]
    identities = [*processes["redis"], processes["batch"], processes["loadgen"]]
    return [
        {
            "pid": item["pid"],
            "start_time_ticks": item["start_time_ticks"],
            "executable": item["executable"],
        }
        for item in identities
    ]


def parse_redis_benchmark(text: str) -> dict[str, float]:
    summary = re.search(
        r"latency summary \(msec\):.*?\n\s*avg\s+min\s+p50\s+p95\s+p99\s+max\s*\n"
        r"\s*([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)\s+([0-9.]+)",
        text,
        flags=re.IGNORECASE | re.DOTALL,
    )
    throughput = re.search(r"throughput summary:\s*([0-9.]+)", text, flags=re.IGNORECASE)
    if summary is None or throughput is None:
        raise RedisCpuError("redis-benchmark output is missing latency or throughput summary")
    return {
        "p50_latency_us": float(summary.group(3)) * 1_000.0,
        "p95_latency_us": float(summary.group(4)) * 1_000.0,
        "p99_latency_us": float(summary.group(5)) * 1_000.0,
        "qps": float(throughput.group(1)),
    }


def aggregate_shards(
    first: dict[str, float],
    second: dict[str, float],
    *,
    redis_usage_usec: int,
    batch_usage_usec: int,
    pressure_usec: int,
    elapsed_seconds: float,
    weight: int,
) -> dict[str, float]:
    if min(redis_usage_usec, batch_usage_usec, pressure_usec) < 0 or elapsed_seconds <= 0:
        raise RedisCpuError("loadgen counters must be monotonic and elapsed time must be positive")
    total_usage = redis_usage_usec + batch_usage_usec
    if total_usage <= 0:
        raise RedisCpuError("Redis and batch consumed no CPU")
    metrics = {
        "redis_p50_latency_us": max(first["p50_latency_us"], second["p50_latency_us"]),
        "redis_p95_latency_us": max(first["p95_latency_us"], second["p95_latency_us"]),
        "redis_p99_latency_us": max(first["p99_latency_us"], second["p99_latency_us"]),
        "redis_qps": first["qps"] + second["qps"],
        "redis_cpu_rate": redis_usage_usec / 1_000_000.0 / elapsed_seconds,
        "batch_cpu_rate": batch_usage_usec / 1_000_000.0 / elapsed_seconds,
        "redis_cpu_share_pct": redis_usage_usec / total_usage * 100.0,
        "batch_cpu_share_pct": batch_usage_usec / total_usage * 100.0,
        "target_cpu_weight": float(weight),
        "cpu_pressure_some_pct": pressure_usec / 1_000_000.0 / elapsed_seconds * 100.0,
    }
    if set(metrics) != set(METRIC_NAMES):
        raise RedisCpuError("loadgen metric schema is incomplete")
    return metrics


def content_digest(value: Any) -> str:
    payload = json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return "sha256:" + hashlib.sha256(payload).hexdigest()


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise RedisCpuError(f"failed to read JSON from {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise RedisCpuError(f"JSON document must be an object: {path}")
    return value


def write_json_atomic(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + f".{os.getpid()}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def wait_for_empty(scope: RedisCpuScope, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not any(read_members(path) for path in (scope.redis, scope.batch, scope.driver)):
            return
        time.sleep(0.05)
    raise RedisCpuError("Redis CPU scenario cgroups still contain processes")


def _group_state(path: Path) -> dict[str, Any]:
    return {
        "path": str(path),
        "inode": path.stat().st_ino,
        "weight": read_weight(path),
        "members": list(read_members(path)),
        "cpu_stat": read_cpu_stat(path),
    }


def _enable_controller(path: Path, controller: str) -> None:
    available = set(_read_text(path / "cgroup.controllers").split())
    if controller not in available:
        raise RedisCpuError(f"controller {controller!r} is unavailable below {path}")
    resource = path / "cgroup.subtree_control"
    enabled = set(_read_text(resource).split())
    if controller in enabled:
        return
    try:
        resource.write_text(f"+{controller}\n", encoding="utf-8")
    except OSError as exc:
        raise RedisCpuError(f"failed to enable {controller} below {path}: {exc}") from exc


def _validate_weight(value: int) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or not MIN_CPU_WEIGHT <= value <= MAX_CPU_WEIGHT:
        raise RedisCpuError(f"cpu.weight must be between {MIN_CPU_WEIGHT} and {MAX_CPU_WEIGHT}")


def _is_digest(value: Any) -> bool:
    return isinstance(value, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", value) is not None


def _require_thread_affinity(pid: int, expected: list[int]) -> None:
    task_root = Path("/proc") / str(pid) / "task"
    try:
        tids = [int(path.name) for path in task_root.iterdir() if path.name.isdigit()]
    except OSError as exc:
        raise RedisCpuError(f"failed to enumerate threads for process {pid}: {exc}") from exc
    if not tids:
        raise RedisCpuError(f"process {pid} has no observable threads")
    for tid in tids:
        try:
            affinity = sorted(os.sched_getaffinity(tid))
        except OSError as exc:
            raise RedisCpuError(f"failed to read affinity for thread {tid}: {exc}") from exc
        if affinity != expected:
            raise RedisCpuError(
                f"thread {tid} affinity {affinity} does not match expected {expected}"
            )


def _is_descendant(pid: int, ancestor: int) -> bool:
    current = pid
    visited: set[int] = set()
    while current > 1 and current not in visited:
        if current == ancestor:
            return True
        visited.add(current)
        proc = Path("/proc") / str(current) / "stat"
        try:
            stat_text = proc.read_text(encoding="utf-8")
            _, tail = stat_text.rsplit(")", 1)
            current = int(tail.split()[1])
        except (OSError, ValueError, IndexError):
            return False
    return False


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise RedisCpuError(f"failed to read {path}: {exc}") from exc


def _exec(argv: list[str]) -> int:
    if len(argv) < 4 or argv[2] != "--":
        raise RedisCpuError("invalid scoped exec invocation")
    cgroup = Path(argv[0])
    try:
        cpus = tuple(int(value) for value in argv[1].split(","))
    except ValueError as exc:
        raise RedisCpuError("invalid scoped exec CPU list") from exc
    (cgroup / "cgroup.procs").write_text(f"{os.getpid()}\n", encoding="utf-8")
    set_current_affinity(cpus)
    os.execv(argv[3], argv[3:])
    return 1


if __name__ == "__main__":
    try:
        if len(sys.argv) > 1 and sys.argv[1] == "_exec":
            raise SystemExit(_exec(sys.argv[2:]))
        raise SystemExit("common.py is not a standalone command")
    except RedisCpuError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(125) from exc
