#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


MEASUREMENT_ID = "mcp/cgroup-cpu/cpu.service"
COMPARISON_ID = "mcp/cgroup-cpu/cpu.balance-slo"


def main() -> int:
    parser = argparse.ArgumentParser(description="Deterministic LLM for the real cgroup CPU MCP")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=18081)
    parser.add_argument(
        "--scenario",
        choices=("positive", "no_signal", "unsafe", "recovery"),
        default="positive",
    )
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), CgroupCpuHandler)
    server.scenario = args.scenario  # type: ignore[attr-defined]
    server.serve_forever()
    return 0


class CgroupCpuHandler(BaseHTTPRequestHandler):
    server_version = "CgroupCpuMockLLM/1"

    def do_POST(self) -> None:
        if self.path != "/v1/chat/completions":
            self.send_error(404)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length).decode("utf-8"))
            response = self._completion(body)
        except Exception as exc:
            self.send_response(500)
            self.end_headers()
            self.wfile.write(str(exc).encode("utf-8"))
            return
        payload = json.dumps(response, separators=(",", ":")).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, _format: str, *_args: Any) -> None:
        return

    def _completion(self, body: dict[str, Any]) -> dict[str, Any]:
        messages = body.get("messages", [])
        tools = body.get("tools", [])
        scenario = self.server.scenario  # type: ignore[attr-defined]
        last_tool = _last_tool_result(messages)
        if last_tool is None:
            call = _tool_call(
                "probe",
                _find_capability_tool(tools, "probe", "mcp/cgroup-cpu/cpu.snapshot"),
                {"window_ms": 250},
            )
        elif last_tool["tool_call_id"] == "probe" and scenario == "no_signal":
            call = _tool_call(
                "abort",
                "abort",
                {"reason": "the configured cgroups already have balanced CPU service"},
            )
        elif last_tool["tool_call_id"] == "probe":
            call = _tool_call("begin", "begin_experiment", _begin_arguments())
        elif last_tool["tool_call_id"] == "begin":
            call = _tool_call(
                "mutate",
                _find_capability_tool(tools, "experiment", "mcp/cgroup-cpu/cpu.weight"),
                {
                    "arguments": {"value": _desired_weight(scenario)},
                    "reason": "bounded cgroup CPU share experiment",
                },
            )
        elif last_tool["tool_call_id"] == "mutate":
            call = _tool_call(
                "commit",
                "request_commit",
                {
                    "change_ids": [_change_id(last_tool)],
                    "reason": "candidate satisfies the frozen cgroup CPU share objective",
                },
            )
        else:
            return _final_response("cgroup CPU episode complete")
        return _tool_response(call)


def _begin_arguments() -> dict[str, Any]:
    return {
        "objective": "restore balanced CPU service for the configured target cgroup",
        "evaluation_contract": {
            "measurement": {
                "capability_id": MEASUREMENT_ID,
                "specification": {"window_ms": 1_000},
            },
            "primary": [
                {
                    "capability_id": COMPARISON_ID,
                    "specification": {"policy": "target_share_slo_v1"},
                }
            ],
            "sampling": {
                "settle_ms": 250,
                "sample_count": 3,
                "sample_interval_ms": 200,
            },
        },
    }


def _desired_weight(scenario: str) -> int:
    return {"positive": 100, "unsafe": 10_000, "recovery": 100}[scenario]


def _last_tool_result(messages: list[dict[str, Any]]) -> dict[str, Any] | None:
    for message in reversed(messages):
        if message.get("role") == "tool":
            return message
    return None


def _change_id(tool_message: dict[str, Any]) -> str:
    content = json.loads(tool_message["content"])
    if not content.get("ok"):
        raise RuntimeError(f"mutation failed: {content}")
    return content["result"]["change"]["change_id"]


def _find_capability_tool(tools: list[dict[str, Any]], prefix: str, capability_id: str) -> str:
    for tool in tools:
        function = tool.get("function", {})
        description = function.get("description")
        name = function.get("name")
        if (
            isinstance(name, str)
            and name.startswith(f"{prefix}_")
            and isinstance(description, str)
            and f"capability_id={capability_id}" in description
        ):
            return name
    raise RuntimeError(f"tool for capability {capability_id!r} not found")


def _tool_call(call_id: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments, separators=(",", ":"))},
    }


def _tool_response(call: dict[str, Any]) -> dict[str, Any]:
    return {"choices": [{"message": {"role": "assistant", "content": None, "tool_calls": [call]}}]}


def _final_response(content: str) -> dict[str, Any]:
    return {"choices": [{"message": {"role": "assistant", "content": content}}]}


if __name__ == "__main__":
    raise SystemExit(main())
