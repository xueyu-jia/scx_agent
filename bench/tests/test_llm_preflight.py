from __future__ import annotations

import json
import unittest
from unittest.mock import MagicMock, patch

from bench.integrations.tuning_agent.llm_preflight import (
    LlmPreflightError,
    LlmSettings,
    normalize_base_url,
    preflight_protocol,
    probe_transport,
    settings_from_config,
)


class _Response:
    def __init__(self, value: object) -> None:
        self.payload = json.dumps(value).encode("utf-8")

    def __enter__(self) -> "_Response":
        return self

    def __exit__(self, *_args: object) -> None:
        return None

    def read(self, _size: int = -1) -> bytes:
        return self.payload


class LlmPreflightTest(unittest.TestCase):
    def settings(self) -> LlmSettings:
        return LlmSettings(
            base_url="http://127.0.0.1:18080/custom/openai/v1",
            api_key="test-api-key",
            model="test-model",
        )

    def test_protocol_preflight_uses_the_configured_api_base(self) -> None:
        response = _Response(
            {
                "choices": [
                    {
                        "message": {
                            "tool_calls": [
                                {
                                    "type": "function",
                                    "function": {
                                        "name": "preflight_ping",
                                        "arguments": "{}",
                                    },
                                }
                            ]
                        }
                    }
                ]
            }
        )
        with patch(
            "bench.integrations.tuning_agent.llm_preflight.urllib.request.urlopen",
            return_value=response,
        ) as urlopen:
            result = preflight_protocol(self.settings(), timeout=2)

        self.assertTrue(result["ok"])
        request = urlopen.call_args.args[0]
        self.assertEqual(
            request.full_url,
            "http://127.0.0.1:18080/custom/openai/v1/chat/completions",
        )
        self.assertEqual(request.get_header("Authorization"), "Bearer test-api-key")
        self.assertEqual(json.loads(request.data)["model"], "test-model")

    def test_protocol_preflight_requires_the_expected_tool_call(self) -> None:
        response = _Response(
            {"choices": [{"message": {"role": "assistant", "content": "plain"}}]}
        )
        with patch(
            "bench.integrations.tuning_agent.llm_preflight.urllib.request.urlopen",
            return_value=response,
        ), self.assertRaisesRegex(LlmPreflightError, "preflight_ping"):
            preflight_protocol(self.settings(), timeout=2)

    def test_transport_probe_uses_the_configured_host_and_port(self) -> None:
        connection = MagicMock()
        connection.__enter__.return_value = connection
        with patch(
            "bench.integrations.tuning_agent.llm_preflight.socket.create_connection",
            return_value=connection,
        ) as create_connection:
            result = probe_transport(self.settings(), timeout=2)

        self.assertTrue(result["reachable"])
        self.assertEqual(result["host"], "127.0.0.1")
        self.assertEqual(result["port"], 18080)
        self.assertEqual(result["scheme"], "http")
        create_connection.assert_called_once_with(("127.0.0.1", 18080), timeout=2)

    def test_config_selection_rejects_placeholders_and_mismatched_consumers(self) -> None:
        base = {
            "SCX_TUNING_AGENT_LLM_BASE_URL": "https://llm.example/v1",
            "SCX_TUNING_AGENT_LLM_API_KEY": "test-api-key",
            "SCX_TUNING_AGENT_LLM_MODEL": "model-a",
        }
        config = {
            "schedulers": {
                "first": {"env": base},
                "second": {"env": {**base, "SCX_TUNING_AGENT_LLM_MODEL": "model-b"}},
            }
        }

        with self.assertRaisesRegex(LlmPreflightError, "one identical"):
            settings_from_config(
                config,
                scheduler_names=["first", "second"],
                require_single=True,
            )
        with self.assertRaisesRegex(LlmPreflightError, "placeholder"):
            LlmSettings.from_env(
                {**base, "SCX_TUNING_AGENT_LLM_API_KEY": "replace-in-local-profile"},
                "test",
            )

    def test_base_url_validation_preserves_a_versioned_path(self) -> None:
        self.assertEqual(
            normalize_base_url("https://llm.example/custom/v1/"),
            "https://llm.example/custom/v1",
        )
        with self.assertRaisesRegex(LlmPreflightError, "credentials"):
            normalize_base_url("https://user:password@llm.example/v1")


if __name__ == "__main__":
    unittest.main()
