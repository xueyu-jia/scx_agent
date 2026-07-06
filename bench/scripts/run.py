#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from bench.analysis.compare import build_analysis
from bench.analysis.loader import load_result_dir
from bench.analysis.report import write_html_report
from bench.config.parser import ConfigError, expand_plan, load_config
from bench.runner import run_specs


DEFAULT_EXPERIMENT_ROOT = Path("bench/results/experiments")
LATEST_REPORT_LINK = Path("bench/results/latest_report.html")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run a full baseline/candidate benchmark experiment")
    parser.add_argument("--config", default="bench/configs/example.config")
    parser.add_argument("--plan", required=True)
    parser.add_argument("--baseline", required=True, help="baseline scheduler name from config.schedulers")
    parser.add_argument("--candidate", required=True, help="candidate scheduler name from config.schedulers")
    parser.add_argument(
        "--order",
        choices=("alternating", "sequential"),
        default="alternating",
        help="execution order across baseline/candidate",
    )
    parser.add_argument("--output", help="experiment output directory")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)

    try:
        config = load_config(args.config)
        baseline = _scheduler(config, args.baseline)
        candidate = _scheduler(config, args.candidate)
        specs = expand_plan(config, args.plan)
    except ConfigError as exc:
        print(f"config error: {exc}", file=sys.stderr)
        return 2

    if args.baseline == args.candidate:
        print("baseline and candidate must be different schedulers", file=sys.stderr)
        return 2

    experiment_dir = Path(args.output) if args.output else _default_experiment_dir(
        args.baseline,
        args.candidate,
    )
    runs_dir = experiment_dir / "runs"
    analysis_dir = experiment_dir / "analysis"
    runs_dir.mkdir(parents=True, exist_ok=True)
    analysis_dir.mkdir(parents=True, exist_ok=True)

    metadata = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "config": str(Path(args.config).resolve()),
        "plan": args.plan,
        "baseline": args.baseline,
        "candidate": args.candidate,
        "order": args.order,
        "dry_run": args.dry_run,
        "experiment_dir": str(experiment_dir.resolve()),
        "execution_order": [],
    }
    _write_json(experiment_dir / "metadata.json", metadata)

    execution_order: list[dict[str, Any]] = []
    if args.order == "sequential":
        _run_scheduler(specs, runs_dir, args.baseline, baseline, args, execution_order)
        _run_scheduler(specs, runs_dir, args.candidate, candidate, args, execution_order)
    else:
        for run_index in sorted({spec.run_index for spec in specs}):
            round_specs = [spec for spec in specs if spec.run_index == run_index]
            order = (
                [(args.baseline, baseline), (args.candidate, candidate)]
                if run_index % 2 == 1
                else [(args.candidate, candidate), (args.baseline, baseline)]
            )
            for scheduler_name, scheduler in order:
                _run_scheduler(
                    round_specs,
                    runs_dir,
                    scheduler_name,
                    scheduler,
                    args,
                    execution_order,
                )

    metadata["execution_order"] = execution_order
    _write_json(experiment_dir / "metadata.json", metadata)

    baseline_dir = runs_dir / args.baseline
    candidate_dir = runs_dir / args.candidate
    analysis = build_analysis(
        load_result_dir(baseline_dir, args.baseline),
        load_result_dir(candidate_dir, args.candidate),
        baseline_label=args.baseline,
        candidate_label=args.candidate,
    )
    _write_json(analysis_dir / "analysis.json", analysis)
    _write_json(
        analysis_dir / "metadata.json",
        {
            **metadata,
            "baseline_result_dir": str(baseline_dir.resolve()),
            "candidate_result_dir": str(candidate_dir.resolve()),
        },
    )
    write_html_report(analysis, analysis_dir / "report.html")
    latest_report = _update_latest_report_link(analysis_dir / "report.html")

    print(f"experiment: {experiment_dir}")
    print(f"baseline results: {baseline_dir}")
    print(f"candidate results: {candidate_dir}")
    print(f"report: {analysis_dir / 'report.html'}")
    print(f"latest report: {latest_report}")
    return 0


def _run_scheduler(
    specs: list[Any],
    runs_dir: Path,
    scheduler_name: str,
    scheduler: dict[str, Any],
    args: argparse.Namespace,
    execution_order: list[dict[str, Any]],
) -> None:
    if not specs:
        return
    execution_order.append(
        {
            "scheduler": scheduler_name,
            "run_indexes": sorted({spec.run_index for spec in specs}),
            "spec_count": len(specs),
        }
    )
    run_specs(
        specs,
        output_dir=runs_dir / scheduler_name,
        dry_run=args.dry_run,
        label=scheduler_name,
        scheduler=scheduler,
        config_path=args.config,
    )


def _scheduler(config: dict[str, Any], name: str) -> dict[str, Any]:
    schedulers = config["schedulers"]
    if name not in schedulers:
        raise ConfigError(f"unknown scheduler: {name}")
    scheduler = dict(schedulers[name])
    scheduler["name"] = name
    return scheduler


def _default_experiment_dir(baseline: str, candidate: str) -> Path:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_EXPERIMENT_ROOT / f"{timestamp}__{_safe(baseline)}_vs_{_safe(candidate)}"


def _safe(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in ("-", "_") else "_" for ch in value)


def _write_json(path: Path, data: Any) -> None:
    path.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")


def _update_latest_report_link(report_path: Path) -> Path:
    link = LATEST_REPORT_LINK
    link.parent.mkdir(parents=True, exist_ok=True)
    if link.exists() or link.is_symlink():
        link.unlink()
    link.symlink_to(Path.cwd() / report_path)
    return link


if __name__ == "__main__":
    raise SystemExit(main())
