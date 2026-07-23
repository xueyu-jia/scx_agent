#!/usr/bin/env python3
from __future__ import annotations

import argparse
import grp
import os
import pwd
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from bench.core.config import ConfigError, load_config


DEFAULT_CONFIG = REPO_ROOT / "bench" / "configs" / "local.config"
DEFAULT_RUNTIME_DIR = Path("/var/lib/libvirt/scx-bench-runs")
QEMU_CONF = Path("/etc/libvirt/qemu.conf")
QEMU_CONF_BACKUP = Path("/etc/libvirt/qemu.conf.scx-bench.bak")
QEMU_CONF_MISSING_MARKER = Path("/etc/libvirt/qemu.conf.scx-bench.missing")
QEMU_CONF_BEGIN = "# BEGIN scx-bench qemu user"
QEMU_CONF_END = "# END scx-bench qemu user"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python3 -m bench.env.libvirt",
        description="Prepare or restore scx bench libvirt/QEMU host settings",
    )
    subparsers = parser.add_subparsers(dest="action", required=True)

    prepare = subparsers.add_parser("prepare", help="prepare libvirt/QEMU host settings")
    prepare.add_argument("--config", default=str(DEFAULT_CONFIG))
    prepare.add_argument("--dry-run", action="store_true")

    verify = subparsers.add_parser("verify", help="verify libvirt/QEMU host settings")
    verify.add_argument("--config", default=str(DEFAULT_CONFIG))

    restore = subparsers.add_parser("restore", help="restore libvirt/QEMU host settings")
    restore.add_argument("--dry-run", action="store_true")

    args = parser.parse_args(argv)
    try:
        if args.action == "prepare":
            config = load_config(args.config)
            prepare_environment(config["libvirt"], dry_run=args.dry_run)
            return 0
        if args.action == "verify":
            config = load_config(args.config)
            verify_environment(config["libvirt"])
            return 0
        if args.action == "restore":
            restore_environment(dry_run=args.dry_run)
            return 0
    except ConfigError as exc:
        print(f"config error: {exc}", file=sys.stderr)
        return 2
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


def prepare_environment(libvirt: dict[str, Any], dry_run: bool) -> None:
    prepare_qemu_user_config(dry_run=dry_run)
    prepare_runtime_dir(libvirt, dry_run=dry_run)
    ensure_libvirt_runtime(libvirt, dry_run=dry_run)


def verify_environment(libvirt: dict[str, Any]) -> None:
    verify_qemu_user_config()
    require_libvirt(libvirt)
    runtime_dir = Path(libvirt.get("runtime_dir", DEFAULT_RUNTIME_DIR))
    if not runtime_dir.exists():
        raise RuntimeError(f"missing runtime_dir: {runtime_dir}")


def restore_environment(dry_run: bool) -> None:
    restore_qemu_user_config(dry_run=dry_run)


def prepare_qemu_user_config(dry_run: bool) -> None:
    user, group = benchmark_user_group()
    original = read_optional_root_file(QEMU_CONF, dry_run=dry_run)
    backup_qemu_conf(existed=original is not None, dry_run=dry_run)
    updated = qemu_conf_with_managed_block(original or "", user=user, group=group)
    write_root_file(QEMU_CONF, updated, dry_run=dry_run)
    restart_libvirt(dry_run=dry_run)


def restore_qemu_user_config(dry_run: bool) -> None:
    if QEMU_CONF_BACKUP.exists():
        run_sudo(["cp", "-a", str(QEMU_CONF_BACKUP), str(QEMU_CONF)], dry_run=dry_run)
        run_sudo(["rm", "-f", str(QEMU_CONF_BACKUP), str(QEMU_CONF_MISSING_MARKER)], dry_run=dry_run)
    elif QEMU_CONF_MISSING_MARKER.exists():
        run_sudo(["rm", "-f", str(QEMU_CONF), str(QEMU_CONF_MISSING_MARKER)], dry_run=dry_run)
    else:
        print(f"no scx-bench qemu.conf backup found: {QEMU_CONF_BACKUP}")
    restart_libvirt(dry_run=dry_run)


def backup_qemu_conf(existed: bool, dry_run: bool) -> None:
    if QEMU_CONF_BACKUP.exists() or QEMU_CONF_MISSING_MARKER.exists():
        return
    if existed:
        run_sudo(["cp", "-a", str(QEMU_CONF), str(QEMU_CONF_BACKUP)], dry_run=dry_run)
    else:
        run_sudo(["install", "-D", "-m", "0644", "/dev/null", str(QEMU_CONF_MISSING_MARKER)], dry_run=dry_run)


def qemu_conf_with_managed_block(text: str, user: str, group: str) -> str:
    lines: list[str] = []
    in_managed_block = False
    for line in text.splitlines():
        if line.strip() == QEMU_CONF_BEGIN:
            in_managed_block = True
            continue
        if line.strip() == QEMU_CONF_END:
            in_managed_block = False
            continue
        if not in_managed_block:
            lines.append(line)

    while lines and not lines[-1].strip():
        lines.pop()
    lines.extend(
        [
            "",
            QEMU_CONF_BEGIN,
            f'user = "{user}"',
            f'group = "{group}"',
            "dynamic_ownership = 1",
            QEMU_CONF_END,
        ]
    )
    return "\n".join(lines) + "\n"


