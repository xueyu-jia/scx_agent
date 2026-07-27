#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path
from typing import Any, Callable


if __package__:
    from .common import (
        MAX_CPU_WEIGHT,
        METRIC_NAMES,
        MIN_CPU_WEIGHT,
        RedisCpuError,
        RedisCpuScope,
        content_digest,
        read_json,
        read_weight,
        scope_state,
        validate_runtime_identity,
        write_json_atomic,
        write_weight,
    )
else:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    sys.path.insert(0, str(Path.cwd()))
    from common import (  # noqa: E402
        MAX_CPU_WEIGHT,
        METRIC_NAMES,
        MIN_CPU_WEIGHT,
        RedisCpuError,
        RedisCpuScope,
        content_digest,
        read_json,
        read_weight,
        scope_state,
        validate_runtime_identity,
        write_json_atomic,
        write_weight,
    )


PROTOCOL_VERSION = "2024-11-05"
CAPABILITY_URI = "tuning://capabilities/v1"
PROVIDER_VERSION = "1.0.0"


def main() -> int:
    server = RedisCpuMcp()
    for line in sys.stdin.buffer:
        request: Any = None
        try:
            request = json.loads(line.decode("utf-8"))
            response = server.handle(request)
        except Exception as exc:
            response = {
                "jsonrpc": "2.0",
                "id": request.get("id") if isinstance(request, dict) else None,
                "error": {"code": -32603, "message": str(exc)},
            }
        if response is not None:
            sys.stdout.write(json.dumps(response, separators=(",", ":")) + "\n")
            sys.stdout.flush()
    return 0


