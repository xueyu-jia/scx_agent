from __future__ import annotations

import json
import os
import subprocess
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from bench.collectors.guest import parse_bench_metrics, write_guest_script

from bench.config.parser import RunSpec, parse_cpu_list


class PreflightError(RuntimeError):
    """Raised when host isolation requirements are not satisfied."""


def run_specs(
    specs: list[RunSpec],
    output_dir: str | Path,
    dry_run: bool = False,
    label: str = "candidate",
    scheduler: dict[str, Any] | None = None,
    config_path: str | None = None,
) -> Path:
    started_at = datetime.now(timezone.utc)
    result_dir = Path(output_dir)
    result_dir.mkdir(parents=True, exist_ok=True)

    _append_manifest(
        result_dir,
        {
            "started_at": started_at.isoformat(),
            "dry_run": dry_run,
            "label": label,
            "scheduler": scheduler or {"kind": "builtin"},
            "config": config_path,
            "run_count": len(specs),
            "runs": [_manifest_entry(spec, label, scheduler) for spec in specs],
        },
    )

    for spec in specs:
        _run_one(spec, result_dir, dry_run, label, scheduler or {"kind": "builtin"})

    return result_dir


def _run_one(
    spec: RunSpec,
    result_dir: Path,
    dry_run: bool,
    label: str,
    scheduler: dict[str, Any],
) -> None:
    run_dir = result_dir / _run_dir_name(spec)
    run_dir.mkdir(parents=True, exist_ok=False)

    guest_script = run_dir / "run_guest.sh"
    guest_output_dir = str(run_dir.resolve())
    write_guest_script(
        guest_script,
        _bench_command(spec),
        spec.bench.get("env", {}),
        scheduler=scheduler,
        output_dir=guest_output_dir,
    )
    command = _command(spec, run_dir, guest_output_dir)
    metadata = {
        "spec": _manifest_entry(spec, label, scheduler),
        "command": command,
        "dry_run": dry_run,
        "started_at": datetime.now(timezone.utc).isoformat(),
    }

    if dry_run:
        metadata.update(
            {
                "status": "DRY_RUN",
                "returncode": None,
                "finished_at": datetime.now(timezone.utc).isoformat(),
                "duration_seconds": 0,
                "bench_metrics": {},
            }
        )
        _write_json(run_dir / "result.json", metadata)
        return

    try:
        _preflight_machine(spec)
    except PreflightError as exc:
        metadata.update(
            {
                "status": "PREFLIGHT_FAILED",
                "returncode": None,
                "finished_at": datetime.now(timezone.utc).isoformat(),
                "duration_seconds": 0,
                "bench_metrics": {},
                "error": str(exc),
            }
        )
        _write_json(run_dir / "result.json", metadata)
        (run_dir / "stdout.log").write_text("", encoding="utf-8")
        (run_dir / "stderr.log").write_text(str(exc), encoding="utf-8")
        return

    started_at = datetime.now(timezone.utc)
    try:
        completed = subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
            env=os.environ.copy(),
            timeout=_vng_timeout(spec),
        )
        vng_returncode: int | None = completed.returncode
        vng_stdout = completed.stdout
        vng_stderr = completed.stderr
    except FileNotFoundError as exc:
        status = "VNG_NOT_FOUND"
        vng_stdout = ""
        vng_stderr = str(exc)
        vng_returncode = None
    except subprocess.TimeoutExpired as exc:
        status = "TIMEOUT"
        vng_stdout = _ensure_text(exc.stdout)
        vng_stderr = _ensure_text(exc.stderr)
        vng_returncode = None

    finished_at = datetime.now(timezone.utc)
    duration = (finished_at - started_at).total_seconds()

    (run_dir / "vng_stdout.log").write_text(vng_stdout, encoding="utf-8")
    (run_dir / "vng_stderr.log").write_text(vng_stderr, encoding="utf-8")

    guest_result = _read_guest_result(run_dir / "guest_result.json")
    bench_metrics = parse_bench_metrics(run_dir / "stdout.log")
    _write_json(run_dir / "bench_metrics.json", bench_metrics)

    if "status" not in locals():
        bench_returncode = guest_result.get("bench_returncode")
        scheduler_returncode = guest_result.get("scheduler_start_returncode")
        if vng_returncode == 0 and scheduler_returncode == 0 and bench_returncode == 0:
            status = "PASS"
        elif scheduler_returncode not in (None, 0):
            status = "SCHEDULER_FAILED"
        elif bench_returncode not in (None, 0):
            status = "BENCH_FAILED"
        else:
            status = "FAILED"

    metadata.update(
        {
            "status": status,
            "returncode": vng_returncode,
            "vng_returncode": vng_returncode,
            "guest_result": guest_result,
            "finished_at": finished_at.isoformat(),
            "duration_seconds": duration,
            "bench_metrics": bench_metrics,
        }
    )
    _write_json(run_dir / "result.json", metadata)


def _bench_command(spec: RunSpec) -> list[str]:
    return [spec.bench["command"], *spec.bench.get("args", [])]


