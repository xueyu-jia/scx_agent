#!/usr/bin/env python3
from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
from typing import Any


OUTCOME_VERSION = 2
DEFAULT_DURATION_SECONDS = 60.0
DEFAULT_CLASSIFY_TIMEOUT_SECONDS = 300.0
DEFAULT_IO_TIMEOUT_SECONDS = 5.0
DEFAULT_EXPECTED_MODEL = "deepseek-v4-flash"
DEFAULT_STATE_DIR = Path("/tmp/scx-real-llm")
CONTROL_VERSION = 1
MAX_FRAME_BYTES = 1024 * 1024
CONTROL_TARGET_SCRIPT = r"""
import sys
import time

with open("/proc/self/comm", "w", encoding="utf-8") as stream:
    stream.write(sys.argv[1])
while True:
    time.sleep(0.005)
"""
GROUP_TARGET_SCRIPT = r"""
import os
import sys
import time

ready_fd = int(sys.argv[2])
release_fd = int(sys.argv[3])
os.write(ready_fd, b"1")
os.close(ready_fd)
if os.read(release_fd, 1) != b"1":
    raise SystemExit("classification barrier closed before release")
os.close(release_fd)
with open("/proc/self/comm", "w", encoding="utf-8") as stream:
    stream.write(sys.argv[1])
while True:
    time.sleep(0.005)
"""


class TreatmentError(RuntimeError):
    pass


class ControlClient:
    def __init__(self, path: Path, timeout: float) -> None:
        self.path = path
        self.timeout = timeout

    def request(self, payload: dict[str, Any]) -> dict[str, Any]:
        request_id = payload["request_id"]
        data = json.dumps(payload, separators=(",", ":")).encode() + b"\n"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(self.timeout)
            connection.connect(str(self.path))
            connection.sendall(data)
            connection.shutdown(socket.SHUT_WR)
            chunks: list[bytes] = []
            size = 0
            while True:
                chunk = connection.recv(64 * 1024)
                if not chunk:
                    break
                size += len(chunk)
                if size > MAX_FRAME_BYTES:
                    raise TreatmentError("control response is too large")
                chunks.append(chunk)

        try:
            response = json.loads(b"".join(chunks))
        except json.JSONDecodeError as error:
            raise TreatmentError("control response is not valid JSON") from error
        if not isinstance(response, dict):
            raise TreatmentError("control response is not an object")
        if response.get("version") != CONTROL_VERSION:
            raise TreatmentError("control response has an unsupported version")
        if response.get("request_id") != request_id:
            raise TreatmentError("control response request_id does not match")
        if response.get("status") == "error":
            raise TreatmentError(response.get("message") or "control request failed")
        return response


@dataclass(frozen=True)
class Target:
    comm: str
    rule_class: str


@dataclass(frozen=True)
class Options:
    mode: str
    duration_seconds: float
    targets: tuple[Target, ...]
    classify_timeout_seconds: float
    io_timeout_seconds: float
    control_socket: Path
    evidence_dir: Path