class RedisCpuMcp:
    def __init__(self) -> None:
        root = os.environ.get("SCX_REDIS_CPU_ROOT", "/sys/fs/cgroup/scx-bench")
        self.scope = RedisCpuScope.from_root(root)
        output_dir = Path(os.environ.get("SCX_REDIS_CPU_OUTPUT_DIR", "/tmp"))
        self.runtime_path = Path(
            os.environ.get("SCX_REDIS_CPU_RUNTIME_PATH", str(output_dir / "runtime.json"))
        )
        self.loadgen_state_path = Path(
            os.environ.get(
                "SCX_REDIS_CPU_LOADGEN_STATE_PATH",
                str(output_dir / "loadgen-state.json"),
            )
        )
        operation_path = Path(
            os.environ.get(
                "SCX_REDIS_CPU_OPERATION_STATE_PATH",
                str(output_dir / "redis-cpu-operations.json"),
            )
        )
        self.journal_path = Path(
            os.environ.get(
                "SCX_REDIS_CPU_JOURNAL_PATH",
                str(output_dir / "redis-cpu-mcp-journal.jsonl"),
            )
        )
        self.restore_failure = os.environ.get("SCX_REDIS_CPU_RESTORE_FAILURE", "never")
        if self.restore_failure not in {"never", "always"}:
            raise RedisCpuError("SCX_REDIS_CPU_RESTORE_FAILURE must be 'never' or 'always'")
        self.operations = OperationStore(operation_path)
        self.sessions: dict[str, dict[str, Any]] = {}

    def handle(self, request: dict[str, Any]) -> dict[str, Any] | None:
        method = request.get("method")
        if "id" not in request:
            return None
        try:
            if method == "initialize":
                result = {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "redis-cpu-tuning-mcp", "version": PROVIDER_VERSION},
                }
            elif method == "resources/read":
                result = self._resources_read(request.get("params", {}))
            elif method == "tools/list":
                result = self._tools_list()
            elif method == "tools/call":
                result = self._tools_call(request.get("params", {}))
            else:
                return self._error(request, -32601, f"unknown method: {method}")
            return {"jsonrpc": "2.0", "id": request["id"], "result": result}
        except (RedisCpuError, McpToolError, OSError, ValueError) as exc:
            return {
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "isError": True,
                    "content": [{"type": "text", "text": str(exc)}],
                },
            }

    def _resources_read(self, params: dict[str, Any]) -> dict[str, Any]:
        if params.get("uri") != CAPABILITY_URI:
            raise McpToolError(f"unsupported resource uri: {params.get('uri')!r}")
        return {
            "contents": [
                {
                    "uri": CAPABILITY_URI,
                    "mimeType": "application/json",
                    "text": json.dumps(self._manifest(), separators=(",", ":")),
                }
            ]
        }

    def _tools_list(self) -> dict[str, Any]:
        schema = {"type": "object", "additionalProperties": True}
        return {
            "tools": [
                {"name": name, "description": name, "inputSchema": schema}
                for name in self._handlers()
            ]
        }

    def _tools_call(self, params: dict[str, Any]) -> dict[str, Any]:
        name = params.get("name")
        arguments = params.get("arguments", {})
        handler = self._handlers().get(name)
        if handler is None:
            raise McpToolError(f"unknown tool: {name}")
        self._journal(name, "started", {"arguments_digest": content_digest(arguments)})
        try:
            result = handler(arguments)
        except Exception as exc:
            self._journal(name, "failed", {"error": str(exc)})
            raise
        self._journal(name, "completed", {})
        return {"structuredContent": result}

    def _handlers(self) -> dict[str, Callable[[dict[str, Any]], dict[str, Any]]]:
        return {
            "probe.snapshot": self._probe,
            "measurement.validate": self._measurement_validate,
            "measurement.open": self._measurement_open,
            "measurement.sample": self._measurement_sample,
            "measurement.close": self._measurement_close,
            "mutation.prepare": self._mutation_prepare,
            "mutation.apply": self._mutation_apply,
            "mutation.status": self._mutation_status,
            "mutation.verify": self._mutation_verify,
            "mutation.restore": self._mutation_restore,
            "mutation.finalize": self._mutation_finalize,
        }

    def _probe(self, request: dict[str, Any]) -> dict[str, Any]:
        arguments = _object(request.get("arguments", {}), "probe arguments")
        if set(arguments) - {"recent_samples"}:
            raise McpToolError("probe accepts only recent_samples")
        recent_count = _bounded_int(arguments.get("recent_samples", 3), 1, 8, "recent_samples")
        runtime, fingerprint = self._runtime()
        state = self._loadgen_state(runtime)
        valid = [sample for sample in state["samples"] if sample.get("quality") == "valid"]
        recent = valid[-recent_count:]
        latest = recent[-1] if recent else None
        return {
            "observed_at_ns": time.time_ns(),
            "data": {
                "workload_fingerprint": fingerprint,
                "workload_digest": runtime["workload_digest"],
                "scope": scope_state(self.scope),
                "latest_sequence": state["next_sequence"] - 1,
                "latest_valid_metrics": latest.get("metrics") if latest else None,
                "recent_valid_samples": [
                    {
                        "sequence": sample["sequence"],
                        "ended_at_ns": sample["ended_at_ns"],
                        "metrics": sample["metrics"],
                    }
                    for sample in recent
                ],
                "metric_semantics": {
                    "latency": "microseconds; each percentile is the slower of the two shards",
                    "redis_qps": "sum of both shard request rates",
                    "cpu_rate": "CPU seconds consumed per wall-clock second",
                    "cpu_share": "share of Redis plus batch cgroup CPU usage",
                    "cpu_pressure_some_pct": "cgroup CPU some-stall total delta per wall-clock window",
                },
            },
            "warnings": [] if latest is not None else ["no valid loadgen sample is available"],
        }

    def _measurement_validate(self, request: dict[str, Any]) -> dict[str, Any]:
        try:
            _measurement_spec(request.get("specification"))
        except McpToolError as exc:
            return {"valid": False, "message": str(exc)}
        return {"valid": True}

    def _measurement_open(self, request: dict[str, Any]) -> dict[str, Any]:
        specification = _measurement_spec(request.get("specification"))
        runtime, fingerprint = self._runtime()
        state = self._loadgen_state(runtime)
        operation = _required_text(
            _object(request.get("context"), "measurement context").get("operation_id"),
            "operation_id",
        )
        session_id = f"redis-cpu-{operation}"
        session = {
            "run_id": runtime["run_id"],
            "workload_fingerprint": fingerprint,
            "opened_monotonic_ns": time.monotonic_ns(),
            "last_sequence": state["next_sequence"] - 1,
            "expected_weight": read_weight(self.scope.redis),
            **specification,
        }
        self.sessions[session_id] = session
        return {"id": session_id, "driver_data": dict(session)}

    def _measurement_sample(self, request: dict[str, Any]) -> dict[str, Any]:
        wire_session = _object(request.get("session"), "measurement session")
        session_id = _required_text(wire_session.get("id"), "measurement session id")
        driver_data = _object(wire_session.get("driver_data"), "measurement driver data")
        session = self.sessions.get(session_id)
        if session is None:
            raise McpToolError("measurement session is unknown")
        # The Runtime echoes immutable open data; last_sequence is tracked only server-side.
        immutable = dict(session)
        immutable.pop("last_sequence", None)
        echoed = dict(driver_data)
        echoed.pop("last_sequence", None)
        if echoed != immutable:
            raise McpToolError("measurement session was modified")
        runtime, fingerprint = self._runtime()
        if runtime["run_id"] != session["run_id"] or fingerprint != session["workload_fingerprint"]:
            raise McpToolError("measurement workload identity changed")
        if read_weight(self.scope.redis) != session["expected_weight"]:
            raise McpToolError("measurement opened for a different cpu.weight")

        deadline = time.monotonic() + session["wait_timeout_ms"] / 1_000.0
        skipped = []
        while time.monotonic() < deadline:
            state = self._loadgen_state(runtime)
            candidates = sorted(
                (
                    sample
                    for sample in state["samples"]
                    if isinstance(sample.get("sequence"), int)
                    and sample["sequence"] > session["last_sequence"]
                ),
                key=lambda sample: sample["sequence"],
            )
            for sample in candidates:
                session["last_sequence"] = sample["sequence"]
                if sample.get("monotonic_started_ns", 0) < session["opened_monotonic_ns"]:
                    skipped.append({"sequence": sample["sequence"], "reason": "started_before_open"})
                    continue
                return self._wire_sample(sample, session, skipped)
            time.sleep(0.05)
        return self._invalid_fallback(runtime, session, "timed out waiting for a new sequence", skipped)

    def _wire_sample(
        self,
        sample: dict[str, Any],
        session: dict[str, Any],
        skipped: list[dict[str, Any]],
    ) -> dict[str, Any]:
        errors = list(sample.get("errors", []))
        if sample.get("weight_at_start") != session["expected_weight"]:
            errors.append("sample started at a different cpu.weight")
        if sample.get("weight_at_end") != session["expected_weight"]:
            errors.append("sample ended at a different cpu.weight")
        if sample.get("workload_fingerprint") != session["workload_fingerprint"]:
            errors.append("sample workload fingerprint drifted")
        age_ms = max(0, time.monotonic_ns() - int(sample["monotonic_ended_ns"])) / 1_000_000.0
        if age_ms > session["max_sample_age_ms"]:
            errors.append("sample exceeded the configured freshness limit")
        metrics = sample.get("metrics")
        if not isinstance(metrics, dict) or set(metrics) != set(METRIC_NAMES):
            runtime, _ = self._runtime()
            return self._invalid_fallback(
                runtime,
                session,
                "new sequence did not contain a complete metric vector",
                skipped,
                failed_sequence=sample.get("sequence"),
            )
        quality = "valid" if sample.get("quality") == "valid" and not errors else "invalid"
        return _metric_batch(
            sample,
            metrics,
            quality=quality,
            fingerprint=session["workload_fingerprint"],
            provenance={
                "provider": "redis-cpu-mcp",
                "version": PROVIDER_VERSION,
                "sequence": sample["sequence"],
                "age_ms": age_ms,
                "skipped": skipped,
                "errors": errors,
            },
        )

    def _invalid_fallback(
        self,
        runtime: dict[str, Any],
        session: dict[str, Any],
        reason: str,
        skipped: list[dict[str, Any]],
        *,
        failed_sequence: int | None = None,
    ) -> dict[str, Any]:
        state = self._loadgen_state(runtime)
        fallback = next(
            (
                sample
                for sample in reversed(state["samples"])
                if sample.get("quality") == "valid"
                and isinstance(sample.get("metrics"), dict)
                and set(sample["metrics"]) == set(METRIC_NAMES)
            ),
            None,
        )
        if fallback is None:
            raise McpToolError(f"{reason}; no complete diagnostic fallback exists")
        return _metric_batch(
            fallback,
            fallback["metrics"],
            quality="invalid",
            fingerprint=session["workload_fingerprint"],
            provenance={
                "provider": "redis-cpu-mcp",
                "version": PROVIDER_VERSION,
                "reason": reason,
                "failed_sequence": failed_sequence,
                "fallback_sequence": fallback["sequence"],
                "skipped": skipped,
            },
        )

    def _measurement_close(self, request: dict[str, Any]) -> dict[str, Any]:
        session_id = _required_text(request.get("id"), "measurement close session id")
        self.sessions.pop(session_id, None)
        return {"session_id": session_id, "cleaned_up": True, "details": {}}

    def _mutation_prepare(self, request: dict[str, Any]) -> dict[str, Any]:
        context = _object(request.get("context"), "mutation context")
        _required_text(context.get("operation_id"), "operation_id")
        arguments = _object(request.get("arguments"), "mutation arguments")
        if set(arguments) != {"value"}:
            raise McpToolError("mutation arguments require only value")
        desired = _bounded_int(arguments.get("value"), MIN_CPU_WEIGHT, MAX_CPU_WEIGHT, "value")
        runtime, fingerprint = self._runtime()
        baseline = read_weight(self.scope.redis)
        if baseline == desired:
            raise McpToolError("cpu.weight mutation must change the current value")
        inode = self.scope.redis.stat().st_ino
        return {
            "resource": f"cgroup:{inode}/cpu.weight",
            "baseline": {"value": baseline},
            "desired": {"value": desired},
            "driver_data": {
                "target": str(self.scope.redis),
                "target_inode": inode,
                "workload_digest": runtime["workload_digest"],
                "workload_fingerprint": fingerprint,
                "provider_version": PROVIDER_VERSION,
            },
        }

    def _mutation_apply(self, request: dict[str, Any]) -> dict[str, Any]:
        operation_id = _required_text(request.get("operation_id"), "operation_id")
        prepared = self._prepared(request.get("prepared"))
        baseline = _prepared_value(prepared, "baseline")
        desired = _prepared_value(prepared, "desired")
        record = self.operations.begin(operation_id, "apply", baseline, desired, prepared)
        completed = self._completed_receipt(operation_id, record)
        if completed is not None:
            return completed
        observed = read_weight(self.scope.redis)
        _require_known_state(observed, baseline, desired, "apply")
        if observed != desired:
            write_weight(self.scope.redis, desired)
        self.operations.complete(operation_id, "applied")
        return self._receipt(operation_id, "applied")

    def _mutation_status(self, request: dict[str, Any]) -> dict[str, Any]:
        operation_id = _required_text(request.get("operation_id"), "operation_id")
        record = self.operations.get(operation_id)
        if record is None:
            return {"operation_id": operation_id, "state": "unknown", "driver_data": {}}
        observed = read_weight(self.scope.redis)
        baseline = record["baseline"]
        desired = record["desired"]
        if observed not in {baseline, desired}:
            state = "unknown"
        elif record["kind"] == "apply":
            state = "applied" if observed == desired else "not_applied"
        elif record["kind"] == "restore":
            state = "restored" if observed == baseline else "applied"
        elif observed == desired and record.get("phase") == "completed":
            state = "finalized"
        else:
            state = "applied" if observed == desired else "not_applied"
        return {
            "operation_id": operation_id,
            "state": state,
            "observed": {"value": observed},
            "driver_data": {"kind": record["kind"], "phase": record["phase"]},
        }

    def _mutation_verify(self, request: dict[str, Any]) -> dict[str, Any]:
        _required_text(request.get("operation_id"), "operation_id")
        self._prepared(request.get("prepared"))
        expected = _object(request.get("expected"), "expected mutation value")
        expected_value = _bounded_int(
            expected.get("value"), MIN_CPU_WEIGHT, MAX_CPU_WEIGHT, "expected value"
        )
        observed = read_weight(self.scope.redis)
        return {
            "matched": observed == expected_value,
            "observed": {"value": observed},
            "details": {},
        }

    def _mutation_restore(self, request: dict[str, Any]) -> dict[str, Any]:
        operation_id = _required_text(request.get("operation_id"), "operation_id")
        prepared = self._prepared(request.get("prepared"), require_live_workload=False)
        baseline = _prepared_value(prepared, "baseline")
        desired = _prepared_value(prepared, "desired")
        record = self.operations.begin(operation_id, "restore", baseline, desired, prepared)
        completed = self._completed_receipt(operation_id, record)
        if completed is not None:
            return completed
        observed = read_weight(self.scope.redis)
        _require_known_state(observed, baseline, desired, "restore")
        if self.restore_failure == "always":
            raise McpToolError("injected Redis cpu.weight restore failure")
        if observed != baseline:
            write_weight(self.scope.redis, baseline)
        self.operations.complete(operation_id, "restored")
        return self._receipt(operation_id, "restored")

    def _mutation_finalize(self, request: dict[str, Any]) -> dict[str, Any]:
        operation_id = _required_text(request.get("operation_id"), "operation_id")
        prepared = self._prepared(request.get("prepared"))
        baseline = _prepared_value(prepared, "baseline")
        desired = _prepared_value(prepared, "desired")
        record = self.operations.begin(operation_id, "finalize", baseline, desired, prepared)
        completed = self._completed_receipt(operation_id, record)
        if completed is not None:
            return completed
        if read_weight(self.scope.redis) != desired:
            raise McpToolError("cannot finalize cpu.weight because desired state is not active")
        self.operations.complete(operation_id, "finalized")
        return self._receipt(operation_id, "finalized")

    def _prepared(
        self,
        value: Any,
        *,
        require_live_workload: bool = True,
    ) -> dict[str, Any]:
        prepared = _object(value, "prepared mutation")
        driver_data = _object(prepared.get("driver_data"), "prepared driver data")
        if set(driver_data) != {
            "target",
            "target_inode",
            "workload_digest",
            "workload_fingerprint",
            "provider_version",
        }:
            raise McpToolError("prepared mutation driver_data has an invalid schema")
        if driver_data["provider_version"] != PROVIDER_VERSION:
            raise McpToolError("prepared mutation provider version changed")
        if driver_data["target"] != str(self.scope.redis):
            raise McpToolError("prepared mutation target changed")
        if driver_data["target_inode"] != self.scope.redis.stat().st_ino:
            raise McpToolError("prepared mutation target inode changed")
        if require_live_workload:
            runtime, fingerprint = self._runtime()
        else:
            runtime = self._runtime_static()
            fingerprint = driver_data["workload_fingerprint"]
        if driver_data["workload_digest"] != runtime["workload_digest"]:
            raise McpToolError("prepared mutation workload digest changed")
        if driver_data["workload_fingerprint"] != fingerprint:
            raise McpToolError("prepared mutation workload fingerprint changed")
        baseline = _prepared_value(prepared, "baseline")
        desired = _prepared_value(prepared, "desired")
        if baseline == desired:
            raise McpToolError("prepared mutation baseline and desired states are identical")
        return prepared

    def _receipt(self, operation_id: str, state: str) -> dict[str, Any]:
        return {
            "operation_id": operation_id,
            "state": state,
            "observed": {"value": read_weight(self.scope.redis)},
            "driver_data": {},
        }

    def _completed_receipt(
        self,
        operation_id: str,
        record: dict[str, Any],
    ) -> dict[str, Any] | None:
        if record["phase"] != "completed":
            return None
        expected = record["baseline"] if record["kind"] == "restore" else record["desired"]
        if read_weight(self.scope.redis) != expected:
            raise McpToolError(f"completed {record['kind']} operation drifted from physical state")
        return self._receipt(operation_id, record["result_state"])

    def _runtime(self) -> tuple[dict[str, Any], str]:
        runtime = self._runtime_static()
        return runtime, validate_runtime_identity(runtime, self.scope)

    def _runtime_static(self) -> dict[str, Any]:
        return read_json(self.runtime_path)

    def _loadgen_state(self, runtime: dict[str, Any]) -> dict[str, Any]:
        state = read_json(self.loadgen_state_path)
        if state.get("version") != 1 or state.get("run_id") != runtime.get("run_id"):
            raise McpToolError("loadgen state does not match the runtime identity")
        next_sequence = state.get("next_sequence")
        samples = state.get("samples")
        if (
            isinstance(next_sequence, bool)
            or not isinstance(next_sequence, int)
            or next_sequence < 1
            or not isinstance(samples, list)
            or len(samples) > 32
        ):
            raise McpToolError("loadgen state has an invalid sequence ring")
        return state

    def _journal(self, tool: str, status: str, details: dict[str, Any]) -> None:
        self.journal_path.parent.mkdir(parents=True, exist_ok=True)
        record = {
            "observed_at_ns": time.time_ns(),
            "tool": tool,
            "status": status,
            "details": details,
        }
        with self.journal_path.open("a", encoding="utf-8") as output:
            output.write(json.dumps(record, sort_keys=True) + "\n")

    def _manifest(self) -> dict[str, Any]:
        return {
            "schema_version": 1,
            "provider": {"id": "redis-cpu", "version": PROVIDER_VERSION},
            "capabilities": [
                _capability(
                    "redis.snapshot.v1",
                    "probe",
                    "read_only",
                    ["clean", "experimenting"],
                    _probe_schema(),
                    {"probe": "probe.snapshot"},
                    "Observe Redis latency, throughput, CPU service, batch service, "
                    "and pressure without recommending a policy",
                ),
                _capability(
                    "redis.window.v1",
                    "measurement",
                    "read_only",
                    ["commit_pending"],
                    _measurement_schema(),
                    {
                        "validate": "measurement.validate",
                        "open": "measurement.open",
                        "sample": "measurement.sample",
                        "close": "measurement.close",
                    },
                    "Measure fresh complete Redis and competing batch samples from the immutable rolling workload",
                ),
                _capability(
                    "redis.target-cpu-weight.v1",
                    "mutation",
                    "reversible_mutation",
                    ["clean", "experimenting"],
                    _mutation_schema(),
                    {
                        "prepare": "mutation.prepare",
                        "apply": "mutation.apply",
                        "status": "mutation.status",
                        "verify": "mutation.verify",
                        "restore": "mutation.restore",
                        "finalize": "mutation.finalize",
                    },
                    "Set only the bound Redis cgroup cpu.weight to any Linux-valid integer from 1 through 10000. "
                    "It may be called repeatedly in one episode to try another value; each verified call supersedes the prior value",
                    idempotent=True,
                ),
            ],
        }

    @staticmethod
    def _error(request: dict[str, Any], code: int, message: str) -> dict[str, Any]:
        return {"jsonrpc": "2.0", "id": request["id"], "error": {"code": code, "message": message}}


