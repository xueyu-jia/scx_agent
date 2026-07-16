#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


MEASUREMENT_ID = "mcp/deterministic-test/measurement"
COMPARISON_ID = "mcp/deterministic-test/comparison"


def main() -> int:
    parser = argparse.ArgumentParser(description="Deterministic OpenAI-compatible test LLM")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=18080)
    args = parser.parse_args()
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.serve_forever()
    return 0


class Handler(BaseHTTPRequestHandler):
    server_version = "MockOpenAILLM/1"

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
        tool_names = [
            tool.get("function", {}).get("name")
            for tool in tools
            if isinstance(tool, dict)
        ]
        last_tool = _last_tool_result(messages)
        if last_tool is None:
            call = _tool_call("begin", "begin_experiment", _begin_arguments())
        elif last_tool["tool_call_id"] == "begin":
            mutation_tool = _find_tool(tool_names, "experiment_")
            call = _tool_call(
                "mutate",
                mutation_tool,
                {"arguments": {"value": "new"}, "reason": "deterministic candidate"},
            )
        elif last_tool["tool_call_id"] == "mutate":
            change_id = _change_id(last_tool)
            call = _tool_call(
                "commit",
                "request_commit",
                {"change_ids": [change_id], "reason": "deterministic candidate is ready"},
            )
        else:
            return _final_response("deterministic episode complete")
        return _tool_response(call)


def _begin_arguments() -> dict[str, Any]:
    return {
        "objective": "increase deterministic throughput",
        "evaluation_contract": {
            "measurement": {"capability_id": MEASUREMENT_ID, "specification": {}},
            "primary": [
                {
                    "capability_id": COMPARISON_ID,
                    "specification": {"policy": "primary"},
                }
            ],
            "regression_guards": [
                {
                    "capability_id": COMPARISON_ID,
                    "specification": {"policy": "regression"},
                }
            ],
            "sampling": {
                "settle_ms": 0,
                "sample_count": 1,
                "sample_interval_ms": 0,
            },
        },
    }


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


def _find_tool(tool_names: list[Any], prefix: str) -> str:
    for name in tool_names:
        if isinstance(name, str) and name.startswith(prefix):
            return name
    raise RuntimeError(f"tool with prefix {prefix!r} not found")


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
