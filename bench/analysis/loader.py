from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class RunMetricSet:
    result_dir: Path
    run_dir: Path
    label: str
    status: str
    run_index: int
    plan: str
    machine: str
    suite: str
    bench: str
    metric_profile: str
    metric_profile_config: dict[str, Any]
    metrics: dict[str, Any]
    returncode: int | None
    vm_returncode: int | None
    bench_returncode: int | None
    scheduler_start_returncode: int | None
    failure_reason: str
    treatment_phase_status: str = ""
    treatment_disposition: str = ""
    treatment_reason_code: str = ""


def load_result_dir(path: str | Path, label: str) -> list[RunMetricSet]:
    root = Path(path)
    runs: list[RunMetricSet] = []
    for result_path in sorted(root.glob("*/result.json")):
        run = _load_one(root, result_path.parent, result_path, label)
        if run is not None:
            runs.append(run)
    return runs


def _load_one(
    result_dir: Path,
    run_dir: Path,
    result_path: Path,
    label: str,
) -> RunMetricSet | None:
    result = _read_json(result_path)
    bench_metrics = _read_json(run_dir / "bench_metrics.json")
    spec = result.get("spec", {})
    if not isinstance(spec, dict):
        return None

    metrics = bench_metrics.get("metrics", {})
    guest_result = _as_dict(result.get("guest_result", {}))
    phases = _as_dict(guest_result.get("phases", {}))
    measurement = _as_dict(phases.get("measurement", {}))
    scheduler = _as_dict(phases.get("scheduler", {}))
    treatment = _as_dict(phases.get("treatment", {}))
    treatment_outcome = _as_dict(treatment.get("outcome", {}))
    treatment_reason = _as_dict(treatment_outcome.get("reason", {}))
    return RunMetricSet(
        result_dir=result_dir,
        run_dir=run_dir,
        label=label,
        status=str(result.get("status", "UNKNOWN")),
        run_index=_as_int(spec.get("run_index")) or 0,
        plan=str(spec.get("plan", "")),
        machine=str(spec.get("machine", "")),
        suite=str(spec.get("suite", "")),
        bench=str(spec.get("bench", "")),
        metric_profile=str(spec.get("metric_profile", "")),
        metric_profile_config=_as_dict(spec.get("metric_profile_config", {})),
        metrics=_as_dict(metrics),
        returncode=_as_int(result.get("returncode")),
        vm_returncode=_as_int(
            result.get("vm_returncode", result.get("libvirt_returncode", result.get("vng_returncode")))
        ),
        bench_returncode=_as_int(measurement.get("returncode")),
        scheduler_start_returncode=_as_int(scheduler.get("start_returncode")),
        failure_reason=_failure_reason(result, run_dir),
        treatment_phase_status=_as_string(treatment.get("status")),
        treatment_disposition=_as_string(treatment_outcome.get("disposition")),
        treatment_reason_code=_as_string(treatment_reason.get("code")),
    )


def _read_json(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}
    return data if isinstance(data, dict) else {}


def _as_dict(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def _as_int(value: Any) -> int | None:
    return value if isinstance(value, int) else None


def _as_string(value: Any) -> str:
    return value if isinstance(value, str) else ""


def _failure_reason(result: dict[str, Any], run_dir: Path) -> str:
    status = str(result.get("status", "UNKNOWN"))
    if status == "PASS":
        return ""

    result_reason = result.get("failure_reason")
    if isinstance(result_reason, str) and result_reason:
        return result_reason

    libvirt_stderr = _first_non_empty_line(run_dir / "libvirt_stderr.log")
    if libvirt_stderr:
        return f"libvirt: {libvirt_stderr}"

    legacy_vng_stderr = _first_non_empty_line(run_dir / "vng_stderr.log")
    if legacy_vng_stderr:
        return f"vng: {legacy_vng_stderr}"

    scheduler_stderr = _first_non_empty_line(run_dir / "scheduler_stderr.log")
    if scheduler_stderr:
        return f"scheduler: {scheduler_stderr}"

    workload_stderr = _first_non_empty_line(run_dir / "workload_stderr.log")
    if workload_stderr:
        return f"benchmark: {workload_stderr}"

    guest_result = _as_dict(result.get("guest_result", {}))
    guest_reason = guest_result.get("failure_reason")
    if isinstance(guest_reason, str) and guest_reason:
        return guest_reason
    vm_returncode = result.get(
        "vm_returncode",
        result.get("libvirt_returncode", result.get("vng_returncode")),
    )
    if vm_returncode not in (None, 0):
        return f"vm returncode {vm_returncode}"
    phases = _as_dict(guest_result.get("phases", {}))
    scheduler = _as_dict(phases.get("scheduler", {}))
    measurement = _as_dict(phases.get("measurement", {}))
    if scheduler.get("start_returncode") not in (None, 0):
        return f"scheduler returncode {scheduler.get('start_returncode')}"
    if measurement.get("returncode") not in (None, 0):
        return f"benchmark returncode {measurement.get('returncode')}"
    return status


def _first_non_empty_line(path: Path) -> str:
    if not path.exists():
        return ""
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        text = line.strip()
        if text:
            return text[:240]
    return ""