def main(argv: list[str] | None = None) -> int:
    try:
        parsed = _parser().parse_args(argv)
        outcome_path = _required_path("SCX_BENCH_TREATMENT_OUTCOME")
        output_dir = Path(os.environ.get("SCX_BENCH_OUT", outcome_path.parent)).resolve()
        output_dir.mkdir(parents=True, exist_ok=True)
        options = _options(parsed, output_dir)
    except (OSError, TreatmentError) as error:
        print(f"scx performance treatment configuration failed: {error}", file=sys.stderr)
        return 2

    started = time.monotonic()
    details: dict[str, Any] = {
        "mode": options.mode,
        "duration_seconds": options.duration_seconds,
        "targets": [
            {"comm": target.comm, "class": target.rule_class}
            for target in options.targets
        ],
    }
    try:
        if options.mode == "control":
            details["discovery"] = _run_control(options, output_dir, started)
        else:
            details["classification"] = _run_classification(options)
            elapsed = time.monotonic() - started
            if elapsed > options.duration_seconds:
                raise TreatmentError(
                    "real-LLM classification exceeded the fixed treatment budget: "
                    f"{elapsed:.3f}s > {options.duration_seconds:.3f}s"
                )

        _pad_until(started + options.duration_seconds)
        if options.mode == "classify":
            details["quiet_state"] = _verify_quiet_state(
                options,
                details["classification"],
            )
        details["elapsed_seconds"] = time.monotonic() - started
        _write_outcome(
            outcome_path,
            "proceed",
            f"scx_perf.{options.mode}_ready",
            (
                "control discovery completed"
                if options.mode == "control"
                else "all real-LLM classifications were committed and persisted"
            ),
            details,
        )
        return 0
    except Exception as error:
        details["elapsed_seconds"] = time.monotonic() - started
        details["error"] = str(error)
        try:
            _write_outcome(
                outcome_path,
                "unsafe",
                f"scx_perf.{options.mode}_failed",
                str(error),
                details,
            )
        except OSError as write_error:
            print(f"failed to write treatment outcome: {write_error}", file=sys.stderr)
            return 1
        print(f"scx performance treatment failed: {error}", file=sys.stderr)
        return 0


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Prepare a fixed-duration scx_agent_classed performance run"
    )
    parser.add_argument(
        "--mode",
        choices=("control", "classify"),
        default=os.environ.get("SCX_PERF_TREATMENT_MODE"),
        required="SCX_PERF_TREATMENT_MODE" not in os.environ,
    )
    parser.add_argument(
        "--duration-seconds",
        type=_positive_float,
        default=os.environ.get(
            "SCX_PERF_TREATMENT_SECONDS",
            str(DEFAULT_DURATION_SECONDS),
        ),
    )
    parser.add_argument(
        "--target",
        action="append",
        type=_target,
        required=True,
        metavar="COMM=CLASS",
        help="classification target; repeat for every workload comm",
    )
    parser.add_argument(
        "--classify-timeout-seconds",
        type=_positive_float,
        default=os.environ.get(
            "SCX_PERF_CLASSIFY_TIMEOUT_SECONDS",
            str(DEFAULT_CLASSIFY_TIMEOUT_SECONDS),
        ),
    )
    parser.add_argument(
        "--io-timeout-seconds",
        type=_positive_float,
        default=os.environ.get(
            "SCX_PERF_IO_TIMEOUT_SECONDS",
            str(DEFAULT_IO_TIMEOUT_SECONDS),
        ),
    )
    parser.add_argument("--control-socket", default=os.environ.get("SCX_PERF_CONTROL_SOCKET"))
    parser.add_argument("--evidence-dir", default=os.environ.get("SCX_PERF_EVIDENCE_DIR"))
    return parser


def _options(args: argparse.Namespace, output_dir: Path) -> Options:
    targets = list(args.target)
    comms = [target.comm for target in targets]
    if len(comms) != len(set(comms)):
        raise TreatmentError("target comms must be unique")

    state_dir = Path(
        os.environ.get("SCX_REAL_LLM_STATE_DIR", str(DEFAULT_STATE_DIR))
    ).resolve()
    evidence_dir = (
        Path(args.evidence_dir).resolve()
        if args.evidence_dir
        else output_dir.parent / "real_llm"
    )
    return Options(
        mode=args.mode,
        duration_seconds=args.duration_seconds,
        targets=tuple(targets),
        classify_timeout_seconds=args.classify_timeout_seconds,
        io_timeout_seconds=args.io_timeout_seconds,
        control_socket=Path(args.control_socket or state_dir / "control.sock").resolve(),
        evidence_dir=evidence_dir.resolve(),
    )


