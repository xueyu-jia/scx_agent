#!/usr/bin/env python3
from __future__ import annotations

import argparse
import errno
import json
import os
import re
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from bench.config.parser import ConfigError, expand_plan, load_config, parse_cpu_list


STATE_PATH = Path("/var/lib/scx-bench/isolation-state.json")
RUNTIME_REPORT_PATH = Path("/var/lib/scx-bench/runtime-isolation.json")
GRUB_PATH = Path("/etc/default/grub")
SERVICE_PATH = Path("/etc/systemd/system/scx-bench-isolation.service")

BOOT_ARG_KEYS = {
    "isolcpus",
    "nohz_full",
    "rcu_nocbs",
    "irqaffinity",
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Prepare or restore host isolation for scx bench")
    parser.add_argument("action", choices=("prepare", "restore", "status", "apply-runtime"))
    parser.add_argument("--config", default="bench/configs/local_config")
    parser.add_argument("--plan", help="only use machines referenced by this plan")
    parser.add_argument("--state", default=str(STATE_PATH))
    parser.add_argument("--grub", default=str(GRUB_PATH))
    parser.add_argument("--no-reboot", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args(argv)

    state_path = Path(args.state)
    grub_path = Path(args.grub)

    try:
        if args.action == "prepare":
            return prepare(args, state_path, grub_path)
        if args.action == "restore":
            return restore(args, state_path, grub_path)
        if args.action == "status":
            return status(args, state_path)
        if args.action == "apply-runtime":
            return apply_runtime(state_path, dry_run=args.dry_run)
    except ConfigError as exc:
        print(f"config error: {exc}", file=sys.stderr)
        return 2
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    return 0


def prepare(args: argparse.Namespace, state_path: Path, grub_path: Path) -> int:
    if not args.dry_run:
        require_root()
    if state_path.exists() and not args.force:
        raise RuntimeError(f"state already exists: {state_path}; restore first or use --force")

    config = load_config(args.config)
    target_cpus = collect_target_cpus(config, args.plan)
    host_cpus = read_host_cpus()
    unknown = sorted(set(target_cpus) - set(host_cpus))
    if unknown:
        raise RuntimeError(f"configured CPU(s) do not exist on host: {unknown}")

    housekeeping_cpus = sorted(set(host_cpus) - set(target_cpus))
    if not housekeeping_cpus:
        raise RuntimeError("at least one housekeeping CPU is required")

    target_cpu_list = format_cpu_list(target_cpus)
    housekeeping_cpu_list = format_cpu_list(housekeeping_cpus)
    irq_cpus = parse_cpu_list(config.get("executor", {}).get("irq_cpus") or housekeeping_cpu_list)
    unknown_irq_cpus = sorted(set(irq_cpus) - set(host_cpus))
    if unknown_irq_cpus:
        raise RuntimeError(f"configured IRQ CPU(s) do not exist on host: {unknown_irq_cpus}")
    irq_target_overlap = sorted(set(irq_cpus) & set(target_cpus))
    if irq_target_overlap:
        raise RuntimeError(
            "executor.irq_cpus must not overlap isolated benchmark CPU(s): "
            f"{irq_target_overlap}"
        )
    irq_cpu_list = format_cpu_list(irq_cpus)
    boot_args = [
        f"isolcpus=domain,managed_irq,{target_cpu_list}",
        f"nohz_full={target_cpu_list}",
        f"rcu_nocbs={target_cpu_list}",
        f"irqaffinity={irq_cpu_list}",
    ]

    original_grub = grub_path.read_text(encoding="utf-8")
    new_grub = update_grub_cmdline(original_grub, boot_args)
    state = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "config": str(Path(args.config).resolve()),
        "plan": args.plan,
        "target_cpus": target_cpus,
        "housekeeping_cpus": housekeeping_cpus,
        "irq_cpus": irq_cpus,
        "boot_args": boot_args,
        "grub_path": str(grub_path),
        "original_grub": original_grub,
        "original_proc_cmdline": read_text_optional(Path("/proc/cmdline")),
        "cpufreq": capture_cpufreq(target_cpus),
        "irq": capture_irq_state(),
        "service_path": str(SERVICE_PATH),
    }

    if args.dry_run:
        print(json.dumps({**state, "new_grub": new_grub}, indent=2, sort_keys=True))
        return 0

    state_path.parent.mkdir(parents=True, exist_ok=True)
    state_path.write_text(json.dumps(state, indent=2, sort_keys=True), encoding="utf-8")
    grub_path.write_text(new_grub, encoding="utf-8")
    write_systemd_service(state_path)
    run(["systemctl", "daemon-reload"])
    run(["systemctl", "enable", SERVICE_PATH.name])
    run(["systemctl", "start", SERVICE_PATH.name])
    update_grub()

    if not args.no_reboot:
        run(["systemctl", "reboot"])
    else:
        print("isolation prepared; reboot is still required")
    return 0


def restore(args: argparse.Namespace, state_path: Path, grub_path: Path) -> int:
    if not args.dry_run:
        require_root()
    if not state_path.exists():
        print(f"no scx-bench isolation state found: {state_path}")
        return 0

    state = json.loads(state_path.read_text(encoding="utf-8"))
    if args.dry_run:
        print(json.dumps(state, indent=2, sort_keys=True))
        return 0

    grub_path.write_text(state["original_grub"], encoding="utf-8")
    restore_cpufreq(state.get("cpufreq", {}))
    restore_irq_state(state.get("irq", {}))

    if SERVICE_PATH.exists():
        run(["systemctl", "disable", SERVICE_PATH.name], check=False)
        SERVICE_PATH.unlink()
        run(["systemctl", "daemon-reload"])

    update_grub()
    state_path.unlink()

    if not args.no_reboot:
        run(["systemctl", "reboot"])
    else:
        print("isolation restored; reboot is still required")
    return 0


def status(args: argparse.Namespace, state_path: Path) -> int:
    config = load_config(args.config)
    target_cpus = collect_target_cpus(config, args.plan)
    host_cpus = read_host_cpus()
    default_irq_cpus = sorted(set(host_cpus) - set(target_cpus))
    irq_cpus = parse_cpu_list(
        config.get("executor", {}).get("irq_cpus") or format_cpu_list(default_irq_cpus)
    )
    print(f"target_cpus={format_cpu_list(target_cpus)}")
    print(f"isolated={read_sys_cpu_list('/sys/devices/system/cpu/isolated')}")
    print(f"nohz_full={read_sys_cpu_list('/sys/devices/system/cpu/nohz_full')}")
    print(f"irq_cpus={format_cpu_list(irq_cpus)}")
    print(f"proc_cmdline={read_text_optional(Path('/proc/cmdline'))}")
    print(f"state_exists={state_path.exists()}")
    return 0


def apply_runtime(state_path: Path, dry_run: bool = False) -> int:
    require_root()
    if not state_path.exists():
        raise RuntimeError(f"state does not exist: {state_path}")
    state = json.loads(state_path.read_text(encoding="utf-8"))
    set_fixed_cpufreq(state.get("cpufreq", {}), dry_run=dry_run)
    irq_cpus = state.get("irq_cpus") or state.get("housekeeping_cpus")
    report: dict[str, Any] = {}
    if irq_cpus:
        report = apply_irq_isolation(
            [int(cpu) for cpu in irq_cpus],
            [int(cpu) for cpu in state.get("target_cpus", [])],
            dry_run=dry_run,
        )
    if not dry_run and report:
        RUNTIME_REPORT_PATH.parent.mkdir(parents=True, exist_ok=True)
        RUNTIME_REPORT_PATH.write_text(json.dumps(report, indent=2, sort_keys=True), encoding="utf-8")
    if report.get("errors"):
        raise RuntimeError("; ".join(report["errors"]))
    return 0


def apply_irq_isolation(
    cpus: list[int],
    isolated_cpus: list[int],
    dry_run: bool = False,
) -> dict[str, Any]:
    cpu_list = format_cpu_list(sorted(cpus))
    cpu_mask = format_cpu_mask(sorted(cpus))
    report: dict[str, Any] = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "irq_cpus": cpu_list,
        "isolated_cpus": format_cpu_list(sorted(isolated_cpus)),
        "managed_irq_policy": "fail_on_delta",
        "irqs": {},
        "net_queues": {},
        "errors": [],
    }
    irq_writes = apply_irq_affinity(cpu_list, isolated_cpus, report, dry_run=dry_run)
    rps_writes = apply_net_queue_masks(cpu_mask, "rps_cpus", report, dry_run=dry_run)
    xps_writes = apply_net_queue_masks(cpu_mask, "xps_cpus", report, dry_run=dry_run)
    print(
        "irq isolation applied: "
        f"cpus={cpu_list} irq_files={irq_writes} rps_files={rps_writes} xps_files={xps_writes}"
    )
    return report


