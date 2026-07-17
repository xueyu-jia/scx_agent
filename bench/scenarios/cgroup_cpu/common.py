from __future__ import annotations

import hashlib
import json
import os
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


DEFAULT_SCOPE_ROOT = Path("/sys/fs/cgroup/scx-bench")
MIN_CPU_WEIGHT = 1
MAX_CPU_WEIGHT = 10_000


class CgroupCpuError(RuntimeError):
    pass


@dataclass(frozen=True)
class CgroupCpuScope:
    root: Path
    target: Path
    neighbor: Path

    @classmethod
    def from_root(cls, root: str | Path = DEFAULT_SCOPE_ROOT) -> "CgroupCpuScope":
        canonical = Path(root).resolve(strict=False)
        return cls(
            root=canonical,
            target=canonical / "target",
            neighbor=canonical / "neighbor",
        )


def prepare_scope(
    scope: CgroupCpuScope,
    *,
    target_weight: int,
    neighbor_weight: int,
) -> dict[str, Any]:
    _validate_weight(target_weight)
    _validate_weight(neighbor_weight)
    parent = scope.root.parent
    if not parent.is_dir():
        raise CgroupCpuError(f"cgroup parent does not exist: {parent}")

    _enable_controller(parent, "cpu")
    try:
        scope.root.mkdir(mode=0o755, exist_ok=True)
    except OSError as exc:
        raise CgroupCpuError(f"failed to create cgroup scope root {scope.root}: {exc}") from exc
    if read_members(scope.root):
        raise CgroupCpuError(f"cgroup scope root must not contain processes: {scope.root}")

    _enable_controller(scope.root, "cpu")
    for path in (scope.target, scope.neighbor):
        try:
            path.mkdir(mode=0o755, exist_ok=True)
        except OSError as exc:
            raise CgroupCpuError(f"failed to create cgroup {path}: {exc}") from exc

    write_weight(scope.target, target_weight)
    write_weight(scope.neighbor, neighbor_weight)
    return scope_state(scope)


def require_scope(scope: CgroupCpuScope) -> None:
    for path in (scope.root, scope.target, scope.neighbor):
        if not path.is_dir():
            raise CgroupCpuError(f"required cgroup does not exist: {path}")
    for path in (scope.target, scope.neighbor):
        for name in ("cgroup.procs", "cpu.stat", "cpu.weight"):
            resource = path / name
            if not resource.is_file():
                raise CgroupCpuError(f"required cgroup resource does not exist: {resource}")


def scope_state(scope: CgroupCpuScope) -> dict[str, Any]:
    require_scope(scope)
    return {
        "root": str(scope.root),
        "target": _group_state(scope.target),
        "neighbor": _group_state(scope.neighbor),
    }


def read_weight(path: Path) -> int:
    value = _read_text(path / "cpu.weight")
    try:
        weight = int(value)
    except ValueError as exc:
        raise CgroupCpuError(f"invalid cpu.weight in {path}: {value!r}") from exc
    _validate_weight(weight)
    return weight


def write_weight(path: Path, value: int) -> None:
    _validate_weight(value)
    resource = path / "cpu.weight"
    try:
        resource.write_text(f"{value}\n", encoding="utf-8")
    except OSError as exc:
        raise CgroupCpuError(f"failed to write {resource}: {exc}") from exc
    observed = read_weight(path)
    if observed != value:
        raise CgroupCpuError(
            f"cpu.weight readback mismatch for {path}: expected {value}, observed {observed}"
        )


def read_cpu_stat(path: Path) -> dict[str, int]:
    values: dict[str, int] = {}
    for line in _read_text(path / "cpu.stat").splitlines():
        fields = line.split()
        if len(fields) != 2:
            continue
        try:
            values[fields[0]] = int(fields[1])
        except ValueError as exc:
            raise CgroupCpuError(f"invalid cpu.stat row in {path}: {line!r}") from exc
    if "usage_usec" not in values:
        raise CgroupCpuError(f"cpu.stat in {path} does not contain usage_usec")
    return values


