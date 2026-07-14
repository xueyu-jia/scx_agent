from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any


MANIFEST_VERSION = 1
BENCHMARK_WRAPPER_ROOT = Path("bench/benchmarks")
REBUILD_HINT = "run `python3 bench/scripts/prepare_env.py rebuild-image`"


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
