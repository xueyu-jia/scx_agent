#!/usr/bin/env python3
"""Sync the current project tree into the existing VM base image."""
from __future__ import annotations

import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(REPO_ROOT))
from bench.config.parser import load_config


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


def run(cmd, **kwargs):
    print("+", " ".join(str(c) for c in cmd))
    subprocess.run(cmd, check=True, **kwargs)


def main():
    config = load_config(REPO_ROOT / "bench" / "configs" / "local.config")
    libvirt = config["libvirt"]
    name = "scx-bench-sync"
    uri = libvirt.get("uri", "qemu:///system")

    # Destroy any leftover
    for args in (["destroy", name], ["undefine", name]):
        subprocess.run(["virsh", "--connect", uri, *args], check=False, capture_output=True)

    print("=== Starting temporary VM ===")
    run([
        "virt-install",
        "--connect", uri,
        "--name", name,
        "--memory", "2048",
        "--vcpus", "2",
        "--disk", f"path={libvirt['root_image']},format=qcow2,bus=virtio",
        "--os-variant", "ubuntu22.04",
        "--import",
        "--network", f"network={libvirt.get('network', 'default')},model=virtio",
        "--graphics", "none",
        "--noautoconsole",
    ])

    print("=== Waiting for VM IP ===")
    host = None
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        result = subprocess.run(
            ["virsh", "--connect", uri, "domifaddr", name, "--source", "lease"],
            check=False, capture_output=True, text=True,
        )
        for token in result.stdout.split():
            if "/" in token and token.count(".") == 3:
                host = token.split("/", 1)[0]
                break
        if host:
            break
        time.sleep(5)
    if not host:
        raise RuntimeError("Could not get VM IP")
    print(f"VM IP: {host}")

    print("=== Waiting for SSH ===")
    ssh_base = [
        "ssh", "-i", libvirt["ssh_key"],
        "-p", str(libvirt.get("ssh_port", 22)),
        "-o", "BatchMode=yes",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        f"{libvirt['ssh_user']}@{host}",
    ]
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        result = subprocess.run(ssh_base + ["true"], check=False, capture_output=True)
        if result.returncode == 0:
            break
        time.sleep(5)
    else:
        raise RuntimeError("SSH not ready")

    print("=== Creating project tarball ===")
    with tempfile.NamedTemporaryFile(suffix=".tar.gz", delete=False) as tmp:
        tarball = Path(tmp.name)
    try:
        with tarfile.open(tarball, "w:gz") as archive:
            archive.add(REPO_ROOT, arcname=".", filter=_tar_filter)
        print(f"Tarball size: {tarball.stat().st_size / 1024 / 1024:.1f} MB")

        print("=== Uploading to VM ===")
        target = f"{libvirt['ssh_user']}@{host}:/tmp/scx_agent.tar.gz"
        run([
            "scp", "-i", libvirt["ssh_key"],
            "-P", str(libvirt.get("ssh_port", 22)),
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            str(tarball), target,
        ])

        print("=== Extracting in VM ===")
        workdir = libvirt["workdir"]
        run(ssh_base + [
            f"rm -rf {workdir} && mkdir -p {workdir} && "
            f"tar -xzf /tmp/scx_agent.tar.gz -C {workdir} && "
            "rm /tmp/scx_agent.tar.gz && "
            "echo OK"
        ])
    finally:
        tarball.unlink()

    print("=== Shutting down VM ===")
    subprocess.run(ssh_base + ["poweroff"], check=False, capture_output=True)
    time.sleep(5)
    for args in (["destroy", name], ["undefine", name]):
        subprocess.run(["virsh", "--connect", uri, *args], check=False, capture_output=True)

    print("=== Done: VM base image updated ===")


if __name__ == "__main__":
    raise SystemExit(main())
