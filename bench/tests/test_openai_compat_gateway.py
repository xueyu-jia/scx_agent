from __future__ import annotations

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading
import unittest
import urllib.error
import urllib.request

from bench.integrations.tuning_agent.openai_compat_gateway import OpenAiCompatGateway


class _UpstreamHandler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        self._respond({"object": "list", "data": [{"id": "deepseek-v4-flash"}]})

    def do_POST(self) -> None:
        length = int(self.headers["Content-Length"])
        self.server.request_path = self.path  # type: ignore[attr-defined]
        self.server.authorization = self.headers.get("Authorization")  # type: ignore[attr-defined]
        self.server.request_body = json.loads(self.rfile.read(length))  # type: ignore[attr-defined]
        self._respond(
            {
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": "probe",
                                        "arguments": "{}",
                                    },
                                }
                            ],
                        }
                    }
                ]
            }
        )

    def _respond(self, value: dict[str, object]) -> None:
        payload = json.dumps(value).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, _format: str, *_args: object) -> None:
        return


class OpenAiCompatGatewayTest(unittest.TestCase):
    def setUp(self) -> None:
        self.upstream = ThreadingHTTPServer(("127.0.0.1", 0), _UpstreamHandler)
        self.upstream_thread = threading.Thread(
            target=self.upstream.serve_forever,
            daemon=True,
        )
        self.upstream_thread.start()
        self.gateway = OpenAiCompatGateway(
            ("127.0.0.1", 0),
            upstream=f"http://127.0.0.1:{self.upstream.server_port}",
            upstream_api_key="upstream-test-key",
            proxy_token="local-test",
            timeout=2.0,
            strip_v1=True,
        )
        self.gateway_thread = threading.Thread(
            target=self.gateway.serve_forever,
            daemon=True,
        )
        self.gateway_thread.start()

    def tearDown(self) -> None:
        self.gateway.shutdown()
        self.gateway.server_close()
        self.upstream.shutdown()
        self.upstream.server_close()
        self.gateway_thread.join(timeout=2.0)
        self.upstream_thread.join(timeout=2.0)

    def test_models_and_chat_are_forwarded_without_protocol_translation(self) -> None:
        models = self._request("/v1/models")
        self.assertEqual(models["data"][0]["id"], "deepseek-v4-flash")

        body = {
            "model": "deepseek-v4-flash",
            "messages": [{"role": "user", "content": "Call probe."}],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "probe",
                        "parameters": {"type": "object", "properties": {}},
                    },
                }
            ],
        }
        completion = self._request("/v1/chat/completions", body)

        self.assertEqual(
            completion["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "probe",
        )
        self.assertEqual(  # type: ignore[attr-defined]
            self.upstream.request_path, "/chat/completions"
        )
        self.assertEqual(  # type: ignore[attr-defined]
            self.upstream.authorization, "Bearer upstream-test-key"
        )
        self.assertEqual(self.upstream.request_body, body)  # type: ignore[attr-defined]

    def test_invalid_local_token_is_rejected_without_contacting_upstream(self) -> None:
        request = urllib.request.Request(
            f"http://127.0.0.1:{self.gateway.server_port}/v1/models",
            headers={"Authorization": "Bearer wrong-token"},
        )
        with self.assertRaises(urllib.error.HTTPError) as raised:
            urllib.request.urlopen(request, timeout=2.0)
        self.assertEqual(raised.exception.code, 401)

    def _request(
        self,
        path: str,
        value: dict[str, object] | None = None,
    ) -> dict[str, object]:
        body = json.dumps(value).encode("utf-8") if value is not None else None
        request = urllib.request.Request(
            f"http://127.0.0.1:{self.gateway.server_port}{path}",
            data=body,
            headers={
                "Authorization": "Bearer local-test",
                "Content-Type": "application/json",
            },
            method="POST" if body is not None else "GET",
        )
        with urllib.request.urlopen(request, timeout=2.0) as response:
            return json.load(response)


if __name__ == "__main__":
    unittest.main()
