#!/usr/bin/env python3
from __future__ import annotations

import argparse
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
import json
import os
import socket
import ssl
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit


BASE_URL_ENV = "SCX_TUNING_AGENT_LLM_BASE_URL"
API_KEY_ENV = "SCX_TUNING_AGENT_LLM_API_KEY"
MODEL_ENV = "SCX_TUNING_AGENT_LLM_MODEL"
PLACEHOLDER_API_KEYS = frozenset({"replace-in-local-profile"})
MAX_RESPONSE_BYTES = 2 * 1024 * 1024


class LlmPreflightError(RuntimeError):
    pass


@dataclass(frozen=True)
class LlmSettings:
    base_url: str
    api_key: str
    model: str

    @classmethod
    def from_env(cls, env: Mapping[str, Any], owner: str) -> "LlmSettings":
        base_url = _required_text(env.get(BASE_URL_ENV), f"{owner}.{BASE_URL_ENV}")
        api_key = _required_text(env.get(API_KEY_ENV), f"{owner}.{API_KEY_ENV}")
        model = _required_text(env.get(MODEL_ENV), f"{owner}.{MODEL_ENV}")
        if api_key in PLACEHOLDER_API_KEYS:
            raise LlmPreflightError(
                f"{owner}.{API_KEY_ENV} is still a local-profile placeholder"
            )
        return cls(
            base_url=normalize_base_url(base_url),
            api_key=api_key,
            model=model,
        )

    @property
    def chat_completions_url(self) -> str:
        return f"{self.base_url}/chat/completions"

    def public(self) -> dict[str, str]:
        return {"base_url": self.base_url, "model": self.model}


def normalize_base_url(value: str) -> str:
    normalized = value.strip().rstrip("/")
    parsed = urlsplit(normalized)
    if parsed.scheme not in {"http", "https"} or not parsed.hostname:
        raise LlmPreflightError("LLM base URL must be an HTTP(S) URL with a host")
    if parsed.username is not None or parsed.password is not None:
        raise LlmPreflightError("LLM base URL must not contain credentials")
    if parsed.query or parsed.fragment:
        raise LlmPreflightError("LLM base URL must not contain a query or fragment")
    return normalized


def settings_from_config(
    config: Mapping[str, Any],
    *,
    scheduler_names: Sequence[str] = (),
    treatment_names: Sequence[str] = (),
    require_single: bool = False,
) -> list[LlmSettings]:
    selected: list[tuple[str, Mapping[str, Any]]] = []
    selected.extend(
        _selected_consumers(config, "schedulers", scheduler_names)
    )
    selected.extend(
        _selected_consumers(config, "treatments", treatment_names)
    )
    if not scheduler_names and not treatment_names:
        for section in ("schedulers", "treatments"):
            consumers = config.get(section, {})
            if not isinstance(consumers, Mapping):
                continue
            for name, consumer in consumers.items():
                if not isinstance(consumer, Mapping):
                    continue
                env = consumer.get("env", {})
                if isinstance(env, Mapping) and BASE_URL_ENV in env:
                    selected.append((f"{section}.{name}", consumer))
    if not selected:
        raise LlmPreflightError("no configured LLM consumers were selected")

    unique: list[LlmSettings] = []
    for owner, consumer in selected:
        env = consumer.get("env", {})
        if not isinstance(env, Mapping):
            raise LlmPreflightError(f"{owner}.env must be a mapping")
        settings = LlmSettings.from_env(env, f"{owner}.env")
        if settings not in unique:
            unique.append(settings)
    if require_single and len(unique) != 1:
        raise LlmPreflightError(
            "selected LLM consumers must use one identical endpoint, model, and API key"
        )
    return unique


def probe_transport(settings: LlmSettings, timeout: float = 10.0) -> dict[str, Any]:
    parsed = urlsplit(settings.base_url)
    assert parsed.hostname is not None
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    try:
        with socket.create_connection((parsed.hostname, port), timeout=timeout) as connection:
            if parsed.scheme == "https":
                context = ssl.create_default_context()
                with context.wrap_socket(
                    connection,
                    server_hostname=parsed.hostname,
                ) as tls_connection:
                    tls_version = tls_connection.version()
            else:
                tls_version = None
    except (OSError, ssl.SSLError) as error:
        raise LlmPreflightError(
            f"configured LLM endpoint transport failed: {error}"
        ) from error
    return {
        **settings.public(),
        "host": parsed.hostname,
        "port": port,
        "scheme": parsed.scheme,
        "tls_version": tls_version,
        "reachable": True,
    }