class OperationStore:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.path.parent.mkdir(parents=True, exist_ok=True)

    def get(self, operation_id: str) -> dict[str, Any] | None:
        return self._load()["operations"].get(operation_id)

    def begin(
        self,
        operation_id: str,
        kind: str,
        baseline: int,
        desired: int,
        prepared: dict[str, Any],
    ) -> dict[str, Any]:
        if kind not in {"apply", "restore", "finalize"}:
            raise McpToolError(f"invalid mutation operation kind: {kind!r}")
        data = self._load()
        expected = {
            "kind": kind,
            "phase": "started",
            "result_state": None,
            "baseline": baseline,
            "desired": desired,
            "prepared": prepared,
        }
        existing = data["operations"].get(operation_id)
        if existing is not None:
            for field in ("kind", "baseline", "desired", "prepared"):
                if existing.get(field) != expected[field]:
                    raise McpToolError(
                        f"operation_id {operation_id!r} was reused with different {field}"
                    )
            return existing
        data["operations"][operation_id] = expected
        self._save(data)
        return expected

    def complete(self, operation_id: str, result_state: str) -> None:
        if result_state not in {"applied", "restored", "finalized"}:
            raise McpToolError(f"invalid completed mutation state: {result_state!r}")
        data = self._load()
        record = data["operations"].get(operation_id)
        if record is None:
            raise McpToolError(f"operation_id {operation_id!r} was not started")
        if record["phase"] == "completed":
            if record["result_state"] != result_state:
                raise McpToolError(f"operation_id {operation_id!r} completed differently")
            return
        record["phase"] = "completed"
        record["result_state"] = result_state
        self._save(data)

    def _save(self, data: dict[str, Any]) -> None:
        write_json_atomic(self.path, data)

    def _load(self) -> dict[str, Any]:
        if not self.path.exists():
            return {"version": 2, "operations": {}}
        data = read_json(self.path)
        if data.get("version") != 2 or not isinstance(data.get("operations"), dict):
            raise McpToolError("Redis CPU operation store is invalid")
        return data


