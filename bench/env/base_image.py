from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import tarfile
import tempfile
import time
import urllib.request
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
MANIFEST_VERSION = 1
BENCHMARK_WRAPPER_ROOT = Path("bench/benchmarks")
REBUILD_HINT = "run `python3 -m bench.env rebuild-image`"
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


class BaseImageManifestError(RuntimeError):
    """Raised when base-image provenance is missing, invalid, or stale."""


def base_image_manifest_path(root_image: str | Path) -> Path:
    image = Path(root_image).expanduser().resolve()
    return image.with_name(f"{image.name}.scx-bench-manifest.json")


def benchmark_wrapper_snapshot(repo_root: str | Path) -> dict[str, Any]:
    repository = Path(repo_root).resolve()
    wrapper_root = repository / BENCHMARK_WRAPPER_ROOT
    if not wrapper_root.is_dir():
        raise BaseImageManifestError(
            f"benchmark wrapper directory does not exist: {wrapper_root}"
        )

    files: dict[str, str] = {}
    for path in sorted(wrapper_root.rglob("*")):
        relative = path.relative_to(wrapper_root)
        if _is_generated_file(relative) or not path.is_file():
            continue
        try:
            files[relative.as_posix()] = _sha256_file(path)
        except OSError as exc:
            raise BaseImageManifestError(
                f"cannot hash benchmark wrapper {path}: {exc}"
            ) from exc

    if not files:
        raise BaseImageManifestError(
            f"benchmark wrapper directory contains no source files: {wrapper_root}"
        )
    return {
        "root": BENCHMARK_WRAPPER_ROOT.as_posix(),
        "files": files,
    }


def build_base_image_manifest(
    root_image: str | Path,
    repo_root: str | Path,
    *,
    wrappers: dict[str, Any] | None = None,
) -> dict[str, Any]:
    image = Path(root_image).expanduser().resolve()
    try:
        stat = image.stat()
    except OSError as exc:
        raise BaseImageManifestError(f"cannot stat base image {image}: {exc}") from exc

    return {
        "version": MANIFEST_VERSION,
        "image": {
            "path": str(image),
            "device": stat.st_dev,
            "inode": stat.st_ino,
            "size": stat.st_size,
            "mtime_ns": stat.st_mtime_ns,
        },
        "benchmark_wrappers": (
            wrappers if wrappers is not None else benchmark_wrapper_snapshot(repo_root)
        ),
    }


def serialize_base_image_manifest(manifest: dict[str, Any]) -> str:
    return json.dumps(manifest, indent=2, sort_keys=True) + "\n"


def verify_base_image_manifest(
    root_image: str | Path,
    repo_root: str | Path,
) -> dict[str, Any]:
    image = Path(root_image).expanduser().resolve()
    manifest_path = base_image_manifest_path(image)
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise BaseImageManifestError(
            f"base image manifest is missing: {manifest_path}; {REBUILD_HINT}"
        ) from exc
    except (OSError, json.JSONDecodeError) as exc:
        raise BaseImageManifestError(
            f"cannot read base image manifest {manifest_path}: {exc}"
        ) from exc

    version = manifest.get("version") if isinstance(manifest, dict) else None
    if isinstance(version, bool) or version != MANIFEST_VERSION:
        raise BaseImageManifestError(
            f"unsupported base image manifest: {manifest_path}; {REBUILD_HINT}"
        )

    _verify_image_identity(image, manifest.get("image"), manifest_path)
    expected = _wrapper_files(manifest.get("benchmark_wrappers"), manifest_path)
    current_snapshot = benchmark_wrapper_snapshot(repo_root)
    current = current_snapshot["files"]
    if expected != current:
        raise BaseImageManifestError(
            "base image benchmark wrappers are stale: "
            f"{_format_wrapper_diff(expected, current)}; {REBUILD_HINT}"
        )
    return manifest


