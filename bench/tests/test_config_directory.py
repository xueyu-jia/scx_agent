from __future__ import annotations

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import yaml

from bench.core.config import (
    CONFIG_PART_KEYS,
    ConfigError,
    load_config,
    load_config_data,
)
from bench.env.manager import _build_local_config, _write_config


class ConfigDirectoryLoadingTest(unittest.TestCase):
    def test_tracked_config_directories_are_valid(self) -> None:
        example = load_config_data("bench/configs/example_config")
        redis = load_config_data("bench/configs/redis_cpu_tuning")

        self.assertIn("scx_agent_classed_llm_latency", example["schedulers"])
        self.assertIn("llm_latency_classify", example["treatments"])
        self.assertIn("redis_cpu_agent_positive", redis["treatments"])

    def test_example_config_contains_only_the_active_performance_matrix(self) -> None:
        config = load_config_data("bench/configs/example_config")

        self.assertEqual(
            set(config["schedulers"]),
            {
                "default",
                "alt_default",
                "scx_agent_classed",
                "scx_agent_classed_llm_latency",
                "scx_agent_classed_llm_batch",
                "scx_agent_classed_llm_mixed",
            },
        )
        self.assertEqual(
            set(config["treatments"]),
            {
                "llm_latency_control",
                "llm_latency_classify",
                "llm_batch_control",
                "llm_batch_classify",
                "llm_mixed_control",
                "llm_mixed_classify",
            },
        )
        self.assertEqual(
            set(config["plans"]),
            {
                "kernel_migration_smoke",
                "single_latency_candidate_gate",
                "single_batch_candidate_gate",
                "mixed_candidate_gate",
                "single_latency_core_priming",
                "single_latency_core_measured",
                "single_batch_core_priming",
                "single_batch_core_measured",
                "mixed_fixed_rps_core_priming",
                "mixed_fixed_rps_core_measured",
            },
        )
        self.assertEqual(set(config["machines"]), {"small_core"})
        self.assertIn("psi=1", config["libvirt"]["kernel_args"].split())
        self.assertEqual(config["plans"]["single_latency_core_priming"]["runs"], 2)
        self.assertEqual(config["plans"]["single_batch_core_priming"]["runs"], 2)
        self.assertEqual(config["plans"]["mixed_fixed_rps_core_priming"]["runs"], 2)
        self.assertEqual(config["plans"]["single_latency_core_measured"]["runs"], 8)
        self.assertEqual(config["plans"]["single_batch_core_measured"]["runs"], 4)
        self.assertEqual(config["plans"]["mixed_fixed_rps_core_measured"]["runs"], 4)
        for treatment in config["treatments"].values():
            duration_index = treatment["args"].index("--duration-seconds")
            self.assertEqual(treatment["args"][duration_index + 1], "240")
            self.assertEqual(treatment["timeout_seconds"], 300)
            mode_index = treatment["args"].index("--mode")
            if treatment["args"][mode_index + 1] == "classify":
                classify_index = treatment["args"].index(
                    "--classify-timeout-seconds"
                )
                self.assertEqual(treatment["args"][classify_index + 1], "180")
        self.assertEqual(
            set(config["benches"]),
            {
                "schbench_latency_contended_perf",
                "stress_ng_batch_oversubscribed_perf",
                "mixed_schbench_stress_fixed_rps_100_perf",
            },
        )
        for bench in config["benches"].values():
            args = bench["measurement"]["args"]
            self.assertIn("context-switches,cpu-migrations", args)
            self.assertNotIn("context-switches", args)
            self.assertNotIn("cpu-migrations", args)
        llm_schedulers = {
            name: scheduler
            for name, scheduler in config["schedulers"].items()
            if name.startswith("scx_agent_classed_llm_")
        }
        self.assertEqual(len(llm_schedulers), 3)
        for scheduler in llm_schedulers.values():
            self.assertEqual(
                scheduler["env"]["SCX_REAL_LLM_MODEL"],
                "deepseek-v4-flash",
            )
            self.assertNotIn(
                "perf",
                scheduler["env"]["SCX_REAL_LLM_ACTIVATION_COMMS"],
            )
        classify_treatments = {
            name: treatment
            for name, treatment in config["treatments"].items()
            if name.endswith("_classify")
        }
        self.assertEqual(len(classify_treatments), 3)
        for treatment in classify_treatments.values():
            self.assertEqual(
                treatment["env"]["SCX_PERF_EXPECTED_MODEL"],
                "deepseek-v4-flash",
            )
            self.assertNotIn("perf=batch", treatment["args"])

    def test_config_path_must_be_a_directory(self) -> None:
        with TemporaryDirectory() as tmp:
            path = Path(tmp) / "config.config"
            path.write_text("libvirt: {}\n", encoding="utf-8")

            with self.assertRaisesRegex(ConfigError, "config path must be a directory"):
                load_config_data(path)

    def test_all_config_parts_are_required(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_part(root / "environment.config", {})
            self._write_part(root / "benches.config", {})

            with self.assertRaisesRegex(ConfigError, r"missing config part: .*plan\.config"):
                load_config_data(root)

    def test_unknown_config_part_is_rejected(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_empty_parts(root)
            self._write_part(root / "extra.config", {})

            with self.assertRaisesRegex(ConfigError, "unexpected config part.*extra.config"):
                load_config_data(root)

    def test_duplicate_top_level_key_is_rejected(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_part(root / "environment.config", {"libvirt": {}})
            self._write_part(root / "benches.config", {"libvirt": {}})
            self._write_part(root / "plan.config", {})

            with self.assertRaisesRegex(ConfigError, "duplicate top-level key 'libvirt'"):
                load_config_data(root)

    def test_misplaced_top_level_key_is_rejected(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            self._write_part(root / "environment.config", {})
            self._write_part(root / "benches.config", {"plans": {}})
            self._write_part(root / "plan.config", {})

            with self.assertRaisesRegex(ConfigError, "misplaced top-level key.*plans"):
                load_config_data(root)

    @staticmethod
    def _write_empty_parts(root: Path) -> None:
        for name, _ in CONFIG_PART_KEYS:
            ConfigDirectoryLoadingTest._write_part(root / name, {})

    @staticmethod
    def _write_part(path: Path, data: dict[str, object]) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(yaml.safe_dump(data, sort_keys=False), encoding="utf-8")


class ConfigDirectoryWritingTest(unittest.TestCase):
    def test_local_config_generated_from_template_is_runtime_valid(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            kernel_source = root / "linux"
            (kernel_source / "tools" / "perf").mkdir(parents=True)
            kernel = kernel_source / "arch" / "x86" / "boot" / "bzImage"
            kernel.parent.mkdir(parents=True)
            kernel.write_text("kernel", encoding="utf-8")
            kernel_config = kernel_source / ".config"
            kernel_config.write_text("CONFIG_SCHED_CLASS_EXT=y\n", encoding="utf-8")
            ssh_key = root / "id_ed25519"
            ssh_key.write_text("key", encoding="utf-8")
            workdir = root / "workdir"
            workdir.mkdir()

            data = _build_local_config(
                template_path=Path("bench/configs/example_config"),
                kernel_source=kernel_source,
                kernel=kernel,
                root_image=root / "root.qcow2",
                ssh_key=ssh_key,
                workdir=workdir,
                emulator_cpus="0",
                isolated_cpus="1",
            )
            target = root / "local_config"
            _write_config(target, data, force=False, dry_run=False)

            loaded = load_config(target)
            self.assertEqual(loaded["libvirt"]["kernel_config"], str(kernel_config))
            self.assertEqual(
                loaded["schedulers"]["scx_agent_classed"]["host_kconfig"],
                str(kernel_config),
            )
            self.assertEqual(
                loaded["schedulers"]["scx_agent_classed_llm_latency"]["host_kconfig"],
                str(kernel_config),
            )
            self.assertIn("treatments", loaded)

    def test_writer_preserves_all_owned_sections(self) -> None:
        data = load_config_data("bench/configs/example_config")
        with TemporaryDirectory() as tmp:
            target = Path(tmp) / "local_config"

            _write_config(target, data, force=False, dry_run=False)
            loaded = load_config_data(target)

            self.assertEqual(set(loaded), set(data))
            self.assertIn("treatments", yaml.safe_load((target / "plan.config").read_text()))
            self.assertIn("suites", yaml.safe_load((target / "benches.config").read_text()))
            self.assertNotIn(
                "treatments",
                yaml.safe_load((target / "benches.config").read_text()),
            )
            self.assertNotIn("suites", yaml.safe_load((target / "plan.config").read_text()))

    def test_writer_requires_force_before_replacing_a_part(self) -> None:
        data = load_config_data("bench/configs/example_config")
        with TemporaryDirectory() as tmp:
            target = Path(tmp) / "local_config"
            _write_config(target, data, force=False, dry_run=False)

            with self.assertRaisesRegex(RuntimeError, "use --force to overwrite"):
                _write_config(target, data, force=False, dry_run=False)

            _write_config(target, data, force=True, dry_run=False)
            self.assertEqual(load_config_data(target), data)

    def test_writer_rejects_unowned_top_level_keys(self) -> None:
        data = load_config_data("bench/configs/example_config")
        data["unknown"] = {}

        with TemporaryDirectory() as tmp, self.assertRaisesRegex(
            RuntimeError, "unowned top-level key.*unknown"
        ):
            _write_config(Path(tmp) / "local_config", data, force=False, dry_run=False)


if __name__ == "__main__":
    unittest.main()
