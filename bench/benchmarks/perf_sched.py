#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.benchmarks.util import emit, run_command


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="perf bench sched wrapper")
    parser.add_argument("--binary", default=None, help="perf binary path")
    parser.add_argument("bench", choices=("pipe", "messaging"))
    parser.add_argument("args", nargs=argparse.REMAINDER)
    ns = parser.parse_args(argv)
    args = ns.args[1:] if ns.args[:1] == ["--"] else ns.args

    perf = ns.binary or resolve_perf_binary()
    result = run_command([perf, "bench", "sched", ns.bench, *args])
    text = result.stdout + "\n" + result.stderr
    metrics = {"elapsed_time_sec": result.elapsed_time_sec}
    metrics.update(parse_metrics(text, args))
    emit(result, metrics, tool=f"perf bench sched {ns.bench}")
    return result.returncode


def resolve_perf_binary() -> str:
    local = Path("bench/workloads/bin/perf")
    if local.exists():
        return str(local)
    return "perf"


def parse_metrics(text: str, args: list[str] | None = None) -> dict[str, float]:
    metrics: dict[str, float] = {}
    total = re.search(r"Total time:\s*([0-9.]+)\s*\[sec\]", text)
    if total and float(total.group(1)) > 0:
        metrics["elapsed_time_sec"] = float(total.group(1))
    usecs = re.search(r"([0-9.]+)\s+usecs/op", text)
    if usecs:
        metrics["latency_us_per_op"] = float(usecs.group(1))
    ops = re.search(r"([0-9.]+)\s+ops/sec", text)
    if ops:
        metrics["throughput"] = float(ops.group(1))
    elif total and args:
        total_ops = estimate_operations(text, args)
        elapsed = float(total.group(1))
        if total_ops and elapsed > 0:
            metrics["throughput"] = total_ops / elapsed
            metrics["latency_us_per_op"] = elapsed * 1_000_000.0 / total_ops
    return metrics


def estimate_operations(text: str, args: list[str]) -> int | None:
    loops = _option_int(args, "-l", "--loop", "--loops")
    if loops is None:
        return None

    senders = 1
    groups = 1
    sender_match = re.search(r"(\d+)\s+sender", text)
    if sender_match:
        senders = int(sender_match.group(1))
    group_match = re.search(r"(\d+)\s+groups?\s*==", text)
    if group_match:
        groups = int(group_match.group(1))
    return loops * senders * groups


def _option_int(args: list[str], *names: str) -> int | None:
    for index, arg in enumerate(args):
        if arg in names and index + 1 < len(args):
            try:
                return int(args[index + 1])
            except ValueError:
                return None
        for name in names:
            prefix = f"{name}="
            if arg.startswith(prefix):
                try:
                    return int(arg[len(prefix) :])
                except ValueError:
                    return None
    return None


if __name__ == "__main__":
    raise SystemExit(main())