def apply_irq_affinity(
    cpu_list: str,
    isolated_cpus: list[int],
    report: dict[str, Any],
    dry_run: bool = False,
) -> int:
    count = 0
    interrupt_actions = read_interrupt_actions()
    for path in sorted(Path("/proc/irq").glob("[0-9]*/smp_affinity_list")):
        irq = path.parent.name
        entry: dict[str, Any] = {
            "path": str(path),
            "actions": interrupt_actions.get(irq, ""),
            "smp_affinity": read_text_optional(path),
            "effective_affinity": read_text_optional(path.parent / "effective_affinity_list"),
        }
        if dry_run:
            print(f"write {cpu_list} > {path}")
            entry["status"] = "dry_run"
            report["irqs"][irq] = entry
            count += 1
            continue
        try:
            path.write_text(cpu_list, encoding="utf-8")
        except OSError as exc:
            if exc.errno == errno.ENOENT:
                entry["status"] = "disappeared"
                report["irqs"][irq] = entry
                continue
            if exc.errno in (errno.EIO, errno.EINVAL):
                entry["status"] = "unmovable"
                entry["error_errno"] = errno.errorcode.get(exc.errno, str(exc.errno))
                entry["error"] = str(exc)
                entry["smp_affinity"] = read_text_optional(path)
                entry["effective_affinity"] = read_text_optional(path.parent / "effective_affinity_list")
                report["irqs"][irq] = entry
                continue
            entry["status"] = "error"
            entry["error_errno"] = errno.errorcode.get(exc.errno or 0, str(exc.errno))
            entry["error"] = str(exc)
            report["irqs"][irq] = entry
            report["errors"].append(f"could not write {path}: {exc}")
            continue

        entry["smp_affinity"] = read_text_optional(path)
        entry["effective_affinity"] = read_text_optional(path.parent / "effective_affinity_list")
        entry["status"] = "movable_ok"
        try:
            actual_cpus = parse_cpu_list(entry["smp_affinity"])
        except ConfigError:
            actual_cpus = []
        overlap = sorted(set(actual_cpus) & set(isolated_cpus))
        if overlap:
            entry["status"] = "movable_mismatch"
            entry["overlap_isolated_cpus"] = overlap
            report["errors"].append(f"{path} still overlaps isolated CPU(s): {overlap}")
        report["irqs"][irq] = entry
        count += 1
    return count