def _run_control(
    options: Options,
    output_dir: Path,
    started: float,
) -> dict[str, Any]:
    stdout_path = output_dir / "discovery.stdout.log"
    stderr_path = output_dir / "discovery.stderr.log"
    processes: list[tuple[str, subprocess.Popen[bytes]]] = []
    with stdout_path.open("ab") as stdout, stderr_path.open("ab") as stderr:
        try:
            for target in options.targets:
                process = subprocess.Popen(
                    [sys.executable, "-c", CONTROL_TARGET_SCRIPT, target.comm],
                    stdout=stdout,
                    stderr=stderr,
                    start_new_session=True,
                )
                processes.append((target.comm, process))
            for comm, process in processes:
                _wait_for_comm(process, comm, options.io_timeout_seconds)

            deadline = started + options.duration_seconds
            while time.monotonic() < deadline:
                for comm, process in processes:
                    if process.poll() is not None:
                        raise TreatmentError(
                            f"control discovery {comm!r} exited with status {process.returncode}"
                        )
                time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))
        finally:
            for _comm, process in processes:
                _terminate(process)

    return {
        "kind": "periodic-comm",
        "process_count": len(processes),
        "stdout": str(stdout_path),
        "stderr": str(stderr_path),
    }


def _run_classification(options: Options) -> dict[str, Any]:
    evidence_dir = options.evidence_dir
    evidence_dir.mkdir(parents=True, exist_ok=True)
    paths = {
        "audit": evidence_dir / "audit.jsonl",
        "learned": evidence_dir / "learned.json",
        "supervisor": evidence_dir / "supervisor.json",
    }
    supervisor = _wait_for_json(paths["supervisor"], options.io_timeout_seconds)
    expected_model = os.environ.get("SCX_PERF_EXPECTED_MODEL", DEFAULT_EXPECTED_MODEL)
    if not expected_model:
        raise TreatmentError("SCX_PERF_EXPECTED_MODEL must not be empty")
    if supervisor.get("model") != expected_model:
        raise TreatmentError(
            f"scheduler supervisor is not configured for model {expected_model}"
        )

    control = ControlClient(options.control_socket, options.io_timeout_seconds)
    episode = _classify_group(options, paths, control)
    expected_rules = {target.comm: target.rule_class for target in options.targets}

    learned = _read_json(paths["learned"])
    final_snapshot = _snapshot(control, options.targets, "final")
    final_revision = _integer(final_snapshot, "revision", "final control snapshot")
    final_seq = _integer(final_snapshot, "rules_seq", "final control snapshot")
    _verify_snapshot_rules(final_snapshot, options.targets)
    _verify_learned(learned, expected_rules, final_revision)
    audit = _read_jsonl(paths["audit"])
    _require_all_episodes_finished(audit)
    episode_count = _require_episode_count(audit, 1)

    verification = {
        "schema_version": 1,
        "classification_mode": "group",
        "model": supervisor.get("model"),
        "base_url": supervisor.get("base_url"),
        "activation_comms": [target.comm for target in options.targets],
        "mutation_count": episode["mutation_count"],
        "classification_seconds": episode["classification_seconds"],
        "episodes": [episode],
        "final_revision": final_revision,
        "final_rules_seq": final_seq,
        "learned_sha256": _sha256(paths["learned"]),
        "audit_episode_count": episode_count,
    }
    verification_path = evidence_dir / "perf-verification.json"
    _write_json(verification_path, verification)
    return {
        **verification,
        "verification_path": str(verification_path),
        "learned_path": str(paths["learned"]),
    }


