#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


OUTCOME_VERSION = 1
READY = "ready"
NO_COMMIT = "no_commit"
RECOVERY_REQUIRED = "recovery_required"


class HarnessError(RuntimeError):
    pass


@dataclass(frozen=True)
class ManagedProcess:
    name: str
    process: subprocess.Popen[bytes]


def main() -> int:
    outcome_path = _required_path("SCX_BENCH_TREATMENT_OUTCOME")
    output_dir = Path(os.environ.get("SCX_BENCH_OUT", outcome_path.parent))
    output_dir.mkdir(parents=True, exist_ok=True)

    processes: list[ManagedProcess] = []
    cgroup_path: Path | None = None
    details: dict[str, Any] = {}
    try:
        cgroup_path = _create_cgroup()
        details["cgroup_path"] = str(cgroup_path)
        config_path = _ensure_config(output_dir)
        details["config_path"] = str(config_path)
        _start_support_processes(processes, output_dir)
        _start_daemon(processes, output_dir)
        _start_training_workload(processes, cgroup_path, output_dir)
        response = _activate(cgroup_path, output_dir)
        details["activation_response"] = response
        status = _outcome_status(response)
        _write_outcome(outcome_path, status, details)
        return 0
    except HarnessError as exc:
        details["error"] = str(exc)
        _write_json(output_dir / "harness_error.json", details)
        print(str(exc), file=sys.stderr)
        return 1
    finally:
        _stop_processes(processes)
        if cgroup_path is not None:
            _remove_cgroup(cgroup_path)


def _create_cgroup() -> Path:
    root = Path(os.environ.get("SCX_TUNING_AGENT_CGROUP_ROOT", "/sys/fs/cgroup"))
    if not root.is_dir():
        raise HarnessError(f"cgroup root does not exist: {root}")
    path = root / f"scx-bench-treatment-{os.getpid()}"
    try:
        path.mkdir(mode=0o755)
    except OSError as exc:
        raise HarnessError(f"failed to create treatment cgroup {path}: {exc}") from exc
    procs = path / "cgroup.procs"
    try:
        procs.touch(exist_ok=True)
    except OSError:
        pass
    return path


def _ensure_config(output_dir: Path) -> Path:
    configured = os.environ.get("SCX_TUNING_AGENT_CONFIG")
    if configured:
        return Path(configured)

    binary = os.environ.get("SCX_TUNING_AGENT_BIN", "tuning-agent")
    mcp_command = os.environ.get(
        "SCX_TUNING_AGENT_MCP_COMMAND",
        "/tmp/scx-bench-treatment.d/deterministic_tuning_mcp.py",
    )
    base_url = os.environ.get("SCX_TUNING_AGENT_LLM_BASE_URL", "http://127.0.0.1:18080")
    config_path = output_dir / "tuning-agent.toml"
    socket_path = output_dir / "tuning-agent.sock"
    audit_path = output_dir / "audit.jsonl"
    wal_dir = output_dir / "transactions"
    mcp_command_json = json.dumps(mcp_command)
    base_url_json = json.dumps(base_url)
    socket_path_json = json.dumps(str(socket_path))
    audit_path_json = json.dumps(str(audit_path))
    wal_dir_json = json.dumps(str(wal_dir))
    scenario_json = json.dumps(os.environ.get("SCX_DETERMINISTIC_SCENARIO", "positive"))
    state_json = json.dumps(str(output_dir / "deterministic-state.json"))
    config_path.write_text(
        f"""
[llm]
base_url = {base_url_json}
api_key = "test"
model = "mock"
timeout_ms = 30000
retry_count = 0

[reasoning]
max_rounds = 4

[safety]
evaluation_timeout_ms = 60000
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
id = "deterministic-test"
enabled = true
command = {mcp_command_json}
request_timeout_ms = 30000
allow_mutations = true

[mcp.servers.env]
SCX_DETERMINISTIC_SCENARIO = {scenario_json}
SCX_DETERMINISTIC_STATE_PATH = {state_json}
""".lstrip(),
        encoding="utf-8",
    )
    os.environ["SCX_TUNING_AGENT_CONFIG"] = str(config_path)
    os.environ.setdefault("SCX_TUNING_AGENT_BIN", binary)
    os.environ["SCX_TUNING_AGENT_SOCKET_PATH"] = str(socket_path)
    return config_path


