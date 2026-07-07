from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path

from .compare import build_analysis
from .loader import load_result_dir
from .report import write_html_report

DEFAULT_COMPARISONS_ROOT = Path("bench/results/comparisons")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Compare baseline and candidate benchmark results")
    parser.add_argument("--baseline", required=True, help="baseline result directory")
    parser.add_argument("--candidate", required=True, help="candidate result directory")
    parser.add_argument(
        "--output",
        help="analysis output directory; defaults to bench/results/comparisons/<timestamp>__<baseline>_vs_<candidate>",
    )
    parser.add_argument("--baseline-label", default="baseline")
    parser.add_argument("--candidate-label", default="candidate")
    args = parser.parse_args(argv)

    baseline_runs = load_result_dir(args.baseline, args.baseline_label)
    candidate_runs = load_result_dir(args.candidate, args.candidate_label)
    analysis = build_analysis(
        baseline_runs,
        candidate_runs,
        baseline_label=args.baseline_label,
        candidate_label=args.candidate_label,
    )

    output = Path(args.output) if args.output else _default_output(args.baseline_label, args.candidate_label)
    output.mkdir(parents=True, exist_ok=True)
    metadata = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "baseline": str(Path(args.baseline).resolve()),
        "candidate": str(Path(args.candidate).resolve()),
        "baseline_label": args.baseline_label,
        "candidate_label": args.candidate_label,
    }
    (output / "metadata.json").write_text(
        json.dumps(metadata, indent=2, sort_keys=True),
        encoding="utf-8",
    )
    (output / "analysis.json").write_text(
        json.dumps(analysis, indent=2, sort_keys=True),
        encoding="utf-8",
    )
    write_html_report(analysis, output / "report.html")

    summary = analysis["summary"]
    print(
        "primary: "
        f"{summary['primary_improvements']} improvement(s), "
        f"{summary['primary_regressions']} regression(s), "
        f"{summary['primary_failed']} failed, "
        f"{summary['primary_partial_failed']} partial failed, "
        f"{summary['primary_missing']} missing"
    )
    print(f"output: {output}")
    return 0


def _default_output(baseline_label: str, candidate_label: str) -> Path:
    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    name = f"{timestamp}__{_safe(baseline_label)}_vs_{_safe(candidate_label)}"
    return DEFAULT_COMPARISONS_ROOT / name


def _safe(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in ("-", "_") else "_" for ch in value)


if __name__ == "__main__":
    raise SystemExit(main())