def prepare_base_image(
    config: dict[str, Any],
    cloud_image: Path,
    seed_image: Path,
    image_url: str,
    image_size: str,
    force: bool,
    dry_run: bool,
) -> None:
    libvirt = config["libvirt"]
    root_image = Path(libvirt["root_image"])
    manifest_path = base_image_manifest_path(root_image)
    if root_image.exists() and not force:
        verify_base_image_manifest(root_image, REPO_ROOT)
        print(f"base image is current: {root_image}")
        return

    if force:
        _run_sudo(
            ["rm", "-f", str(root_image), str(manifest_path)],
            dry_run=dry_run,
        )
    else:
        _run_sudo(["rm", "-f", str(manifest_path)], dry_run=dry_run)
    _download_cloud_image(image_url, cloud_image, dry_run=dry_run)
    _run_sudo(
        ["qemu-img", "convert", "-O", "qcow2", str(cloud_image), str(root_image)],
        dry_run,
    )
    _run_sudo(["qemu-img", "resize", str(root_image), image_size], dry_run)
    _write_seed_image(libvirt, seed_image, dry_run=dry_run)
    wrappers = _initialize_base_vm(libvirt, seed_image, dry_run=dry_run)
    _write_base_image_manifest(root_image, wrappers, dry_run=dry_run)


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


def _write_seed_image(
    libvirt: dict[str, Any],
    seed_image: Path,
    dry_run: bool,
) -> None:
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
{chr(10).join(f"  - {package}" for package in DEFAULT_PACKAGES)}
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


def _initialize_base_vm(
    libvirt: dict[str, Any],
    seed_image: Path,
    dry_run: bool,
) -> dict[str, Any] | None:
    name = "scx-bench-base-init"
    _run_sudo(
        ["virsh", "--connect", libvirt["uri"], "destroy", name],
        dry_run=dry_run,
        check=False,
    )
    _run_sudo(
        ["virsh", "--connect", libvirt["uri"], "undefine", name],
        dry_run=dry_run,
        check=False,
    )
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
        return None

    host = _wait_for_domain_ip(libvirt, name)
    _wait_for_ssh(libvirt, host)
    _ssh(libvirt, host, "cloud-init status --wait")
    _sanitize_guest_image(libvirt, host)
    wrappers_before, wrappers_after, guest_wrappers_match = _sync_repo_to_guest(
        libvirt,
        host,
    )
    _ssh(libvirt, host, "sync && poweroff", check=False)
    time.sleep(5)
    _run_sudo(
        ["virsh", "--connect", libvirt["uri"], "destroy", name],
        dry_run=False,
        check=False,
    )
    _run_sudo(
        ["virsh", "--connect", libvirt["uri"], "undefine", name],
        dry_run=False,
        check=False,
    )
    if not guest_wrappers_match:
        raise BaseImageManifestError(
            "base image benchmark wrappers do not match the host snapshot; rebuild it"
        )
    if wrappers_before != wrappers_after:
        raise BaseImageManifestError(
            "benchmark wrappers changed while the base image was being built; rebuild it"
        )
    return wrappers_before


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
      dhcp-identifier: mac
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


def _sync_repo_to_guest(
    libvirt: dict[str, Any],
    host: str,
) -> tuple[dict[str, Any], dict[str, Any], bool]:
    wrappers_before = benchmark_wrapper_snapshot(REPO_ROOT)
    workdir = Path(libvirt["workdir"])
    remote_parent = str(workdir.parent)
    _ssh(libvirt, host, f"mkdir -p {shlex.quote(remote_parent)}")
    with tempfile.NamedTemporaryFile(suffix=".tar.gz") as tmp:
        with tarfile.open(tmp.name, "w:gz") as archive:
            archive.add(REPO_ROOT, arcname=".", filter=_tar_filter)
        _scp(libvirt, host, Path(tmp.name), "/tmp/scx_agent.tar.gz")
    _ssh(
        libvirt,
        host,
        f"rm -rf {shlex.quote(libvirt['workdir'])} && "
        f"mkdir -p {shlex.quote(libvirt['workdir'])} && "
        f"tar -xzf /tmp/scx_agent.tar.gz -C {shlex.quote(libvirt['workdir'])} && "
        "rm -f /tmp/scx_agent.tar.gz",
    )
    guest_wrappers_match = _verify_guest_wrapper_snapshot(
        libvirt,
        host,
        wrappers_before,
    )
    wrappers_after = benchmark_wrapper_snapshot(REPO_ROOT)
    return wrappers_before, wrappers_after, guest_wrappers_match


