#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


DEFAULT_CONFIG = Path("bench/configs/local_config")
DEFAULT_ROOT = Path("bench/results/cgroup_cpu_matrix")


@dataclass(frozen=True)
class MatrixCase:
    name: str
    baseline_treatment: str
    candidate_treatment: str
    expected_status: str
    expected_disposition: str
    expected_reason_code: str
    expected_weight: float | None
    require_target_improvement: bool = False


CASES = {
    case.name: case
    for case in (
        MatrixCase(
            "positive",
            "cgroup_cpu_control_low",
            "cgroup_cpu_agent_positive",
            "PASS",
            "proceed",
            "tuning_agent.committed",
            100.0,
            True,
        ),
        MatrixCase(
            "no_signal",
            "cgroup_cpu_control_balanced",
            "cgroup_cpu_agent_no_signal",
            "PASS",
            "proceed",
            "tuning_agent.no_commit_baseline",
            100.0,
        ),
        MatrixCase(
            "unsafe",
            "cgroup_cpu_control_low",
            "cgroup_cpu_agent_unsafe",
            "PASS",
            "proceed",
            "tuning_agent.no_commit_baseline",
            10.0,
        ),
        MatrixCase(
            "recovery",
            "cgroup_cpu_control_low",
            "cgroup_cpu_agent_recovery",
            "TREATMENT_UNSAFE_STATE",
            "unsafe",
            "tuning_agent.recovery_required",
            None,
        ),
    )
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run the cgroup CPU tuning-agent test matrix")
    parser.add_argument("--config", default=str(DEFAULT_CONFIG))
    parser.add_argument("--plan", default="cgroup_cpu_smoke")
    parser.add_argument("--scheduler", default="default")
    parser.add_argument("--output")
    parser.add_argument("--case", action="append", choices=tuple(CASES), dest="cases")
    parser.add_argument("--progress-interval", type=int, default=15)
    args = parser.parse_args(argv)

    selected = args.cases or list(CASES)
    output_root = Path(args.output) if args.output else _default_output()
    output_root.mkdir(parents=True, exist_ok=False)
    results = []
    failed = False
    for name in selected:
        case = CASES[name]
        case_dir = output_root / name
        command = [
            sys.executable,
            "-m",
            "bench.scripts.run",
            "--config",
            args.config,
            "--plan",
            args.plan,
            "--baseline",
            args.scheduler,
            "--baseline-treatment",
            case.baseline_treatment,
            "--candidate",
            args.scheduler,
            "--candidate-treatment",
            case.candidate_treatment,
            "--order",
            "alternating",
            "--parallel",
            "1",
            "--progress-interval",
            str(args.progress_interval),
            "--output",
            str(case_dir),
        ]
        print(f"matrix case {name}: {' '.join(command)}", flush=True)
        completed = subprocess.run(command, check=False)
        try:
            verification = _verify_case(case_dir, args.scheduler, case)
        except (OSError, ValueError, KeyError) as exc:
            verification = {"ok": False, "error": str(exc)}
        case_ok = completed.returncode == 0 and verification.get("ok") is True
        failed = failed or not case_ok
        results.append(
            {
                "case": asdict(case),
                "command": command,
                "returncode": completed.returncode,
                "verification": verification,
                "ok": case_ok,
            }
        )
        print(f"matrix case {name}: {'PASS' if case_ok else 'FAIL'}", flush=True)

    summary = {
        "version": 1,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "config": str(Path(args.config).resolve()),
        "plan": args.plan,
        "scheduler": args.scheduler,
        "results": results,
        "passed": sum(item["ok"] for item in results),
        "failed": sum(not item["ok"] for item in results),
    }
    (output_root / "matrix.json").write_text(
        json.dumps(summary, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(f"matrix summary: {output_root / 'matrix.json'}")
    return 1 if failed else 0


def _verify_case(case_dir: Path, scheduler: str, case: MatrixCase) -> dict[str, Any]:
    candidate_label = f"{scheduler}__{case.candidate_treatment}"
    result_paths = sorted((case_dir / "runs" / candidate_label).glob("*/result.json"))
    if not result_paths:
        raise ValueError(f"candidate result is missing below {case_dir}")
    observations = []
    for result_path in result_paths:
        result = _read_json(result_path)
        guest_result = _object(result.get("guest_result"), "guest_result")
        phases = _object(guest_result.get("phases"), "guest_result.phases")
        treatment = _object(phases.get("treatment"), "treatment phase")
        outcome = _object(treatment.get("outcome"), "treatment outcome")
        observed = {
            "result": str(result_path),
            "status": result.get("status"),
            "disposition": outcome.get("disposition"),
            "reason_code": _object(outcome.get("reason"), "treatment reason").get("code"),
            "target_weight": None,
        }
        if result.get("status") != case.expected_status:
            raise ValueError(
                f"{result_path} status={result.get('status')!r}, expected {case.expected_status!r}"
            )
        if outcome.get("disposition") != case.expected_disposition:
            raise ValueError(
                f"{result_path} disposition={outcome.get('disposition')!r}, "
                f"expected {case.expected_disposition!r}"
            )
        if observed["reason_code"] != case.expected_reason_code:
            raise ValueError(
                f"{result_path} reason.code={observed['reason_code']!r}, "
                f"expected {case.expected_reason_code!r}"
            )
        if case.expected_weight is not None:
            bench_metrics = _read_json(result_path.with_name("bench_metrics.json"))
            metrics = _object(bench_metrics.get("metrics"), "bench metrics")
            weight = metrics.get("target_cpu_weight")
            observed["target_weight"] = weight
            if weight != case.expected_weight:
                raise ValueError(
                    f"{result_path} target_cpu_weight={weight!r}, expected {case.expected_weight}"
                )
        observations.append(observed)

    if case.require_target_improvement:
        analysis = _read_json(case_dir / "analysis" / "analysis.json")
        comparisons = analysis.get("comparisons")
        if not isinstance(comparisons, list):
            raise ValueError("analysis comparisons are missing")
        verdicts = [
            item.get("verdict")
            for item in comparisons
            if isinstance(item, dict) and item.get("metric") == "target_throughput"
        ]
        if verdicts != ["improvement"]:
            raise ValueError(f"target_throughput verdicts={verdicts!r}, expected ['improvement']")
    return {"ok": True, "observations": observations}


def _read_json(path: Path) -> dict[str, Any]:
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return data


def _object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{label} must be an object")
    return value


def _default_output() -> Path:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_ROOT / timestamp


if __name__ == "__main__":
    raise SystemExit(main())
