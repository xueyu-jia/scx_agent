from __future__ import annotations

from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from bench.config.parser import load_config
from bench.scripts.prepare_env import REPO_ROOT, _build_local_config, _write_config


class PrepareEnvConfigTest(unittest.TestCase):
    def test_write_split_local_config_from_example_template(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            kernel_source = root / "linux"
            (kernel_source / "arch" / "x86" / "boot").mkdir(parents=True)
            (kernel_source / "arch" / "x86" / "boot" / "bzImage").write_text(
                "kernel",
                encoding="utf-8",
            )
            (kernel_source / "tools" / "perf").mkdir(parents=True)
            ssh_key = root / "id_ed25519"
            ssh_key.write_text("key", encoding="utf-8")
            workdir = root / "work"
            workdir.mkdir()

            data = _build_local_config(
                template_path=REPO_ROOT / "bench" / "configs" / "example_config",
                kernel_source=kernel_source,
                kernel=kernel_source / "arch" / "x86" / "boot" / "bzImage",
                root_image=root / "root.qcow2",
                ssh_key=ssh_key,
                workdir=workdir,
                emulator_cpus="0",
                isolated_cpus="1",
            )
            config_path = root / "local_config"

            _write_config(config_path, data, force=False, dry_run=False)
            config = load_config(config_path)
            args = config["benches"]["kernel_build_bzimage"]["measurement"]["args"]

            self.assertTrue((config_path / "environment.config").is_file())
            self.assertTrue((config_path / "benches.config").is_file())
            self.assertTrue((config_path / "plan.config").is_file())
            self.assertEqual(config["libvirt"]["kernel_source"], str(kernel_source))
            self.assertIn(str(kernel_source), args)
            self.assertIn(
                "\n\nexecutor:\n",
                (config_path / "environment.config").read_text(encoding="utf-8"),
            )
            self.assertIn(
                "\n\n  schbench_smoke:\n",
                (config_path / "benches.config").read_text(encoding="utf-8"),
            )


if __name__ == "__main__":
    unittest.main()
