#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import pwd
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

try:
    import yaml
except ImportError as exc:  # pragma: no cover - environment guard
    raise SystemExit("PyYAML is required: install the 'yaml' Python package") from exc

from bench.core.config import (
    CONFIG_PART_KEYS,
    ConfigError,
    load_config,
    load_config_data,
    parse_cpu_list,
)
from bench.env.base_image import (
    BaseImageManifestError,
    base_image_manifest_path,
    prepare_base_image,
    verify_base_image_manifest,
)
from bench.env.libvirt import (
    prepare_environment as prepare_libvirt_environment,
    restore_environment as restore_libvirt_environment,
    verify_environment as verify_libvirt_environment,
)
from bench.env.workloads import prepare_workloads


DEFAULT_CONFIG = REPO_ROOT / "bench" / "configs" / "local_config"
TEMPLATE_CONFIG = REPO_ROOT / "bench" / "configs" / "example_config"
DEFAULT_ROOT_IMAGE = Path("/var/lib/libvirt/images/scx-bench-base.qcow2")
DEFAULT_CLOUD_IMAGE = Path("/var/lib/libvirt/images/scx-bench-cloudimg.qcow2")
DEFAULT_SEED_IMAGE = Path("/var/lib/libvirt/images/scx-bench-seed.iso")
DEFAULT_RUNTIME_DIR = Path("/var/lib/libvirt/scx-bench-runs")
DEFAULT_KERNEL_DIR = Path("/var/lib/libvirt/scx-bench-kernels")
DEFAULT_IMAGE_URL = (
    "https://mirrors.tuna.tsinghua.edu.cn/ubuntu-cloud-images/jammy/current/"
    "jammy-server-cloudimg-amd64.img"
)
REQUIRED_COMMANDS = {
    "cloud-localds": "cloud-image-utils",
    "qemu-img": "qemu-utils",
    "scp": "openssh-client",
    "ssh": "openssh-client",
    "tar": "tar",
    "virsh": "libvirt-clients",
    "virt-install": "virtinst",
}
REQUIRED_PACKAGES = (
    "libvirt-daemon-system",
    "libvirt-daemon-driver-qemu",
)
DEFAULT_WORKLOADS = (
    "hackbench",
    "schbench",
    "stress-ng",
    "fio",
    "redis",
    "rt-tests",
    "will-it-scale",
    "perf",
    "bpftool",
    "batch-microbench",
)


def _default_user_home() -> Path:
    sudo_user = os.environ.get("SUDO_USER")
    if sudo_user and sudo_user != "root":
        return Path(pwd.getpwnam(sudo_user).pw_dir)
    return Path.home()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python3 -m bench.env",
        description="Prepare a local scx benchmark environment",
    )
    subparsers = parser.add_subparsers(dest="action", required=True)

    init = subparsers.add_parser("init", help="generate local config and prepare host/guest assets")
    init.add_argument("--kernel-source", required=True, help="kernel source tree to benchmark")
    init.add_argument("--config", default=str(DEFAULT_CONFIG))
    init.add_argument("--template", default=str(TEMPLATE_CONFIG))
    init.add_argument("--root-image", default=str(DEFAULT_ROOT_IMAGE))
    init.add_argument("--libvirt-kernel-dir", default=str(DEFAULT_KERNEL_DIR))
    init.add_argument("--cloud-image", default=str(DEFAULT_CLOUD_IMAGE))
    init.add_argument("--seed-image", default=str(DEFAULT_SEED_IMAGE))
    init.add_argument("--image-url", default=DEFAULT_IMAGE_URL)
    init.add_argument("--image-size", default="40G")
    init.add_argument("--ssh-key", default=str(_default_user_home() / ".ssh" / "scx-bench"))
    init.add_argument("--workdir", default=str(REPO_ROOT))
    init.add_argument("--workloads", nargs="*", default=list(DEFAULT_WORKLOADS))
    init.add_argument("--force", action="store_true")
    init.add_argument("--no-install-deps", action="store_true")
    init.add_argument("--skip-workloads", action="store_true")
    init.add_argument("--skip-image", action="store_true")
    init.add_argument("--skip-isolation", action="store_true")
    init.add_argument("--dry-run", action="store_true")

    verify = subparsers.add_parser("verify", help="verify generated local environment")
    verify.add_argument("--config", default=str(DEFAULT_CONFIG))

    rebuild_image = subparsers.add_parser(
        "rebuild-image",
        help="rebuild the base image from the existing local config",
    )
    rebuild_image.add_argument("--config", default=str(DEFAULT_CONFIG))
    rebuild_image.add_argument("--cloud-image", default=str(DEFAULT_CLOUD_IMAGE))
    rebuild_image.add_argument("--seed-image", default=str(DEFAULT_SEED_IMAGE))
    rebuild_image.add_argument("--image-url", default=DEFAULT_IMAGE_URL)
    rebuild_image.add_argument("--image-size", default="40G")
    rebuild_image.add_argument("--no-install-deps", action="store_true")
    rebuild_image.add_argument("--dry-run", action="store_true")

    restore = subparsers.add_parser("restore", help="restore host settings changed by init")
    restore.add_argument("--config", default=str(DEFAULT_CONFIG))
    restore.add_argument("--reboot", action="store_true", help="reboot after restoring isolation")
    restore.add_argument("--dry-run", action="store_true")

    args = parser.parse_args(argv)
    try:
        if args.action == "init":
            return init_environment(args)
        if args.action == "verify":
            return verify_environment(args)
        if args.action == "rebuild-image":
            return rebuild_base_image(args)
        if args.action == "restore":
            return restore_environment(args)
    except ConfigError as exc:
        print(f"config error: {exc}", file=sys.stderr)
        return 2
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