def _classify_group(
    options: Options,
    paths: dict[str, Path],
    control: ControlClient,
) -> dict[str, Any]:
    before = _wait_for_snapshot(control, options.targets, options.io_timeout_seconds)
    for target in options.targets:
        _verify_rule(before, target, learned=False)
        _assert_no_comm_collision(target.comm)

    started = time.monotonic()
    processes: list[tuple[Target, subprocess.Popen[bytes]]] = []
    try:
        processes = _start_target_group(options.targets, options.io_timeout_seconds)
        episode_id, finished, audit = _wait_for_group_episode(
            paths["audit"],
            options.targets,
            processes,
            options.classify_timeout_seconds,
        )
        after = _snapshot(control, options.targets, "after")
    finally:
        for _target, process in processes:
            _terminate(process)

    before_revision, before_seq = _control_state(before, "before")
    after_revision, after_seq = _control_state(after, "after")
    _verify_snapshot_rules(after, options.targets)
    expected_revision_delta = 3 * len(options.targets)
    expected_seq_delta = 6 * len(options.targets)
    if after_revision - before_revision != expected_revision_delta:
        raise TreatmentError(
            "group revision delta was "
            f"{after_revision - before_revision}, expected {expected_revision_delta}"
        )
    if after_seq - before_seq != expected_seq_delta:
        raise TreatmentError(
            f"group rules_seq delta was {after_seq - before_seq}, "
            f"expected {expected_seq_delta}"
        )
    if before_seq & 1 or after_seq & 1:
        raise TreatmentError("rules_seq was odd outside rule publication")
    mutation_count = _verify_group_episode(audit, episode_id, finished, options.targets)
    return {
        "comms": [target.comm for target in options.targets],
        "episode_id": episode_id,
        "phase": finished.get("phase"),
        "verdict": (finished.get("data", {}).get("decision") or {}).get("verdict"),
        "mutation_count": mutation_count,
        "classification_seconds": time.monotonic() - started,
        "revision_before": before_revision,
        "revision_after": after_revision,
        "rules_seq_before": before_seq,
        "rules_seq_after": after_seq,
    }


def _start_target_group(
    targets: tuple[Target, ...],
    timeout: float,
) -> list[tuple[Target, subprocess.Popen[bytes]]]:
    ready_read, ready_write = os.pipe()
    release_read, release_write = os.pipe()
    processes: list[tuple[Target, subprocess.Popen[bytes]]] = []
    try:
        for target in targets:
            process = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    GROUP_TARGET_SCRIPT,
                    target.comm,
                    str(ready_write),
                    str(release_read),
                ],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                pass_fds=(ready_write, release_read),
                start_new_session=True,
            )
            processes.append((target, process))
        os.close(ready_write)
        ready_write = -1
        os.close(release_read)
        release_read = -1
        _wait_for_group_ready(processes, ready_read, timeout)
        released = os.write(release_write, b"1" * len(processes))
        if released != len(processes):
            raise TreatmentError("classification barrier release was incomplete")
        os.close(release_write)
        release_write = -1
        for target, process in processes:
            _wait_for_comm(process, target.comm, timeout)
        return processes
    except Exception:
        for _target, process in processes:
            _terminate(process)
        raise
    finally:
        for descriptor in (ready_read, ready_write, release_read, release_write):
            if descriptor >= 0:
                os.close(descriptor)


def _wait_for_group_ready(
    processes: list[tuple[Target, subprocess.Popen[bytes]]],
    ready_fd: int,
    timeout: float,
) -> None:
    os.set_blocking(ready_fd, False)
    deadline = time.monotonic() + timeout
    ready = 0
    while ready < len(processes) and time.monotonic() < deadline:
        _require_processes_running(processes)
        try:
            data = os.read(ready_fd, len(processes) - ready)
        except BlockingIOError:
            data = b""
        ready += len(data)
        if ready < len(processes):
            time.sleep(0.01)
    if ready != len(processes):
        raise TreatmentError(
            f"classification barrier received {ready} ready processes; "
            f"expected {len(processes)}"
        )


