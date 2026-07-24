#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


OUTCOME_VERSION = 2
COMMITTED = "committed"
NO_COMMIT = "no_commit"
RECOVERY_REQUIRED = "recovery_required"


class AdapterError(RuntimeError):
    pass


@dataclass(frozen=True)
class ManagedProcess:
    name: str
    process: subprocess.Popen[bytes]


@dataclass(frozen=True)
class CgroupLease:
    path: Path
    scope: Path
    preserve: bool


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    outcome_path = _required_path("SCX_BENCH_TREATMENT_OUTCOME")
    output_dir = Path(os.environ.get("SCX_BENCH_OUT", outcome_path.parent))
    output_dir.mkdir(parents=True, exist_ok=True)

    processes: list[ManagedProcess] = []
    cgroup: CgroupLease | None = None
    baseline_state: dict[str, Any] | None = None
    activation_status: str | None = None
    failure: AdapterError | None = None
    details: dict[str, Any] = {}
    try:
        cgroup = _create_cgroup()
        details["cgroup_path"] = str(cgroup.path)
        details["cgroup_scope"] = str(cgroup.scope)
        details["cgroup_lifecycle"] = "vm" if cgroup.preserve else "treatment"
        config_path = _ensure_config(output_dir, cgroup.path)
        details["config_path"] = str(config_path)
        _start_support_processes(processes, output_dir)
        _start_daemon(processes, output_dir)
        training_ready = _start_training_workload(processes, cgroup.path, output_dir)
        if training_ready is not None:
            details["training_ready"] = training_ready
        baseline_state = _snapshot_cgroup(cgroup.scope)
        details["baseline_state"] = baseline_state
        response = _activate(cgroup.path, output_dir)
        details["activation_response"] = response
        activation_status = _activation_status(response)
    except AdapterError as exc:
        failure = exc
    finally:
        _stop_processes(processes)
        _remove_generated_config()

    if failure is not None:
        if cgroup is not None and not cgroup.preserve:
            _remove_cgroup(cgroup.path)
        return _fail(output_dir, details, failure)

    assert cgroup is not None
    assert baseline_state is not None
    assert activation_status is not None
    verification = _verify_handoff(cgroup, baseline_state, activation_status)
    details["state_verification"] = verification
    disposition, reason = _map_outcome(
        activation_status,
        verification,
        args.no_commit_disposition,
    )
    if not cgroup.preserve:
        _remove_cgroup(cgroup.path)
    _write_outcome(outcome_path, disposition, reason, details)
    return 0


def _parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Map a tuning-agent episode to a generic Bench treatment outcome"
    )
    parser.add_argument(
        "--no-commit-disposition",
        choices=("proceed", "stop"),
        default="stop",
        help="generic disposition after a verified no-commit episode",
    )
    return parser.parse_args(argv)


def _fail(
    output_dir: Path,
    details: dict[str, Any],
    error: AdapterError,
) -> int:
    details["error"] = str(error)
    _write_json(output_dir / "adapter_error.json", details)
    print(str(error), file=sys.stderr)
    return 1


def _create_cgroup() -> CgroupLease:
    root = Path(os.environ.get("SCX_TUNING_AGENT_CGROUP_ROOT", "/sys/fs/cgroup")).resolve()
    if not root.is_dir():
        raise AdapterError(f"cgroup root does not exist: {root}")
    preserve = _bool_env("SCX_TUNING_AGENT_PRESERVE_CGROUP", False)

    configured = os.environ.get("SCX_TUNING_AGENT_CGROUP_PATH")
    if configured:
        requested = Path(configured)
        if not requested.is_absolute():
            raise AdapterError("SCX_TUNING_AGENT_CGROUP_PATH must be absolute")
        path = requested.resolve(strict=False)
        if path == root or root not in path.parents:
            raise AdapterError(f"treatment cgroup must be a child of {root}: {path}")
        if path.exists():
            raise AdapterError(f"treatment cgroup already exists: {path}")
        try:
            path.parent.mkdir(mode=0o755, parents=True, exist_ok=True)
        except OSError as exc:
            raise AdapterError(f"failed to create cgroup parent {path.parent}: {exc}") from exc
    else:
        path = root / f"scx-bench-treatment-{os.getpid()}"

    try:
        path.mkdir(mode=0o755)
    except OSError as exc:
        raise AdapterError(f"failed to create treatment cgroup {path}: {exc}") from exc
    procs = path / "cgroup.procs"
    try:
        procs.touch(exist_ok=True)
    except OSError:
        pass
    return CgroupLease(
        path=path,
        scope=path.parent if configured else path,
        preserve=preserve,
    )


