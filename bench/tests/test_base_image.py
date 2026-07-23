from __future__ import annotations

from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import tempfile
import unittest

from bench.env.base_image import (
    BaseImageManifestError,
    base_image_manifest_path,
    benchmark_wrapper_snapshot,
    build_base_image_manifest,
    _kernel_source_tar_filter,
    _tar_filter,
    prepare_base_image,
    serialize_base_image_manifest,
    verify_base_image_manifest,
)


class BaseImageManifestTest(unittest.TestCase):
    def _fixture(self, root: Path) -> tuple[Path, Path, Path]:
        repository = root / "repository"
        wrappers = repository / "bench" / "benchmarks"
        wrappers.mkdir(parents=True)
        wrapper = wrappers / "example.py"
        wrapper.write_text("print('v1')\n", encoding="utf-8")
        image = root / "base.qcow2"
        image.write_bytes(b"qcow2-fixture")
        return repository, wrapper, image

    def _write_manifest(self, repository: Path, image: Path) -> Path:
        manifest = build_base_image_manifest(image, repository)
        path = base_image_manifest_path(image)
        path.write_text(serialize_base_image_manifest(manifest), encoding="utf-8")
        return path

    def test_current_image_and_wrappers_pass(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repository, _wrapper, image = self._fixture(root)
            self._write_manifest(repository, image)

            manifest = verify_base_image_manifest(image, repository)

            self.assertEqual(manifest["version"], 1)
            self.assertEqual(
                set(manifest["benchmark_wrappers"]["files"]),
                {"example.py"},
            )

    def test_wrapper_modification_requires_rebuild(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repository, wrapper, image = self._fixture(root)
            self._write_manifest(repository, image)
            wrapper.write_text("print('v2')\n", encoding="utf-8")

            with self.assertRaisesRegex(
                BaseImageManifestError,
                r"stale: changed=\['example\.py'\]",
            ):
                verify_base_image_manifest(image, repository)

    def test_added_and_removed_wrappers_require_rebuild(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repository, wrapper, image = self._fixture(root)
            self._write_manifest(repository, image)
            wrapper.unlink()
            replacement = wrapper.with_name("replacement.py")
            replacement.write_text("pass\n", encoding="utf-8")

            with self.assertRaisesRegex(BaseImageManifestError, "added=.*removed="):
                verify_base_image_manifest(image, repository)

    def test_generated_python_files_do_not_invalidate_image(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repository, _wrapper, image = self._fixture(root)
            self._write_manifest(repository, image)
            cache = repository / "bench" / "benchmarks" / "__pycache__"
            cache.mkdir()
            (cache / "example.cpython-311.pyc").write_bytes(b"generated")

            verify_base_image_manifest(image, repository)

            self.assertEqual(
                set(benchmark_wrapper_snapshot(repository)["files"]),
                {"example.py"},
            )

    def test_image_replacement_invalidates_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repository, _wrapper, image = self._fixture(root)
            self._write_manifest(repository, image)
            original = image.stat()
            replacement = image.with_name("replacement.qcow2")
            replacement.write_bytes(image.read_bytes())
            os.utime(
                replacement,
                ns=(original.st_atime_ns, original.st_mtime_ns),
            )
            replacement.replace(image)

            with self.assertRaisesRegex(
                BaseImageManifestError,
                "base image does not match its manifest",
            ):
                verify_base_image_manifest(image, repository)

    def test_missing_or_invalid_manifest_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            repository, _wrapper, image = self._fixture(root)

            with self.assertRaisesRegex(BaseImageManifestError, "manifest is missing"):
                verify_base_image_manifest(image, repository)

            manifest_path = base_image_manifest_path(image)
            manifest_path.write_text(json.dumps({"version": True}), encoding="utf-8")
            with self.assertRaisesRegex(BaseImageManifestError, "unsupported"):
                verify_base_image_manifest(image, repository)

    def test_dry_run_build_does_not_create_image_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            image = root / "base.qcow2"
            with redirect_stdout(io.StringIO()):
                prepare_base_image(
                    config={
                        "libvirt": {
                            "uri": "qemu:///system",
                            "root_image": str(image),
                            "ssh_key": str(root / "key"),
                            "network": "default",
                        }
                    },
                    cloud_image=root / "cloud.qcow2",
                    seed_image=root / "seed.iso",
                    image_url="https://example.invalid/cloud.qcow2",
                    image_size="40G",
                    force=False,
                    dry_run=True,
                )

            self.assertFalse(image.exists())
            self.assertFalse(base_image_manifest_path(image).exists())

    def test_kernel_source_filter_excludes_generated_repository_state(self) -> None:
        import tarfile

        self.assertIsNone(_kernel_source_tar_filter(tarfile.TarInfo("./.git/index")))
        self.assertIsNone(
            _kernel_source_tar_filter(tarfile.TarInfo("./tools/__pycache__/tool.pyc"))
        )
        source = tarfile.TarInfo("./kernel/sched/ext.c")
        self.assertIs(_kernel_source_tar_filter(source), source)

    def test_repository_filter_keeps_only_scheduler_release_binaries(self) -> None:
        import tarfile

        debug = tarfile.TarInfo(
            "./schedule/scx_agent_classed/target/debug/scx_agent_classed"
        )
        self.assertIsNone(_tar_filter(debug))

        release = tarfile.TarInfo(
            "./schedule/scx_agent_classed/target/release/scx_agent_classed"
        )
        release.mode = 0o755
        release.type = tarfile.REGTYPE
        self.assertIs(_tar_filter(release), release)


if __name__ == "__main__":
    unittest.main()
