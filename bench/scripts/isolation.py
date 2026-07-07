#!/usr/bin/env python3
from __future__ import annotations

import argparse
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
    parser.add_argument("--config", default="bench/configs/local.config")
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
    boot_args = [
        f"isolcpus=domain,managed_irq,{target_cpu_list}",
        f"nohz_full={target_cpu_list}",
        f"rcu_nocbs={target_cpu_list}",
        f"irqaffinity={housekeeping_cpu_list}",
    ]

    original_grub = grub_path.read_text(encoding="utf-8")
    new_grub = update_grub_cmdline(original_grub, boot_args)
    state = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "config": str(Path(args.config).resolve()),
        "plan": args.plan,
        "target_cpus": target_cpus,
        "housekeeping_cpus": housekeeping_cpus,
        "boot_args": boot_args,
        "grub_path": str(grub_path),
        "original_grub": original_grub,
        "original_proc_cmdline": read_text_optional(Path("/proc/cmdline")),
        "cpufreq": capture_cpufreq(target_cpus),
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
        raise RuntimeError(f"state does not exist: {state_path}")

    state = json.loads(state_path.read_text(encoding="utf-8"))
    if args.dry_run:
        print(json.dumps(state, indent=2, sort_keys=True))
        return 0

    grub_path.write_text(state["original_grub"], encoding="utf-8")
    restore_cpufreq(state.get("cpufreq", {}))

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
    print(f"target_cpus={format_cpu_list(target_cpus)}")
    print(f"isolated={read_sys_cpu_list('/sys/devices/system/cpu/isolated')}")
    print(f"nohz_full={read_sys_cpu_list('/sys/devices/system/cpu/nohz_full')}")
    print(f"proc_cmdline={read_text_optional(Path('/proc/cmdline'))}")
    print(f"state_exists={state_path.exists()}")
    return 0


def apply_runtime(state_path: Path, dry_run: bool = False) -> int:
    require_root()
    if not state_path.exists():
        raise RuntimeError(f"state does not exist: {state_path}")
    state = json.loads(state_path.read_text(encoding="utf-8"))
    set_fixed_cpufreq(state.get("cpufreq", {}), dry_run=dry_run)
    return 0


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
    return path.read_text(encoding="utf-8").strip()


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