def _wait_for_group_episode(
    audit_path: Path,
    targets: tuple[Target, ...],
    processes: list[tuple[Target, subprocess.Popen[bytes]]],
    timeout: float,
) -> tuple[int, dict[str, Any], list[dict[str, Any]]]:
    expected_comms = sorted(target.comm for target in targets)
    deadline = time.monotonic() + timeout
    episode_id: int | None = None
    while time.monotonic() < deadline:
        _require_processes_running(processes)
        records = _read_jsonl(audit_path)
        started = [
            record for record in records if record.get("event") == "episode_started"
        ]
        if len(started) > 1:
            raise TreatmentError(
                f"group classification started {len(started)} tuning episodes"
            )
        if started:
            unknown = _unknown_comms(started[0])
            if unknown != expected_comms:
                raise TreatmentError(
                    f"group activation contained {unknown!r}, expected {expected_comms!r}"
                )
            observed_id = started[0].get("episode_id")
            if not isinstance(observed_id, int):
                raise TreatmentError("group episode_started omitted its episode_id")
            episode_id = observed_id
        if episode_id is not None:
            for record in records:
                if (
                    record.get("event") == "episode_finished"
                    and record.get("episode_id") == episode_id
                ):
                    return episode_id, record, records
        time.sleep(0.1)
    raise TreatmentError("timed out waiting for the completed group classification episode")


def _verify_group_episode(
    audit: list[dict[str, Any]],
    episode_id: int,
    finished: dict[str, Any],
    targets: tuple[Target, ...],
) -> int:
    started = next(
        (
            record
            for record in audit
            if record.get("event") == "episode_started"
            and record.get("episode_id") == episode_id
        ),
        None,
    )
    if started is None:
        raise TreatmentError("missing group episode_started record")
    expected_rules = {target.comm: target.rule_class for target in targets}
    unknown = _unknown_comms(started)
    if unknown != sorted(expected_rules):
        raise TreatmentError(
            f"group activation contained {unknown!r}, expected {sorted(expected_rules)!r}"
        )
    decision = finished.get("data", {}).get("decision") or {}
    if finished.get("phase") != "committed" or decision.get("verdict") != "improved":
        raise TreatmentError("group episode did not commit as improved")
    return _verify_group_transaction(audit, episode_id, expected_rules)


def _verify_group_transaction(
    audit: list[dict[str, Any]],
    episode_id: int,
    expected_rules: dict[str, str],
) -> int:
    records = [record for record in audit if record.get("episode_id") == episode_id]
    commands = {
        record.get("data", {}).get("call_id"): record
        for record in records
        if record.get("event") == "agent_command"
        and isinstance(record.get("data", {}).get("call_id"), str)
    }
    mutations: dict[str, str] = {}
    change_ids: set[str] = set()
    for record in records:
        data = record.get("data", {})
        content = data.get("content", {})
        change = content.get("change", {}) if isinstance(content, dict) else {}
        if (
            record.get("event") != "agent_command_result"
            or not data.get("ok")
            or change.get("capability_id")
            != "mcp/scx-agent-classed/rule.upsert.v1"
        ):
            continue
        command = commands.get(data.get("call_id"), {}).get("data", {})
        arguments = command.get("arguments", {}).get("arguments", {})
        comm = arguments.get("comm")
        rule_class = arguments.get("class")
        change_id = change.get("change_id")
        if not all(isinstance(value, str) for value in (comm, rule_class, change_id)):
            raise TreatmentError("group mutation audit record is incomplete")
        if comm in mutations or change_id in change_ids:
            raise TreatmentError("group episode contains duplicate mutations")
        mutations[comm] = rule_class
        change_ids.add(change_id)
    if mutations != expected_rules:
        raise TreatmentError(
            f"group mutations differ from requested targets: {mutations!r}"
        )

    begin_commands = [
        record
        for record in records
        if record.get("event") == "agent_command"
        and record.get("data", {}).get("tool") == "begin_experiment"
    ]
    if len(begin_commands) != 1:
        raise TreatmentError(
            f"group episode issued {len(begin_commands)} begin_experiment commands"
        )
    measurement_targets = (
        begin_commands[0]
        .get("data", {})
        .get("arguments", {})
        .get("evaluation_contract", {})
        .get("measurement", {})
        .get("specification", {})
        .get("targets")
    )
    if _target_rules(measurement_targets) != expected_rules:
        raise TreatmentError("group measurement targets differ from requested targets")

    commit_commands = [
        record
        for record in records
        if record.get("event") == "agent_command"
        and record.get("data", {}).get("tool") == "request_commit"
    ]
    if len(commit_commands) != 1:
        raise TreatmentError(
            f"group episode issued {len(commit_commands)} request_commit commands"
        )
    commit_data = commit_commands[0].get("data", {})
    committed_ids = commit_data.get("arguments", {}).get("change_ids")
    if (
        not isinstance(committed_ids, list)
        or not all(isinstance(change_id, str) for change_id in committed_ids)
        or len(committed_ids) != len(change_ids)
        or set(committed_ids) != change_ids
    ):
        raise TreatmentError("group request_commit omitted mutation change IDs")

    commit_result = next(
        (
            record.get("data", {})
            for record in records
            if record.get("event") == "agent_command_result"
            and record.get("data", {}).get("call_id") == commit_data.get("call_id")
        ),
        None,
    )
    if not isinstance(commit_result, dict) or not commit_result.get("ok"):
        raise TreatmentError("group request_commit did not succeed")
    result_content = commit_result.get("content", {})
    if not isinstance(result_content, dict):
        raise TreatmentError("group request_commit returned invalid content")
    finalized = result_content.get("finalized_changes")
    finalized_ids = (
        {
            change.get("change_id")
            for change in finalized
            if isinstance(change, dict) and isinstance(change.get("change_id"), str)
        }
        if isinstance(finalized, list)
        else set()
    )
    if (
        not result_content.get("committed")
        or not isinstance(finalized, list)
        or len(finalized) != len(change_ids)
        or finalized_ids != change_ids
    ):
        raise TreatmentError("group commit did not finalize every mutation")
    return len(mutations)


