from __future__ import annotations

import json
from pathlib import Path
from typing import Any


def load_bench_metrics(stdout_path: Path) -> dict[str, Any]:
    if not stdout_path.exists():
        return {"metrics": {}, "metadata": {}, "parse_status": "missing_stdout"}

    text = stdout_path.read_text(encoding="utf-8", errors="replace").strip()
    if not text:
        return {"metrics": {}, "metadata": {}, "parse_status": "empty_stdout"}

    try:
        parsed = json.loads(text)
    except json.JSONDecodeError as exc:
        return {
            "metrics": {},
            "metadata": {},
            "parse_status": "non_json_stdout",
            "parse_error": str(exc),
        }

    if not isinstance(parsed, dict):
        return {
            "metrics": {},
            "metadata": {},
            "parse_status": "json_not_object",
        }

    if "metrics" in parsed:
        metrics = parsed.get("metrics")
        return {
            "metrics": metrics if isinstance(metrics, dict) else {},
            "metadata": parsed.get("metadata", {}) if isinstance(parsed.get("metadata", {}), dict) else {},
            "raw": parsed.get("raw", {}),
            "parse_status": "ok",
        }

    return {
        "metrics": parsed,
        "metadata": {},
        "parse_status": "legacy_json_object",
    }