def _ensure_config(output_dir: Path, cgroup_path: Path) -> Path:
    configured = os.environ.get("SCX_TUNING_AGENT_CONFIG")
    if configured:
        return Path(configured)

    binary = os.environ.get("SCX_TUNING_AGENT_BIN", "tuning-agent")
    mcp_command = os.environ.get(
        "SCX_TUNING_AGENT_MCP_COMMAND",
        "/tmp/scx-bench-treatment.d/deterministic_mcp.py",
    )
    mcp_server_id = os.environ.get("SCX_TUNING_AGENT_MCP_SERVER_ID", "deterministic-test")
    base_url = os.environ.get("SCX_TUNING_AGENT_LLM_BASE_URL", "http://127.0.0.1:18080")
    api_key = os.environ.get("SCX_TUNING_AGENT_LLM_API_KEY", "test")
    model = os.environ.get("SCX_TUNING_AGENT_LLM_MODEL", "mock")
    llm_timeout_ms = _positive_int_env("SCX_TUNING_AGENT_LLM_TIMEOUT_MS", 30_000)
    llm_retry_count = _non_negative_int_env("SCX_TUNING_AGENT_LLM_RETRY_COUNT", 0)
    max_reasoning_rounds = _positive_int_env(
        "SCX_TUNING_AGENT_MAX_REASONING_ROUNDS", 6
    )
    evaluation_timeout_ms = _positive_int_env(
        "SCX_TUNING_AGENT_EVALUATION_TIMEOUT_MS", 60_000
    )
    mcp_request_timeout_ms = _positive_int_env(
        "SCX_TUNING_AGENT_MCP_REQUEST_TIMEOUT_MS", 30_000
    )
    redacted_config_path = output_dir / "tuning-agent.toml"
    socket_path = output_dir / "tuning-agent.sock"
    audit_path = output_dir / "audit.jsonl"
    wal_dir = output_dir / "transactions"
    mcp_env = _mcp_environment(output_dir, cgroup_path)
    mcp_command_json = json.dumps(mcp_command)
    mcp_server_id_json = json.dumps(mcp_server_id)
    base_url_json = json.dumps(base_url)
    api_key_json = json.dumps(api_key)
    model_json = json.dumps(model)
    socket_path_json = json.dumps(str(socket_path))
    audit_path_json = json.dumps(str(audit_path))
    wal_dir_json = json.dumps(str(wal_dir))
    mcp_env_toml = "\n".join(
        f"{json.dumps(key)} = {json.dumps(value)}"
        for key, value in sorted(mcp_env.items())
    )
    config_text = f"""
[llm]
base_url = {base_url_json}
api_key = {api_key_json}
model = {model_json}
timeout_ms = {llm_timeout_ms}
retry_count = {llm_retry_count}

[reasoning]
max_rounds = {max_reasoning_rounds}

[safety]
evaluation_timeout_ms = {evaluation_timeout_ms}
cooldown_ms = 0

[activation]
socket_path = {socket_path_json}

[audit]
path = {audit_path_json}

[transaction]
wal_dir = {wal_dir_json}

[mcp]
enabled = true

[[mcp.servers]]
id = {mcp_server_id_json}
enabled = true
command = {mcp_command_json}
request_timeout_ms = {mcp_request_timeout_ms}
allow_mutations = true

[mcp.servers.env]
{mcp_env_toml}
""".lstrip()
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        prefix="scx-tuning-agent-adapter-",
        suffix=".toml",
        dir="/tmp",
        delete=False,
    ) as config_file:
        config_file.write(config_text)
        config_path = Path(config_file.name)
    config_path.chmod(0o600)
    redacted_config_path.write_text(
        config_text.replace(
            f"api_key = {api_key_json}",
            'api_key = "<redacted>"',
            1,
        ),
        encoding="utf-8",
    )
    os.environ["SCX_TUNING_AGENT_CONFIG"] = str(config_path)
    os.environ["SCX_TUNING_AGENT_GENERATED_CONFIG"] = str(config_path)
    os.environ.setdefault("SCX_TUNING_AGENT_BIN", binary)
    os.environ["SCX_TUNING_AGENT_SOCKET_PATH"] = str(socket_path)
    return redacted_config_path