def read_members(path: Path) -> tuple[int, ...]:
    resource = path / "cgroup.procs"
    if not resource.exists():
        return ()
    members = []
    for line in _read_text(resource).splitlines():
        try:
            pid = int(line)
        except ValueError as exc:
            raise CgroupCpuError(f"invalid PID in {resource}: {line!r}") from exc
        if pid > 0:
            members.append(pid)
    return tuple(sorted(set(members)))


def cgroup_exec_argv(path: Path, argv: list[str]) -> list[str]:
    if not argv or any(not item for item in argv):
        raise CgroupCpuError("cgroup exec requires a non-empty argv")
    return [sys.executable, str(Path(__file__).resolve()), "_exec", str(path), "--", *argv]


def sample_cpu_service(
    scope: CgroupCpuScope,
    *,
    window_ms: int,
) -> dict[str, Any]:
    require_scope(scope)
    if not 100 <= window_ms <= 5_000:
        raise CgroupCpuError("measurement window_ms must be between 100 and 5000")

    fingerprint_before = workload_fingerprint(scope)
    target_before = read_cpu_stat(scope.target)
    neighbor_before = read_cpu_stat(scope.neighbor)
    started_at_ns = time.time_ns()
    monotonic_started = time.monotonic_ns()
    time.sleep(window_ms / 1000.0)
    target_after = read_cpu_stat(scope.target)
    neighbor_after = read_cpu_stat(scope.neighbor)
    monotonic_ended = time.monotonic_ns()
    ended_at_ns = time.time_ns()
    fingerprint_after = workload_fingerprint(scope)
    if fingerprint_before != fingerprint_after:
        raise CgroupCpuError("cgroup workload changed during the measurement window")

    target_usage = _delta(target_before, target_after, "usage_usec", scope.target)
    neighbor_usage = _delta(neighbor_before, neighbor_after, "usage_usec", scope.neighbor)
    total_usage = target_usage + neighbor_usage
    if total_usage <= 0:
        raise CgroupCpuError("cgroup workload consumed no CPU during the measurement window")
    elapsed_seconds = (monotonic_ended - monotonic_started) / 1_000_000_000.0
    if elapsed_seconds <= 0:
        raise CgroupCpuError("measurement window did not advance the monotonic clock")

    target_rate = target_usage / 1_000_000.0 / elapsed_seconds
    neighbor_rate = neighbor_usage / 1_000_000.0 / elapsed_seconds
    target_share = target_usage / total_usage * 100.0
    metrics = {
        "target_cpu_share_pct": target_share,
        "neighbor_cpu_share_pct": 100.0 - target_share,
        "target_cpu_rate": target_rate,
        "neighbor_cpu_rate": neighbor_rate,
        "aggregate_cpu_rate": target_rate + neighbor_rate,
        "target_cpu_weight": float(read_weight(scope.target)),
        "neighbor_cpu_weight": float(read_weight(scope.neighbor)),
    }
    return {
        "started_at_ns": started_at_ns,
        "ended_at_ns": ended_at_ns,
        "workload_fingerprint": fingerprint_before,
        "metrics": metrics,
        "details": {
            "window_ms": window_ms,
            "target_usage_usec": target_usage,
            "neighbor_usage_usec": neighbor_usage,
            "target_members": list(read_members(scope.target)),
            "neighbor_members": list(read_members(scope.neighbor)),
        },
    }


def workload_fingerprint(scope: CgroupCpuScope) -> str:
    target_members = read_members(scope.target)
    neighbor_members = read_members(scope.neighbor)
    if not target_members or not neighbor_members:
        raise CgroupCpuError("both target and neighbor cgroups require live workload processes")
    payload = {
        "boot_id": _optional_text(Path("/proc/sys/kernel/random/boot_id")),
        "scope": str(scope.root),
        "target_inode": scope.target.stat().st_ino,
        "neighbor_inode": scope.neighbor.stat().st_ino,
        "target_processes": [_process_signature(pid) for pid in target_members],
        "neighbor_processes": [_process_signature(pid) for pid in neighbor_members],
    }
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return "sha256:" + hashlib.sha256(encoded).hexdigest()