def _target_rules(value: Any) -> dict[str, str]:
    if not isinstance(value, list):
        return {}
    observed: dict[str, str] = {}
    for target in value:
        if not isinstance(target, dict) or set(target) != {"comm", "class"}:
            return {}
        comm, rule_class = target.get("comm"), target.get("class")
        if not isinstance(comm, str) or not isinstance(rule_class, str) or comm in observed:
            return {}
        observed[comm] = rule_class
    return observed


def _unknown_comms(record: dict[str, Any]) -> list[str]:
    unknown = (
        record.get("data", {})
        .get("activation", {})
        .get("evidence", {})
        .get("unknown_comms")
    )
    if (
        not isinstance(unknown, list)
        or not all(isinstance(comm, str) for comm in unknown)
        or len(unknown) != len(set(unknown))
    ):
        raise TreatmentError("group activation has invalid unknown_comms")
    return sorted(unknown)


def _require_processes_running(
    processes: list[tuple[Target, subprocess.Popen[bytes]]],
) -> None:
    for target, process in processes:
        if process.poll() is not None:
            raise TreatmentError(
                f"target {target.comm!r} exited with status {process.returncode}"
            )


def _verify_quiet_state(
    options: Options,
    classification: dict[str, Any],
) -> dict[str, Any]:
    learned_path = Path(classification["learned_path"])
    learned = _read_json(learned_path)
    learned_sha256 = _sha256(learned_path)
    if learned_sha256 != classification["learned_sha256"]:
        raise TreatmentError("learned rule file changed during the quiet window")

    control = ControlClient(options.control_socket, options.io_timeout_seconds)
    snapshot = _snapshot(control, options.targets, "quiet")
    revision, rules_seq = _control_state(snapshot, "quiet")
    if revision != classification["final_revision"]:
        raise TreatmentError("persistent revision changed during the quiet window")
    if rules_seq != classification["final_rules_seq"]:
        raise TreatmentError("rules_seq changed during the quiet window")
    _verify_snapshot_rules(snapshot, options.targets)
    _verify_learned(
        learned,
        {target.comm: target.rule_class for target in options.targets},
        revision,
    )

    audit = _read_jsonl(options.evidence_dir / "audit.jsonl")
    _require_all_episodes_finished(audit)
    episode_count = _episode_count(audit)
    if episode_count != classification["audit_episode_count"]:
        raise TreatmentError("a new tuning episode started during the quiet window")
    return {
        "revision": revision,
        "rules_seq": rules_seq,
        "learned_sha256": learned_sha256,
        "episode_count": episode_count,
    }


