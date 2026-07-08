#!/usr/bin/env python3
from __future__ import annotations

import argparse
import grp
import os
import pwd
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

try:
    import yaml
except ImportError as exc:  # pragma: no cover - environment guard
    raise SystemExit("PyYAML is required: install the 'yaml' Python package") from exc

from bench.config.parser import ConfigError, load_config, parse_cpu_list


DEFAULT_CONFIG = REPO_ROOT / "bench" / "configs" / "local.config"
TEMPLATE_CONFIG = REPO_ROOT / "bench" / "configs" / "example.config"
DEFAULT_ROOT_IMAGE = Path("/var/lib/libvirt/images/scx-bench-base.qcow2")
DEFAULT_CLOUD_IMAGE = Path("/var/lib/libvirt/images/scx-bench-cloudimg.qcow2")
DEFAULT_SEED_IMAGE = Path("/var/lib/libvirt/images/scx-bench-seed.iso")
DEFAULT_RUNTIME_DIR = Path("/var/lib/libvirt/scx-bench-runs")
DEFAULT_KERNEL_DIR = Path("/var/lib/libvirt/scx-bench-kernels")
QEMU_CONF = Path("/etc/libvirt/qemu.conf")
QEMU_CONF_BACKUP = Path("/etc/libvirt/qemu.conf.scx-bench.bak")
QEMU_CONF_MISSING_MARKER = Path("/etc/libvirt/qemu.conf.scx-bench.missing")
QEMU_CONF_BEGIN = "# BEGIN scx-bench qemu user"
QEMU_CONF_END = "# END scx-bench qemu user"
DEFAULT_IMAGE_URL = (
    "https://mirrors.tuna.tsinghua.edu.cn/ubuntu-cloud-images/jammy/current/"
    "jammy-server-cloudimg-amd64.img"
)
DEFAULT_PACKAGES = (
    "python3",
    "python3-yaml",
    "openssh-server",
    "libelf1",
    "libnuma1",
    "libpython3.10",
    "libseccomp2",
    "libstdc++6",
    "zlib1g",
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
)


