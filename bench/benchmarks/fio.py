#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="fio wrapper")
    parser.add_argument("--binary", default="bench/workloads/bin/fio")
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    result = run_command([ns.binary, *args])
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(result.stdout))
    emit(result, metrics, tool="fio")
    return result.returncode


def parse_metrics(stdout: str) -> dict[str, float]:
    try:
        data = json.loads(stdout)
    except json.JSONDecodeError:
        return {}

    jobs = data.get("jobs", [])
    if not isinstance(jobs, list):
        return {}

    metrics: dict[str, float] = {}
    for rw in ("read", "write"):
        sections = [job.get(rw, {}) for job in jobs if isinstance(job, dict)]
        iops = sum(float(section.get("iops", 0.0)) for section in sections if isinstance(section, dict))
        bw = sum(float(section.get("bw_bytes", 0.0)) for section in sections if isinstance(section, dict))
        if iops:
            metrics["iops"] = metrics.get("iops", 0.0) + iops
            metrics[f"{rw}_iops"] = iops
        if bw:
            metrics["bandwidth_bytes_per_sec"] = metrics.get("bandwidth_bytes_per_sec", 0.0) + bw
            metrics[f"{rw}_bandwidth_bytes_per_sec"] = bw

        percentiles = []
        for section in sections:
            if not isinstance(section, dict):
                continue
            clat = section.get("clat_ns", {})
            pct = clat.get("percentile", {}) if isinstance(clat, dict) else {}
            if isinstance(pct, dict):
                percentiles.append(pct)
        for source, target in (("99.000000", "p99_latency_us"), ("99.900000", "p999_latency_us")):
            values = [float(p[source]) / 1000.0 for p in percentiles if source in p]
            if values:
                metrics[target] = max(metrics.get(target, 0.0), max(values))
    return metrics


if __name__ == "__main__":
    raise SystemExit(main())