def _snapshot(
    control: ControlClient,
    targets: tuple[Target, ...],
    suffix: str,
) -> dict[str, Any]:
    return control.request(
        {
            "version": 1,
            "request_id": f"scx-perf-{os.getpid()}-{suffix}",
            "op": "snapshot",
            "comms": [target.comm for target in targets],
        }
    )


def _wait_for_snapshot(
    control: ControlClient,
    targets: tuple[Target, ...],
    timeout: float,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last_error = "not attempted"
    while time.monotonic() < deadline:
        try:
            return _snapshot(control, targets, "before")
        except (OSError, TreatmentError) as error:
            last_error = str(error)
            time.sleep(0.05)
    raise TreatmentError(
        f"scheduler control socket did not become ready: {last_error}"
    )


def _wait_for_json(path: Path, timeout: float) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last_error = "file does not exist"
    while time.monotonic() < deadline:
        try:
            return _read_json(path)
        except (OSError, ValueError, TreatmentError) as error:
            last_error = str(error)
            time.sleep(0.05)
    raise TreatmentError(f"timed out reading {path}: {last_error}")


def _control_state(snapshot: dict[str, Any], label: str) -> tuple[int, int]:
    return (
        _integer(snapshot, "revision", f"{label} control snapshot"),
        _integer(snapshot, "rules_seq", f"{label} control snapshot"),
    )


def _verify_rule(snapshot: dict[str, Any], target: Target, *, learned: bool) -> None:
    rules = snapshot.get("rules")
    matches = (
        [rule for rule in rules if isinstance(rule, dict) and rule.get("comm") == target.comm]
        if isinstance(rules, list)
        else []
    )
    if len(matches) != 1:
        raise TreatmentError(f"control snapshot contains {len(matches)} rules for {target.comm!r}")
    if learned:
        expected = {
            "comm": target.comm,
            "class": target.rule_class,
            "source": "learned",
            "active_class": target.rule_class,
            "persisted_class": target.rule_class,
            "consistent": True,
        }
    else:
        expected = {
            "comm": target.comm,
            "class": "batch",
            "source": "default",
            "consistent": True,
        }
    if matches[0] != expected:
        raise TreatmentError(f"unexpected rule state for {target.comm!r}: {matches[0]!r}")


def _verify_snapshot_rules(snapshot: dict[str, Any], targets: tuple[Target, ...]) -> None:
    for target in targets:
        _verify_rule(snapshot, target, learned=True)


def _verify_learned(
    learned: dict[str, Any],
    expected_rules: dict[str, str],
    revision: int,
) -> None:
    rules = learned.get("rules")
    if learned.get("schema_version") != 1 or learned.get("revision") != revision:
        raise TreatmentError("learned rule metadata does not match control state")
    if not isinstance(rules, list):
        raise TreatmentError("learned rule document omitted its rule list")
    observed: dict[str, str] = {}
    for rule in rules:
        if not isinstance(rule, dict) or set(rule) != {"comm", "class"}:
            raise TreatmentError(f"invalid learned rule: {rule!r}")
        comm, rule_class = rule.get("comm"), rule.get("class")
        if not isinstance(comm, str) or not isinstance(rule_class, str) or comm in observed:
            raise TreatmentError("learned rule contains an invalid or duplicate comm")
        observed[comm] = rule_class
    if observed != expected_rules:
        raise TreatmentError(f"learned rules differ from requested targets: {observed!r}")


def _require_all_episodes_finished(records: list[dict[str, Any]]) -> None:
    started = _episode_ids(records, "episode_started")
    pending = sorted(started - _episode_ids(records, "episode_finished"))
    if pending:
        raise TreatmentError(f"tuning episodes are still pending: {pending!r}")


def _episode_ids(records: list[dict[str, Any]], event: str) -> set[int]:
    return {
        episode_id
        for record in records
        if record.get("event") == event
        and isinstance((episode_id := record.get("episode_id")), int)
    }


def _episode_count(records: list[dict[str, Any]]) -> int:
    return len(_episode_ids(records, "episode_started"))


def _require_episode_count(records: list[dict[str, Any]], expected: int) -> int:
    observed = _episode_count(records)
    if observed != expected:
        raise TreatmentError(
            f"audit contains {observed} tuning episodes, expected {expected}"
        )
    return observed


def _wait_for_comm(process: subprocess.Popen[bytes], comm: str, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    path = Path(f"/proc/{process.pid}/comm")
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise TreatmentError(f"target {comm!r} exited with status {process.returncode}")
        try:
            if path.read_text(encoding="utf-8").rstrip("\n") == comm:
                return
        except OSError:
            pass
        time.sleep(0.01)
    raise TreatmentError(f"target did not publish comm {comm!r}")


def _terminate(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def _pad_until(deadline: float) -> None:
    while (remaining := deadline - time.monotonic()) > 0:
        time.sleep(min(remaining, 0.25))


def _target(value: str) -> Target:
    comm, separator, rule_class = value.partition("=")
    if not separator or rule_class not in {"latency", "batch"}:
        raise argparse.ArgumentTypeError("target must be COMM=latency or COMM=batch")
    try:
        encoded = comm.encode("utf-8")
    except UnicodeEncodeError as error:
        raise argparse.ArgumentTypeError("target comm must be valid UTF-8") from error
    if not encoded or len(encoded) > 15 or b"\0" in encoded:
        raise argparse.ArgumentTypeError("target comm must occupy 1..15 UTF-8 bytes")
    return Target(comm, rule_class)


def _integer(value: dict[str, Any], key: str, label: str) -> int:
    observed = value.get(key)
    if isinstance(observed, bool) or not isinstance(observed, int):
        raise TreatmentError(f"{label} omitted integer {key}")
    return observed


def _required_path(name: str) -> Path:
    value = os.environ.get(name)
    if not value:
        raise TreatmentError(f"{name} is required")
    path = Path(value)
    if not path.is_absolute():
        raise TreatmentError(f"{name} must be an absolute path")
    return path


def _positive_float(value: str) -> float:
    try:
        parsed = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("must be a positive number") from error
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be a positive number")
    return parsed


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _assert_no_comm_collision(comm: str) -> None:
    matches = []
    for path in Path("/proc").glob("[0-9]*/task/[0-9]*/comm"):
        try:
            if path.read_text(encoding="utf-8").strip() == comm:
                matches.append(path)
        except OSError:
            continue
    if matches:
        raise TreatmentError(
            f"target comm {comm!r} already belongs to {len(matches)} task(s)"
        )


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise TreatmentError(f"invalid JSON artifact: {path}") from error
    if not isinstance(value, dict):
        raise TreatmentError(f"JSON artifact is not an object: {path}")
    return value


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    lines = path.read_bytes().split(b"\n")
    records: list[dict[str, Any]] = []
    for line in lines[:-1]:
        if not line.strip():
            continue
        value = json.loads(line)
        if isinstance(value, dict):
            records.append(value)
    if lines[-1].strip():
        try:
            value = json.loads(lines[-1])
        except (UnicodeDecodeError, json.JSONDecodeError):
            pass
        else:
            if isinstance(value, dict):
                records.append(value)
    return records


def _write_outcome(
    path: Path,
    disposition: str,
    code: str,
    message: str,
    details: dict[str, Any],
) -> None:
    _write_json(
        path,
        {
            "version": OUTCOME_VERSION,
            "disposition": disposition,
            "reason": {"code": code, "message": message},
            "details": details,
        },
    )


def _write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, path)


if __name__ == "__main__":
    raise SystemExit(main())