def _verify_guest_wrapper_snapshot(
    libvirt: dict[str, Any],
    host: str,
    expected: dict[str, Any],
) -> bool:
    remote_manifest = "/tmp/scx-bench-wrapper-snapshot.json"
    with tempfile.NamedTemporaryFile(
        "w",
        encoding="utf-8",
        delete=False,
    ) as temporary:
        json.dump(expected, temporary, sort_keys=True)
        temporary_path = Path(temporary.name)
    try:
        _scp(libvirt, host, temporary_path, remote_manifest)
    finally:
        temporary_path.unlink(missing_ok=True)

    code = (
        "import json, sys; "
        "from pathlib import Path; "
        "from bench.env.base_image import benchmark_wrapper_snapshot; "
        "expected = json.loads(Path(sys.argv[1]).read_text(encoding='utf-8')); "
        "actual = benchmark_wrapper_snapshot(sys.argv[2]); "
        "raise SystemExit(0 if actual == expected else "
        "'guest benchmark wrappers do not match host snapshot')"
    )
    command = shlex.join(
        ["python3", "-c", code, remote_manifest, libvirt["workdir"]]
    )
    cleanup = f"rm -f {shlex.quote(remote_manifest)}"
    remote = (
        f"set -eu; trap {shlex.quote(cleanup)} EXIT; "
        f"cd {shlex.quote(libvirt['workdir'])}; {command}"
    )
    ssh_command = _ssh_base(libvirt, host) + [remote]
    print("+", shlex.join(ssh_command), flush=True)
    completed = subprocess.run(ssh_command, check=False)
    return completed.returncode == 0


def _write_base_image_manifest(
    root_image: Path,
    wrappers: dict[str, Any] | None,
    *,
    dry_run: bool,
) -> None:
    destination = base_image_manifest_path(root_image)
    if dry_run:
        print(f"would write base image manifest: {destination}")
        return
    if wrappers is None:
        raise BaseImageManifestError(
            "base image build did not produce a wrapper snapshot"
        )

    manifest = build_base_image_manifest(
        root_image,
        REPO_ROOT,
        wrappers=wrappers,
    )
    with tempfile.NamedTemporaryFile(
        "w",
        encoding="utf-8",
        delete=False,
    ) as temporary:
        temporary.write(serialize_base_image_manifest(manifest))
        temporary_path = Path(temporary.name)
    try:
        _run_sudo(
            ["install", "-m", "0644", str(temporary_path), str(destination)],
            dry_run=False,
        )
    finally:
        temporary_path.unlink(missing_ok=True)


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
        relative = Path(name).relative_to("schedule/scx/target/release")
        if len(relative.parts) == 1 and info.isfile() and info.mode & 0o111:
            filename = relative.parts[0]
            if filename.startswith("scx"):
                return info
        return None
    if name == "tuning_agent/target" or name.startswith("tuning_agent/target/"):
        return None
    if name.startswith("schedule/scx/target/"):
        return None
    if "__pycache__" in parts:
        return None
    return info