class McpToolError(RuntimeError):
    pass


def _capability(
    capability_id: str,
    kind: str,
    effect: str,
    phases: list[str],
    input_schema: dict[str, Any],
    operations: dict[str, str],
    description: str,
    *,
    idempotent: bool = False,
) -> dict[str, Any]:
    return {
        "id": capability_id,
        "kind": kind,
        "effect": effect,
        "description": description,
        "input_schema": input_schema,
        "output_schema": {"type": "object", "additionalProperties": True},
        "allowed_phases": phases,
        "limits": {
            "timeout_ms": 20_000 if kind == "measurement" else 5_000,
            "max_output_bytes": 65_536,
        },
        "deterministic": False,
        "idempotent": idempotent,
        "operations": operations,
    }


def _probe_schema() -> dict[str, Any]:
    return {
        "type": "object",
        "additionalProperties": False,
        "properties": {
            "recent_samples": {"type": "integer", "minimum": 1, "maximum": 8}
        },
    }


def _measurement_schema() -> dict[str, Any]:
    return {
        "type": "object",
        "additionalProperties": False,
        "required": ["max_sample_age_ms", "wait_timeout_ms"],
        "properties": {
            "max_sample_age_ms": {"type": "integer", "minimum": 1_000, "maximum": 30_000},
            "wait_timeout_ms": {"type": "integer", "minimum": 500, "maximum": 15_000},
        },
    }