def read_interrupt_actions() -> dict[str, str]:
    path = Path("/proc/interrupts")
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return {}
    actions: dict[str, str] = {}
    for line in lines:
        stripped = line.lstrip()
        if ":" not in stripped:
            continue
        irq, rest = stripped.split(":", 1)
        if not irq.isdigit():
            continue
        tokens = rest.split()
        first_non_count = 0
        while first_non_count < len(tokens) and tokens[first_non_count].isdigit():
            first_non_count += 1
        actions[irq] = " ".join(tokens[first_non_count:])
    return actions


def capture_irq_state() -> dict[str, str]:
    state: dict[str, str] = {}
    paths = list(Path("/proc/irq").glob("[0-9]*/smp_affinity_list"))
    paths.extend(Path("/sys/class/net").glob("*/queues/*/*ps_cpus"))
    for path in sorted(paths):
        try:
            state[str(path)] = path.read_text(encoding="utf-8").strip()
        except OSError:
            continue
    return state


def restore_irq_state(state: dict[str, str]) -> None:
    for path_text, value in sorted(state.items()):
        path = Path(path_text)
        if not path.exists():
            continue
        try:
            path.write_text(value, encoding="utf-8")
        except OSError as exc:
            print(f"warning: could not restore {path}: {exc}", file=sys.stderr)


