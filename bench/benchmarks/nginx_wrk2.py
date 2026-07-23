#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import signal
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="nginx + wrk2 wrapper")
    parser.add_argument("--nginx", default="bench/workloads/bin/nginx")
    parser.add_argument("--wrk", default="bench/workloads/bin/wrk")
    parser.add_argument("--port", default="8080")
    parser.add_argument("--threads", default="4")
    parser.add_argument("--connections", default="50")
    parser.add_argument("--duration", default="10")
    parser.add_argument("--rate", default="1000")
    ns = parser.parse_args(argv)

    out_dir = Path(os.environ.get("SCX_BENCH_OUT", "."))
    stdout_log = out_dir / "workload_stdout.log"
    stderr_log = out_dir / "workload_stderr.log"
    stdout_parts: list[str] = []
    stderr_parts: list[str] = []
    started = time.monotonic()
    returncode = 0
    server: subprocess.Popen[str] | None = None

    with tempfile.TemporaryDirectory(prefix="scx-nginx-") as temp_dir:
        temp = Path(temp_dir)
        temp.chmod(0o755)
        (temp / "logs").mkdir()
        html = temp / "html"
        html.mkdir()
        html.chmod(0o755)
        index = html / "index.html"
        index.write_text("scx bench\n", encoding="utf-8")
        index.chmod(0o644)
        config = _nginx_config(temp, html, ns.port)
        try:
            server = subprocess.Popen(
                [ns.nginx, "-c", str(config), "-p", str(temp)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            _wait_for_http(ns.port)
            command = [
                ns.wrk,
                f"-t{ns.threads}",
                f"-c{ns.connections}",
                f"-d{ns.duration}s",
                f"-R{ns.rate}",
                "--latency",
                f"http://127.0.0.1:{ns.port}/",
            ]
            completed = subprocess.run(command, capture_output=True, text=True)
            returncode = completed.returncode
            stdout_parts.append(completed.stdout)
            stderr_parts.append(completed.stderr)
            metrics = {"elapsed_time_sec": time.monotonic() - started}
            metrics.update(parse_metrics(completed.stdout + "\n" + completed.stderr))
            if "requests_per_second" in metrics:
                metrics["throughput"] = metrics["requests_per_second"]
        except FileNotFoundError as exc:
            returncode = 127
            metrics = {"elapsed_time_sec": time.monotonic() - started}
            stderr_parts.append(str(exc))
        except Exception as exc:
            returncode = 1
            metrics = {"elapsed_time_sec": time.monotonic() - started}
            stderr_parts.append(str(exc))
        finally:
            if server is not None:
                quit_result = subprocess.run(
                    [ns.nginx, "-s", "quit", "-c", str(config), "-p", str(temp)],
                    capture_output=True,
                    text=True,
                )
                stdout_parts.append(quit_result.stdout)
                stderr_parts.append(quit_result.stderr)
                try:
                    server_out, server_err = server.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    server.terminate()
                    try:
                        server_out, server_err = server.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        server.kill()
                        server_out, server_err = server.communicate()
                stdout_parts.append(server_out)
                stderr_parts.append(server_err)

    stdout_log.write_text("\n".join(part for part in stdout_parts if part), encoding="utf-8")
    stderr_log.write_text("\n".join(part for part in stderr_parts if part), encoding="utf-8")
    print(
        json.dumps(
            {
                "metrics": metrics,
                "metadata": {"tool": "nginx_wrk2", "returncode": returncode},
                "raw": {"stdout_path": str(stdout_log), "stderr_path": str(stderr_log)},
            },
            sort_keys=True,
        )
    )
    return returncode


def parse_metrics(text: str) -> dict[str, float]:
    metrics: dict[str, float] = {}
    rps = re.search(r"Requests/sec:\s+([\d.]+)", text)
    if rps:
        metrics["requests_per_second"] = float(rps.group(1))
    transfer = re.search(r"Transfer/sec:\s+([\d.]+)([KMGT]?B)", text)
    if transfer:
        metrics["transfer_bytes_per_sec"] = _bytes(float(transfer.group(1)), transfer.group(2))
    total = re.search(r"(\d+)\s+requests in", text)
    if total:
        metrics["total_requests"] = float(total.group(1))
    for label, key in (
        ("50.000%", "p50_latency_ms"),
        ("75.000%", "p75_latency_ms"),
        ("90.000%", "p90_latency_ms"),
        ("99.000%", "p99_latency_ms"),
        ("99.900%", "p999_latency_ms"),
    ):
        match = re.search(rf"{re.escape(label)}\s+([\d.]+)(us|ms|s)", text)
        if match:
            metrics[key] = _seconds(float(match.group(1)), match.group(2)) * 1000.0
    return metrics


def _nginx_config(temp: Path, html: Path, port: str) -> Path:
    config = temp / "nginx.conf"
    config.write_text(
        f"""
worker_processes 1;
daemon off;
error_log stderr notice;
pid {temp / 'nginx.pid'};
events {{ worker_connections 512; }}
http {{
  access_log off;
  default_type application/octet-stream;
  sendfile on;
  server {{
    listen 127.0.0.1:{port};
    root {html};
    location / {{ try_files $uri /index.html; }}
  }}
}}
""".lstrip(),
        encoding="utf-8",
    )
    return config


def _wait_for_http(port: str) -> None:
    url = f"http://127.0.0.1:{port}/"
    for _attempt in range(40):
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                if response.status == 200:
                    return
        except Exception:
            pass
        time.sleep(0.25)
    raise RuntimeError("nginx did not become ready")


def _bytes(value: float, unit: str) -> float:
    multipliers = {"B": 1, "KB": 1024, "MB": 1024**2, "GB": 1024**3, "TB": 1024**4}
    return value * multipliers.get(unit, 1)


def _seconds(value: float, unit: str) -> float:
    if unit == "us":
        return value / 1_000_000.0
    if unit == "ms":
        return value / 1000.0
    return value


if __name__ == "__main__":
    raise SystemExit(main())