def wait_for_empty(scope: CgroupCpuScope, timeout: float = 2.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not read_members(scope.target) and not read_members(scope.neighbor):
            return
        time.sleep(0.05)
    raise CgroupCpuError(
        "cgroup workload processes did not exit: "
        f"target={read_members(scope.target)}, neighbor={read_members(scope.neighbor)}"
    )


def _enable_controller(path: Path, controller: str) -> None:
    available = set(_read_text(path / "cgroup.controllers").split())
    if controller not in available:
        raise CgroupCpuError(f"controller {controller!r} is unavailable below {path}")
    resource = path / "cgroup.subtree_control"
    enabled = set(_read_text(resource).split())
    if controller in enabled:
        return
    try:
        resource.write_text(f"+{controller}\n", encoding="utf-8")
    except OSError as exc:
        raise CgroupCpuError(f"failed to enable {controller} below {path}: {exc}") from exc
    enabled = set(_read_text(resource).split())
    if controller not in enabled:
        raise CgroupCpuError(f"controller {controller!r} was not enabled below {path}")


def _group_state(path: Path) -> dict[str, Any]:
    return {
        "path": str(path),
        "inode": path.stat().st_ino,
        "weight": read_weight(path),
        "members": list(read_members(path)),
        "cpu_stat": read_cpu_stat(path),
    }


def _process_signature(pid: int) -> dict[str, Any]:
    proc = Path("/proc") / str(pid)
    try:
        stat_text = (proc / "stat").read_text(encoding="utf-8")
        _, tail = stat_text.rsplit(")", 1)
        # The tail starts at field 3 (state), so starttime (field 22) is index 19.
        start_time = int(tail.split()[19])
        executable = os.readlink(proc / "exe")
        command = (proc / "cmdline").read_bytes().split(b"\0")
    except (OSError, ValueError, IndexError) as exc:
        raise CgroupCpuError(f"failed to fingerprint workload process {pid}: {exc}") from exc
    return {
        "pid": pid,
        "start_time_ticks": start_time,
        "executable": executable,
        "argv": [item.decode("utf-8", errors="replace") for item in command if item],
    }


def _delta(before: dict[str, int], after: dict[str, int], key: str, path: Path) -> int:
    value = after.get(key, 0) - before.get(key, 0)
    if value < 0:
        raise CgroupCpuError(f"{key} moved backwards in {path}")
    return value


def _validate_weight(value: int) -> None:
    if isinstance(value, bool) or not isinstance(value, int):
        raise CgroupCpuError("cpu.weight must be an integer")
    if not MIN_CPU_WEIGHT <= value <= MAX_CPU_WEIGHT:
        raise CgroupCpuError(
            f"cpu.weight must be between {MIN_CPU_WEIGHT} and {MAX_CPU_WEIGHT}"
        )


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise CgroupCpuError(f"failed to read {path}: {exc}") from exc


def _optional_text(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8").strip() or None
    except OSError:
        return None


def _exec_in_cgroup(argv: list[str]) -> int:
    if len(argv) < 3 or argv[1] != "--":
        raise CgroupCpuError("usage: common.py _exec CGROUP -- COMMAND [ARG ...]")
    cgroup = Path(argv[0])
    command = argv[2:]
    if not command:
        raise CgroupCpuError("cgroup exec requires a command")
    try:
        (cgroup / "cgroup.procs").write_text(f"{os.getpid()}\n", encoding="utf-8")
    except OSError as exc:
        raise CgroupCpuError(f"failed to enter cgroup {cgroup}: {exc}") from exc
    os.execvpe(command[0], command, os.environ.copy())
    return 127


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if args[:1] != ["_exec"]:
        raise CgroupCpuError("common.py is an internal exec helper")
    return _exec_in_cgroup(args[1:])


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except CgroupCpuError as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(125) from exc