def _wait_for_domain_ip(
    libvirt: dict[str, Any],
    name: str,
    timeout: int = 300,
) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        completed = subprocess.run(
            _sudo_command(
                [
                    "virsh",
                    "--connect",
                    libvirt["uri"],
                    "domifaddr",
                    name,
                    "--source",
                    "lease",
                ]
            ),
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


def _verify_image_identity(
    image: Path,
    value: Any,
    manifest_path: Path,
) -> None:
    if not isinstance(value, dict):
        raise BaseImageManifestError(
            f"invalid image identity in base image manifest: {manifest_path}"
        )
    expected_size = value.get("size")
    expected_mtime_ns = value.get("mtime_ns")
    expected_device = value.get("device")
    expected_inode = value.get("inode")
    if (
        isinstance(expected_size, bool)
        or not isinstance(expected_size, int)
        or isinstance(expected_mtime_ns, bool)
        or not isinstance(expected_mtime_ns, int)
        or isinstance(expected_device, bool)
        or not isinstance(expected_device, int)
        or isinstance(expected_inode, bool)
        or not isinstance(expected_inode, int)
    ):
        raise BaseImageManifestError(
            f"invalid image identity in base image manifest: {manifest_path}"
        )

    try:
        stat = image.stat()
    except OSError as exc:
        raise BaseImageManifestError(f"cannot stat base image {image}: {exc}") from exc
    if (
        stat.st_dev != expected_device
        or stat.st_ino != expected_inode
        or stat.st_size != expected_size
        or stat.st_mtime_ns != expected_mtime_ns
    ):
        raise BaseImageManifestError(
            f"base image does not match its manifest: {image}; {REBUILD_HINT}"
        )


def _wrapper_files(value: Any, manifest_path: Path) -> dict[str, str]:
    if (
        not isinstance(value, dict)
        or value.get("root") != BENCHMARK_WRAPPER_ROOT.as_posix()
    ):
        raise BaseImageManifestError(
            f"invalid benchmark wrapper root in base image manifest: {manifest_path}"
        )
    files = value.get("files")
    if not isinstance(files, dict) or not files:
        raise BaseImageManifestError(
            f"invalid benchmark wrapper files in base image manifest: {manifest_path}"
        )
    if any(
        not isinstance(path, str)
        or not path
        or not isinstance(digest, str)
        or len(digest) != 64
        or any(character not in "0123456789abcdef" for character in digest)
        for path, digest in files.items()
    ):
        raise BaseImageManifestError(
            f"invalid benchmark wrapper digest in base image manifest: {manifest_path}"
        )
    return dict(files)


def _format_wrapper_diff(expected: dict[str, str], current: dict[str, str]) -> str:
    expected_names = set(expected)
    current_names = set(current)
    added = sorted(current_names - expected_names)
    removed = sorted(expected_names - current_names)
    changed = sorted(
        name
        for name in expected_names & current_names
        if expected[name] != current[name]
    )
    parts: list[str] = []
    for label, names in (("added", added), ("removed", removed), ("changed", changed)):
        if names:
            shown = names[:5]
            suffix = (
                f" (+{len(names) - len(shown)} more)"
                if len(names) > len(shown)
                else ""
            )
            parts.append(f"{label}={shown}{suffix}")
    return ", ".join(parts) or "manifest content differs"


def _is_generated_file(relative: Path) -> bool:
    return "__pycache__" in relative.parts or relative.suffix in {".pyc", ".pyo"}


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _run_sudo(command: list[str], dry_run: bool, check: bool = True) -> None:
    _run(_sudo_command(command), dry_run=dry_run, check=check)


def _sudo_command(command: list[str]) -> list[str]:
    no_sudo = os.environ.get("SCX_BENCH_NO_SUDO", "").lower() in {
        "1",
        "true",
        "yes",
    }
    if os.geteuid() != 0 and not no_sudo:
        return ["sudo", *command]
    return command


def _run(command: list[str], dry_run: bool, check: bool = True) -> None:
    print("+", " ".join(command), flush=True)
    if not dry_run:
        subprocess.run(command, check=check)