def init_environment(args: argparse.Namespace) -> int:
    kernel_source = Path(args.kernel_source).expanduser().resolve()
    kernel = kernel_source / "arch" / "x86" / "boot" / "bzImage"
    if not kernel_source.exists():
        raise RuntimeError(f"kernel source does not exist: {kernel_source}")
    if not kernel.exists():
        raise RuntimeError(f"kernel image does not exist: {kernel}")

    _check_host_dependencies(
        commands=_required_commands_for_init(args),
        install=not args.no_install_deps,
        dry_run=args.dry_run,
    )
    ssh_key = Path(args.ssh_key).expanduser().resolve()
    _ensure_ssh_key(ssh_key, dry_run=args.dry_run)
    libvirt_kernel = _install_kernel_image(
        source=kernel,
        target_dir=Path(args.libvirt_kernel_dir).expanduser().resolve(),
        dry_run=args.dry_run,
    )

    emulator_cpus, isolated_cpus = _select_cpu_sets()
    config_path = Path(args.config)
    root_image = Path(args.root_image)
    local_config = _build_local_config(
        template_path=Path(args.template),
        kernel_source=kernel_source,
        kernel=libvirt_kernel,
        root_image=root_image,
        ssh_key=ssh_key,
        workdir=Path(args.workdir).expanduser().resolve(),
        emulator_cpus=emulator_cpus,
        isolated_cpus=isolated_cpus,
    )
    _write_config(config_path, local_config, force=args.force, dry_run=args.dry_run)
    _prepare_libvirt_env(config_path, dry_run=args.dry_run)

    if not args.skip_workloads:
        _fetch_workloads(config_path, args.workloads, dry_run=args.dry_run)
    if not args.skip_image:
        prepare_base_image(
            config=local_config,
            cloud_image=Path(args.cloud_image),
            seed_image=Path(args.seed_image),
            image_url=args.image_url,
            image_size=args.image_size,
            force=args.force,
            dry_run=args.dry_run,
        )
    if not args.skip_isolation:
        _prepare_isolation(config_path, force=args.force, dry_run=args.dry_run)

    print("environment initialized")
    print(f"config: {config_path}")
    print("reboot is required after isolation preparation")
    return 0


