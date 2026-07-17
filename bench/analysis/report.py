from __future__ import annotations

import html
from pathlib import Path
from typing import Any


def write_html_report(analysis: dict[str, Any], path: str | Path) -> None:
    Path(path).write_text(render_html(analysis), encoding="utf-8")


def render_html(analysis: dict[str, Any]) -> str:
    comparisons = analysis.get("comparisons", [])
    primary = [item for item in comparisons if item.get("role") == "primary"]
    invalid_runs = analysis.get("invalid_runs", [])
    summary = analysis.get("summary", {})

    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>scx bench report</title>
  <style>
    :root {{
      color-scheme: light;
      --border: #d7dee8;
      --text: #17202a;
      --muted: #667085;
      --bg: #f6f8fb;
      --panel: #ffffff;
      --good: #137333;
      --bad: #b42318;
      --warn: #b54708;
      --neutral: #475467;
    }}
    body {{
      margin: 0;
      background: var(--bg);
      color: var(--text);
      font: 14px/1.45 system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }}
    main {{
      max-width: 1200px;
      margin: 0 auto;
      padding: 24px;
    }}
    h1, h2 {{
      margin: 0 0 12px;
    }}
    section {{
      margin: 0 0 24px;
    }}
    table {{
      width: 100%;
      border-collapse: collapse;
      background: var(--panel);
      border: 1px solid var(--border);
    }}
    th, td {{
      padding: 8px 10px;
      border-bottom: 1px solid var(--border);
      text-align: left;
      vertical-align: middle;
    }}
    th {{
      background: #eef2f7;
      font-weight: 650;
    }}
    .summary {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
      gap: 12px;
    }}
    .tile {{
      background: var(--panel);
      border: 1px solid var(--border);
      padding: 12px;
    }}
    .tile strong {{
      display: block;
      font-size: 22px;
    }}
    .muted {{
      color: var(--muted);
    }}
    .verdict-improvement {{ color: var(--good); font-weight: 650; }}
    .verdict-regression, .verdict-failed {{ color: var(--bad); font-weight: 650; }}
    .verdict-no_change, .verdict-informational {{ color: var(--neutral); }}
    .run-failed {{ color: var(--bad); font-weight: 650; }}
    .run-partial_failed {{ color: var(--warn); font-weight: 650; }}
    .run-ok {{ color: var(--neutral); }}
    .delta-improvement {{ color: var(--good); font-weight: 650; }}
    .delta-regression, .delta-failed {{ color: var(--bad); font-weight: 650; }}
    .delta-no_change, .delta-informational {{ color: var(--neutral); }}
    .bar {{
      width: 150px;
      height: 14px;
      background: #eef2f7;
      border: 1px solid #d0d5dd;
      border-radius: 999px;
      overflow: hidden;
    }}
    .bar span {{
      display: block;
      height: 100%;
      max-width: 100%;
    }}
    .bar .positive {{ background: var(--good); }}
    .bar .negative {{ background: var(--bad); }}
    .bar .neutral {{ background: #98a2b3; }}
    code {{
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      font-size: 12px;
    }}
  </style>
</head>
<body>
<main>
  <h1>scx bench report</h1>
  <section>
    <div class="summary">
      {_summary_tile("Primary", summary.get("primary_total", 0))}
      {_summary_tile("Improvements", summary.get("primary_improvements", 0))}
      {_summary_tile("Regressions", summary.get("primary_regressions", 0))}
      {_summary_tile("No Change", summary.get("primary_no_change", 0))}
      {_summary_tile("Failed", summary.get("primary_failed", 0))}
      {_summary_tile("Partial Failed", summary.get("primary_partial_failed", 0))}
      {_summary_tile("Missing", summary.get("primary_missing", 0))}
      {_summary_tile("Invalid Runs", summary.get("invalid_runs", 0))}
    </div>
  </section>

  <section>
    <h2>Primary Metrics</h2>
    {_comparison_table(primary)}
  </section>

  <section>
    <h2>All Comparisons</h2>
    {_comparison_table(comparisons)}
  </section>

  <section>
    <h2>Invalid Runs</h2>
    {_invalid_table(invalid_runs)}
  </section>
</main>
</body>
</html>
"""


def _summary_tile(label: str, value: Any) -> str:
    return f'<div class="tile"><span class="muted">{_h(label)}</span><strong>{_h(value)}</strong></div>'


def _comparison_table(comparisons: list[dict[str, Any]]) -> str:
    if not comparisons:
        return '<p class="muted">No comparisons.</p>'

    rows = []
    for item in comparisons:
        rows.append(
            "<tr "
            f"data-verdict=\"{_h(item.get('verdict'))}\" "
            f"data-run-status=\"{_h(item.get('run_status'))}\" "
            f"data-failure-reason=\"{_h(item.get('failure_reason'))}\">"
            f"<td>{_h(item.get('machine'))}</td>"
            f"<td>{_h(item.get('suite'))}</td>"
            f"<td>{_h(item.get('bench'))}</td>"
            f"<td>{_h(item.get('metric'))}</td>"
            f"<td>{_h(item.get('role'))}</td>"
            f"<td>{_fmt_stat(item.get('baseline'), item.get('unit'))}</td>"
            f"<td>{_fmt_stat(item.get('candidate'), item.get('unit'))}</td>"
            f"<td>{_fmt_delta(item.get('delta_pct'), item.get('verdict'))}</td>"
            f"<td>{_fmt_paired(item.get('paired'))}</td>"
            f"<td>{_bar(item.get('delta_pct'), item.get('verdict'))}</td>"
            "</tr>"
        )

    return (
        "<table><thead><tr>"
        "<th>Machine</th><th>Suite</th><th>Bench</th><th>Metric</th><th>Role</th>"
        "<th>Baseline</th><th>Candidate</th><th>Delta</th><th>Paired Δ</th><th>Chart</th>"
        "</tr></thead><tbody>"
        + "".join(rows)
        + "</tbody></table>"
    )


def _invalid_table(invalid_runs: list[dict[str, str]]) -> str:
    if not invalid_runs:
        return '<p class="muted">No invalid runs.</p>'
    rows = []
    for run in invalid_runs:
        rows.append(
            f"<tr data-path=\"{_h(run.get('path'))}\">"
            f"<td>{_h(run.get('label'))}</td>"
            f"<td>{_h(run.get('status'))}</td>"
            f"<td>{_h(run.get('machine'))}</td>"
            f"<td>{_h(run.get('suite'))}</td>"
            f"<td>{_h(run.get('bench'))}</td>"
            f"<td>{_h(run.get('reason'))}</td>"
            "</tr>"
        )
    return (
        "<table><thead><tr>"
        "<th>Label</th><th>Status</th><th>Machine</th><th>Suite</th><th>Bench</th><th>Reason</th>"
        "</tr></thead><tbody>"
        + "".join(rows)
        + "</tbody></table>"
    )


def _fmt_stat(stats: Any, unit: Any) -> str:
    if not isinstance(stats, dict) or "mean" not in stats:
        if isinstance(stats, dict):
            counts = _fmt_counts(stats)
            return f'<span class="muted">missing</span> <span class="muted">{counts}</span>'
        return '<span class="muted">missing</span>'
    unit_text = f" {_h(unit)}" if unit else ""
    return f"{float(stats['mean']):.4g}{unit_text} <span class=\"muted\">{_fmt_counts(stats)}</span>"


def _fmt_counts(stats: dict[str, Any]) -> str:
    count = stats.get("count", 0)
    passed = stats.get("passed_runs", 0)
    total = stats.get("total_runs", 0)
    failed = stats.get("failed_runs", 0)
    missing = stats.get("metric_missing_runs", 0)
    suffix = []
    if failed:
        suffix.append(f"failed={failed}")
    if missing:
        suffix.append(f"metric_missing={missing}")
    extra = f"; {', '.join(suffix)}" if suffix else ""
    return f"n={_h(count)}/{_h(passed)}/{_h(total)}{_h(extra)}"


def _fmt_delta(value: Any, verdict: Any = None) -> str:
    if not isinstance(value, (int, float)):
        return '<span class="muted">missing</span>'
    sign = "+" if value > 0 else ""
    return f'<span class="delta-{_h(verdict)}">{sign}{value:.2f}%</span>'


def _fmt_paired(paired: Any) -> str:
    if not isinstance(paired, dict):
        return '<span class="muted">missing</span>'
    stats = paired.get("percent")
    if not isinstance(stats, dict):
        return '<span class="muted">missing</span>'
    mean = stats.get("mean")
    low = stats.get("ci95_low")
    high = stats.get("ci95_high")
    if not isinstance(mean, (int, float)):
        return '<span class="muted">missing</span>'
    interval = (
        f" [{low:.2f}, {high:.2f}]" if isinstance(low, (int, float)) and isinstance(high, (int, float)) else ""
    )
    return f"{mean:+.2f}% <span class=\"muted\">n={_h(stats.get('n', 0))}{interval}</span>"


def _bar(value: Any, verdict: Any = None) -> str:
    if not isinstance(value, (int, float)):
        return ""
    width = min(abs(value), 100.0)
    klass = "positive" if verdict == "improvement" else "negative" if verdict == "regression" else "neutral"
    return f'<div class="bar"><span class="{klass}" style="width:{width:.2f}%"></span></div>'


def _h(value: Any) -> str:
    return html.escape("" if value is None else str(value))
