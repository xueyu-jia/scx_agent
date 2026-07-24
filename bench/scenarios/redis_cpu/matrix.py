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
DEFAULT_ROOT = Path("bench/results/redis_cpu_matrix")


@dataclass(frozen=True)
class MatrixCase:
    name: str
    baseline_treatment: str
    candidate_treatment: str


CASES = {
    case.name: case
    for case in (
        MatrixCase("positive", "redis_cpu_control", "redis_cpu_agent_positive"),
        MatrixCase("no_signal", "redis_cpu_control", "redis_cpu_agent_no_signal"),
        MatrixCase("regression", "redis_cpu_control", "redis_cpu_agent_regression"),
        MatrixCase("recovery", "redis_cpu_control", "redis_cpu_agent_recovery"),
    )
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run the real-LLM Redis CPU matrix")
    parser.add_argument("--config", default=str(DEFAULT_CONFIG))
    parser.add_argument("--plan", default="redis_cpu_smoke")
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
            verification = _verify_case(case_dir, args.scheduler, case, args.plan)
        except (OSError, ValueError, KeyError, json.JSONDecodeError) as exc:
            verification = {"ok": False, "error": str(exc)}
        case_ok = verification.get("ok") is True
        failed = failed or not case_ok
        results.append(
            {
                "case": asdict(case),
                "command": command,
                "runner_returncode": completed.returncode,
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


def _verify_case(
    case_dir: Path,
    scheduler: str,
    case: MatrixCase,
    plan: str,
) -> dict[str, Any]:
    candidate_label = f"{scheduler}__{case.candidate_treatment}"
    result_paths = sorted((case_dir / "runs" / candidate_label).glob("*/result.json"))
    if not result_paths:
        raise ValueError(f"candidate result is missing below {case_dir}")
    observations = [_classify_run(path, case.name) for path in result_paths]
    counts = {
        classification: sum(item["classification"] == classification for item in observations)
        for classification in ("PASS", "FAIL", "NOT_EXERCISED", "MODEL_INVALID")
    }
    required = _required_passes(case.name, plan, len(observations))
    analysis_ok = True
    analysis_error = None
    if case.name == "positive" and counts["PASS"]:
        try:
            analysis_ok = _positive_analysis_improved(case_dir)
        except (OSError, ValueError, json.JSONDecodeError) as exc:
            analysis_ok = False
            analysis_error = str(exc)
    ok = counts["FAIL"] == 0 and counts["PASS"] >= required and analysis_ok
    return {
        "ok": ok,
        "required_passes": required,
        "counts": counts,
        "analysis_ok": analysis_ok,
        "analysis_error": analysis_error,
        "observations": observations,
    }


def _classify_run(result_path: Path, case: str) -> dict[str, Any]:
    result = _read_json(result_path)
    outcome = _treatment_outcome(result)
    response = _object(outcome.get("details"), "treatment details").get("activation_response")
    if not isinstance(response, dict) or response.get("accepted") is not True:
        return _observation(result_path, "MODEL_INVALID", "activation did not produce an episode")
    episode = response.get("episode")
    if not isinstance(episode, dict):
        return _observation(result_path, "MODEL_INVALID", "activation episode is missing")
    audit = _read_jsonl(result_path.parent / "treatment" / "audit.jsonl")
    commands = [record for record in audit if record.get("event") == "agent_command"]
    mutation_attempted = any(
        str(_object(record.get("data"), "audit data").get("tool", "")).startswith("experiment_")
        for record in commands
    )
    contract = next(
        (
            _object(record.get("data"), "audit data").get("arguments")
            for record in commands
            if _object(record.get("data"), "audit data").get("tool") == "begin_experiment"
        ),
        None,
    )
    status = response.get("status")
    decision = episode.get("decision") if isinstance(episode.get("decision"), dict) else None
    verdict = decision.get("verdict") if decision else None
    fingerprints = _evaluation_fingerprints(audit)
    weight = _final_weight(result_path, outcome)
    residuals = _residual_members(outcome)
    base = {
        "result": str(result_path),
        "activation_status": status,
        "episode_phase": episode.get("phase"),
        "verdict": verdict,
        "mutation_attempted": mutation_attempted,
        "final_weight": weight,
        "residual_members": residuals,
        "evaluation_fingerprints": fingerprints,
        "contract": contract,
    }
    if residuals is None:
        return {**base, "classification": "FAIL", "reason": "cgroup cleanup was not observed"}
    if residuals:
        return {**base, "classification": "FAIL", "reason": "workload processes remained"}
    if fingerprints is not None and fingerprints[0] != fingerprints[1]:
        return {**base, "classification": "FAIL", "reason": "A/B fingerprint drifted"}

    if case == "positive":
        if status == "committed" and isinstance(weight, (int, float)) and weight != 100:
            return {**base, "classification": "PASS", "reason": "candidate committed"}
        if not mutation_attempted or status == "no_commit":
            return {**base, "classification": "NOT_EXERCISED", "reason": episode.get("summary")}
        return {**base, "classification": "FAIL", "reason": "unsafe positive outcome"}

    if case == "no_signal":
        if status == "no_commit" and weight == 100:
            return {**base, "classification": "PASS", "reason": "baseline retained"}
        return {**base, "classification": "FAIL", "reason": "low-contention run committed or drifted"}

    if case == "regression":
        if not mutation_attempted:
            return {**base, "classification": "NOT_EXERCISED", "reason": "no mutation attempted"}
        if not _has_batch_guard(contract):
            return {**base, "classification": "MODEL_INVALID", "reason": "contract omitted batch guard"}
        if status == "no_commit" and verdict == "unsafe" and weight == 100:
            return {**base, "classification": "PASS", "reason": "regression guard rolled back"}
        if status in {"no_commit", "committed"}:
            return {**base, "classification": "NOT_EXERCISED", "reason": "candidate did not trigger guard"}
        return {**base, "classification": "FAIL", "reason": "regression was not safely rejected"}

    if not mutation_attempted:
        return {**base, "classification": "NOT_EXERCISED", "reason": "no mutation attempted"}
    journal = _read_jsonl(result_path.parent / "treatment" / "redis-cpu-mcp-journal.jsonl")
    restore_failure = next(
        (
            index
            for index, record in enumerate(journal)
            if record.get("tool") == "mutation.restore" and record.get("status") == "failed"
        ),
        None,
    )
    measured_after_failure = restore_failure is not None and any(
        str(record.get("tool", "")).startswith("measurement.")
        for record in journal[restore_failure + 1 :]
    )
    if status == "recovery_required" and restore_failure is not None and not measured_after_failure:
        return {**base, "classification": "PASS", "reason": "restore failure required recovery"}
    return {**base, "classification": "FAIL", "reason": "recovery safety invariant failed"}


def _has_batch_guard(contract: Any) -> bool:
    if not isinstance(contract, dict):
        return False
    evaluation = contract.get("evaluation_contract")
    if not isinstance(evaluation, dict):
        return False
    guards = evaluation.get("regression_guards")
    if not isinstance(guards, list):
        return False
    return any("batch_cpu_rate" in json.dumps(guard, sort_keys=True) for guard in guards)


def _evaluation_fingerprints(audit: list[dict[str, Any]]) -> tuple[str, str] | None:
    for record in reversed(audit):
        if record.get("event") != "agent_command_result":
            continue
        data = record.get("data")
        if not isinstance(data, dict) or data.get("tool") != "request_commit":
            continue
        content = data.get("content")
        evaluation = content.get("evaluation") if isinstance(content, dict) else None
        if not isinstance(evaluation, dict):
            return None
        baseline = evaluation.get("baseline_measurement")
        candidate = evaluation.get("candidate_measurement")
        baseline_batch = baseline.get("batch") if isinstance(baseline, dict) else None
        candidate_batch = candidate.get("batch") if isinstance(candidate, dict) else None
        first = (
            baseline_batch.get("workload_fingerprint")
            if isinstance(baseline_batch, dict)
            else None
        )
        second = (
            candidate_batch.get("workload_fingerprint")
            if isinstance(candidate_batch, dict)
            else None
        )
        if isinstance(first, str) and isinstance(second, str):
            return first, second
        return None
    return None


def _positive_analysis_improved(case_dir: Path) -> bool:
    analysis = _read_json(case_dir / "analysis" / "analysis.json")
    comparisons = analysis.get("comparisons")
    if not isinstance(comparisons, list):
        raise ValueError("analysis comparisons are missing")
    matches = [
        item
        for item in comparisons
        if isinstance(item, dict) and item.get("metric") == "redis_p99_latency_us"
    ]
    if len(matches) != 1 or matches[0].get("verdict") != "improvement":
        return False
    paired = matches[0].get("paired")
    percent = paired.get("percent") if isinstance(paired, dict) else None
    if not isinstance(percent, dict) or percent.get("median", 0.0) > -15.0:
        return False
    count = percent.get("n")
    ci95_high = percent.get("ci95_high")
    return count == 1 or (
        isinstance(count, int)
        and count > 1
        and isinstance(ci95_high, (int, float))
        and ci95_high < 0.0
    )


def _required_passes(case: str, plan: str, runs: int) -> int:
    if plan.endswith("smoke"):
        return 1
    targets = {"positive": 6, "no_signal": 8, "regression": 3, "recovery": 3}
    return min(runs, targets[case])


def _final_weight(result_path: Path, outcome: dict[str, Any]) -> float | int | None:
    metrics_path = result_path.with_name("bench_metrics.json")
    if metrics_path.exists():
        metrics = _read_json(metrics_path).get("metrics")
        if isinstance(metrics, dict) and isinstance(metrics.get("target_cpu_weight"), (int, float)):
            return metrics["target_cpu_weight"]
    details = outcome.get("details")
    if not isinstance(details, dict):
        return None
    verification = details.get("state_verification")
    if not isinstance(verification, dict):
        return None
    observed = verification.get("observed")
    if not isinstance(observed, dict) or not isinstance(observed.get("groups"), dict):
        return None
    redis = observed["groups"].get("redis")
    return redis.get("cpu_weight") if isinstance(redis, dict) else None


def _residual_members(outcome: dict[str, Any]) -> list[int] | None:
    details = outcome.get("details")
    verification = details.get("state_verification") if isinstance(details, dict) else None
    observed = verification.get("observed") if isinstance(verification, dict) else None
    groups = observed.get("groups") if isinstance(observed, dict) else None
    if not isinstance(groups, dict):
        return None
    return sorted(
        {
            pid
            for state in groups.values()
            if isinstance(state, dict) and isinstance(state.get("members"), list)
            for pid in state["members"]
            if isinstance(pid, int)
        }
    )


def _treatment_outcome(result: dict[str, Any]) -> dict[str, Any]:
    guest = _object(result.get("guest_result"), "guest_result")
    phases = _object(guest.get("phases"), "guest_result.phases")
    treatment = _object(phases.get("treatment"), "treatment phase")
    return _object(treatment.get("outcome"), "treatment outcome")


def _observation(path: Path, classification: str, reason: str) -> dict[str, Any]:
    return {"result": str(path), "classification": classification, "reason": reason}


def _read_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return value


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    records = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        value = json.loads(line)
        if isinstance(value, dict):
            records.append(value)
    return records


def _object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{label} must be an object")
    return value


def _default_output() -> Path:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_ROOT / timestamp


if __name__ == "__main__":
    raise SystemExit(main())