def verify_environment(args: argparse.Namespace) -> int:
    config_path = Path(args.config)
    config = load_config(config_path)
    libvirt = config["libvirt"]

    missing = _missing_commands(REQUIRED_COMMANDS)
    if missing:
        raise RuntimeError(f"missing command(s): {', '.join(sorted(missing))}")

    checks = [
        ("kernel", Path(libvirt["kernel"]).exists()),
        ("kernel_source", Path(libvirt["kernel_source"]).exists()),
        ("root_image", Path(libvirt["root_image"]).exists()),
        ("runtime_dir", Path(libvirt.get("runtime_dir", DEFAULT_RUNTIME_DIR)).exists()),
        ("ssh_key", Path(libvirt["ssh_key"]).exists()),
        ("workdir", Path(libvirt["workdir"]).exists()),
    ]
    for name, ok in checks:
        if not ok:
            raise RuntimeError(f"missing {name}: {libvirt.get(name)}")

    verify_base_image_manifest(libvirt["root_image"], REPO_ROOT)
    _verify_libvirt_env(config_path)
    _verify_isolation(config)
    _verify_workloads()

    print("environment verified")
    return 0


def rebuild_base_image(args: argparse.Namespace) -> int:
    config_path = Path(args.config)
    config = load_config(config_path)
    _check_host_dependencies(
        commands={"sudo": "sudo", **REQUIRED_COMMANDS},
        install=not args.no_install_deps,
        dry_run=args.dry_run,
    )
    prepare_base_image(
        config=config,
        cloud_image=Path(args.cloud_image),
        seed_image=Path(args.seed_image),
        image_url=args.image_url,
        image_size=args.image_size,
        force=True,
        dry_run=args.dry_run,
    )
    print("base image rebuild planned" if args.dry_run else "base image rebuilt")
    print(f"image: {config['libvirt']['root_image']}")
    print(f"manifest: {base_image_manifest_path(config['libvirt']['root_image'])}")
    return 0


def restore_environment(args: argparse.Namespace) -> int:
    _restore_libvirt_env(dry_run=args.dry_run)
    _restore_isolation(Path(args.config), no_reboot=not args.reboot, dry_run=args.dry_run)
    print("environment restored")
    return 0


def _required_commands_for_init(args: argparse.Namespace) -> dict[str, str]:
    commands: dict[str, str] = {"sudo": "sudo"}
    if not args.skip_workloads:
        commands.update({"git": "git", "make": "make"})
    if not args.skip_image:
        commands.update(REQUIRED_COMMANDS)
    return commands


def _check_host_dependencies(commands: dict[str, str], install: bool, dry_run: bool) -> None:
    missing = _missing_commands(commands)
    missing_packages = _missing_required_packages(REQUIRED_PACKAGES)
    if not missing and not missing_packages:
        return

    packages = sorted({commands[command] for command in missing} | set(missing_packages))
    command = ["apt-get", "install", "-y", *packages]
    if install:
        _run_sudo(["apt-get", "update"], dry_run=dry_run)
        _run_sudo(command, dry_run=dry_run)
        return

    raise RuntimeError(
        "missing host dependencies; install them with: sudo "
        + " ".join(command)
        + " or rerun without --no-install-deps"
    )


def _missing_commands(commands: dict[str, str]) -> list[str]:
    return [command for command in commands if shutil.which(command) is None]


