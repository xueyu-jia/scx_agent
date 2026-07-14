from __future__ import annotations

import json
import hashlib
import os
import re
import shlex
import shutil
import subprocess
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable
from xml.etree import ElementTree as ET

from bench.collectors.guest import (
    GUEST_EXECUTOR_PATH,
    GUEST_EXECUTOR_SOURCE,
    GUEST_OUTPUT_DIR,
    GUEST_PLAN_PATH,
    build_guest_run_plan,
    write_guest_plan,
)
from bench.metrics import load_bench_metrics, load_perf_stat_metrics

from bench.config.parser import RunSpec, parse_cpu_list


_MANIFEST_LOCK = threading.Lock()
REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_RUNTIME_DIR = Path("/var/lib/libvirt/scx-bench-runs")
RUNTIME_ISOLATION_REPORT = Path("/var/lib/scx-bench/runtime-isolation.json")


class PreflightError(RuntimeError):
    """Raised when host isolation requirements are not satisfied."""


class BootTimeout(RuntimeError):
    """Raised when the guest does not become reachable after VM start."""


def run_specs(
    specs: list[RunSpec],
    output_dir: str | Path,
    dry_run: bool = False,
    label: str = "candidate",
    scheduler: dict[str, Any] | None = None,
    config_path: str | None = None,
    progress_callback: Callable[[str, dict[str, Any]], None] | None = None,
    progress_interval: int = 30,
    placement: dict[str, Any] | None = None,
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
            "placement": placement,
            "runs": [_manifest_entry(spec, label, scheduler, placement) for spec in specs],
        },
    )

    for spec in specs:
        _emit_progress(progress_callback, "start", {"label": label, "spec": spec})
        result = _run_one(
            spec,
            result_dir,
            dry_run,
            label,
            scheduler or {"kind": "builtin"},
            progress_callback,
            progress_interval,
            placement,
        )
        _emit_progress(progress_callback, "end", {"label": label, "spec": spec, "result": result})

    return result_dir