def _default_user_home() -> Path:
    sudo_user = os.environ.get("SUDO_USER")
    if sudo_user and sudo_user != "root":
        return Path(pwd.getpwnam(sudo_user).pw_dir)
    return Path.home()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Prepare a local scx benchmark environment")
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

    restore = subparsers.add_parser("restore", help="restore libvirt/qemu settings changed by init")
    restore.add_argument("--dry-run", action="store_true")

    args = parser.parse_args(argv)
    try:
        if args.action == "init":
            return init_environment(args)
        if args.action == "verify":
            return verify_environment(args)
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
    _prepare_qemu_user_config(dry_run=args.dry_run)
    _prepare_runtime_dir(local_config["libvirt"], dry_run=args.dry_run)

    if not args.skip_workloads:
        _fetch_workloads(config_path, args.workloads, dry_run=args.dry_run)
    if not args.skip_image:
        _prepare_base_image(
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

    _require_libvirt(libvirt)
    _verify_qemu_user_config()
    _verify_isolation(config)
    _verify_workloads()

    print("environment verified")
    return 0


def restore_environment(args: argparse.Namespace) -> int:
    _restore_qemu_user_config(dry_run=args.dry_run)
    print("libvirt/qemu config restored")
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
    data = yaml.safe_load(template_path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise RuntimeError(f"template config is invalid: {template_path}")

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
        "boot_timeout_seconds": 30,
        "timeout_extra_seconds": 120,
        "destroy_on_exit": True,
        "cpu_mode": "host-passthrough",
    }
    data["executor"] = {
        **data.get("executor", {}),
        "parallel": "auto",
        "cpu_source": "isolated",
        "isolated_cpus": isolated_cpus,
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
    args = bench.get("args")
    if not isinstance(args, list):
        return
    for index, value in enumerate(args[:-1]):
        if value == "--source":
            args[index + 1] = str(kernel_source)
            return


def _write_config(path: Path, data: dict[str, Any], force: bool, dry_run: bool) -> None:
    if path.exists() and not force:
        raise RuntimeError(f"config already exists: {path}; use --force to overwrite")
    text = yaml.safe_dump(data, sort_keys=False)
    if dry_run:
        print(f"would write config: {path}")
        print(text)
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def _fetch_workloads(config_path: Path, workloads: list[str], dry_run: bool) -> None:
    command = [
        sys.executable,
        str(REPO_ROOT / "bench" / "scripts" / "fetch_workloads.py"),
        "--config",
        str(config_path),
        *workloads,
    ]
    _run(command, dry_run=dry_run)


def _prepare_base_image(
    config: dict[str, Any],
    cloud_image: Path,
    seed_image: Path,
    image_url: str,
    image_size: str,
    force: bool,
    dry_run: bool,
) -> None:
    libvirt = config["libvirt"]
    _ensure_libvirt_runtime(libvirt, dry_run=dry_run)

    root_image = Path(libvirt["root_image"])
    if root_image.exists() and not force:
        print(f"base image exists: {root_image}")
        return

    if force:
        _run_sudo(["rm", "-f", str(root_image)], dry_run=dry_run)
    _download_cloud_image(image_url, cloud_image, dry_run=dry_run)
    _run_sudo(["qemu-img", "convert", "-O", "qcow2", str(cloud_image), str(root_image)], dry_run)
    _run_sudo(["qemu-img", "resize", str(root_image), image_size], dry_run)
    _write_seed_image(libvirt, seed_image, dry_run=dry_run)
    _initialize_base_vm(libvirt, seed_image, dry_run=dry_run)


def _prepare_runtime_dir(libvirt: dict[str, Any], dry_run: bool) -> None:
    runtime_dir = Path(libvirt.get("runtime_dir", DEFAULT_RUNTIME_DIR))
    _run_sudo(["mkdir", "-p", str(runtime_dir)], dry_run=dry_run)
    user, group = _benchmark_user_group()
    _run_sudo(["chown", f"{user}:{group}", str(runtime_dir)], dry_run=dry_run)
    _run_sudo(["chmod", "0775", str(runtime_dir)], dry_run=dry_run)


def _ensure_libvirt_runtime(libvirt: dict[str, Any], dry_run: bool) -> None:
    if shutil.which("systemctl") is not None:
        _run_sudo(["systemctl", "enable", "--now", "libvirtd"], dry_run=dry_run)

    network = libvirt.get("network")
    if network:
        _run_sudo(
            ["virsh", "--connect", libvirt["uri"], "net-start", network],
            dry_run=dry_run,
            check=False,
        )
        _run_sudo(
            ["virsh", "--connect", libvirt["uri"], "net-autostart", network],
            dry_run=dry_run,
            check=False,
        )


def _prepare_qemu_user_config(dry_run: bool) -> None:
    user, group = _benchmark_user_group()
    original = _read_optional_root_file(QEMU_CONF, dry_run=dry_run)
    _backup_qemu_conf(existed=original is not None, dry_run=dry_run)
    updated = _qemu_conf_with_managed_block(original or "", user=user, group=group)
    _write_root_file(QEMU_CONF, updated, dry_run=dry_run)
    _restart_libvirt(dry_run=dry_run)


def _restore_qemu_user_config(dry_run: bool) -> None:
    if QEMU_CONF_BACKUP.exists():
        _run_sudo(["cp", "-a", str(QEMU_CONF_BACKUP), str(QEMU_CONF)], dry_run=dry_run)
        _run_sudo(["rm", "-f", str(QEMU_CONF_BACKUP), str(QEMU_CONF_MISSING_MARKER)], dry_run=dry_run)
    elif QEMU_CONF_MISSING_MARKER.exists():
        _run_sudo(["rm", "-f", str(QEMU_CONF), str(QEMU_CONF_MISSING_MARKER)], dry_run=dry_run)
    else:
        print(f"no scx-bench qemu.conf backup found: {QEMU_CONF_BACKUP}")
    _restart_libvirt(dry_run=dry_run)


def _backup_qemu_conf(existed: bool, dry_run: bool) -> None:
    if QEMU_CONF_BACKUP.exists() or QEMU_CONF_MISSING_MARKER.exists():
        return
    if existed:
        _run_sudo(["cp", "-a", str(QEMU_CONF), str(QEMU_CONF_BACKUP)], dry_run=dry_run)
    else:
        _run_sudo(["install", "-D", "-m", "0644", "/dev/null", str(QEMU_CONF_MISSING_MARKER)], dry_run=dry_run)


def _qemu_conf_with_managed_block(text: str, user: str, group: str) -> str:
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


def _verify_qemu_user_config() -> None:
    user, group = _benchmark_user_group()
    text = _read_optional_root_file(QEMU_CONF, dry_run=False)
    if text is None:
        raise RuntimeError(f"missing libvirt qemu config: {QEMU_CONF}")
    values = _parse_qemu_conf_values(text)
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
            + "; run: python3 bench/scripts/prepare_env.py init --kernel-source <path> --force"
        )


def _parse_qemu_conf_values(text: str) -> dict[str, str]:
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


def _benchmark_user_group() -> tuple[str, str]:
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


def _read_optional_root_file(path: Path, dry_run: bool) -> str | None:
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
        command = _sudo_command(["cat", str(path)])
        completed = subprocess.run(command, check=True, capture_output=True, text=True)
        return completed.stdout


def _write_root_file(path: Path, text: str, dry_run: bool) -> None:
    if dry_run:
        print(f"would write {path}:")
        print(text)
        return
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", delete=False) as tmp:
        tmp.write(text)
        tmp_path = Path(tmp.name)
    try:
        _run_sudo(["install", "-D", "-m", "0644", str(tmp_path), str(path)], dry_run=False)
    finally:
        tmp_path.unlink(missing_ok=True)


def _restart_libvirt(dry_run: bool) -> None:
    if shutil.which("systemctl") is None:
        return
    _run_sudo(["systemctl", "restart", "libvirtd"], dry_run=dry_run)


def _download_cloud_image(url: str, path: Path, dry_run: bool) -> None:
    if path.exists():
        return
    if dry_run:
        print(f"would download {url} -> {path}")
        return
    with tempfile.NamedTemporaryFile(delete=False) as tmp:
        tmp_path = Path(tmp.name)
    try:
        urllib.request.urlretrieve(url, tmp_path)
        _run_sudo(["mkdir", "-p", str(path.parent)], dry_run=False)
        _run_sudo(["mv", str(tmp_path), str(path)], dry_run=False)
    finally:
        if tmp_path.exists():
            tmp_path.unlink()


def _write_seed_image(libvirt: dict[str, Any], seed_image: Path, dry_run: bool) -> None:
    public_key_path = Path(f"{libvirt['ssh_key']}.pub")
    if public_key_path.exists():
        public_key = public_key_path.read_text(encoding="utf-8").strip()
    elif dry_run:
        public_key = "ssh-ed25519 DRY_RUN scx-bench"
    else:
        raise RuntimeError(f"missing SSH public key: {public_key_path}")
    user_data = f"""#cloud-config
users:
  - name: root
    ssh_authorized_keys:
      - {public_key}
    lock_passwd: false

disable_root: false
ssh_pwauth: false
apt:
  primary:
    - arches: [default]
      uri: http://mirrors.tuna.tsinghua.edu.cn/ubuntu
  security:
    - arches: [default]
      uri: http://mirrors.tuna.tsinghua.edu.cn/ubuntu
package_update: true
packages:
{chr(10).join(f"  - {pkg}" for pkg in DEFAULT_PACKAGES)}
runcmd:
  - systemctl enable ssh
  - systemctl start ssh
"""
    meta_data = "instance-id: scx-bench-base\nlocal-hostname: scx-bench-base\n"
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = Path(tmpdir)
        user_data_path = tmp / "user-data"
        meta_data_path = tmp / "meta-data"
        user_data_path.write_text(user_data, encoding="utf-8")
        meta_data_path.write_text(meta_data, encoding="utf-8")
        _run_sudo(["rm", "-f", str(seed_image)], dry_run=dry_run)
        _run_sudo(
            ["cloud-localds", str(seed_image), str(user_data_path), str(meta_data_path)],
            dry_run=dry_run,
        )


def _initialize_base_vm(libvirt: dict[str, Any], seed_image: Path, dry_run: bool) -> None:
    name = "scx-bench-base-init"
    _run_sudo(["virsh", "--connect", libvirt["uri"], "destroy", name], dry_run=dry_run, check=False)
    _run_sudo(["virsh", "--connect", libvirt["uri"], "undefine", name], dry_run=dry_run, check=False)
    command = [
        "virt-install",
        "--connect",
        libvirt["uri"],
        "--name",
        name,
        "--memory",
        "4096",
        "--vcpus",
        "2",
        "--disk",
        f"path={libvirt['root_image']},format=qcow2,bus=virtio",
        "--disk",
        f"path={seed_image},device=cdrom",
        "--os-variant",
        "ubuntu22.04",
        "--import",
        "--network",
        f"network={libvirt.get('network', 'default')},model=virtio",
        "--graphics",
        "none",
        "--noautoconsole",
    ]
    _run_sudo(command, dry_run=dry_run)
    if dry_run:
        return

    host = _wait_for_domain_ip(libvirt, name)
    _wait_for_ssh(libvirt, host)
    _ssh(libvirt, host, "cloud-init status --wait")
    _sanitize_guest_image(libvirt, host)
    _sync_repo_to_guest(libvirt, host)
    _ssh(libvirt, host, "sync && poweroff", check=False)
    time.sleep(5)
    _run_sudo(["virsh", "--connect", libvirt["uri"], "destroy", name], dry_run=False, check=False)
    _run_sudo(["virsh", "--connect", libvirt["uri"], "undefine", name], dry_run=False, check=False)


def _sanitize_guest_image(libvirt: dict[str, Any], host: str) -> None:
    # Direct kernel boot does not need the guest EFI partition. If the matching
    # filesystem modules are unavailable, systemd drops into emergency mode.
    _ssh(
        libvirt,
        host,
        r"sed -i.bak '\|[[:space:]]/boot/efi[[:space:]]|s|^|# scx-bench disabled: |' /etc/fstab",
    )
    _ssh(
        libvirt,
        host,
        r"""cat > /etc/netplan/01-scx-bench.yaml <<'EOF'
network:
  version: 2
  ethernets:
    scxbench:
      match:
        name: "e*"
      dhcp4: true
      dhcp6: false
      optional: true
EOF
chmod 600 /etc/netplan/01-scx-bench.yaml
rm -f /etc/netplan/50-cloud-init.yaml
systemctl mask systemd-networkd-wait-online.service
systemctl disable --now snapd.service snapd.socket snapd.seeded.service snap.lxd.activate.service 2>/dev/null || true
systemctl mask snapd.service snapd.socket snapd.seeded.service snap.lxd.activate.service 2>/dev/null || true
""",
    )


def _sync_repo_to_guest(libvirt: dict[str, Any], host: str) -> None:
    workdir = Path(libvirt["workdir"])
    remote_parent = str(workdir.parent)
    _ssh(libvirt, host, f"mkdir -p {remote_parent}")
    with tempfile.NamedTemporaryFile(suffix=".tar.gz") as tmp:
        with tarfile.open(tmp.name, "w:gz") as archive:
            archive.add(REPO_ROOT, arcname=".", filter=_tar_filter)
        _scp(libvirt, host, Path(tmp.name), "/tmp/scx_agent.tar.gz")
    _ssh(
        libvirt,
        host,
        f"rm -rf {libvirt['workdir']} && mkdir -p {libvirt['workdir']} && "
        f"tar -xzf /tmp/scx_agent.tar.gz -C {libvirt['workdir']}",
    )


def _tar_filter(info: tarfile.TarInfo) -> tarfile.TarInfo | None:
    name = info.name.lstrip("./")
    parts = Path(name).parts
    if ".git" in parts:
        return None
    if name == "bench/results" or name.startswith("bench/results/"):
        return None
    if name == "bench/workloads/src" or name.startswith("bench/workloads/src/"):
        return None
    if name == "bench/workloads/build" or name.startswith("bench/workloads/build/"):
        return None
    if name == "schedule/scx/target":
        return info
    if name == "schedule/scx/target/release":
        return info
    if name.startswith("schedule/scx/target/release/"):
        rel = Path(name).relative_to("schedule/scx/target/release")
        if len(rel.parts) == 1 and info.isfile() and info.mode & 0o111:
            filename = rel.parts[0]
            if filename.startswith("scx"):
                return info
        return None
    if name.startswith("schedule/scx/target/"):
        return None
    if "__pycache__" in parts:
        return None
    return info


def _prepare_isolation(config_path: Path, force: bool, dry_run: bool) -> None:
    command = [
        sys.executable,
        str(REPO_ROOT / "bench" / "scripts" / "isolation.py"),
        "prepare",
        "--config",
        str(config_path),
        "--no-reboot",
    ]
    if force:
        command.append("--force")
    _run_sudo(command, dry_run=dry_run)


def _require_libvirt(libvirt: dict[str, Any]) -> None:
    _run_sudo(["virsh", "--connect", libvirt["uri"], "uri"], dry_run=False)
    network = libvirt.get("network")
    if network is not None:
        _run_sudo(["virsh", "--connect", libvirt["uri"], "net-info", network], dry_run=False)


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


def _wait_for_domain_ip(libvirt: dict[str, Any], name: str, timeout: int = 300) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        completed = subprocess.run(
            ["sudo", "virsh", "--connect", libvirt["uri"], "domifaddr", name, "--source", "lease"],
            check=False,
            capture_output=True,
            text=True,
        )
        for token in completed.stdout.split():
            if "/" in token and token.count(".") == 3:
                return token.split("/", 1)[0]
        time.sleep(5)
    raise RuntimeError(f"could not determine IP for domain: {name}")


def _wait_for_ssh(libvirt: dict[str, Any], host: str, timeout: int = 300) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        completed = subprocess.run(
            _ssh_base(libvirt, host) + ["true"],
            check=False,
            capture_output=True,
            text=True,
        )
        if completed.returncode == 0:
            return
        time.sleep(5)
    raise RuntimeError(f"SSH did not become ready: {host}")


def _ssh(libvirt: dict[str, Any], host: str, command: str, check: bool = True) -> None:
    _run(_ssh_base(libvirt, host) + [command], dry_run=False, check=check)


def _scp(libvirt: dict[str, Any], host: str, src: Path, dst: str) -> None:
    target = f"{libvirt['ssh_user']}@{host}:{dst}"
    _run(
        [
            "scp",
            "-i",
            libvirt["ssh_key"],
            "-P",
            str(libvirt.get("ssh_port", 22)),
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            str(src),
            target,
        ],
        dry_run=False,
    )


def _ssh_base(libvirt: dict[str, Any], host: str) -> list[str]:
    return [
        "ssh",
        "-i",
        libvirt["ssh_key"],
        "-p",
        str(libvirt.get("ssh_port", 22)),
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        f"{libvirt['ssh_user']}@{host}",
    ]


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
    if os.geteuid() != 0:
        return ["sudo", *command]
    return command


def _run(command: list[str], dry_run: bool, check: bool = True) -> None:
    print("+", " ".join(command))
    if not dry_run:
        subprocess.run(command, check=check)


if __name__ == "__main__":
    raise SystemExit(main())