def _missing_required_packages(packages: tuple[str, ...]) -> list[str]:
    missing: list[str] = []
    for package in packages:
        result = subprocess.run(
            ["dpkg-query", "-W", "-f=${Status}", package],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        if result.stdout.strip() != "install ok installed":
            missing.append(package)
    return missing


def _ensure_ssh_key(path: Path, dry_run: bool) -> None:
    pub = Path(f"{path}.pub")
    if path.exists() and pub.exists():
        return
    command = ["ssh-keygen", "-t", "ed25519", "-f", str(path), "-N", ""]
    if dry_run:
        print("+", " ".join(command))
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(command, check=True)


def _install_kernel_image(source: Path, target_dir: Path, dry_run: bool) -> Path:
    target = target_dir / "bzImage"
    _run_sudo(
        ["install", "-D", "-m", "0644", str(source), str(target)],
        dry_run=dry_run,
    )
    return target


def _build_local_config(
    template_path: Path,
    kernel_source: Path,
    kernel: Path,
    root_image: Path,
    ssh_key: Path,
    workdir: Path,
    emulator_cpus: str,
    isolated_cpus: str,
) -> dict[str, Any]:
    try:
        data = load_config_data(template_path)
    except ConfigError as exc:
        raise RuntimeError(f"template config is invalid: {exc}") from exc

    data["libvirt"] = {
        **data.get("libvirt", {}),
        "uri": "qemu:///system",
        "kernel": str(kernel),
        "kernel_args": "root=/dev/vda1 console=ttyS0 systemd.mask=boot-efi.mount",
        "kernel_source": str(kernel_source),
        "initrd": None,
        "root_image": str(root_image),
        "runtime_dir": str(DEFAULT_RUNTIME_DIR),
        "network": "default",
        "ssh_user": "root",
        "ssh_key": str(ssh_key),
        "ssh_port": 22,
        "workdir": str(workdir),
        "guest_output_dir": "/scx_bench_out",
        "emulator_cpus": emulator_cpus,
        "iothread_cpus": emulator_cpus,
        "pin_vhost_threads": True,
        "boot_timeout_seconds": 90,
        "timeout_extra_seconds": 120,
        "destroy_on_exit": True,
        "cpu_mode": "host-passthrough",
    }
    data["executor"] = {
        **data.get("executor", {}),
        "parallel": "auto",
        "cpu_source": "isolated",
        "isolated_cpus": isolated_cpus,
        "irq_cpus": emulator_cpus,
        "smt_policy": "use_all_siblings",
        "pair_policy": "sequential",
        "memory_guard_gb": 16,
    }
    _patch_kernel_build_source(data, kernel_source)
    return data


def _patch_kernel_build_source(data: dict[str, Any], kernel_source: Path) -> None:
    bench = data.get("benches", {}).get("kernel_build_bzimage")
    if not isinstance(bench, dict):
        return
    measurement = bench.get("measurement")
    args = measurement.get("args") if isinstance(measurement, dict) else bench.get("args")
    if not isinstance(args, list):
        return
    for index, value in enumerate(args[:-1]):
        if value == "--source":
            args[index + 1] = str(kernel_source)
            return


def _write_config(path: Path, data: dict[str, Any], force: bool, dry_run: bool) -> None:
    if path.exists() and not path.is_dir():
        raise RuntimeError(f"config path exists and is not a directory: {path}")

    owned_keys = {key for _, keys in CONFIG_PART_KEYS for key in keys}
    unowned_keys = sorted(set(data) - owned_keys)
    if unowned_keys:
        raise RuntimeError(f"config contains unowned top-level key(s): {', '.join(unowned_keys)}")

    parts = {
        name: {key: data[key] for key in keys if key in data}
        for name, keys in CONFIG_PART_KEYS
    }
    existing_parts = [path / name for name, _ in CONFIG_PART_KEYS if (path / name).exists()]
    if existing_parts and not force:
        raise RuntimeError(
            f"config already exists: {existing_parts[0]}; use --force to overwrite"
        )

    if dry_run:
        print(f"would write config directory: {path}")
        for name, _ in CONFIG_PART_KEYS:
            print(f"--- {path / name}")
            print(yaml.safe_dump(parts[name], sort_keys=False), end="")
        return

    path.mkdir(parents=True, exist_ok=True)
    for name, _ in CONFIG_PART_KEYS:
        (path / name).write_text(
            yaml.safe_dump(parts[name], sort_keys=False),
            encoding="utf-8",
        )


def _fetch_workloads(config_path: Path, workloads: list[str], dry_run: bool) -> None:
    if dry_run:
        print(f"would prepare workloads: {', '.join(workloads)}")
        return
    prepare_workloads(load_config(config_path), workloads)


def _prepare_isolation(config_path: Path, force: bool, dry_run: bool) -> None:
    command = [
        sys.executable,
        str(REPO_ROOT / "bench" / "env" / "isolation.py"),
        "prepare",
        "--config",
        str(config_path),
        "--no-reboot",
    ]
    if force:
        command.append("--force")
    _run_sudo(command, dry_run=dry_run)


def _restore_isolation(config_path: Path, no_reboot: bool, dry_run: bool) -> None:
    command = [
        sys.executable,
        str(REPO_ROOT / "bench" / "env" / "isolation.py"),
        "restore",
        "--config",
        str(config_path),
    ]
    if no_reboot:
        command.append("--no-reboot")
    if dry_run:
        command.append("--dry-run")
        _run(command, dry_run=False)
    else:
        _run_sudo(command, dry_run=False)


def _prepare_libvirt_env(config_path: Path, dry_run: bool) -> None:
    config = load_config(config_path)
    prepare_libvirt_environment(config["libvirt"], dry_run=dry_run)


def _verify_libvirt_env(config_path: Path) -> None:
    config = load_config(config_path)
    verify_libvirt_environment(config["libvirt"])


def _restore_libvirt_env(dry_run: bool) -> None:
    restore_libvirt_environment(dry_run=dry_run)


def _verify_isolation(config: dict[str, Any]) -> None:
    expected = set(parse_cpu_list(config["executor"]["isolated_cpus"]))
    isolated = set(_read_sys_cpu_list(Path("/sys/devices/system/cpu/isolated")))
    missing = sorted(expected - isolated)
    if missing:
        raise RuntimeError(f"CPU(s) not isolated after reboot: {_format_cpu_list(missing)}")


def _verify_workloads() -> None:
    required = [
        "hackbench",
        "schbench",
        "stress-ng",
        "fio",
        "perf",
        "bpftool",
        "batch_microbench",
    ]
    missing = [
        name for name in required if not (REPO_ROOT / "bench" / "workloads" / "bin" / name).exists()
    ]
    if missing:
        raise RuntimeError(f"missing workload binary/binaries: {', '.join(missing)}")


def _select_cpu_sets() -> tuple[str, str]:
    groups = _read_core_groups()
    if len(groups) < 2:
        raise RuntimeError("at least two physical core sibling groups are required")
    emulator = groups[0]
    isolated = [cpu for group in groups[1:] for cpu in group]
    return _format_cpu_list(emulator), _format_cpu_list(isolated)


def _read_core_groups() -> list[list[int]]:
    groups: dict[tuple[int, ...], list[int]] = {}
    for cpu_path in sorted(
        Path("/sys/devices/system/cpu").glob("cpu[0-9]*"),
        key=lambda path: int(path.name[3:]),
    ):
        cpu = int(cpu_path.name[3:])
        siblings_path = cpu_path / "topology" / "thread_siblings_list"
        siblings = parse_cpu_list(siblings_path.read_text(encoding="utf-8").strip())
        groups[tuple(siblings)] = siblings
    return [groups[key] for key in sorted(groups)]


def _read_sys_cpu_list(path: Path) -> list[int]:
    if not path.exists():
        return []
    text = path.read_text(encoding="utf-8").strip()
    if not text or text == "(null)":
        return []
    return parse_cpu_list(text)


def _format_cpu_list(cpus: list[int]) -> str:
    values = sorted(cpus)
    ranges: list[str] = []
    start = prev = values[0]
    for cpu in values[1:]:
        if cpu == prev + 1:
            prev = cpu
            continue
        ranges.append(f"{start}-{prev}" if start != prev else str(start))
        start = prev = cpu
    ranges.append(f"{start}-{prev}" if start != prev else str(start))
    return ",".join(ranges)


def _run_sudo(command: list[str], dry_run: bool, check: bool = True) -> None:
    command = _sudo_command(command)
    _run(command, dry_run=dry_run, check=check)


def _sudo_command(command: list[str]) -> list[str]:
    no_sudo = os.environ.get("SCX_BENCH_NO_SUDO", "").lower() in {"1", "true", "yes"}
    if os.geteuid() != 0 and not no_sudo:
        return ["sudo", *command]
    return command


def _run(command: list[str], dry_run: bool, check: bool = True) -> None:
    print("+", " ".join(command), flush=True)
    if not dry_run:
        subprocess.run(command, check=check)


if __name__ == "__main__":
    raise SystemExit(main())