def preflight_protocol(
    settings: LlmSettings,
    timeout: float = 120.0,
) -> dict[str, Any]:
    body = json.dumps(
        {
            "model": settings.model,
            "messages": [
                {
                    "role": "user",
                    "content": "You must call preflight_ping now; do not answer with text.",
                }
            ],
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "preflight_ping",
                        "description": "Validate OpenAI-compatible tool calling.",
                        "parameters": {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": False,
                        },
                    },
                }
            ],
            "tool_choice": "auto",
            "stream": False,
        },
        separators=(",", ":"),
    ).encode("utf-8")
    request = urllib.request.Request(
        settings.chat_completions_url,
        data=body,
        headers={
            "Authorization": f"Bearer {settings.api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            payload = response.read(MAX_RESPONSE_BYTES + 1)
    except urllib.error.HTTPError as error:
        detail = error.read(4096).decode("utf-8", errors="replace")
        raise LlmPreflightError(
            f"configured LLM endpoint returned HTTP {error.code}: {detail}"
        ) from error
    except urllib.error.URLError as error:
        raise LlmPreflightError(
            f"configured LLM endpoint request failed: {error.reason}"
        ) from error
    if len(payload) > MAX_RESPONSE_BYTES:
        raise LlmPreflightError("configured LLM endpoint response is too large")
    try:
        value = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise LlmPreflightError(
            "configured LLM endpoint returned invalid JSON"
        ) from error
    tool_calls = _tool_calls(value)
    if not any(
        call.get("type") == "function"
        and isinstance(call.get("function"), Mapping)
        and call["function"].get("name") == "preflight_ping"
        for call in tool_calls
        if isinstance(call, Mapping)
    ):
        raise LlmPreflightError(
            "configured LLM endpoint did not return the preflight_ping tool call"
        )
    return {
        **settings.public(),
        "checked_url": settings.chat_completions_url,
        "tool_call": "preflight_ping",
        "ok": True,
    }


def _selected_consumers(
    config: Mapping[str, Any],
    section: str,
    names: Sequence[str],
) -> list[tuple[str, Mapping[str, Any]]]:
    consumers = config.get(section, {})
    if not isinstance(consumers, Mapping):
        raise LlmPreflightError(f"config section {section} must be a mapping")
    selected = []
    for name in names:
        consumer = consumers.get(name)
        if not isinstance(consumer, Mapping):
            raise LlmPreflightError(f"configured {section[:-1]} is missing: {name}")
        selected.append((f"{section}.{name}", consumer))
    return selected


def _tool_calls(value: Any) -> list[Any]:
    if not isinstance(value, Mapping):
        return []
    choices = value.get("choices")
    if not isinstance(choices, list) or not choices:
        return []
    first = choices[0]
    message = first.get("message") if isinstance(first, Mapping) else None
    calls = message.get("tool_calls") if isinstance(message, Mapping) else None
    return calls if isinstance(calls, list) else []


def _required_text(value: Any, name: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise LlmPreflightError(f"{name} must be a non-empty string")
    return value.strip()


def _load_config(path: str) -> Mapping[str, Any]:
    from bench.core.config import load_config_data

    return load_config_data(Path(path))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Validate configured OpenAI-compatible LLM access"
    )
    parser.add_argument("--config")
    parser.add_argument("--scheduler", action="append", default=[])
    parser.add_argument("--treatment", action="append", default=[])
    parser.add_argument("--require-single", action="store_true")
    parser.add_argument("--transport-only", action="store_true")
    parser.add_argument("--timeout", type=float, default=120.0)
    args = parser.parse_args(argv)
    try:
        if args.config:
            settings_list = settings_from_config(
                _load_config(args.config),
                scheduler_names=args.scheduler,
                treatment_names=args.treatment,
                require_single=args.require_single,
            )
        else:
            if args.scheduler or args.treatment or args.require_single:
                raise LlmPreflightError(
                    "consumer selectors require --config"
                )
            settings_list = [LlmSettings.from_env(os.environ, "environment")]
        checks = [
            probe_transport(settings, min(args.timeout, 10.0))
            if args.transport_only
            else preflight_protocol(settings, args.timeout)
            for settings in settings_list
        ]
    except (LlmPreflightError, OSError, ValueError) as error:
        print(f"LLM preflight failed: {error}", file=sys.stderr)
        return 2
    print(json.dumps({"ok": True, "checks": checks}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
