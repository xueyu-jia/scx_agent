#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import time
from pathlib import Path
from typing import Any


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="PyVSAG HNSW ANN wrapper")
    parser.add_argument("--dim", type=int, default=128)
    parser.add_argument("--num-elements", type=int, default=50000)
    parser.add_argument("--num-queries", type=int, default=1000)
    parser.add_argument("--k", type=int, default=10)
    parser.add_argument("--max-degree", type=int, default=16)
    parser.add_argument("--ef-construction", type=int, default=200)
    parser.add_argument("--ef-search", type=int, default=100)
    ns = parser.parse_args(argv)

    out_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    stdout_log = out_dir / "workload_stdout.log"
    stderr_log = out_dir / "workload_stderr.log"
    started = time.monotonic()
    stdout = ""
    stderr = ""
    returncode = 0

    try:
        metrics = run_benchmark(ns)
        stdout = json.dumps(metrics, sort_keys=True)
    except Exception as exc:
        returncode = 1
        metrics = {"elapsed_time_sec": time.monotonic() - started}
        stderr = str(exc)

    metrics.setdefault("elapsed_time_sec", time.monotonic() - started)
    stdout_log.write_text(stdout, encoding="utf-8")
    stderr_log.write_text(stderr, encoding="utf-8")
    print(
        json.dumps(
            {
                "metrics": metrics,
                "metadata": {"tool": "pyvsag", "returncode": returncode},
                "raw": {"stdout_path": str(stdout_log), "stderr_path": str(stderr_log)},
            },
            sort_keys=True,
        )
    )
    return returncode


def run_benchmark(ns: argparse.Namespace) -> dict[str, float]:
    import numpy as np
    import pyvsag

    np.random.seed(42)
    data = np.random.random((ns.num_elements, ns.dim)).astype(np.float32)
    queries = np.random.random((ns.num_queries, ns.dim)).astype(np.float32)
    ids = list(range(ns.num_elements))
    index_params = json.dumps(
        {
            "dtype": "float32",
            "metric_type": "l2",
            "dim": ns.dim,
            "hnsw": {
                "max_degree": ns.max_degree,
                "ef_construction": ns.ef_construction,
            },
        }
    )
    search_params = json.dumps({"hnsw": {"ef_search": ns.ef_search}})

    index = pyvsag.Index("hnsw", index_params)
    build_started = time.monotonic()
    index.build(vectors=data, ids=ids, num_elements=ns.num_elements, dim=ns.dim)
    build_time = time.monotonic() - build_started

    for query in queries[: min(10, len(queries))]:
        index.knn_search(vector=query, k=ns.k, parameters=search_params)

    search_times: list[float] = []
    total_results = 0
    search_started = time.monotonic()
    for query in queries:
        query_started = time.monotonic()
        found_ids, _distances = index.knn_search(vector=query, k=ns.k, parameters=search_params)
        search_times.append(time.monotonic() - query_started)
        total_results += len(found_ids)
    search_time = time.monotonic() - search_started

    recall = 0.0
    recall_count = min(100, len(data))
    if recall_count:
        hits = 0
        for index_id in range(recall_count):
            found_ids, _distances = index.knn_search(
                vector=data[index_id],
                k=ns.k,
                parameters=search_params,
            )
            if ids[index_id] in found_ids:
                hits += 1
        recall = hits / recall_count

    return {
        "build_time_sec": build_time,
        "search_time_sec": search_time,
        "throughput": len(queries) / search_time if search_time else 0.0,
        "qps": len(queries) / search_time if search_time else 0.0,
        "avg_latency_ms": _mean(search_times) * 1000.0,
        "p95_latency_ms": _percentile(search_times, 95) * 1000.0,
        "p99_latency_ms": _percentile(search_times, 99) * 1000.0,
        "recall": recall,
        "num_queries": float(len(queries)),
        "total_results": float(total_results),
    }


def _mean(values: list[float]) -> float:
    return sum(values) / len(values) if values else 0.0


def _percentile(values: list[float], percentile: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * percentile / 100.0
    lower = int(rank)
    upper = min(lower + 1, len(ordered) - 1)
    weight = rank - lower
    return ordered[lower] * (1.0 - weight) + ordered[upper] * weight


if __name__ == "__main__":
    raise SystemExit(main())