def _mutation_schema() -> dict[str, Any]:
    return {
        "type": "object",
        "additionalProperties": False,
        "required": ["value"],
        "properties": {
            "value": {"type": "integer", "minimum": MIN_CPU_WEIGHT, "maximum": MAX_CPU_WEIGHT}
        },
    }


def _measurement_spec(value: Any) -> dict[str, int]:
    specification = _object(value, "measurement specification")
    if set(specification) != {"max_sample_age_ms", "wait_timeout_ms"}:
        raise McpToolError(
            "measurement specification requires max_sample_age_ms and wait_timeout_ms"
        )
    return {
        "max_sample_age_ms": _bounded_int(
            specification["max_sample_age_ms"], 1_000, 30_000, "max_sample_age_ms"
        ),
        "wait_timeout_ms": _bounded_int(
            specification["wait_timeout_ms"], 500, 15_000, "wait_timeout_ms"
        ),
    }


def _metric_batch(
    sample: dict[str, Any],
    metrics: dict[str, Any],
    *,
    quality: str,
    fingerprint: str,
    provenance: dict[str, Any],
) -> dict[str, Any]:
    return {
        "started_at_ns": sample["started_at_ns"],
        "ended_at_ns": sample["ended_at_ns"],
        "quality": quality,
        "workload_fingerprint": fingerprint,
        "metrics": {
            name: {"value": value, "unit": _metric_unit(name), "kind": "gauge"}
            for name, value in metrics.items()
        },
        "provenance": provenance,
    }