def _mcp_environment(output_dir: Path, cgroup_path: Path) -> dict[str, str]:
    configured = _json_env("SCX_TUNING_AGENT_MCP_ENV", None)
    if configured is None:
        return {
            "SCX_DETERMINISTIC_SCENARIO": os.environ.get(
                "SCX_DETERMINISTIC_SCENARIO", "positive"
            ),
            "SCX_DETERMINISTIC_STATE_PATH": str(output_dir / "deterministic-state.json"),
        }
    if not isinstance(configured, dict) or any(
        not isinstance(key, str) or not isinstance(value, str)
        for key, value in configured.items()
    ):
        raise AdapterError("SCX_TUNING_AGENT_MCP_ENV must be a JSON string:string object")
    replacements = {
        "{output_dir}": str(output_dir),
        "{cgroup_path}": str(cgroup_path),
        "{cgroup_parent}": str(cgroup_path.parent),
    }
    return {
        key: _replace_placeholders(value, replacements)
        for key, value in configured.items()
    }


def _replace_placeholders(value: str, replacements: dict[str, str]) -> str:
    expanded = value
    for placeholder, replacement in replacements.items():
        expanded = expanded.replace(placeholder, replacement)
    return expanded


def _start_support_processes(processes: list[ManagedProcess], output_dir: Path) -> None:
    specs = _json_env("SCX_TUNING_AGENT_SUPPORT_PROCESSES", [])
    if not isinstance(specs, list):
        raise AdapterError("SCX_TUNING_AGENT_SUPPORT_PROCESSES must be a JSON array")
    for index, spec in enumerate(specs):
        if not isinstance(spec, dict):
            raise AdapterError("support process entries must be objects")
        name = str(spec.get("name") or f"support-{index}")
        argv = _argv(spec.get("argv"), f"support process {name}")
        env = _child_env(spec.get("env", {}))
        cwd = spec.get("cwd")
        process = _spawn(name, argv, output_dir=output_dir, env=env, cwd=cwd)
        processes.append(ManagedProcess(name, process))
    settle = _non_negative_float_env("SCX_TUNING_AGENT_SUPPORT_SETTLE_SECONDS", 0.0)
    if settle:
        time.sleep(settle)
    _require_running(processes)


def _start_daemon(processes: list[ManagedProcess], output_dir: Path) -> None:
    if os.environ.get("SCX_TUNING_AGENT_START_DAEMON", "1") in {"0", "false", "False"}:
        return
    daemon_argv = _json_env("SCX_TUNING_AGENT_DAEMON_ARGV", None)
    if daemon_argv is None:
        binary = os.environ.get("SCX_TUNING_AGENT_BIN", "tuning-agent")
        config = _required("SCX_TUNING_AGENT_CONFIG")
        daemon_argv = [binary, "--config", config, "daemon"]
    argv = _argv(daemon_argv, "daemon")
    process = _spawn("tuning-agent-daemon", argv, output_dir=output_dir)
    processes.append(ManagedProcess("tuning-agent-daemon", process))
    settle = _non_negative_float_env("SCX_TUNING_AGENT_DAEMON_SETTLE_SECONDS", 1.0)
    socket_path = os.environ.get("SCX_TUNING_AGENT_SOCKET_PATH")
    if socket_path:
        _wait_for_path(Path(socket_path), timeout=max(settle, 10.0))
    elif settle:
        time.sleep(settle)
    _require_running(processes)


def _start_training_workload(
    processes: list[ManagedProcess],
    cgroup_path: Path,
    output_dir: Path,
) -> dict[str, Any] | None:
    workload_argv = _json_env("SCX_TUNING_AGENT_TRAINING_ARGV", None)
    if workload_argv is None:
        return None
    argv = _argv(workload_argv, "training workload")
    process = _spawn("training-workload", argv, output_dir=output_dir)
    try:
        (cgroup_path / "cgroup.procs").write_text(f"{process.pid}\n", encoding="utf-8")
    except OSError as exc:
        _terminate_process_group(process.pid)
        process.wait()
        raise AdapterError(f"failed to move training workload into {cgroup_path}: {exc}") from exc
    processes.append(ManagedProcess("training-workload", process))
    ready_path = os.environ.get("SCX_TUNING_AGENT_TRAINING_READY_PATH")
    if ready_path:
        path = Path(ready_path)
        if not path.is_absolute():
            path = output_dir / path
        timeout = float(
            _positive_int_env("SCX_TUNING_AGENT_TRAINING_READY_TIMEOUT_SECONDS", 60)
        )
        ready = _wait_for_training_ready(path, processes[-1], timeout)
        return {"path": str(path), **ready}

    time.sleep(_non_negative_float_env("SCX_TUNING_AGENT_TRAINING_SETTLE_SECONDS", 0.0))
    _require_running([processes[-1]])
    return None