def apply_net_queue_masks(
    cpu_mask: str,
    filename: str,
    report: dict[str, Any],
    dry_run: bool = False,
) -> int:
    count = 0
    for path in sorted(Path("/sys/class/net").glob(f"*/queues/*/{filename}")):
        entry: dict[str, Any] = {
            "path": str(path),
            "target_mask": cpu_mask,
            "value": read_text_optional(path),
        }
        if dry_run:
            print(f"write {cpu_mask} > {path}")
            entry["status"] = "dry_run"
            report["net_queues"][str(path)] = entry
            count += 1
            continue
        try:
            path.write_text(cpu_mask, encoding="utf-8")
        except OSError as exc:
            if exc.errno == errno.ENOENT:
                entry["status"] = "disappeared"
                report["net_queues"][str(path)] = entry
                continue
            entry["status"] = "error"
            entry["error_errno"] = errno.errorcode.get(exc.errno or 0, str(exc.errno))
            entry["error"] = str(exc)
            report["net_queues"][str(path)] = entry
            report["errors"].append(f"could not write {path}: {exc}")
            continue
        entry["value"] = read_text_optional(path)
        entry["status"] = "ok"
        report["net_queues"][str(path)] = entry
        count += 1
    return count


def format_cpu_mask(cpus: list[int]) -> str:
    mask = 0
    for cpu in cpus:
        mask |= 1 << cpu
    if mask == 0:
        return "0"
    chunks: list[str] = []
    while mask:
        chunks.append(f"{mask & 0xFFFFFFFF:08x}")
        mask >>= 32
    chunks[-1] = chunks[-1].lstrip("0") or "0"
    return ",".join(reversed(chunks))


def collect_target_cpus(config: dict[str, Any], plan_name: str | None) -> list[int]:
    executor = config.get("executor", {})
    if isinstance(executor, dict) and executor.get("isolated_cpus"):
        return parse_cpu_list(executor["isolated_cpus"])

    if plan_name:
        specs = expand_plan(config, plan_name)
        machines = [spec.machine for spec in specs]
    else:
        machines = list(config["machines"].values())

    cpus: set[int] = set()
    for machine in machines:
        pin_cpus = machine["pin_cpus"]
        if pin_cpus == "auto":
            raise ConfigError(
                "executor.isolated_cpus is required when machines use pin_cpus: auto"
            )
        cpus.update(parse_cpu_list(pin_cpus))
    return sorted(cpus)


def update_grub_cmdline(text: str, boot_args: list[str]) -> str:
    lines = text.splitlines()
    updated = False
    new_lines: list[str] = []
    for line in lines:
        if line.strip().startswith("GRUB_CMDLINE_LINUX_DEFAULT="):
            new_lines.append(set_grub_var(line, boot_args))
            updated = True
        else:
            new_lines.append(line)
    if not updated:
        new_lines.append(f'GRUB_CMDLINE_LINUX_DEFAULT="{join_cmdline(boot_args)}"')
    return "\n".join(new_lines) + "\n"


