#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


SUPPORT_DIR = Path("/tmp/scx-bench-scheduler.d")
DEFAULT_STATE_DIR = Path("/tmp/scx-real-llm")
DEFAULT_TARGET_COMM = "redis-server"
POST_ATTACH_COMMS = {"ctrl-c", "dmesg", "sched_ext_helpe"}
EXPECTED_CAPABILITIES = {
    "mcp/scx-agent-classed/rules.snapshot.v1",
    "mcp/scx-agent-classed/rule.upsert.v1",
    "mcp/scx-agent-classed/classification.integrity.v1",
}
OWNED_SCHEDULER_OPTIONS = {
    "--rules",
    "--learned-rules",
    "--control-socket",
    "--tuning-agent-socket",
    "--activation-comm",
    "--diagnostic-counters",
}


def main(argv: list[str] | None = None) -> int:
    forwarded = list(sys.argv[1:] if argv is None else argv)
    _reject_owned_options(forwarded)

    output_dir = Path(os.environ.get("SCX_BENCH_OUT", ".")).resolve()
    evidence_dir = output_dir / "real_llm"
    evidence_dir.mkdir(parents=True, exist_ok=True)
    state_dir = Path(
        os.environ.get("SCX_REAL_LLM_STATE_DIR", str(DEFAULT_STATE_DIR))
    ).resolve()
    _reset_state_dir(state_dir)

    target_comm = os.environ.get("SCX_REAL_LLM_TARGET_COMM", DEFAULT_TARGET_COMM)
    activation_comms = _activation_comms(target_comm)
    base_url = _normalize_base_url(
        os.environ.get("SCX_REAL_LLM_BASE_URL", "http://192.168.122.1:17001")
    )
    model = os.environ.get("SCX_REAL_LLM_MODEL", "deepseek-v4-flash")
    api_key = os.environ.get("SCX_REAL_LLM_API_KEY", "local-test")
    diagnostic_counters = _bool_env("SCX_REAL_LLM_DIAGNOSTIC_COUNTERS", True)
    if not model or not api_key:
        raise RuntimeError("the real LLM model and API key must be non-empty")

    tuning_agent = _binary("SCX_REAL_LLM_TUNING_AGENT_BIN", "tuning-agent")
    mcp_server = _binary("SCX_REAL_LLM_MCP_BIN", "scx_agent_classed_mcp")
    scheduler = _binary("SCX_REAL_LLM_SCHEDULER_BIN", "scx_agent_classed")

    paths = {
        "activation": state_dir / "activation.sock",
        "control": state_dir / "control.sock",
        "config": state_dir / "tuning-agent.toml",
        "learned": evidence_dir / "learned.json",
        "audit": evidence_dir / "audit.jsonl",
        "journal": evidence_dir / "mcp-journal.json",
        "transactions": evidence_dir / "transactions",
        "base_rules": evidence_dir / "base.rules",
        "daemon_stdout": evidence_dir / "tuning-agent.stdout.log",
        "daemon_stderr": evidence_dir / "tuning-agent.stderr.log",
    }

    readiness = _wait_for_llm(base_url, api_key, model, timeout=30.0)
    _write_json(evidence_dir / "llm-readiness.json", readiness)
    config = _render_config(base_url, api_key, model, mcp_server, paths)
    paths["config"].write_text(config, encoding="utf-8")
    paths["config"].chmod(0o600)

    daemon = _start_daemon(tuning_agent, paths)
    try:
        loaded = _wait_for_daemon(
            daemon,
            paths["activation"],
            paths["audit"],
            timeout=30.0,
        )
        base_comms = _collect_base_comms(Path("/proc"), activation_comms)
        scheduler_comm = scheduler.name.encode("utf-8")[:15].decode(
            "utf-8", errors="ignore"
        )
        if scheduler_comm:
            base_comms.add(scheduler_comm)
        _write_base_rules(paths["base_rules"], base_comms)
        metadata = {
            "schema_version": 1,
            "target_comm": target_comm,
            "activation_comms": list(activation_comms),
            "base_url": base_url,
            "model": model,
            "diagnostic_counters": diagnostic_counters,
            "daemon_pid": daemon.pid,
            "base_rule_count": len(base_comms),
            "paths": {name: str(path) for name, path in paths.items()},
            "binaries": {
                "tuning_agent": str(tuning_agent),
                "mcp_server": str(mcp_server),
                "scheduler": str(scheduler),
            },
            "mcp_loaded": loaded,
        }
        _write_json(evidence_dir / "supervisor.json", metadata)

        command = [
            str(scheduler),
            *forwarded,
            "--rules",
            str(paths["base_rules"]),
            "--learned-rules",
            str(paths["learned"]),
            "--control-socket",
            str(paths["control"]),
            "--tuning-agent-socket",
            str(paths["activation"]),
        ]
        if diagnostic_counters:
            command.append("--diagnostic-counters")
        for comm in activation_comms:
            command.extend(("--activation-comm", comm))
        os.execv(command[0], command)
    except BaseException:
        _terminate(daemon)
        raise
    return 1