def _wait_for_training_ready(
    path: Path,
    training: ManagedProcess,
    timeout: float,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        _require_running([training])
        try:
            stat = path.lstat()
        except FileNotFoundError:
            time.sleep(0.05)
            continue
        except OSError as exc:
            raise AdapterError(f"failed to inspect training readiness file {path}: {exc}") from exc
        if not path.is_file() or path.is_symlink():
            raise AdapterError(f"training readiness path must be a regular file: {path}")
        if stat.st_size > 65_536:
            raise AdapterError(f"training readiness file exceeds 64 KiB: {path}")
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise AdapterError(f"training readiness file is invalid: {path}: {exc}") from exc
        ready = _validate_training_ready(value)
        if ready["ready"]:
            return ready
        time.sleep(0.05)
    raise AdapterError(f"timed out waiting for training readiness at {path}")


def _validate_training_ready(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != {
        "version",
        "ready",
        "workload_digest",
        "processes",
    }:
        raise AdapterError("training readiness must be a strict V1 object")
    if value.get("version") != 1:
        raise AdapterError("training readiness version must be 1")
    if not isinstance(value.get("ready"), bool):
        raise AdapterError("training readiness ready must be a boolean")
    digest = value.get("workload_digest")
    if not isinstance(digest, str) or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None:
        raise AdapterError("training readiness workload_digest must be a sha256 digest")
    processes = value.get("processes")
    if not isinstance(processes, list) or not processes:
        raise AdapterError("training readiness processes must be a non-empty array")

    seen: set[int] = set()
    normalized = []
    for item in processes:
        if not isinstance(item, dict) or set(item) != {
            "pid",
            "start_time_ticks",
            "executable",
        }:
            raise AdapterError("training readiness process identity has an invalid schema")
        pid = item.get("pid")
        start_time = item.get("start_time_ticks")
        executable = item.get("executable")
        if isinstance(pid, bool) or not isinstance(pid, int) or pid < 1 or pid in seen:
            raise AdapterError("training readiness process PID is invalid or duplicated")
        if isinstance(start_time, bool) or not isinstance(start_time, int) or start_time < 1:
            raise AdapterError("training readiness process start_time_ticks is invalid")
        if not isinstance(executable, str) or not executable or not Path(executable).is_absolute():
            raise AdapterError("training readiness process executable must be an absolute path")
        observed = _process_identity(pid)
        if observed["start_time_ticks"] != start_time or observed["executable"] != executable:
            raise AdapterError(f"training readiness process identity changed for PID {pid}")
        seen.add(pid)
        normalized.append(item)

    return {
        "version": 1,
        "ready": value["ready"],
        "workload_digest": digest,
        "processes": normalized,
    }


def _process_identity(pid: int) -> dict[str, Any]:
    proc = Path("/proc") / str(pid)
    try:
        stat_text = (proc / "stat").read_text(encoding="utf-8")
        _, tail = stat_text.rsplit(")", 1)
        start_time = int(tail.split()[19])
        executable = os.readlink(proc / "exe")
    except (OSError, ValueError, IndexError) as exc:
        raise AdapterError(f"training readiness process {pid} is not alive: {exc}") from exc
    return {
        "pid": pid,
        "start_time_ticks": start_time,
        "executable": executable,
    }


def _activate(cgroup_path: Path, output_dir: Path) -> dict[str, Any]:
    activate_argv = _json_env("SCX_TUNING_AGENT_ACTIVATE_ARGV", None)
    if activate_argv is None:
        binary = os.environ.get("SCX_TUNING_AGENT_BIN", "tuning-agent")
        config = _required("SCX_TUNING_AGENT_CONFIG")
        timeout = str(_positive_int_env("SCX_TUNING_AGENT_ACTIVATE_TIMEOUT_SECONDS", 600))
        event_type = os.environ.get("SCX_TUNING_AGENT_EVENT_TYPE", "bench_treatment")
        severity = os.environ.get("SCX_TUNING_AGENT_SEVERITY", "info")
        source = os.environ.get("SCX_TUNING_AGENT_SOURCE", "scx-bench")
        activate_argv = [
            binary,
            "--config",
            config,
            "activate",
            "--wait",
            "--json",
            "--timeout-seconds",
            timeout,
            event_type,
            severity,
            source,
            str(cgroup_path),
        ]
    argv = _argv(activate_argv, "activate")
    timeout = _positive_int_env("SCX_TUNING_AGENT_ACTIVATE_TIMEOUT_SECONDS", 600)
    completed = subprocess.run(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout + 5,
        check=False,
    )
    (output_dir / "activate_stdout.json").write_bytes(completed.stdout)
    (output_dir / "activate_stderr.log").write_bytes(completed.stderr)
    if completed.returncode != 0:
        raise AdapterError(f"activate failed with returncode {completed.returncode}")
    try:
        response = json.loads(completed.stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise AdapterError(f"activate did not produce valid JSON: {exc}") from exc
    if not isinstance(response, dict):
        raise AdapterError("activate response must be a JSON object")
    return response


def _activation_status(response: dict[str, Any]) -> str:
    if response.get("version") != 1 or response.get("accepted") is not True:
        raise AdapterError("activate response is not an accepted V1 result")
    status = response.get("status")
    if status in {COMMITTED, NO_COMMIT, RECOVERY_REQUIRED}:
        return status
    raise AdapterError(f"activation ended with unsupported status: {status!r}")


def _snapshot_cgroup(scope: Path) -> dict[str, Any]:
    if not scope.is_dir():
        raise AdapterError(f"cgroup scope does not exist: {scope}")
    groups = {}
    for procs_path in sorted(scope.rglob("cgroup.procs")):
        group = procs_path.parent
        relative = str(group.relative_to(scope)) or "."
        members = _read_cgroup_members(procs_path)
        weight_path = group / "cpu.weight"
        weight = _read_resource(weight_path) if weight_path.is_file() else None
        groups[relative] = {
            "path": str(group),
            "members": members,
            "cpu_weight": weight,
        }
    if not groups:
        raise AdapterError(f"cgroup scope contains no cgroup.procs resources: {scope}")
    return {"scope": str(scope), "groups": groups}


def _verify_handoff(
    cgroup: CgroupLease,
    baseline: dict[str, Any],
    activation_status: str,
) -> dict[str, Any]:
    issues: list[str] = []
    try:
        observed = _snapshot_cgroup(cgroup.scope)
    except AdapterError as exc:
        return {"verified": False, "issues": [str(exc)], "observed": None}

    baseline_groups = baseline["groups"]
    observed_groups = observed["groups"]
    if set(observed_groups) != set(baseline_groups):
        issues.append("cgroup set changed between activation and handoff")
    for name, state in observed_groups.items():
        if state["members"]:
            issues.append(f"cgroup {name} still contains processes")

    if activation_status == NO_COMMIT:
        for name, baseline_state in baseline_groups.items():
            observed_state = observed_groups.get(name)
            if observed_state is None:
                continue
            if observed_state["cpu_weight"] != baseline_state["cpu_weight"]:
                issues.append(f"cgroup {name} cpu.weight was not restored")
    elif activation_status == RECOVERY_REQUIRED:
        issues.append("tuning agent reported recovery_required")

    return {
        "verified": not issues,
        "issues": issues,
        "observed": observed,
    }


def _map_outcome(
    activation_status: str,
    verification: dict[str, Any],
    no_commit_disposition: str,
) -> tuple[str, dict[str, str]]:
    verified = verification.get("verified") is True
    if activation_status == RECOVERY_REQUIRED:
        return "unsafe", {
            "code": "tuning_agent.recovery_required",
            "message": "tuning agent could not establish a recoverable system state",
        }
    if not verified:
        return "unsafe", {
            "code": f"tuning_agent.{activation_status}_unverified",
            "message": "tuning-agent outcome could not be verified after cleanup",
        }
    if activation_status == COMMITTED:
        return "proceed", {
            "code": "tuning_agent.committed",
            "message": "candidate committed and state verified",
        }
    if no_commit_disposition == "proceed":
        return "proceed", {
            "code": "tuning_agent.no_commit_baseline",
            "message": "no candidate committed; verified baseline retained",
        }
    return "stop", {
        "code": "tuning_agent.no_commit",
        "message": "no candidate committed; strict policy stopped the run",
    }


def _read_cgroup_members(path: Path) -> list[int]:
    values = []
    for line in _read_resource(path).splitlines():
        try:
            pid = int(line)
        except ValueError as exc:
            raise AdapterError(f"invalid PID in {path}: {line!r}") from exc
        if pid > 0:
            values.append(pid)
    return sorted(set(values))


def _read_resource(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError as exc:
        raise AdapterError(f"failed to read {path}: {exc}") from exc


def _spawn(
    name: str,
    argv: tuple[str, ...],
    *,
    output_dir: Path,
    env: dict[str, str] | None = None,
    cwd: str | None = None,
) -> subprocess.Popen[bytes]:
    stdout_path = output_dir / f"{name}.stdout.log"
    stderr_path = output_dir / f"{name}.stderr.log"
    try:
        stdout = stdout_path.open("ab")
        stderr = stderr_path.open("ab")
        return subprocess.Popen(
            argv,
            cwd=cwd,
            env=env or os.environ.copy(),
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
    except OSError as exc:
        raise AdapterError(f"failed to start {name}: {exc}") from exc


def _require_running(processes: list[ManagedProcess]) -> None:
    for item in processes:
        returncode = item.process.poll()
        if returncode is not None:
            raise AdapterError(f"{item.name} exited early with returncode {returncode}")


def _stop_processes(processes: list[ManagedProcess]) -> None:
    for item in reversed(processes):
        _terminate_process_group(item.process.pid)
        if item.process.poll() is not None:
            continue
        try:
            item.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            _signal_process_group(item.process.pid, signal.SIGKILL)
            item.process.wait()


def _terminate_process_group(pgid: int) -> None:
    _signal_process_group(pgid, signal.SIGTERM)
    deadline = time.monotonic() + 1.0
    while time.monotonic() < deadline:
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.05)
    _signal_process_group(pgid, signal.SIGKILL)


def _signal_process_group(pgid: int, value: signal.Signals) -> None:
    try:
        os.killpg(pgid, value)
    except ProcessLookupError:
        pass


def _remove_cgroup(path: Path) -> None:
    try:
        path.rmdir()
    except OSError:
        pass


def _remove_generated_config() -> None:
    configured = os.environ.pop("SCX_TUNING_AGENT_GENERATED_CONFIG", None)
    if not configured:
        return
    try:
        Path(configured).unlink(missing_ok=True)
    except OSError:
        pass


def _wait_for_path(path: Path, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        time.sleep(0.05)
    raise AdapterError(f"timed out waiting for {path}")


def _write_outcome(
    path: Path,
    disposition: str,
    reason: dict[str, str],
    details: dict[str, Any],
) -> None:
    _write_json(
        path,
        {
            "version": OUTCOME_VERSION,
            "disposition": disposition,
            "reason": reason,
            "details": details,
        },
    )


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def _json_env(name: str, default: Any) -> Any:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:
        raise AdapterError(f"{name} is not valid JSON: {exc}") from exc


def _argv(value: Any, label: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not value:
        raise AdapterError(f"{label} argv must be a non-empty JSON string array")
    if any(not isinstance(item, str) or not item for item in value):
        raise AdapterError(f"{label} argv entries must be non-empty strings")
    return tuple(value)


def _child_env(value: Any) -> dict[str, str]:
    if not isinstance(value, dict):
        raise AdapterError("support process env must be an object")
    if any(not isinstance(key, str) or not isinstance(item, str) for key, item in value.items()):
        raise AdapterError("support process env entries must be string:string")
    env = os.environ.copy()
    env.update(value)
    return env


def _required(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise AdapterError(f"{name} is required")
    return value


def _required_path(name: str) -> Path:
    return Path(_required(name))


def _positive_int_env(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = int(raw)
    except ValueError as exc:
        raise AdapterError(f"{name} must be an integer") from exc
    if value < 1:
        raise AdapterError(f"{name} must be positive")
    return value


def _non_negative_int_env(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = int(raw)
    except ValueError as exc:
        raise AdapterError(f"{name} must be an integer") from exc
    if value < 0:
        raise AdapterError(f"{name} must be non-negative")
    return value


def _non_negative_float_env(name: str, default: float) -> float:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = float(raw)
    except ValueError as exc:
        raise AdapterError(f"{name} must be a number") from exc
    if value < 0:
        raise AdapterError(f"{name} must be non-negative")
    return value


def _bool_env(name: str, default: bool) -> bool:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    if raw in {"1", "true", "True"}:
        return True
    if raw in {"0", "false", "False"}:
        return False
    raise AdapterError(f"{name} must be a boolean")


if __name__ == "__main__":
    raise SystemExit(main())