def _command(spec: RunSpec, run_dir: Path, guest_output_dir: str) -> list[str]:
    command = [
        "vng",
        "--run",
        spec.vng["kernel"],
    ]
    if spec.vng.get("skip_modules", True):
        command.append("--skip-modules")
    command.extend(
        [
            "--user",
            spec.vng.get("user", "root"),
            "--pin",
            spec.machine["pin_cpus"],
            "--cpus",
            str(spec.machine["vcpus"]),
            "--memory",
            spec.machine["memory"],
            "--cwd",
            str(Path.cwd()),
            "--rwdir",
            str(run_dir.resolve()),
            "--exec",
            f"/bin/sh {guest_output_dir}/run_guest.sh",
        ]
    )
    return command


def _vng_timeout(spec: RunSpec) -> int | None:
    bench_timeout = spec.bench.get("timeout_seconds")
    if bench_timeout is None:
        return None
    return bench_timeout + spec.vng.get("timeout_extra_seconds", 120)


def _run_dir_name(spec: RunSpec) -> str:
    return (
        f"run_{spec.run_index:03d}"
        f"__machine_{spec.machine_name}"
        f"__suite_{spec.suite_name}"
        f"__bench_{spec.bench_name}"
    )


def _manifest_entry(
    spec: RunSpec,
    label: str | None = None,
    scheduler: dict[str, Any] | None = None,
) -> dict[str, Any]:
    return {
        "label": label,
        "scheduler_config": scheduler,
        "plan": spec.plan,
        "run_index": spec.run_index,
        "machine": spec.machine_name,
        "suite": spec.suite_name,
        "bench": spec.bench_name,
        "metric_profile": spec.metric_profile_name,
        "machine_config": spec.machine,
        "bench_config": spec.bench,
        "metric_profile_config": spec.metric_profile,
        "vng_config": spec.vng,
    }


def _preflight_machine(spec: RunSpec) -> None:
    errors: list[str] = []
    pin_cpus = parse_cpu_list(spec.machine["pin_cpus"])
    missing = [cpu for cpu in pin_cpus if not Path(f"/sys/devices/system/cpu/cpu{cpu}").exists()]
    if missing:
        errors.append(f"pinned CPU(s) do not exist on host: {missing}")

    if spec.machine.get("exclusive") is True:
        isolated = _read_isolated_cpus()
        not_isolated = sorted(set(pin_cpus) - set(isolated))
        if not_isolated:
            errors.append(
                "exclusive CPU requirement is not satisfied; "
                f"CPU(s) not isolated: {not_isolated}"
            )

    frequency = spec.machine.get("frequency", {})
    if frequency.get("fixed") is True:
        errors.extend(_check_fixed_frequency(pin_cpus, frequency.get("governor")))

    if errors:
        raise PreflightError(
            "; ".join(errors)
            + "; prepare host isolation first: "
            + "sudo python3 bench/scripts/isolation.py prepare "
            + f"--config bench/configs/example.config --plan {spec.plan}"
        )


def _read_isolated_cpus() -> list[int]:
    path = Path("/sys/devices/system/cpu/isolated")
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8").strip()
    if not text:
        return []
    return parse_cpu_list(text)


def _check_fixed_frequency(pin_cpus: list[int], expected_governor: str | None) -> list[str]:
    errors: list[str] = []
    for cpu in pin_cpus:
        cpufreq = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq")
        if not cpufreq.exists():
            errors.append(f"CPU {cpu} does not expose cpufreq controls")
            continue

        min_freq = _read_optional(cpufreq / "scaling_min_freq")
        max_freq = _read_optional(cpufreq / "scaling_max_freq")
        if min_freq is None or max_freq is None:
            errors.append(f"CPU {cpu} is missing scaling_min_freq or scaling_max_freq")
            continue
        if min_freq != max_freq:
            errors.append(
                f"CPU {cpu} frequency is not fixed: scaling_min_freq={min_freq}, "
                f"scaling_max_freq={max_freq}"
            )

        if expected_governor is not None:
            governor = _read_optional(cpufreq / "scaling_governor")
            if governor is None:
                errors.append(f"CPU {cpu} is missing scaling_governor")
                continue
            if governor != expected_governor:
                errors.append(
                    f"CPU {cpu} governor is {governor}, expected {expected_governor}"
                )
    return errors


def _read_optional(path: Path) -> str | None:
    if not path.exists():
        return None
    return path.read_text(encoding="utf-8").strip()


def _ensure_text(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return str(value)


def _write_json(path: Path, data: Any) -> None:
    path.write_text(json.dumps(data, indent=2, sort_keys=True), encoding="utf-8")


def _read_guest_result(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        return {"parse_error": str(exc)}
    return data if isinstance(data, dict) else {"parse_error": "guest_result is not an object"}


def _append_manifest(result_dir: Path, entry: dict[str, Any]) -> None:
    path = result_dir / "manifest.json"
    if path.exists():
        try:
            manifest = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            manifest = {}
    else:
        manifest = {}

    batches = manifest.get("batches", [])
    if not isinstance(batches, list):
        batches = []
    batches.append(entry)

    all_runs = []
    for batch in batches:
        if isinstance(batch, dict) and isinstance(batch.get("runs"), list):
            all_runs.extend(batch["runs"])

    manifest.update(
        {
            "label": entry["label"],
            "scheduler": entry["scheduler"],
            "config": entry["config"],
            "dry_run": entry["dry_run"],
            "run_count": len(all_runs),
            "runs": all_runs,
            "batches": batches,
        }
    )
    _write_json(path, manifest)