def _binary(env_name: str, default_name: str) -> Path:
    path = Path(os.environ.get(env_name, str(SUPPORT_DIR / default_name))).resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
        raise RuntimeError(f"required executable is missing or not executable: {path}")
    return path


def _normalize_base_url(value: str) -> str:
    normalized = value.strip().rstrip("/")
    if not normalized.startswith(("http://", "https://")):
        raise RuntimeError("SCX_REAL_LLM_BASE_URL must be an HTTP(S) URL")
    if normalized.endswith("/v1"):
        raise RuntimeError(
            "SCX_REAL_LLM_BASE_URL must not end in /v1; tuning-agent appends it"
        )
    return normalized


def _bool_env(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    normalized = value.strip().lower()
    if normalized in {"1", "true", "yes", "on"}:
        return True
    if normalized in {"0", "false", "no", "off"}:
        return False
    raise RuntimeError(f"{name} must be a boolean value")


def _validate_comm(comm: str) -> None:
    encoded = comm.encode("utf-8")
    if not encoded or len(encoded) > 15 or b"\0" in encoded:
        raise RuntimeError("target comm must occupy 1..15 UTF-8 bytes")
    if comm.strip() != comm or any(character in comm for character in "=#\r\n"):
        raise RuntimeError("target comm cannot be represented exactly in a base rule file")


def _activation_comms(target_comm: str) -> tuple[str, ...]:
    _validate_comm(target_comm)
    raw = os.environ.get("SCX_REAL_LLM_ACTIVATION_COMMS")
    if raw is None:
        return (target_comm,)
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise RuntimeError("SCX_REAL_LLM_ACTIVATION_COMMS must be a JSON string list") from error
    if not isinstance(value, list) or not value:
        raise RuntimeError("SCX_REAL_LLM_ACTIVATION_COMMS must be a non-empty JSON string list")

    comms: list[str] = []
    for comm in value:
        if not isinstance(comm, str):
            raise RuntimeError("SCX_REAL_LLM_ACTIVATION_COMMS entries must be strings")
        _validate_comm(comm)
        if comm in comms:
            raise RuntimeError(f"duplicate activation comm: {comm}")
        comms.append(comm)
    if target_comm not in comms:
        raise RuntimeError("SCX_REAL_LLM_TARGET_COMM must be in SCX_REAL_LLM_ACTIVATION_COMMS")
    return tuple(comms)


def _reject_owned_options(arguments: list[str]) -> None:
    for argument in arguments:
        option = argument.split("=", 1)[0]
        if option in OWNED_SCHEDULER_OPTIONS:
            raise RuntimeError(f"scheduler supervisor owns option {option}")


def _reset_state_dir(path: Path) -> None:
    if path == Path("/") or path == Path("/tmp"):
        raise RuntimeError(f"refusing to reset unsafe state directory: {path}")
    if path.exists():
        if path.is_symlink() or not path.is_dir():
            raise RuntimeError(f"state path is not a real directory: {path}")
        shutil.rmtree(path)
    path.mkdir(parents=True, mode=0o700)
    path.chmod(0o700)


def _wait_for_llm(
    base_url: str,
    api_key: str,
    model: str,
    timeout: float,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last_error = "not attempted"
    while time.monotonic() < deadline:
        request = urllib.request.Request(
            f"{base_url}/v1/models",
            headers={"Authorization": f"Bearer {api_key}"},
        )
        try:
            with urllib.request.urlopen(request, timeout=3.0) as response:
                payload = json.load(response)
            models = {
                item.get("id")
                for item in payload.get("data", [])
                if isinstance(item, dict) and isinstance(item.get("id"), str)
            }
            if model not in models:
                raise RuntimeError(f"model {model!r} is absent from /v1/models")
            return {
                "reachable": True,
                "model": model,
                "model_count": len(models),
                "checked_url": f"{base_url}/v1/models",
            }
        except (OSError, ValueError, RuntimeError, urllib.error.URLError) as error:
            last_error = str(error)
            time.sleep(0.25)
    raise RuntimeError(f"real LLM readiness failed: {last_error}")


def _render_config(
    base_url: str,
    api_key: str,
    model: str,
    mcp_server: Path,
    paths: dict[str, Path],
) -> str:
    quote = lambda value: json.dumps(str(value))
    globally_allowed = [
        "builtin/probe.linux-proc-snapshot.v1",
        "builtin/measurement.core-system.v1",
        "builtin/comparison.threshold.v1",
        *sorted(EXPECTED_CAPABILITIES),
    ]
    raw_allowed = [
        "rules.snapshot.v1",
        "rule.upsert.v1",
        "classification.integrity.v1",
    ]
    return (
        "[llm]\n"
        f"base_url = {quote(base_url)}\n"
        f"api_key = {quote(api_key)}\n"
        f"model = {quote(model)}\n"
        "timeout_ms = 120000\n"
        "retry_count = 3\n\n"
        "[reasoning]\n"
        "max_rounds = 12\n\n"
        "[safety]\n"
        "evaluation_timeout_ms = 60000\n"
        "cooldown_ms = 0\n\n"
        "[activation]\n"
        f"socket_path = {quote(paths['activation'])}\n\n"
        "[audit]\n"
        f"path = {quote(paths['audit'])}\n\n"
        "[transaction]\n"
        f"wal_dir = {quote(paths['transactions'])}\n\n"
        "[capabilities]\n"
        f"allowed_capabilities = {json.dumps(globally_allowed)}\n\n"
        "[mcp]\n"
        "enabled = true\n\n"
        "[[mcp.servers]]\n"
        'id = "scx-agent-classed"\n'
        "enabled = true\n"
        f"command = {quote(mcp_server)}\n"
        f"args = {json.dumps(['--control-socket', str(paths['control']), '--journal', str(paths['journal'])])}\n"
        "request_timeout_ms = 30000\n"
        f"allowed_capabilities = {json.dumps(raw_allowed)}\n"
        "allow_mutations = true\n"
    )


def _start_daemon(tuning_agent: Path, paths: dict[str, Path]) -> subprocess.Popen[bytes]:
    with paths["daemon_stdout"].open("wb") as stdout, paths["daemon_stderr"].open(
        "wb"
    ) as stderr:
        return subprocess.Popen(
            [str(tuning_agent), "--config", str(paths["config"]), "daemon"],
            stdout=stdout,
            stderr=stderr,
        )


def _wait_for_daemon(
    daemon: subprocess.Popen[bytes],
    socket_path: Path,
    audit_path: Path,
    timeout: float,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        returncode = daemon.poll()
        if returncode is not None:
            raise RuntimeError(f"tuning-agent exited during startup with status {returncode}")
        records = _read_jsonl(audit_path)
        failures = [record for record in records if record.get("event") == "capability_bootstrap_failed"]
        if failures:
            raise RuntimeError(f"tuning-agent capability bootstrap failed: {failures[-1]}")
        loaded = next(
            (
                record.get("data", {})
                for record in records
                if record.get("event") == "mcp_server_loaded"
                and record.get("data", {}).get("server_id") == "scx-agent-classed"
            ),
            None,
        )
        ready = any(record.get("event") == "capability_registry_ready" for record in records)
        if loaded is not None and ready and socket_path.is_socket():
            capabilities = set(loaded.get("loaded_capabilities", []))
            if capabilities != EXPECTED_CAPABILITIES:
                raise RuntimeError(
                    "scx_agent_classed MCP loaded an unexpected capability set: "
                    f"{sorted(capabilities)}"
                )
            return loaded
        time.sleep(0.05)
    raise RuntimeError("timed out waiting for tuning-agent activation socket")


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    lines = path.read_bytes().split(b"\n")
    records = []
    for line in lines[:-1]:
        if line.strip():
            value = json.loads(line)
            if isinstance(value, dict):
                records.append(value)
    tail = lines[-1]
    if tail.strip():
        try:
            value = json.loads(tail)
        except (UnicodeDecodeError, json.JSONDecodeError):
            pass
        else:
            if isinstance(value, dict):
                records.append(value)
    return records


def _collect_base_comms(proc_root: Path, target_comms: tuple[str, ...]) -> set[str]:
    target_set = set(target_comms)
    comms: set[str] = set()
    for path in proc_root.glob("[0-9]*/task/[0-9]*/comm"):
        try:
            raw_comm = path.read_bytes().removesuffix(b"\n")
            key = raw_comm[:15].split(b"\0", 1)[0]
            comm = key.decode("utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise RuntimeError(f"cannot read an ambient comm from {path}: {error}") from error
        try:
            _validate_comm(comm)
        except RuntimeError as error:
            raise RuntimeError(
                f"ambient comm {raw_comm!r} from {path} cannot be preclassified: {error}"
            ) from error
        if comm in target_set:
            raise RuntimeError(f"target comm {comm!r} already exists before the test")
        comms.add(comm)
        if comm.startswith("kworker/") and "-" in comm:
            comms.add(comm.split("-", 1)[0])
    if not comms:
        raise RuntimeError("ambient comm scan returned no tasks")
    comms.update(POST_ATTACH_COMMS)
    return comms


def _write_base_rules(path: Path, comms: set[str]) -> None:
    path.write_text(
        "".join(f"{comm}=batch\n" for comm in sorted(comms)),
        encoding="utf-8",
    )


def _write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def _terminate(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"scx real-LLM scheduler supervisor: {error}", file=sys.stderr)
        raise SystemExit(1)