def verify_qemu_user_config() -> None:
    user, group = benchmark_user_group()
    text = read_optional_root_file(QEMU_CONF, dry_run=False)
    if text is None:
        raise RuntimeError(f"missing libvirt qemu config: {QEMU_CONF}")
    values = parse_qemu_conf_values(text)
    expected = {
        "user": user,
        "group": group,
        "dynamic_ownership": "1",
    }
    mismatched = [
        f"{key}={values.get(key)!r}, expected {value!r}"
        for key, value in expected.items()
        if values.get(key) != value
    ]
    if mismatched:
        raise RuntimeError(
            "libvirt qemu user config is not prepared: "
            + "; ".join(mismatched)
            + "; run: python3 -m bench.env init --config bench/configs/local.config"
        )


def parse_qemu_conf_values(text: str) -> dict[str, str]:
    values: dict[str, str] = {}
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        if key not in {"user", "group", "dynamic_ownership"}:
            continue
        values[key] = value.strip().strip('"')
    return values


def prepare_runtime_dir(libvirt: dict[str, Any], dry_run: bool) -> None:
    runtime_dir = Path(libvirt.get("runtime_dir", DEFAULT_RUNTIME_DIR))
    run_sudo(["mkdir", "-p", str(runtime_dir)], dry_run=dry_run)
    user, group = benchmark_user_group()
    run_sudo(["chown", f"{user}:{group}", str(runtime_dir)], dry_run=dry_run)
    run_sudo(["chmod", "0775", str(runtime_dir)], dry_run=dry_run)


def ensure_libvirt_runtime(libvirt: dict[str, Any], dry_run: bool) -> None:
    if shutil.which("systemctl") is not None:
        run_sudo(["systemctl", "enable", "--now", "libvirtd"], dry_run=dry_run)

    network = libvirt.get("network")
    if network:
        run_sudo(
            ["virsh", "--connect", libvirt["uri"], "net-start", network],
            dry_run=dry_run,
            check=False,
        )
        run_sudo(
            ["virsh", "--connect", libvirt["uri"], "net-autostart", network],
            dry_run=dry_run,
            check=False,
        )


def require_libvirt(libvirt: dict[str, Any]) -> None:
    run(["virsh", "--connect", libvirt["uri"], "uri"], dry_run=False)
    network = libvirt.get("network")
    if network is not None:
        run(["virsh", "--connect", libvirt["uri"], "net-info", network], dry_run=False)


def benchmark_user_group() -> tuple[str, str]:
    sudo_user = os.environ.get("SUDO_USER")
    if sudo_user and sudo_user != "root":
        user_info = pwd.getpwnam(sudo_user)
    else:
        user_info = pwd.getpwuid(os.getuid())
    try:
        qemu_group = grp.getgrnam("kvm").gr_name
    except KeyError:
        qemu_group = grp.getgrgid(user_info.pw_gid).gr_name
    return user_info.pw_name, qemu_group


def read_optional_root_file(path: Path, dry_run: bool) -> str | None:
    if dry_run:
        print(f"would read {path}")
        try:
            return path.read_text(encoding="utf-8") if path.exists() else None
        except OSError as exc:
            print(f"would need sudo to read {path}: {exc}")
            return None
    if not path.exists():
        return None
    try:
        return path.read_text(encoding="utf-8")
    except OSError:
        command = sudo_command(["cat", str(path)])
        completed = subprocess.run(command, check=True, capture_output=True, text=True)
        return completed.stdout


def write_root_file(path: Path, text: str, dry_run: bool) -> None:
    if dry_run:
        print(f"would write {path}:")
        print(text)
        return
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", delete=False) as tmp:
        tmp.write(text)
        tmp_path = Path(tmp.name)
    try:
        run_sudo(["install", "-D", "-m", "0644", str(tmp_path), str(path)], dry_run=False)
    finally:
        tmp_path.unlink(missing_ok=True)


def restart_libvirt(dry_run: bool) -> None:
    if shutil.which("systemctl") is None:
        return
    run_sudo(["systemctl", "restart", "libvirtd"], dry_run=dry_run)


def run_sudo(command: list[str], dry_run: bool, check: bool = True) -> None:
    command = sudo_command(command)
    run(command, dry_run=dry_run, check=check)


def sudo_command(command: list[str]) -> list[str]:
    if os.geteuid() != 0:
        return ["sudo", *command]
    return command


def run(command: list[str], dry_run: bool, check: bool = True) -> None:
    print("+", " ".join(command), flush=True)
    if not dry_run:
        subprocess.run(command, check=check)


if __name__ == "__main__":
    raise SystemExit(main())
