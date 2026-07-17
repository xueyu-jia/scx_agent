#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path
from typing import Any


PROTOCOL_VERSION = "2024-11-05"
CAPABILITY_URI = "tuning://capabilities/v1"
SERVER_ID = "deterministic-test"
STATE_DEFAULT = "/tmp/scx-deterministic-tuning-state.json"


def main() -> int:
    server = DeterministicMcp()
    for line in sys.stdin.buffer:
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


class DeterministicMcp:
    def __init__(self) -> None:
        self.scenario = os.environ.get("SCX_DETERMINISTIC_SCENARIO", "positive")
        self.state_path = Path(os.environ.get("SCX_DETERMINISTIC_STATE_PATH", STATE_DEFAULT))
        self.state_path.parent.mkdir(parents=True, exist_ok=True)
        if not self.state_path.exists():
            self._write_state("old")

    def handle(self, request: dict[str, Any]) -> dict[str, Any] | None:
        method = request.get("method")
        if "id" not in request:
            return None
        try:
            if method == "initialize":
                result = {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "deterministic-tuning-mcp", "version": "1"},
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
        except McpToolError as exc:
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
                for name in [
                    "probe.observe",
                    "measurement.validate",
                    "measurement.open",
                    "measurement.sample",
                    "measurement.close",
                    "comparison.validate",
                    "comparison.compare",
                    "mutation.prepare",
                    "mutation.apply",
                    "mutation.status",
                    "mutation.verify",
                    "mutation.restore",
                    "mutation.finalize",
                ]
            ]
        }

    def _tools_call(self, params: dict[str, Any]) -> dict[str, Any]:
        name = params.get("name")
        arguments = params.get("arguments", {})
        handlers = {
            "probe.observe": self._probe,
            "measurement.validate": self._validate,
            "measurement.open": self._measurement_open,
            "measurement.sample": self._measurement_sample,
            "measurement.close": self._measurement_close,
            "comparison.validate": self._validate,
            "comparison.compare": self._compare,
            "mutation.prepare": self._mutation_prepare,
            "mutation.apply": self._mutation_apply,
            "mutation.status": self._mutation_status,
            "mutation.verify": self._mutation_verify,
            "mutation.restore": self._mutation_restore,
            "mutation.finalize": self._mutation_finalize,
        }
        if name not in handlers:
            raise McpToolError(f"unknown tool: {name}")
        structured = handlers[name](arguments)
        return {"structuredContent": structured}

    def _probe(self, _arguments: dict[str, Any]) -> dict[str, Any]:
        return {
            "observed_at_ns": time.time_ns(),
            "data": {"scenario": self.scenario, "value": self._read_state()},
            "warnings": [],
        }

    def _validate(self, _arguments: dict[str, Any]) -> dict[str, Any]:
        return {"valid": True}

    def _measurement_open(self, arguments: dict[str, Any]) -> dict[str, Any]:
        operation = arguments.get("context", {}).get("operation_id", "unknown")
        return {"id": f"session-{operation}", "driver_data": {}}

    def _measurement_sample(self, _arguments: dict[str, Any]) -> dict[str, Any]:
        value = self._read_state()
        throughput = 100.0
        if value == "new" and self.scenario in {"positive", "unsafe"}:
            throughput = 120.0
        return {
            "started_at_ns": time.time_ns(),
            "ended_at_ns": time.time_ns() + 1,
            "quality": "valid",
            "workload_fingerprint": "deterministic-workload",
            "metrics": {
                "throughput": _metric(throughput, "ops/s"),
                "psi.cpu.full.avg10": _metric(1.0, "percent"),
                "psi.io.full.avg10": _metric(1.0, "percent"),
                "psi.memory.full.avg10": _metric(1.0, "percent"),
                "loadavg.1m": _metric(1.0, "load"),
            },
            "provenance": {"provider": "deterministic-mcp", "scenario": self.scenario},
        }

    def _measurement_close(self, arguments: dict[str, Any]) -> dict[str, Any]:
        session_id = arguments.get("id", "unknown")
        return {"session_id": session_id, "cleaned_up": True, "details": {}}

    def _compare(self, arguments: dict[str, Any]) -> dict[str, Any]:
        policy = arguments.get("specification", {}).get("policy", "primary")
        conclusion = "improved"
        if policy == "primary" and self.scenario in {"no_signal", "recovery"}:
            conclusion = "not_improved"
        if policy == "regression" and self.scenario == "unsafe":
            conclusion = "not_improved"
        return {
            "conclusion": conclusion,
            "conditions": [{"name": policy, "passed": conclusion == "improved"}],
            "details": {"scenario": self.scenario, "policy": policy},
        }

    def _mutation_prepare(self, arguments: dict[str, Any]) -> dict[str, Any]:
        desired = arguments.get("arguments", {}).get("value", "new")
        return {
            "resource": "deterministic/knob",
            "baseline": {"value": self._read_state()},
            "desired": {"value": desired},
            "driver_data": {},
        }

    def _mutation_apply(self, arguments: dict[str, Any]) -> dict[str, Any]:
        desired = arguments["prepared"]["desired"]["value"]
        self._write_state(desired)
        return self._mutation_receipt(arguments, "applied", desired)

    def _mutation_status(self, arguments: dict[str, Any]) -> dict[str, Any]:
        return self._mutation_receipt(arguments, "unknown", self._read_state())

    def _mutation_verify(self, arguments: dict[str, Any]) -> dict[str, Any]:
        expected = arguments["expected"]["value"]
        observed = self._read_state()
        return {
            "matched": observed == expected,
            "observed": {"value": observed},
            "details": {},
        }

    def _mutation_restore(self, arguments: dict[str, Any]) -> dict[str, Any]:
        if self.scenario == "recovery":
            raise McpToolError("injected deterministic restore failure")
        baseline = arguments["prepared"]["baseline"]["value"]
        self._write_state(baseline)
        return self._mutation_receipt(arguments, "restored", baseline)

    def _mutation_finalize(self, arguments: dict[str, Any]) -> dict[str, Any]:
        desired = arguments["prepared"]["desired"]["value"]
        return self._mutation_receipt(arguments, "finalized", desired)

    def _mutation_receipt(
        self,
        arguments: dict[str, Any],
        state: str,
        observed: Any,
    ) -> dict[str, Any]:
        return {
            "operation_id": arguments["operation_id"],
            "state": state,
            "observed": {"value": observed},
            "driver_data": {},
        }

    def _manifest(self) -> dict[str, Any]:
        return {
            "schema_version": 1,
            "provider": {"id": "deterministic", "version": "1"},
            "capabilities": [
                _capability(
                    "probe",
                    "probe",
                    "read_only",
                    ["clean", "experimenting"],
                    {"probe": "probe.observe"},
                ),
                _capability(
                    "measurement",
                    "measurement",
                    "read_only",
                    ["commit_pending"],
                    {
                        "validate": "measurement.validate",
                        "open": "measurement.open",
                        "sample": "measurement.sample",
                        "close": "measurement.close",
                    },
                ),
                _capability(
                    "comparison",
                    "comparison",
                    "pure_computation",
                    ["commit_pending"],
                    {"validate": "comparison.validate", "compare": "comparison.compare"},
                    deterministic=True,
                ),
                _capability(
                    "knob",
                    "mutation",
                    "reversible_mutation",
                    ["clean", "experimenting"],
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

    def _read_state(self) -> Any:
        return json.loads(self.state_path.read_text(encoding="utf-8"))["value"]

    def _write_state(self, value: Any) -> None:
        tmp = self.state_path.with_suffix(".tmp")
        tmp.write_text(json.dumps({"value": value}) + "\n", encoding="utf-8")
        os.replace(tmp, self.state_path)

    def _error(self, request: dict[str, Any], code: int, message: str) -> dict[str, Any]:
        return {"jsonrpc": "2.0", "id": request["id"], "error": {"code": code, "message": message}}


class McpToolError(RuntimeError):
    pass


def _capability(
    capability_id: str,
    kind: str,
    effect: str,
    phases: list[str],
    operations: dict[str, str],
    *,
    deterministic: bool = False,
    idempotent: bool = False,
) -> dict[str, Any]:
    return {
        "id": capability_id,
        "kind": kind,
        "effect": effect,
        "description": f"deterministic test {kind}",
        "input_schema": {"type": "object", "additionalProperties": True},
        "output_schema": {"type": "object", "additionalProperties": True},
        "allowed_phases": phases,
        "limits": {"timeout_ms": 10000, "max_output_bytes": 65536},
        "deterministic": deterministic,
        "idempotent": idempotent,
        "operations": operations,
    }


def _metric(value: float, unit: str) -> dict[str, Any]:
    return {"value": value, "unit": unit, "kind": "gauge"}


if __name__ == "__main__":
    raise SystemExit(main())
