#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path
from typing import Any, Callable


sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path.cwd()))
from common import (  # noqa: E402
    CgroupCpuError,
    CgroupCpuScope,
    read_cpu_stat,
    read_members,
    read_weight,
    require_scope,
    sample_cpu_service,
    scope_state,
    write_weight,
)


PROTOCOL_VERSION = "2024-11-05"
CAPABILITY_URI = "tuning://capabilities/v1"
PROVIDER_VERSION = "1.1.0"
ALLOWED_WEIGHTS = [25, 50, 100, 200, 10_000]
BALANCE_POLICY = "target_share_slo_v1"


def main() -> int:
    server = CgroupCpuMcp()
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


class CgroupCpuMcp:
    def __init__(self) -> None:
        root = os.environ.get("SCX_CGROUP_CPU_ROOT", "/sys/fs/cgroup/scx-bench")
        self.scope = CgroupCpuScope.from_root(root)
        self.scenario = os.environ.get("SCX_CGROUP_CPU_SCENARIO", "positive")
        state_path = Path(
            os.environ.get(
                "SCX_CGROUP_CPU_STATE_PATH",
                "/tmp/scx-cgroup-cpu-mcp-state.json",
            )
        )
        self.operations = OperationStore(state_path)

    def handle(self, request: dict[str, Any]) -> dict[str, Any] | None:
        method = request.get("method")
        if "id" not in request:
            return None
        try:
            if method == "initialize":
                result = {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "cgroup-cpu-tuning-mcp", "version": PROVIDER_VERSION},
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
        except (CgroupCpuError, McpToolError, OSError, ValueError) as exc:
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
        return {"structuredContent": handler(arguments)}

    def _handlers(self) -> dict[str, Callable[[dict[str, Any]], dict[str, Any]]]:
        return {
            "probe.snapshot": self._probe,
            "measurement.validate": self._measurement_validate,
            "measurement.open": self._measurement_open,
            "measurement.sample": self._measurement_sample,
            "measurement.close": self._measurement_close,
            "comparison.validate": self._comparison_validate,
            "comparison.compare": self._compare,
            "mutation.prepare": self._mutation_prepare,
            "mutation.apply": self._mutation_apply,
            "mutation.status": self._mutation_status,
            "mutation.verify": self._mutation_verify,
            "mutation.restore": self._mutation_restore,
            "mutation.finalize": self._mutation_finalize,
        }

    def _probe(self, request: dict[str, Any]) -> dict[str, Any]:
        arguments = _object(request.get("arguments", {}), "probe arguments")
        window_ms = _bounded_int(arguments.get("window_ms", 250), 100, 2_000, "window_ms")
        sample = sample_cpu_service(self.scope, window_ms=window_ms)
        return {
            "observed_at_ns": time.time_ns(),
            "data": {
                "scope": scope_state(self.scope),
                "service": sample["metrics"],
                "sample": sample["details"],
            },
            "warnings": [],
        }

    def _measurement_validate(self, request: dict[str, Any]) -> dict[str, Any]:
        try:
            _measurement_spec(request.get("specification"))
        except McpToolError as exc:
            return {"valid": False, "message": str(exc)}
        return {"valid": True}

    def _measurement_open(self, request: dict[str, Any]) -> dict[str, Any]:
        specification = _measurement_spec(request.get("specification"))
        require_scope(self.scope)
        operation = request.get("context", {}).get("operation_id", "unknown")
        return {
            "id": f"cgroup-cpu-{operation}",
            "driver_data": {
                "window_ms": specification["window_ms"],
                "target_inode": self.scope.target.stat().st_ino,
                "neighbor_inode": self.scope.neighbor.stat().st_ino,
            },
        }

    def _measurement_sample(self, request: dict[str, Any]) -> dict[str, Any]:
        session = _object(request.get("session"), "measurement session")
        driver_data = _object(session.get("driver_data"), "measurement driver_data")
        self._verify_inodes(driver_data)
        sample = sample_cpu_service(
            self.scope,
            window_ms=_bounded_int(driver_data.get("window_ms"), 100, 5_000, "window_ms"),
        )
        return {
            "started_at_ns": sample["started_at_ns"],
            "ended_at_ns": sample["ended_at_ns"],
            "quality": "valid",
            "workload_fingerprint": sample["workload_fingerprint"],
            "metrics": {
                name: _metric(value, _metric_unit(name))
                for name, value in sample["metrics"].items()
            },
            "provenance": {
                "provider": "cgroup-cpu-mcp",
                "version": PROVIDER_VERSION,
                **sample["details"],
            },
        }

    def _measurement_close(self, request: dict[str, Any]) -> dict[str, Any]:
        session_id = request.get("id")
        if not isinstance(session_id, str) or not session_id:
            raise McpToolError("measurement close requires a session id")
        return {"session_id": session_id, "cleaned_up": True, "details": {}}

    def _comparison_validate(self, request: dict[str, Any]) -> dict[str, Any]:
        try:
            _comparison_spec(request.get("specification"))
        except McpToolError as exc:
            return {"valid": False, "message": str(exc)}
        return {"valid": True}

    def _compare(self, request: dict[str, Any]) -> dict[str, Any]:
        _comparison_spec(request.get("specification"))
        baseline = _metric_values(request.get("baseline"), "baseline")
        candidate = _metric_values(request.get("candidate"), "candidate")
        target_before = _required_metric(baseline, "target_cpu_share_pct")
        target_after = _required_metric(candidate, "target_cpu_share_pct")
        neighbor_after = _required_metric(candidate, "neighbor_cpu_share_pct")
        rate_before = _required_metric(baseline, "aggregate_cpu_rate")
        rate_after = _required_metric(candidate, "aggregate_cpu_rate")
        rate_drop_pct = (
            max(rate_before - rate_after, 0.0) / abs(rate_before) * 100.0
            if rate_before != 0
            else float("inf")
        )
        checks = [
            _condition("target_share_floor", target_after >= 40.0, target_after, 40.0),
            _condition(
                "target_share_gain",
                target_after - target_before >= 20.0,
                target_after - target_before,
                20.0,
            ),
            _condition("neighbor_share_floor", neighbor_after >= 35.0, neighbor_after, 35.0),
            _condition("aggregate_rate_drop", rate_drop_pct <= 5.0, rate_drop_pct, 5.0),
        ]
        improved = all(item["passed"] for item in checks)
        return {
            "conclusion": "improved" if improved else "not_improved",
            "conditions": checks,
            "details": {
                "policy": BALANCE_POLICY,
                "target_share_before": target_before,
                "target_share_after": target_after,
                "neighbor_share_after": neighbor_after,
                "aggregate_rate_drop_pct": rate_drop_pct,
            },
        }

    def _mutation_prepare(self, request: dict[str, Any]) -> dict[str, Any]:
        context = _object(request.get("context"), "mutation context")
        _required_text(context.get("operation_id"), "operation_id")
        arguments = _object(request.get("arguments"), "mutation arguments")
        desired = _bounded_int(arguments.get("value"), 1, 10_000, "value")
        if desired not in ALLOWED_WEIGHTS:
            raise McpToolError(f"cpu.weight must be one of {ALLOWED_WEIGHTS}")
        require_scope(self.scope)
        inode = self.scope.target.stat().st_ino
        return {
            "resource": f"cgroup:{inode}/cpu.weight",
            "baseline": {"value": read_weight(self.scope.target)},
            "desired": {"value": desired},
            "driver_data": {
                "target": str(self.scope.target),
                "target_inode": inode,
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

        observed = read_weight(self.scope.target)
        _require_known_state(observed, baseline, desired, "apply")
        if observed != desired:
            write_weight(self.scope.target, desired)
        if read_weight(self.scope.target) != desired:
            raise McpToolError("cpu.weight apply readback did not match the desired state")
        self.operations.complete(operation_id, "applied")
        return self._receipt(operation_id, "applied")

    def _mutation_status(self, request: dict[str, Any]) -> dict[str, Any]:
        operation_id = _required_text(request.get("operation_id"), "operation_id")
        record = self.operations.get(operation_id)
        if record is None:
            return {"operation_id": operation_id, "state": "unknown", "driver_data": {}}
        observed = read_weight(self.scope.target)
        if record["phase"] == "completed":
            state = record["result_state"]
        elif record["kind"] == "apply" and observed == record["desired"]:
            state = "applied"
        elif record["kind"] == "apply" and observed == record["baseline"]:
            state = "not_applied"
        elif record["kind"] == "restore" and observed == record["baseline"]:
            state = "restored"
        elif record["kind"] == "restore" and observed == record["desired"]:
            state = "applied"
        else:
            state = "unknown"
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
        expected_value = _bounded_int(expected.get("value"), 1, 10_000, "expected value")
        observed = read_weight(self.scope.target)
        return {
            "matched": observed == expected_value,
            "observed": {"value": observed},
            "details": {},
        }

    def _mutation_restore(self, request: dict[str, Any]) -> dict[str, Any]:
        operation_id = _required_text(request.get("operation_id"), "operation_id")
        prepared = self._prepared(request.get("prepared"))
        baseline = _prepared_value(prepared, "baseline")
        desired = _prepared_value(prepared, "desired")
        record = self.operations.begin(operation_id, "restore", baseline, desired, prepared)
        completed = self._completed_receipt(operation_id, record)
        if completed is not None:
            return completed

        observed = read_weight(self.scope.target)
        _require_known_state(observed, baseline, desired, "restore")
        if self.scenario == "recovery":
            raise McpToolError("injected cgroup cpu.weight restore failure")
        if observed != baseline:
            write_weight(self.scope.target, baseline)
        if read_weight(self.scope.target) != baseline:
            raise McpToolError("cpu.weight restore readback did not match the baseline state")
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
        if read_weight(self.scope.target) != desired:
            raise McpToolError("cannot finalize cpu.weight because desired state is not active")
        self.operations.complete(operation_id, "finalized")
        return self._receipt(operation_id, "finalized")

    def _prepared(self, value: Any) -> dict[str, Any]:
        prepared = _object(value, "prepared mutation")
        driver_data = _object(prepared.get("driver_data"), "prepared driver_data")
        if set(driver_data) != {"target", "target_inode", "provider_version"}:
            raise McpToolError("prepared mutation driver_data has an invalid schema")
        if driver_data.get("provider_version") != PROVIDER_VERSION:
            raise McpToolError("prepared mutation provider version does not match")
        if driver_data.get("target") != str(self.scope.target):
            raise McpToolError("prepared mutation target does not match the configured cgroup")
        self._verify_inodes(driver_data)
        baseline = _prepared_value(prepared, "baseline")
        desired = _prepared_value(prepared, "desired")
        if desired not in ALLOWED_WEIGHTS:
            raise McpToolError(f"prepared cpu.weight must be one of {ALLOWED_WEIGHTS}")
        if baseline == desired:
            raise McpToolError("prepared mutation baseline and desired states are identical")
        return prepared

    def _verify_inodes(self, driver_data: dict[str, Any]) -> None:
        require_scope(self.scope)
        expected_target = driver_data.get("target_inode")
        if expected_target != self.scope.target.stat().st_ino:
            raise McpToolError("target cgroup inode changed")
        expected_neighbor = driver_data.get("neighbor_inode")
        if expected_neighbor is not None and expected_neighbor != self.scope.neighbor.stat().st_ino:
            raise McpToolError("neighbor cgroup inode changed")

    def _receipt(self, operation_id: str, state: str) -> dict[str, Any]:
        return {
            "operation_id": operation_id,
            "state": state,
            "observed": {"value": read_weight(self.scope.target)},
            "driver_data": {},
        }

    def _completed_receipt(
        self,
        operation_id: str,
        record: dict[str, Any],
    ) -> dict[str, Any] | None:
        if record["phase"] != "completed":
            return None
        return self._receipt(operation_id, record["result_state"])

    def _manifest(self) -> dict[str, Any]:
        return {
            "schema_version": 1,
            "provider": {"id": "cgroup-cpu", "version": PROVIDER_VERSION},
            "capabilities": [
                _capability(
                    "cpu.snapshot",
                    "probe",
                    "read_only",
                    ["clean", "experimenting"],
                    _probe_schema(),
                    {"probe": "probe.snapshot"},
                ),
                _capability(
                    "cpu.service",
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
                ),
                _capability(
                    "cpu.balance-slo",
                    "comparison",
                    "pure_computation",
                    ["commit_pending"],
                    _comparison_schema(),
                    {"validate": "comparison.validate", "compare": "comparison.compare"},
                    deterministic=True,
                ),
                _capability(
                    "cpu.weight",
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
        if record.get("phase") == "completed":
            if record.get("result_state") != result_state:
                raise McpToolError(
                    f"operation_id {operation_id!r} completed with a different state"
                )
            return
        record["phase"] = "completed"
        record["result_state"] = result_state
        self._save(data)

    def _save(self, data: dict[str, Any]) -> None:
        temporary = self.path.with_suffix(self.path.suffix + ".tmp")
        temporary.write_text(json.dumps(data, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(temporary, self.path)

    def _load(self) -> dict[str, Any]:
        if not self.path.exists():
            return {"version": 2, "operations": {}}
        data = json.loads(self.path.read_text(encoding="utf-8"))
        if not isinstance(data, dict) or data.get("version") != 2:
            raise McpToolError("cgroup CPU operation store is invalid")
        operations = data.get("operations")
        if not isinstance(operations, dict):
            raise McpToolError("cgroup CPU operation store has invalid operations")
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
    *,
    deterministic: bool = False,
    idempotent: bool = False,
) -> dict[str, Any]:
    timeout_ms = 7_000 if kind == "measurement" else 5_000
    return {
        "id": capability_id,
        "kind": kind,
        "effect": effect,
        "description": {
            "probe": "Observe bounded CPU service for the configured cgroup pair",
            "measurement": "Measure CPU service share for the configured live workload",
            "comparison": "Require balanced target service without aggregate CPU regression",
            "mutation": "Set the configured target cgroup cpu.weight from a bounded allowlist",
        }[kind],
        "input_schema": input_schema,
        "output_schema": {"type": "object", "additionalProperties": True},
        "allowed_phases": phases,
        "limits": {"timeout_ms": timeout_ms, "max_output_bytes": 65_536},
        "deterministic": deterministic,
        "idempotent": idempotent,
        "operations": operations,
    }


def _probe_schema() -> dict[str, Any]:
    return {
        "type": "object",
        "additionalProperties": False,
        "properties": {"window_ms": {"type": "integer", "minimum": 100, "maximum": 2_000}},
    }


def _measurement_schema() -> dict[str, Any]:
    return {
        "type": "object",
        "additionalProperties": False,
        "required": ["window_ms"],
        "properties": {"window_ms": {"type": "integer", "minimum": 100, "maximum": 5_000}},
    }


def _comparison_schema() -> dict[str, Any]:
    return {
        "type": "object",
        "additionalProperties": False,
        "required": ["policy"],
        "properties": {"policy": {"const": BALANCE_POLICY}},
    }


def _mutation_schema() -> dict[str, Any]:
    return {
        "type": "object",
        "additionalProperties": False,
        "required": ["value"],
        "properties": {"value": {"type": "integer", "enum": ALLOWED_WEIGHTS}},
    }


def _measurement_spec(value: Any) -> dict[str, int]:
    specification = _object(value, "measurement specification")
    if set(specification) != {"window_ms"}:
        raise McpToolError("measurement specification requires only window_ms")
    return {"window_ms": _bounded_int(specification["window_ms"], 100, 5_000, "window_ms")}


def _comparison_spec(value: Any) -> dict[str, str]:
    specification = _object(value, "comparison specification")
    if specification != {"policy": BALANCE_POLICY}:
        raise McpToolError(f"comparison policy must be {BALANCE_POLICY!r}")
    return {"policy": BALANCE_POLICY}


def _metric_values(value: Any, label: str) -> dict[str, float]:
    batch = _object(value, f"{label} measurement")
    metrics = _object(batch.get("metrics"), f"{label} metrics")
    values: dict[str, float] = {}
    for name, metric in metrics.items():
        item = _object(metric, f"{label} metric {name}")
        number = item.get("value")
        if isinstance(number, bool) or not isinstance(number, (int, float)):
            continue
        values[name] = float(number)
    return values


def _required_metric(metrics: dict[str, float], name: str) -> float:
    if name not in metrics:
        raise McpToolError(f"comparison requires metric {name!r}")
    return metrics[name]


def _prepared_value(prepared: dict[str, Any], name: str) -> int:
    value = _object(prepared.get(name), f"prepared {name}").get("value")
    return _bounded_int(value, 1, 10_000, f"prepared {name} value")


def _require_known_state(observed: int, baseline: int, desired: int, operation: str) -> None:
    if observed not in {baseline, desired}:
        raise McpToolError(
            f"cpu.weight drifted before {operation}: observed {observed}, "
            f"expected baseline {baseline} or desired {desired}"
        )


def _condition(name: str, passed: bool, observed: float, threshold: float) -> dict[str, Any]:
    return {
        "name": name,
        "passed": passed,
        "details": {"observed": observed, "threshold": threshold},
    }


def _metric(value: float, unit: str) -> dict[str, Any]:
    return {"value": value, "unit": unit, "kind": "gauge"}


def _metric_unit(name: str) -> str:
    if name.endswith("_pct"):
        return "percent"
    if name.endswith("_rate"):
        return "cpu"
    if name.endswith("_weight"):
        return "weight"
    return "value"


def _object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise McpToolError(f"{label} must be an object")
    return value


def _bounded_int(value: Any, minimum: int, maximum: int, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise McpToolError(f"{label} must be an integer")
    if not minimum <= value <= maximum:
        raise McpToolError(f"{label} must be between {minimum} and {maximum}")
    return value


def _required_text(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value:
        raise McpToolError(f"{label} must be a non-empty string")
    return value


if __name__ == "__main__":
    raise SystemExit(main())