def set_grub_var(line: str, boot_args: list[str]) -> str:
    match = re.match(r"^(\s*GRUB_CMDLINE_LINUX_DEFAULT\s*=\s*)(['\"])(.*)\2\s*$", line)
    if not match:
        raise RuntimeError(f"unsupported GRUB_CMDLINE_LINUX_DEFAULT format: {line}")
    prefix, quote, value = match.groups()
    tokens = shlex_split(value)
    tokens = [token for token in tokens if token.split("=", 1)[0] not in BOOT_ARG_KEYS]
    tokens.extend(boot_args)
    return f"{prefix}{quote}{join_cmdline(tokens)}{quote}"


def shlex_split(value: str) -> list[str]:
    import shlex

    return shlex.split(value)


def join_cmdline(tokens: list[str]) -> str:
    return " ".join(tokens)


def capture_cpufreq(cpus: list[int]) -> dict[str, dict[str, str]]:
    state: dict[str, dict[str, str]] = {}
    for cpu in cpus:
        base = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq")
        if not base.exists():
            continue
        state[str(cpu)] = {
            "scaling_governor": read_text_optional(base / "scaling_governor"),
            "scaling_min_freq": read_text_optional(base / "scaling_min_freq"),
            "scaling_max_freq": read_text_optional(base / "scaling_max_freq"),
        }
    return state


def set_fixed_cpufreq(cpufreq_state: dict[str, dict[str, str]], dry_run: bool = False) -> None:
    for cpu_text, values in cpufreq_state.items():
        base = Path(f"/sys/devices/system/cpu/cpu{cpu_text}/cpufreq")
        if not base.exists():
            raise RuntimeError(f"CPU {cpu_text} does not expose cpufreq controls")
        max_freq = values.get("scaling_max_freq")
        if not max_freq:
            raise RuntimeError(f"CPU {cpu_text} has no saved scaling_max_freq")
        writes = [
            (base / "scaling_governor", "performance"),
            (base / "scaling_max_freq", max_freq),
            (base / "scaling_min_freq", max_freq),
        ]
        for path, value in writes:
            if dry_run:
                print(f"write {value} > {path}")
            elif path.exists():
                path.write_text(value, encoding="utf-8")


def restore_cpufreq(cpufreq_state: dict[str, dict[str, str]]) -> None:
    for cpu_text, values in cpufreq_state.items():
        base = Path(f"/sys/devices/system/cpu/cpu{cpu_text}/cpufreq")
        if not base.exists():
            continue
        for name in ("scaling_max_freq", "scaling_min_freq", "scaling_governor"):
            value = values.get(name)
            path = base / name
            if value and path.exists():
                path.write_text(value, encoding="utf-8")


def write_systemd_service(state_path: Path) -> None:
    script = Path(__file__).resolve()
    content = f"""[Unit]
Description=scx bench runtime isolation
After=multi-user.target

[Service]
Type=oneshot
ExecStart={sys.executable} {script} apply-runtime --state {state_path}
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
"""
    SERVICE_PATH.write_text(content, encoding="utf-8")


def update_grub() -> None:
    if shutil.which("update-grub"):
        run(["update-grub"])
        return
    grub2_mkconfig = shutil.which("grub2-mkconfig")
    if grub2_mkconfig:
        output = "/boot/grub2/grub.cfg" if Path("/boot/grub2").exists() else "/boot/grub/grub.cfg"
        run([grub2_mkconfig, "-o", output])
        return
    raise RuntimeError("neither update-grub nor grub2-mkconfig was found")


def read_host_cpus() -> list[int]:
    cpus: list[int] = []
    for path in Path("/sys/devices/system/cpu").glob("cpu[0-9]*"):
        cpus.append(int(path.name[3:]))
    return sorted(cpus)


def read_sys_cpu_list(path: str) -> str:
    text = read_text_optional(Path(path)).strip()
    return "" if text == "(null)" else text


def read_text_optional(path: Path) -> str:
    if not path.exists():
        return ""
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return ""


def format_cpu_list(cpus: list[int]) -> str:
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


def require_root() -> None:
    if os.geteuid() != 0:
        raise RuntimeError("this action must be run as root")


def run(command: list[str], check: bool = True) -> None:
    subprocess.run(command, check=check)


if __name__ == "__main__":
    raise SystemExit(main())
