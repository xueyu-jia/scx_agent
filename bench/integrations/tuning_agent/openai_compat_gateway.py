#!/usr/bin/env python3
from __future__ import annotations

import argparse
from collections.abc import Mapping
import hmac
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import urllib.error
import urllib.request
from typing import Any


DEFAULT_UPSTREAM = "https://api.deepseek.com"
MAX_BODY_BYTES = 2 * 1024 * 1024
SUPPORTED_PATHS = frozenset({"/v1/models", "/v1/chat/completions"})


class GatewayError(RuntimeError):
    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status


class OpenAiCompatGateway(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(
        self,
        address: tuple[str, int],
        *,
        upstream: str,
        upstream_api_key: str,
        proxy_token: str,
        timeout: float,
        strip_v1: bool = False,
    ) -> None:
        super().__init__(address, Handler)
        self.upstream = upstream.rstrip("/")
        self.upstream_api_key = upstream_api_key
        self.proxy_token = proxy_token
        self.timeout = timeout
        self.strip_v1 = strip_v1


class Handler(BaseHTTPRequestHandler):
    server_version = "OpenAICompatGateway/1"
    server: OpenAiCompatGateway

    def do_GET(self) -> None:
        self._handle_request()

    def do_POST(self) -> None:
        self._handle_request()

    def _handle_request(self) -> None:
        try:
            self._authorize()
            if self.path not in SUPPORTED_PATHS:
                raise GatewayError(HTTPStatus.NOT_FOUND, "unknown endpoint")
            if self.command == "GET" and self.path != "/v1/models":
                raise GatewayError(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")
            if self.command == "POST" and self.path != "/v1/chat/completions":
                raise GatewayError(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")
            request_body = self._read_body() if self.command == "POST" else None
            status, headers, response_body = self._forward(request_body)
            self._send_response(status, headers, response_body)
        except GatewayError as error:
            body = json.dumps(
                {"error": {"message": str(error), "type": "openai_gateway_error"}},
                separators=(",", ":"),
            ).encode("utf-8")
            self._send_response(
                error.status,
                {"Content-Type": "application/json"},
                body,
            )
        except Exception as error:
            body = json.dumps(
                {"error": {"message": str(error), "type": "openai_gateway_error"}},
                separators=(",", ":"),
            ).encode("utf-8")
            self._send_response(
                HTTPStatus.INTERNAL_SERVER_ERROR,
                {"Content-Type": "application/json"},
                body,
            )

    def _authorize(self) -> None:
        expected = f"Bearer {self.server.proxy_token}"
        supplied = self.headers.get("Authorization", "")
        if not hmac.compare_digest(supplied, expected):
            raise GatewayError(HTTPStatus.UNAUTHORIZED, "invalid proxy authorization")

    def _read_body(self) -> bytes:
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError as error:
            raise GatewayError(HTTPStatus.BAD_REQUEST, "invalid Content-Length") from error
        if length <= 0 or length > MAX_BODY_BYTES:
            raise GatewayError(HTTPStatus.BAD_REQUEST, "request body size is invalid")
        body = self.rfile.read(length)
        try:
            value = json.loads(body)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise GatewayError(HTTPStatus.BAD_REQUEST, "request body is not valid JSON") from error
        if not isinstance(value, dict):
            raise GatewayError(HTTPStatus.BAD_REQUEST, "request body must be an object")
        return body

    def _forward(self, body: bytes | None) -> tuple[int, Mapping[str, str], bytes]:
        upstream_path = self.path[3:] if self.server.strip_v1 else self.path
        request = urllib.request.Request(
            f"{self.server.upstream}{upstream_path}",
            data=body,
            headers={
                "Authorization": f"Bearer {self.server.upstream_api_key}",
                "Content-Type": "application/json",
            },
            method=self.command,
        )
        try:
            with urllib.request.urlopen(request, timeout=self.server.timeout) as response:
                payload = response.read(MAX_BODY_BYTES + 1)
                status = response.status
                content_type = response.headers.get("Content-Type", "application/json")
        except urllib.error.HTTPError as error:
            payload = error.read(MAX_BODY_BYTES + 1)
            status = error.code
            content_type = error.headers.get("Content-Type", "application/json")
        except urllib.error.URLError as error:
            raise GatewayError(
                HTTPStatus.BAD_GATEWAY,
                f"upstream transport failed: {error.reason}",
            ) from error
        if len(payload) > MAX_BODY_BYTES:
            raise GatewayError(HTTPStatus.BAD_GATEWAY, "upstream response is too large")
        return status, {"Content-Type": content_type}, payload

    def _send_response(
        self,
        status: int,
        headers: Mapping[str, str],
        body: bytes,
    ) -> None:
        self.send_response(status)
        for name, value in headers.items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            return

    def log_message(self, _format: str, *_args: Any) -> None:
        return


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Authenticated gateway for an OpenAI-compatible HTTPS API"
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=17002)
    parser.add_argument(
        "--upstream",
        default=os.environ.get("OPENAI_COMPAT_UPSTREAM", DEFAULT_UPSTREAM),
    )
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument(
        "--strip-v1",
        action="store_true",
        help="forward /v1 routes to upstream routes without the /v1 prefix",
    )
    args = parser.parse_args()

    upstream_api_key = os.environ.get("OPENAI_COMPAT_UPSTREAM_API_KEY", "")
    proxy_token = os.environ.get("OPENAI_COMPAT_PROXY_TOKEN", "local-test")
    if not upstream_api_key:
        parser.error("OPENAI_COMPAT_UPSTREAM_API_KEY is required")
    if not proxy_token:
        parser.error("OPENAI_COMPAT_PROXY_TOKEN must not be empty")
    if not args.upstream.startswith("https://"):
        parser.error("--upstream must be an HTTPS URL")
    if "?" in args.upstream or "#" in args.upstream:
        parser.error("--upstream must not contain a query or fragment")

    server = OpenAiCompatGateway(
        (args.host, args.port),
        upstream=args.upstream,
        upstream_api_key=upstream_api_key,
        proxy_token=proxy_token,
        timeout=args.timeout,
        strip_v1=args.strip_v1,
    )
    server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
