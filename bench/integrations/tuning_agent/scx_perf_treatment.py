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
DEFAULT_COMM = "redis-server"
DEFAULT_CLASS = "latency"
DEFAULT_DURATION_SECONDS = 60.0
DEFAULT_CLASSIFY_TIMEOUT_SECONDS = 300.0
DEFAULT_IO_TIMEOUT_SECONDS = 5.0
DEFAULT_EXPECTED_MODEL = "deepseek-v4-flash"
DEFAULT_STATE_DIR = Path("/tmp/scx-real-llm")
CONTROL_VERSION = 1
MAX_FRAME_BYTES = 1024 * 1024
TARGET_SCRIPT = r"""
import sys
import time

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
        default=[],
        metavar="COMM=CLASS",
        help="target classification; repeat for every workload comm",
    )
    parser.add_argument(
        "--comm",
        default=os.environ.get(
            "SCX_PERF_TARGET_COMM",
            os.environ.get("SCX_REAL_LLM_TARGET_COMM", DEFAULT_COMM),
        ),
        help="single-target compatibility option used when --target is absent",
    )
    parser.add_argument(
        "--expected-class",
        choices=("latency", "batch"),
        default=os.environ.get("SCX_PERF_EXPECTED_CLASS", DEFAULT_CLASS),
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
    if not targets:
        targets = [_target(f"{args.comm}={args.expected_class}")]
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
                    [sys.executable, "-c", TARGET_SCRIPT, target.comm],
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
    expected_rules: dict[str, str] = {}
    episodes: list[dict[str, Any]] = []
    for target in options.targets:
        episode = _classify_target(options, target, paths, control)
        episodes.append(episode)
        expected_rules[target.comm] = target.rule_class
        learned = _read_json(paths["learned"])
        _verify_learned(learned, expected_rules, episode["revision_after"])

    learned = _read_json(paths["learned"])
    final_snapshot = _snapshot(control, options.targets, "final")
    final_revision = _integer(final_snapshot, "revision", "final control snapshot")
    final_seq = _integer(final_snapshot, "rules_seq", "final control snapshot")
    _verify_snapshot_rules(final_snapshot, options.targets)
    _verify_learned(learned, expected_rules, final_revision)
    audit = _read_jsonl(paths["audit"])
    _require_all_episodes_finished(audit)
    episode_count = _require_episode_count(audit, len(options.targets))

    verification = {
        "schema_version": 1,
        "model": supervisor.get("model"),
        "base_url": supervisor.get("base_url"),
        "episodes": episodes,
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


def _classify_target(
    options: Options,
    target: Target,
    paths: dict[str, Path],
    control: ControlClient,
) -> dict[str, Any]:
    before = _wait_for_snapshot(control, target, options.io_timeout_seconds)
    _assert_no_comm_collision(target.comm)
    process: subprocess.Popen[bytes] | None = None
    started = time.monotonic()
    try:
        process = subprocess.Popen(
            [sys.executable, "-c", TARGET_SCRIPT, target.comm],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        _wait_for_comm(process, target.comm, options.io_timeout_seconds)
        episode_id, finished, audit = _wait_for_episode(
            paths["audit"],
            target.comm,
            process,
            options.classify_timeout_seconds,
        )
        after = _snapshot(control, (target,), "after")
    finally:
        if process is not None:
            _terminate(process)

    before_revision, before_seq = _control_state(before, "before")
    after_revision, after_seq = _control_state(after, "after")
    _verify_rule(before, target, learned=False)
    _verify_rule(after, target, learned=True)
    if after_revision - before_revision != 3:
        raise TreatmentError(
            f"{target.comm!r} revision delta was {after_revision - before_revision}, expected 3"
        )
    if after_seq - before_seq != 6:
        raise TreatmentError(
            f"{target.comm!r} rules_seq delta was {after_seq - before_seq}, expected 6"
        )
    if before_seq & 1 or after_seq & 1:
        raise TreatmentError("rules_seq was odd outside rule publication")
    _verify_episode(audit, episode_id, finished, target)
    return {
        "comm": target.comm,
        "class": target.rule_class,
        "episode_id": episode_id,
        "phase": finished.get("phase"),
        "verdict": (finished.get("data", {}).get("decision") or {}).get("verdict"),
        "latency_seconds": time.monotonic() - started,
        "revision_before": before_revision,
        "revision_after": after_revision,
        "rules_seq_before": before_seq,
        "rules_seq_after": after_seq,
    }


def _verify_episode(
    audit: list[dict[str, Any]],
    episode_id: int,
    finished: dict[str, Any],
    target: Target,
) -> None:
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
        raise TreatmentError(f"missing episode_started record for {target.comm!r}")
    unknown = (
        started.get("data", {})
        .get("activation", {})
        .get("evidence", {})
        .get("unknown_comms")
    )
    if unknown != [target.comm]:
        raise TreatmentError(
            f"episode activation was not isolated to {target.comm!r}: {unknown!r}"
        )
    decision = finished.get("data", {}).get("decision") or {}
    if finished.get("phase") != "committed" or decision.get("verdict") != "improved":
        raise TreatmentError(
            f"episode for {target.comm!r} did not commit as improved"
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
    target: Target,
    timeout: float,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last_error = "not attempted"
    while time.monotonic() < deadline:
        try:
            return _snapshot(control, (target,), "before")
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


def _wait_for_episode(
    audit_path: Path,
    comm: str,
    target: subprocess.Popen[bytes],
    timeout: float,
) -> tuple[int, dict[str, Any], list[dict[str, Any]]]:
    deadline = time.monotonic() + timeout
    episode_id: int | None = None
    while time.monotonic() < deadline:
        if target.poll() is not None:
            raise TreatmentError(f"target task exited with status {target.returncode}")
        records = _read_jsonl(audit_path)
        if episode_id is None:
            for record in records:
                if record.get("event") != "episode_started":
                    continue
                unknown = (
                    record.get("data", {})
                    .get("activation", {})
                    .get("evidence", {})
                    .get("unknown_comms", [])
                )
                if comm in unknown and isinstance(record.get("episode_id"), int):
                    episode_id = record["episode_id"]
                    break
        if episode_id is not None:
            for record in records:
                if (
                    record.get("event") == "episode_finished"
                    and record.get("episode_id") == episode_id
                ):
                    return episode_id, record, records
        time.sleep(0.1)
    raise TreatmentError(
        f"timed out waiting for a completed episode for comm {comm!r}"
    )


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