def _metric_unit(name: str) -> str:
    if name.endswith("_latency_us"):
        return "us"
    if name == "redis_qps":
        return "ops/s"
    if name.endswith("_rate"):
        return "cpu"
    if name.endswith("_pct"):
        return "percent"
    if name.endswith("_weight"):
        return "weight"
    raise McpToolError(f"metric {name!r} has no declared unit")


def _prepared_value(prepared: dict[str, Any], name: str) -> int:
    value = _object(prepared.get(name), f"prepared {name}").get("value")
    return _bounded_int(value, MIN_CPU_WEIGHT, MAX_CPU_WEIGHT, f"prepared {name} value")


def _require_known_state(observed: int, baseline: int, desired: int, operation: str) -> None:
    if observed not in {baseline, desired}:
        raise McpToolError(
            f"cpu.weight drifted before {operation}: observed {observed}, "
            f"expected baseline {baseline} or desired {desired}"
        )


def _object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise McpToolError(f"{label} must be an object")
    return value


def _required_text(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value or value.strip() != value:
        raise McpToolError(f"{label} must be a non-empty string")
    return value


def _bounded_int(value: Any, minimum: int, maximum: int, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise McpToolError(f"{label} must be an integer between {minimum} and {maximum}")
    return value


if __name__ == "__main__":
    raise SystemExit(main())