def _start_support_processes(processes: list[ManagedProcess], output_dir: Path) -> None:
    specs = _json_env("SCX_TUNING_AGENT_SUPPORT_PROCESSES", [])
    if not isinstance(specs, list):
        raise HarnessError("SCX_TUNING_AGENT_SUPPORT_PROCESSES must be a JSON array")
    for index, spec in enumerate(specs):
        if not isinstance(spec, dict):
            raise HarnessError("support process entries must be objects")
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
) -> None:
    workload_argv = _json_env("SCX_TUNING_AGENT_TRAINING_ARGV", None)
    if workload_argv is None:
        return
    argv = _argv(workload_argv, "training workload")
    process = _spawn("training-workload", argv, output_dir=output_dir)
    try:
        (cgroup_path / "cgroup.procs").write_text(f"{process.pid}\n", encoding="utf-8")
    except OSError as exc:
        _terminate_process_group(process.pid)
        process.wait()
        raise HarnessError(f"failed to move training workload into {cgroup_path}: {exc}") from exc
    processes.append(ManagedProcess("training-workload", process))
    time.sleep(_non_negative_float_env("SCX_TUNING_AGENT_TRAINING_SETTLE_SECONDS", 0.0))
    _require_running([processes[-1]])


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
        raise HarnessError(f"activate failed with returncode {completed.returncode}")
    try:
        response = json.loads(completed.stdout.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise HarnessError(f"activate did not produce valid JSON: {exc}") from exc
    if not isinstance(response, dict):
        raise HarnessError("activate response must be a JSON object")
    return response


def _outcome_status(response: dict[str, Any]) -> str:
    status = response.get("status")
    if status == "committed":
        return READY
    if status == "no_commit":
        return NO_COMMIT
    if status == "recovery_required":
        return RECOVERY_REQUIRED
    raise HarnessError(f"activation ended with unsupported status: {status!r}")


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
        raise HarnessError(f"failed to start {name}: {exc}") from exc


def _require_running(processes: list[ManagedProcess]) -> None:
    for item in processes:
        returncode = item.process.poll()
        if returncode is not None:
            raise HarnessError(f"{item.name} exited early with returncode {returncode}")


def _stop_processes(processes: list[ManagedProcess]) -> None:
    for item in reversed(processes):
        if item.process.poll() is not None:
            continue
        _terminate_process_group(item.process.pid)
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


def _wait_for_path(path: Path, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return
        time.sleep(0.05)
    raise HarnessError(f"timed out waiting for {path}")


def _write_outcome(path: Path, status: str, details: dict[str, Any]) -> None:
    _write_json(
        path,
        {
            "version": OUTCOME_VERSION,
            "status": status,
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
        raise HarnessError(f"{name} is not valid JSON: {exc}") from exc


def _argv(value: Any, label: str) -> tuple[str, ...]:
    if not isinstance(value, list) or not value:
        raise HarnessError(f"{label} argv must be a non-empty JSON string array")
    if any(not isinstance(item, str) or not item for item in value):
        raise HarnessError(f"{label} argv entries must be non-empty strings")
    return tuple(value)


def _child_env(value: Any) -> dict[str, str]:
    if not isinstance(value, dict):
        raise HarnessError("support process env must be an object")
    if any(not isinstance(key, str) or not isinstance(item, str) for key, item in value.items()):
        raise HarnessError("support process env entries must be string:string")
    env = os.environ.copy()
    env.update(value)
    return env


def _required(name: str) -> str:
    value = os.environ.get(name)
    if not value:
        raise HarnessError(f"{name} is required")
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
        raise HarnessError(f"{name} must be an integer") from exc
    if value < 1:
        raise HarnessError(f"{name} must be positive")
    return value


def _non_negative_float_env(name: str, default: float) -> float:
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return default
    try:
        value = float(raw)
    except ValueError as exc:
        raise HarnessError(f"{name} must be a number") from exc
    if value < 0:
        raise HarnessError(f"{name} must be non-negative")
    return value


if __name__ == "__main__":
    raise SystemExit(main())