def _run_one(
    spec: RunSpec,
    result_dir: Path,
    dry_run: bool,
    label: str,
    scheduler: dict[str, Any],
    progress_callback: Callable[[str, dict[str, Any]], None] | None,
    progress_interval: int,
    placement: dict[str, Any] | None,
) -> dict[str, Any]:
    run_dir = result_dir / _run_dir_name(spec)
    run_dir.mkdir(parents=True, exist_ok=False)

    guest_plan_path = run_dir / "guest_plan.json"
    guest_output_dir = spec.libvirt.get("guest_output_dir", GUEST_OUTPUT_DIR)
    guest_scheduler = _guest_scheduler(scheduler)
    guest_plan = build_guest_run_plan(
        spec.bench,
        guest_scheduler,
        spec.libvirt,
        output_dir=guest_output_dir,
    )
    write_guest_plan(guest_plan_path, guest_plan)
    domain_name = _domain_name(label, spec, run_dir)
    runtime_dir = _runtime_run_dir(spec.libvirt, domain_name)
    overlay_path = runtime_dir / "disk.qcow2"
    domain_xml = _domain_xml(
        spec,
        domain_name,
        overlay_path,
        placement,
    )
    domain_xml_path = run_dir / "domain.xml"
    domain_xml_path.write_text(domain_xml, encoding="utf-8")
    if placement is not None:
        _write_json(run_dir / "placement.json", placement)
    metadata = {
        "spec": _manifest_entry(spec, label, scheduler, placement),
        "domain": domain_name,
        "domain_xml": str(domain_xml_path),
        "runtime_dir": str(runtime_dir),
        "runtime_overlay": str(overlay_path),
        "dry_run": dry_run,
        "placement": placement,
        "execution_plan": guest_plan.to_dict(),
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
        return metadata

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
        return metadata

    started_at = datetime.now(timezone.utc)
    completed = _run_libvirt(
        spec,
        run_dir,
        runtime_dir,
        domain_name,
        domain_xml_path,
        overlay_path,
        guest_plan_path,
        guest_output_dir,
        scheduler_host_command=scheduler.get("host_command"),
        scheduler_host_kconfig=scheduler.get("host_kconfig"),
        timeout=guest_plan.host_timeout_seconds(
            int(spec.libvirt.get("timeout_extra_seconds", 120))
        ),
        boot_timeout=_boot_timeout(spec),
        progress_interval=progress_interval,
        heartbeat=lambda elapsed: _emit_progress(
            progress_callback,
            "heartbeat",
            {"label": label, "spec": spec, "elapsed_seconds": elapsed},
        ),
    )
    vm_returncode = completed["returncode"]
    libvirt_stdout = completed["stdout"]
    libvirt_stderr = completed["stderr"]
    status = completed["status"]
    host_thread_pinning = completed.get("host_thread_pinning", {})
    host_irq_delta = completed.get("host_irq_delta", {})
    guest_executor = completed.get("guest_executor", {})

    finished_at = datetime.now(timezone.utc)
    duration = (finished_at - started_at).total_seconds()

    (run_dir / "libvirt_stdout.log").write_text(libvirt_stdout, encoding="utf-8")
    (run_dir / "libvirt_stderr.log").write_text(libvirt_stderr, encoding="utf-8")

    guest_result = _read_guest_result(run_dir / "guest_result.json")
    bench_metrics = load_bench_metrics(run_dir / "stdout.log")
    metrics = bench_metrics.setdefault("metrics", {})
    if isinstance(metrics, dict):
        metrics.update(load_perf_stat_metrics(run_dir / "perf_stat.csv"))
    _write_json(run_dir / "bench_metrics.json", bench_metrics)

    if status is None:
        status = _guest_run_status(vm_returncode, guest_result, host_irq_delta)

    metadata.update(
        {
            "status": status,
            "returncode": vm_returncode,
            "vm_returncode": vm_returncode,
            "libvirt_returncode": vm_returncode,
            "guest_result": guest_result,
            "failure_reason": guest_result.get("failure_reason", ""),
            "host_thread_pinning": host_thread_pinning,
            "host_irq_delta": host_irq_delta,
            "guest_executor": guest_executor,
            "finished_at": finished_at.isoformat(),
            "duration_seconds": duration,
            "bench_metrics": bench_metrics,
        }
    )
    _write_json(run_dir / "result.json", metadata)
    return metadata


def _run_libvirt(
    spec: RunSpec,
    run_dir: Path,
    runtime_dir: Path,
    domain_name: str,
    domain_xml_path: Path,
    overlay_path: Path,
    guest_plan_path: Path,
    guest_output_dir: str,
    scheduler_host_command: str | None,
    scheduler_host_kconfig: str | None,
    timeout: int | None,
    boot_timeout: int,
    progress_interval: int,
    heartbeat: Callable[[float], None],
) -> dict[str, Any]:
    stdout_parts: list[str] = []
    stderr_parts: list[str] = []
    libvirt = spec.libvirt
    status: str | None = None
    returncode: int | None = None
    host_thread_pinning: dict[str, Any] = {}
    host_irq_delta: dict[str, Any] = {}
    guest_executor: dict[str, str] = {}
    try:
        _prepare_runtime_dir(runtime_dir, stdout_parts, stderr_parts)
        _create_overlay(libvirt, overlay_path, stdout_parts, stderr_parts)
        _run_command(_virsh(libvirt, ["define", str(domain_xml_path)]), stdout_parts, stderr_parts)
        _run_command(_virsh(libvirt, ["start", domain_name]), stdout_parts, stderr_parts)
        host_thread_pinning = _apply_host_thread_pinning(libvirt, domain_name)

        host, port = _wait_for_ssh(libvirt, domain_name, boot_timeout, progress_interval, heartbeat)
        if scheduler_host_command:
            _stage_scheduler(
                libvirt,
                host,
                port,
                scheduler_host_command,
                scheduler_host_kconfig,
                stdout_parts,
                stderr_parts,
            )
        _scp_to_guest(
            libvirt,
            host,
            port,
            GUEST_EXECUTOR_SOURCE,
            GUEST_EXECUTOR_PATH,
            stdout_parts,
            stderr_parts,
        )
        _scp_to_guest(
            libvirt,
            host,
            port,
            guest_plan_path,
            GUEST_PLAN_PATH,
            stdout_parts,
            stderr_parts,
        )
        guest_executor = {
            "source": str(GUEST_EXECUTOR_SOURCE),
            "guest_path": GUEST_EXECUTOR_PATH,
            "sha256": _sha256_file(GUEST_EXECUTOR_SOURCE),
        }
        host_irq_before = _host_interrupt_snapshot()
        completed = _run_guest_command(
            libvirt,
            host,
            port,
            timeout,
            progress_interval,
            heartbeat,
            stdout_parts,
            stderr_parts,
        )
        host_irq_after = _host_interrupt_snapshot()
        host_irq_delta = _managed_irq_delta(spec, host_irq_before, host_irq_after)
        _copy_guest_output(
            libvirt,
            host,
            port,
            guest_output_dir,
            run_dir,
            stdout_parts,
            stderr_parts,
        )
        returncode = completed.returncode
    except FileNotFoundError as exc:
        status = "LIBVIRT_TOOL_NOT_FOUND"
        stderr_parts.append(str(exc))
    except BootTimeout as exc:
        status = "BOOT_TIMEOUT"
        stderr_parts.append(str(exc))
    except subprocess.TimeoutExpired as exc:
        status = "TIMEOUT"
        stderr_parts.append(_ensure_text(exc.stderr))
        stdout_parts.append(_ensure_text(exc.stdout))
    except subprocess.CalledProcessError as exc:
        status = "LIBVIRT_FAILED"
        returncode = exc.returncode
        stdout_parts.append(_ensure_text(exc.stdout))
        stderr_parts.append(_ensure_text(exc.stderr))
    except RuntimeError as exc:
        status = "LIBVIRT_FAILED"
        stderr_parts.append(str(exc))
    finally:
        try:
            if libvirt.get("destroy_on_exit", True):
                _cleanup_domain(libvirt, domain_name, stdout_parts, stderr_parts)
                _cleanup_runtime_dir(runtime_dir, stdout_parts, stderr_parts)
            else:
                stdout_parts.append(f"+ preserve runtime dir {runtime_dir}\n")
        except RuntimeError as exc:
            if status is None:
                status = "LIBVIRT_FAILED"
            stderr_parts.append(str(exc))

    return _libvirt_result(
        status,
        returncode,
        stdout_parts,
        stderr_parts,
        host_thread_pinning,
        host_irq_delta,
        guest_executor,
    )


def _guest_scheduler(scheduler: dict[str, Any]) -> dict[str, Any]:
    if scheduler.get("kind") != "scx" or not scheduler.get("host_command"):
        return scheduler

    guest_scheduler = dict(scheduler)
    guest_scheduler["command"] = "/tmp/scx-bench-scheduler"
    return guest_scheduler


def _stage_scheduler(
    libvirt: dict[str, Any],
    host: str,
    port: int,
    host_command: str,
    host_kconfig: str | None,
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> None:
    source = _resolve_host_path(host_command)
    if not source.is_file():
        raise RuntimeError(f"scheduler host_command does not exist: {source}")
    if not os.access(source, os.X_OK):
        raise RuntimeError(f"scheduler host_command is not executable: {source}")

    destination = "/tmp/scx-bench-scheduler"
    _scp_to_guest(libvirt, host, port, source, destination, stdout_parts, stderr_parts)
    _run_command(
        _ssh_command(libvirt, host, port, f"chmod 0755 {shlex.quote(destination)}"),
        stdout_parts,
        stderr_parts,
    )
    if host_kconfig:
        kconfig = _resolve_host_path(host_kconfig)
        if not kconfig.is_file():
            raise RuntimeError(f"scheduler host_kconfig does not exist: {kconfig}")
        _scp_to_guest(
            libvirt,
            host,
            port,
            kconfig,
            "/tmp/scx-bench-kconfig",
            stdout_parts,
            stderr_parts,
        )


def _resolve_host_path(value: str) -> Path:
    path = Path(value).expanduser()
    if not path.is_absolute():
        path = REPO_ROOT / path
    return path.resolve()


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _run_guest_command(
    libvirt: dict[str, Any],
    host: str,
    port: int,
    timeout: int | None,
    progress_interval: int,
    heartbeat: Callable[[float], None],
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> subprocess.CompletedProcess[str]:
    remote = (
        f"python3 {shlex.quote(GUEST_EXECUTOR_PATH)} "
        f"{shlex.quote(GUEST_PLAN_PATH)}"
    )
    command = _ssh_command(libvirt, host, port, remote)
    started_at = time.monotonic()
    last_heartbeat_at = started_at
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )

    while True:
        returncode = process.poll()
        if returncode is not None:
            stdout, stderr = process.communicate()
            stdout_parts.append(stdout)
            stderr_parts.append(stderr)
            return subprocess.CompletedProcess(command, returncode, stdout, stderr)

        now = time.monotonic()
        elapsed = now - started_at
        if timeout is not None and elapsed >= timeout:
            _kill_process_group(process)
            stdout, stderr = process.communicate()
            stdout_parts.append(stdout)
            stderr_parts.append(stderr)
            raise subprocess.TimeoutExpired(command, timeout, output=stdout, stderr=stderr)

        if progress_interval > 0 and now - last_heartbeat_at >= progress_interval:
            heartbeat(elapsed)
            last_heartbeat_at = now

        sleep_for = 1.0
        if timeout is not None:
            sleep_for = min(sleep_for, max(0.1, timeout - elapsed))
        time.sleep(sleep_for)


def _kill_process_group(process: subprocess.Popen[str]) -> None:
    try:
        os.killpg(process.pid, 9)
    except ProcessLookupError:
        return


def _apply_host_thread_pinning(libvirt: dict[str, Any], domain_name: str) -> dict[str, Any]:
    result: dict[str, Any] = {
        "domain": domain_name,
        "qemu_pid": None,
        "qemu_threads": [],
        "vhost_threads": [],
        "errors": [],
    }
    qemu_pid = _find_qemu_pid(domain_name)
    if qemu_pid is None:
        result["errors"].append(f"could not find QEMU process for domain {domain_name}")
        return result

    result["qemu_pid"] = qemu_pid
    result["qemu_threads"] = _thread_affinities(qemu_pid)

    if libvirt.get("pin_vhost_threads", False):
        cpuset = libvirt.get("iothread_cpus") or libvirt.get("emulator_cpus")
        if cpuset:
            result["vhost_threads"] = _pin_vhost_threads(qemu_pid, parse_cpu_list(cpuset))
        else:
            result["errors"].append("pin_vhost_threads is true but no iothread/emulator CPUs are configured")
    return result


def _find_qemu_pid(domain_name: str) -> int | None:
    for path in sorted(Path("/proc").glob("[0-9]*/cmdline")):
        try:
            raw = path.read_bytes()
        except OSError:
            continue
        text = raw.replace(b"\0", b" ").decode("utf-8", errors="replace")
        if domain_name not in text:
            continue
        if "qemu-system" not in text and "qemu-kvm" not in text:
            continue
        return int(path.parent.name)
    return None


def _thread_affinities(pid: int) -> list[dict[str, Any]]:
    threads: list[dict[str, Any]] = []
    for task in sorted(Path(f"/proc/{pid}/task").glob("[0-9]*")):
        tid = int(task.name)
        comm = _read_optional(task / "comm") or ""
        try:
            affinity = sorted(os.sched_getaffinity(tid))
        except OSError as exc:
            threads.append({"tid": tid, "comm": comm, "error": str(exc)})
            continue
        threads.append({"tid": tid, "comm": comm, "affinity": _format_cpu_list(affinity)})
    return threads


def _pin_vhost_threads(qemu_pid: int, cpus: list[int]) -> list[dict[str, Any]]:
    pinned: list[dict[str, Any]] = []
    target = set(cpus)
    for comm_path in sorted(Path("/proc").glob("[0-9]*/comm")):
        tid = int(comm_path.parent.name)
        comm = _read_optional(comm_path) or ""
        if not _is_vhost_thread_for_qemu(comm, qemu_pid):
            continue
        entry: dict[str, Any] = {
            "tid": tid,
            "comm": comm,
            "target_affinity": _format_cpu_list(cpus),
        }
        try:
            before = sorted(os.sched_getaffinity(tid))
            os.sched_setaffinity(tid, target)
            after = sorted(os.sched_getaffinity(tid))
            entry["before_affinity"] = _format_cpu_list(before)
            entry["after_affinity"] = _format_cpu_list(after)
        except OSError as exc:
            entry["error"] = str(exc)
        pinned.append(entry)
    return pinned


def _is_vhost_thread_for_qemu(comm: str, qemu_pid: int) -> bool:
    return comm == f"vhost-{qemu_pid}" or comm.startswith(f"vhost-{qemu_pid}-")


def _host_interrupt_snapshot() -> dict[str, dict[str, Any]]:
    path = Path("/proc/interrupts")
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return {}

    snapshot: dict[str, dict[str, Any]] = {}
    for line in lines:
        stripped = line.lstrip()
        if ":" not in stripped:
            continue
        irq, rest = stripped.split(":", 1)
        if not irq.isdigit():
            continue
        tokens = rest.split()
        counts: list[int] = []
        index = 0
        while index < len(tokens) and tokens[index].isdigit():
            counts.append(int(tokens[index]))
            index += 1
        snapshot[irq] = {
            "counts": counts,
            "actions": " ".join(tokens[index:]),
        }
    return snapshot


def _managed_irq_delta(
    spec: RunSpec,
    before: dict[str, dict[str, Any]],
    after: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    pinned_cpus = parse_cpu_list(spec.machine["pin_cpus"])
    report = _read_runtime_isolation_report()
    unmovable_irqs = _unmovable_irqs(report)
    contaminated: list[dict[str, Any]] = []

    for irq in sorted(unmovable_irqs, key=int):
        before_counts = before.get(irq, {}).get("counts", [])
        after_counts = after.get(irq, {}).get("counts", [])
        per_cpu_delta: dict[str, int] = {}
        total_delta = 0
        for cpu in pinned_cpus:
            before_value = before_counts[cpu] if cpu < len(before_counts) else 0
            after_value = after_counts[cpu] if cpu < len(after_counts) else 0
            delta = max(0, after_value - before_value)
            if delta:
                per_cpu_delta[str(cpu)] = delta
                total_delta += delta
        if total_delta <= 0:
            continue
        report_entry = report.get("irqs", {}).get(irq, {})
        contaminated.append(
            {
                "irq": irq,
                "actions": report_entry.get("actions") or after.get(irq, {}).get("actions", ""),
                "smp_affinity": report_entry.get("smp_affinity", ""),
                "effective_affinity": report_entry.get("effective_affinity", ""),
                "pinned_cpu_delta": per_cpu_delta,
                "total_pinned_delta": total_delta,
            }
        )

    return {
        "managed_irq_policy": "fail_on_delta",
        "pinned_cpus": _format_cpu_list(pinned_cpus),
        "unmovable_irqs": sorted(unmovable_irqs, key=int),
        "contaminated_irqs": contaminated,
    }


def _boot_timeout(spec: RunSpec) -> int:
    return int(spec.libvirt.get("boot_timeout_seconds", 10))


def _guest_run_status(
    vm_returncode: int | None,
    guest_result: dict[str, Any],
    host_irq_delta: dict[str, Any],
) -> str:
    guest_status = str(guest_result.get("status", "")).upper()
    valid_statuses = {
        "PASS",
        "SCHEDULER_FAILED",
        "WARMUP_FAILED",
        "WARMUP_TIMEOUT",
        "BENCH_FAILED",
        "BENCH_TIMEOUT",
        "INTERNAL_ERROR",
    }
    status = guest_status if guest_status in valid_statuses else "FAILED"
    if status == "PASS" and vm_returncode != 0:
        status = "FAILED"

    if host_irq_delta.get("contaminated_irqs") and status == "PASS":
        return "INTERRUPT_CONTAMINATED"
    return status


def _domain_xml(
    spec: RunSpec,
    domain_name: str,
    overlay_path: Path,
    placement: dict[str, Any] | None,
) -> str:
    libvirt = spec.libvirt
    domain = ET.Element("domain", {"type": "kvm"})
    ET.SubElement(domain, "name").text = domain_name
    ET.SubElement(domain, "memory", {"unit": "MiB"}).text = str(_memory_mib(spec.machine["memory"]))
    ET.SubElement(domain, "currentMemory", {"unit": "MiB"}).text = str(
        _memory_mib(spec.machine["memory"])
    )
    ET.SubElement(domain, "vcpu", {"placement": "static"}).text = str(spec.machine["vcpus"])
    iothread_cpus = libvirt.get("iothread_cpus")
    if iothread_cpus:
        ET.SubElement(domain, "iothreads").text = "1"

    cputune = ET.SubElement(domain, "cputune")
    pin_cpus = parse_cpu_list(spec.machine["pin_cpus"])
    for index, cpu in enumerate(pin_cpus):
        ET.SubElement(cputune, "vcpupin", {"vcpu": str(index), "cpuset": str(cpu)})
    ET.SubElement(cputune, "emulatorpin", {"cpuset": libvirt["emulator_cpus"]})
    if iothread_cpus:
        ET.SubElement(cputune, "iothreadpin", {"iothread": "1", "cpuset": iothread_cpus})

    os_node = ET.SubElement(domain, "os")
    ET.SubElement(os_node, "type", {"arch": "x86_64"}).text = "hvm"
    if libvirt.get("kernel"):
        ET.SubElement(os_node, "kernel").text = libvirt["kernel"]
    if libvirt.get("initrd"):
        ET.SubElement(os_node, "initrd").text = libvirt["initrd"]
    if libvirt.get("kernel_args"):
        ET.SubElement(os_node, "cmdline").text = libvirt["kernel_args"]

    features = ET.SubElement(domain, "features")
    ET.SubElement(features, "acpi")
    ET.SubElement(features, "apic")
    ET.SubElement(domain, "cpu", {"mode": libvirt.get("cpu_mode", "host-passthrough"), "check": "none"})
    ET.SubElement(domain, "clock", {"offset": "utc"})
    ET.SubElement(domain, "on_poweroff").text = "destroy"
    ET.SubElement(domain, "on_reboot").text = "restart"
    ET.SubElement(domain, "on_crash").text = "destroy"

    devices = ET.SubElement(domain, "devices")
    disk = ET.SubElement(devices, "disk", {"type": "file", "device": "disk"})
    disk_driver = {"name": "qemu", "type": "qcow2", "cache": "none"}
    if iothread_cpus:
        disk_driver.update({"io": "threads", "iothread": "1"})
    ET.SubElement(disk, "driver", disk_driver)
    ET.SubElement(disk, "source", {"file": str(overlay_path.resolve())})
    ET.SubElement(disk, "target", {"dev": "vda", "bus": "virtio"})

    if libvirt.get("network") is not None:
        interface = ET.SubElement(devices, "interface", {"type": "network"})
        ET.SubElement(interface, "source", {"network": libvirt.get("network", "default")})
        if libvirt.get("pin_vhost_threads", False):
            ET.SubElement(interface, "driver", {"name": "vhost"})
        ET.SubElement(interface, "model", {"type": "virtio"})

    ET.indent(domain)
    return ET.tostring(domain, encoding="unicode") + "\n"


def _create_overlay(
    libvirt: dict[str, Any],
    overlay_path: Path,
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> None:
    base = Path(libvirt["root_image"])
    if not base.exists():
        raise RuntimeError(f"libvirt.root_image does not exist: {base}")
    command = [
        "qemu-img",
        "create",
        "-f",
        "qcow2",
        "-F",
        "qcow2",
        "-b",
        str(base),
        str(overlay_path),
    ]
    _run_command(command, stdout_parts, stderr_parts)
    overlay_path.chmod(0o666)


def _runtime_run_dir(libvirt: dict[str, Any], domain_name: str) -> Path:
    return Path(libvirt.get("runtime_dir") or DEFAULT_RUNTIME_DIR) / domain_name


def _prepare_runtime_dir(
    runtime_dir: Path,
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> None:
    if runtime_dir.exists():
        _remove_path(runtime_dir, stdout_parts, stderr_parts)
    runtime_dir.mkdir(parents=True, exist_ok=False)
    runtime_dir.chmod(0o777)
    stdout_parts.append(f"+ prepare runtime dir {runtime_dir}\n")


def _cleanup_runtime_dir(
    runtime_dir: Path,
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> None:
    if not runtime_dir.exists():
        return
    _remove_path(runtime_dir, stdout_parts, stderr_parts)


def _wait_for_ssh(
    libvirt: dict[str, Any],
    domain_name: str,
    timeout: int | None,
    progress_interval: int,
    heartbeat: Callable[[float], None],
) -> tuple[str, int]:
    configured_host = libvirt.get("ssh_host")
    port = int(libvirt.get("ssh_port", 22))
    started_at = time.monotonic()
    last_heartbeat_at = started_at
    poll_interval = 1.0

    while True:
        host = configured_host or _domain_ip(libvirt, domain_name)
        elapsed = time.monotonic() - started_at
        remaining = None if timeout is None else max(0.1, timeout - elapsed)
        probe_timeout = 1 if remaining is None else min(1, remaining)
        if host and _ssh_probe(libvirt, host, port, probe_timeout):
            return host, port

        elapsed = time.monotonic() - started_at
        if timeout is not None and elapsed >= timeout:
            raise BootTimeout(f"guest SSH did not become ready for domain {domain_name}")

        now = time.monotonic()
        if progress_interval > 0 and now - last_heartbeat_at >= progress_interval:
            heartbeat(elapsed)
            last_heartbeat_at = now

        sleep_for = poll_interval
        if timeout is not None:
            sleep_for = min(sleep_for, max(0.1, timeout - elapsed))
        time.sleep(sleep_for)


def _domain_ip(libvirt: dict[str, Any], domain_name: str) -> str | None:
    for source in ("lease", "agent"):
        completed = subprocess.run(
            _virsh(libvirt, ["domifaddr", domain_name, "--source", source]),
            check=False,
            capture_output=True,
            text=True,
        )
        match = re.search(r"(\d+\.\d+\.\d+\.\d+)/\d+", completed.stdout)
        if match:
            return match.group(1)
    return None


def _ssh_probe(libvirt: dict[str, Any], host: str, port: int, timeout: int) -> bool:
    try:
        completed = subprocess.run(
            _ssh_command(libvirt, host, port, "true"),
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return completed.returncode == 0
    except subprocess.TimeoutExpired:
        return False


def _remove_path(
    path: Path,
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> None:
    try:
        if path.is_dir():
            shutil.rmtree(path)
        else:
            path.unlink()
        stdout_parts.append(f"+ remove path {path}\n")
    except OSError as exc:
        raise RuntimeError(
            f"runtime path is not removable: {path}: {exc}; "
            "run prepare_env.py init to configure libvirt/qemu as the benchmark user"
        ) from exc


def _scp_to_guest(
    libvirt: dict[str, Any],
    host: str,
    port: int,
    src: Path,
    dst: str,
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> None:
    target = f"{libvirt['ssh_user']}@{host}:{dst}"
    _run_command([*_scp_base(libvirt, port), str(src), target], stdout_parts, stderr_parts)


def _copy_guest_output(
    libvirt: dict[str, Any],
    host: str,
    port: int,
    guest_output_dir: str,
    dst: Path,
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> None:
    remote = f"tar -C {shlex.quote(guest_output_dir)} -cf - ."
    ssh_command = _ssh_command(libvirt, host, port, remote)
    stdout_parts.append(f"+ {shlex.join(ssh_command)} | tar -C {shlex.quote(str(dst))} -xf -\n")
    remote_tar = subprocess.run(
        ssh_command,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    stderr_parts.append(_ensure_text(remote_tar.stderr))
    local_tar = subprocess.run(
        ["tar", "-C", str(dst), "-xf", "-"],
        input=remote_tar.stdout,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    stdout_parts.append(_ensure_text(local_tar.stdout))
    stderr_parts.append(_ensure_text(local_tar.stderr))


def _ssh_command(libvirt: dict[str, Any], host: str, port: int, remote_command: str) -> list[str]:
    return [
        "ssh",
        *_ssh_options(libvirt, port),
        f"{libvirt['ssh_user']}@{host}",
        remote_command,
    ]


def _ssh_options(libvirt: dict[str, Any], port: int) -> list[str]:
    return [
        "-i",
        libvirt["ssh_key"],
        "-p",
        str(port),
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
    ]


def _scp_base(libvirt: dict[str, Any], port: int) -> list[str]:
    return [
        "scp",
        "-i",
        libvirt["ssh_key"],
        "-P",
        str(port),
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
    ]


def _virsh(libvirt: dict[str, Any], args: list[str]) -> list[str]:
    return ["virsh", "--connect", libvirt.get("uri", "qemu:///system"), *args]


def _run_command(
    command: list[str],
    stdout_parts: list[str],
    stderr_parts: list[str],
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(command, check=check, capture_output=True, text=True)
    stdout_parts.append(f"+ {shlex.join(command)}\n")
    stdout_parts.append(completed.stdout)
    stderr_parts.append(completed.stderr)
    return completed


def _cleanup_domain(
    libvirt: dict[str, Any],
    domain_name: str,
    stdout_parts: list[str],
    stderr_parts: list[str],
) -> None:
    for args in (["destroy", domain_name], ["undefine", domain_name]):
        try:
            _run_command(_virsh(libvirt, args), stdout_parts, stderr_parts, check=False)
        except FileNotFoundError:
            return


def _libvirt_result(
    status: str | None,
    returncode: int | None,
    stdout_parts: list[str],
    stderr_parts: list[str],
    host_thread_pinning: dict[str, Any],
    host_irq_delta: dict[str, Any],
    guest_executor: dict[str, str],
) -> dict[str, Any]:
    return {
        "status": status,
        "returncode": returncode,
        "stdout": "".join(stdout_parts),
        "stderr": "".join(stderr_parts),
        "host_thread_pinning": host_thread_pinning,
        "host_irq_delta": host_irq_delta,
        "guest_executor": guest_executor,
    }


def _domain_name(label: str, spec: RunSpec, run_dir: Path) -> str:
    suffix = hashlib.sha1(str(run_dir.resolve()).encode("utf-8")).hexdigest()[:8]
    raw = (
        f"scxbench-{label}-{spec.plan}-{spec.run_index}-"
        f"{spec.machine_name}-{spec.suite_name}-{spec.bench_name}-{suffix}"
    )
    return _safe_name(raw)[:63]


def _safe_name(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in ("-", "_") else "-" for ch in value)


def _memory_mib(value: str) -> int:
    bytes_value = _parse_memory_bytes(value)
    return max(1, bytes_value // 1024**2)


def _parse_memory_bytes(value: str) -> int:
    text = value.strip().upper()
    units = {"K": 1024, "M": 1024**2, "G": 1024**3, "T": 1024**4}
    if text[-1:] in units:
        number = text[:-1]
        multiplier = units[text[-1]]
    else:
        number = text
        multiplier = 1
    if not number.isdigit():
        raise RuntimeError(f"invalid memory size: {value}")
    return int(number) * multiplier


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
    placement: dict[str, Any] | None = None,
) -> dict[str, Any]:
    return {
        "label": label,
        "scheduler_config": scheduler,
        "placement": placement,
        "execution_plan": build_guest_run_plan(
            spec.bench,
            _guest_scheduler(scheduler or {"kind": "builtin"}),
            spec.libvirt,
            output_dir=spec.libvirt.get("guest_output_dir", GUEST_OUTPUT_DIR),
        ).to_dict(),
        "plan": spec.plan,
        "run_index": spec.run_index,
        "machine": spec.machine_name,
        "suite": spec.suite_name,
        "bench": spec.bench_name,
        "metric_profile": spec.metric_profile_name,
        "machine_config": spec.machine,
        "bench_config": spec.bench,
        "metric_profile_config": spec.metric_profile,
        "libvirt_config": spec.libvirt,
        "executor_config": spec.executor,
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

    errors.extend(_check_irq_runtime_isolation(spec, pin_cpus))

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


def _check_irq_runtime_isolation(spec: RunSpec, pin_cpus: list[int]) -> list[str]:
    errors: list[str] = []
    irq_cpus_text = spec.executor.get("irq_cpus")
    if not irq_cpus_text:
        return errors

    report = _read_runtime_isolation_report()
    if not report:
        errors.append(
            f"runtime isolation report is missing: {RUNTIME_ISOLATION_REPORT}; "
            "restart scx-bench-isolation.service or run isolation.py apply-runtime"
        )
    elif report.get("errors"):
        errors.append("runtime IRQ isolation errors: " + "; ".join(map(str, report["errors"])))

    irq_cpus = parse_cpu_list(irq_cpus_text)
    missing = [cpu for cpu in irq_cpus if not Path(f"/sys/devices/system/cpu/cpu{cpu}").exists()]
    if missing:
        errors.append(f"IRQ CPU(s) do not exist on host: {missing}")
    overlap = sorted(set(irq_cpus) & set(pin_cpus))
    if overlap:
        errors.append(f"executor.irq_cpus overlaps pinned VM CPU(s): {overlap}")

    irq_mismatches = _irq_affinity_mismatches(pin_cpus, _unmovable_irqs(report), limit=5)
    if irq_mismatches:
        errors.append(
            "IRQ affinity is not fully isolated; examples: "
            + ", ".join(irq_mismatches)
        )
    net_mismatches = _net_queue_mask_mismatches(pin_cpus, limit=5)
    if net_mismatches:
        errors.append(
            "network RPS/XPS masks include pinned VM CPU(s); examples: "
            + ", ".join(net_mismatches)
        )

    return errors


def _read_runtime_isolation_report() -> dict[str, Any]:
    if not RUNTIME_ISOLATION_REPORT.exists():
        return {}
    try:
        data = json.loads(RUNTIME_ISOLATION_REPORT.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return data if isinstance(data, dict) else {}


def _unmovable_irqs(report: dict[str, Any]) -> set[str]:
    irqs = report.get("irqs", {})
    if not isinstance(irqs, dict):
        return set()
    return {
        str(irq)
        for irq, entry in irqs.items()
        if isinstance(entry, dict) and entry.get("status") == "unmovable"
    }


def _irq_affinity_mismatches(
    forbidden_cpus: list[int],
    unmovable_irqs: set[str],
    limit: int,
) -> list[str]:
    mismatches: list[str] = []
    forbidden = set(forbidden_cpus)
    for path in sorted(Path("/proc/irq").glob("[0-9]*/smp_affinity_list")):
        irq = path.parent.name
        if irq in unmovable_irqs:
            continue
        actual = _read_optional(path)
        if actual is None:
            continue
        try:
            actual_cpus = set(parse_cpu_list(actual))
        except ValueError:
            continue
        overlap = sorted(actual_cpus & forbidden)
        if not overlap:
            continue
        mismatches.append(f"{irq}={actual} overlaps {overlap}")
        if len(mismatches) >= limit:
            break
    return mismatches


def _net_queue_mask_mismatches(forbidden_cpus: list[int], limit: int) -> list[str]:
    mismatches: list[str] = []
    forbidden = set(forbidden_cpus)
    for path in sorted(Path("/sys/class/net").glob("*/queues/*/*ps_cpus")):
        text = _read_optional(path)
        if not text:
            continue
        actual_cpus = _parse_cpu_mask(text)
        overlap = sorted(actual_cpus & forbidden)
        if not overlap:
            continue
        mismatches.append(f"{path}={text} overlaps {overlap}")
        if len(mismatches) >= limit:
            break
    return mismatches


def _parse_cpu_mask(text: str) -> set[int]:
    cleaned = text.strip().replace(",", "")
    if not cleaned:
        return set()
    try:
        value = int(cleaned, 16)
    except ValueError:
        return set()
    cpus: set[int] = set()
    cpu = 0
    while value:
        if value & 1:
            cpus.add(cpu)
        value >>= 1
        cpu += 1
    return cpus


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


def _format_cpu_list(cpus: list[int]) -> str:
    if not cpus:
        return ""
    ranges: list[str] = []
    start = prev = cpus[0]
    for cpu in cpus[1:]:
        if cpu == prev + 1:
            prev = cpu
            continue
        ranges.append(f"{start}-{prev}" if start != prev else str(start))
        start = prev = cpu
    ranges.append(f"{start}-{prev}" if start != prev else str(start))
    return ",".join(ranges)


def _read_optional(path: Path) -> str | None:
    if not path.exists():
        return None
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return None


def _ensure_text(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace")
    return str(value)


def _emit_progress(
    callback: Callable[[str, dict[str, Any]], None] | None,
    event: str,
    payload: dict[str, Any],
) -> None:
    if callback is not None:
        callback(event, payload)


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
    with _MANIFEST_LOCK:
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
