from __future__ import annotations

from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

import yaml

from bench.config.parser import (
    ConfigError,
    _bench_with_defaults,
    _validate_bench_defaults,
    _validate_benches,
    _validate_schedulers,
    expand_plan,
    load_config,
)


def command(argv: list[str] | None = None, timeout_seconds: int = 30) -> dict[str, object]:
    values = argv or ["measure"]
    return {
        "command": values[0],
        "args": values[1:],
        "timeout_seconds": timeout_seconds,
    }


class ConfigDirectoryLoadingTest(unittest.TestCase):
    def test_load_config_directory_merges_parts(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            config_dir = root / "local_config"
            self._write_split_config(config_dir, self._valid_config(root))

            config = load_config(config_dir)

            self.assertEqual(config["libvirt"]["root_image"], "/tmp/root.qcow2")
            self.assertIn("bench", config["benches"])
            self.assertEqual([spec.bench_name for spec in expand_plan(config, "smoke")], ["bench"])

    def test_load_config_directory_requires_all_parts(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            config_dir = root / "local_config"
            config = self._valid_config(root)
            self._write_part(
                config_dir / "environment.config",
                config,
                ("libvirt", "executor", "machines"),
            )
            self._write_part(
                config_dir / "benches.config",
                config,
                ("metric_profiles", "benches"),
            )

            with self.assertRaisesRegex(ConfigError, r"missing config part: .*plan\.config"):
                load_config(config_dir)

    def test_load_config_directory_rejects_duplicate_top_level_keys(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            config_dir = root / "local_config"
            config = self._valid_config(root)
            self._write_part(
                config_dir / "environment.config",
                config,
                ("libvirt", "executor", "machines"),
            )
            self._write_part(
                config_dir / "benches.config",
                config,
                ("libvirt", "metric_profiles", "benches"),
            )
            self._write_part(
                config_dir / "plan.config",
                config,
                ("schedulers", "plans", "suites"),
            )

            with self.assertRaisesRegex(ConfigError, r"duplicate top-level key 'libvirt'"):
                load_config(config_dir)

    def test_load_config_rejects_single_file(self) -> None:
        with TemporaryDirectory() as tmp:
            config_path = Path(tmp) / "config.config"
            config_path.write_text("libvirt: {}\n", encoding="utf-8")

            with self.assertRaisesRegex(ConfigError, r"config path must be a directory"):
                load_config(config_path)

    def _valid_config(self, root: Path) -> dict[str, object]:
        kernel_source = root / "linux"
        (kernel_source / "tools" / "perf").mkdir(parents=True)
        return {
            "libvirt": {
                "kernel": None,
                "kernel_args": "",
                "kernel_source": str(kernel_source),
                "initrd": None,
                "root_image": "/tmp/root.qcow2",
                "ssh_user": "root",
                "ssh_key": "/tmp/key",
                "workdir": "/tmp/work",
                "guest_output_dir": "/out",
                "emulator_cpus": "0",
            },
            "executor": {},
            "machines": {
                "small": {
                    "vcpus": 1,
                    "memory": "1G",
                    "pin_cpus": "auto",
                    "exclusive": True,
                    "frequency": {"fixed": True},
                }
            },
            "metric_profiles": {
                "metrics": {
                    "primary": [{"name": "throughput", "direction": "higher"}],
                }
            },
            "benches": {"bench": {"measurement": command()}},
            "schedulers": {"default": {"kind": "builtin"}},
            "plans": {
                "smoke": {
                    "runs": 1,
                    "matrix": [{"machine": "small", "suites": ["suite"]}],
                }
            },
            "suites": {"suite": {"benches": ["bench"], "metric_profile": "metrics"}},
        }

    def _write_split_config(self, path: Path, config: dict[str, object]) -> None:
        self._write_part(path / "environment.config", config, ("libvirt", "executor", "machines"))
        self._write_part(path / "benches.config", config, ("metric_profiles", "benches"))
        self._write_part(path / "plan.config", config, ("schedulers", "plans", "suites"))

    def _write_part(
        self,
        path: Path,
        config: dict[str, object],
        keys: tuple[str, ...],
    ) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            yaml.safe_dump({key: config[key] for key in keys}, sort_keys=False),
            encoding="utf-8",
        )


class BenchCommandValidationTest(unittest.TestCase):
    def _validate(self, bench: dict[str, object]) -> None:
        _validate_benches({"benches": {"test": bench}})

    def test_measurement_and_optional_warmup_are_valid(self) -> None:
        self._validate(
            {
                "measurement": command(["measure", "--seconds", "30"]),
                "warmup": command(["measure", "--seconds", "5"], 10),
                "post_warmup_settle_seconds": 2,
                "cooldown_seconds": 1,
                "env": {"MODE": "test"},
            }
        )

    def test_measurement_is_required(self) -> None:
        with self.assertRaisesRegex(ConfigError, r"benches\.test\.measurement is required"):
            self._validate({})

    def test_args_are_optional(self) -> None:
        self._validate(
            {
                "measurement": {
                    "command": "measure",
                    "timeout_seconds": 30,
                }
            }
        )

    def test_command_and_timeout_are_required_for_each_phase(self) -> None:
        for phase in ("measurement", "warmup"):
            for missing in ("command", "timeout_seconds"):
                with self.subTest(phase=phase, missing=missing), self.assertRaisesRegex(
                    ConfigError,
                    rf"benches\.test\.{phase}\.{missing} is required",
                ):
                    phase_config = command()
                    phase_config.pop(missing)
                    bench = {"measurement": command(), phase: phase_config}
                    self._validate(bench)

    def test_command_types_are_validated(self) -> None:
        invalid_values = (
            ({"command": 1, "timeout_seconds": 10}, "command must be a non-empty string"),
            ({"command": "", "timeout_seconds": 10}, "command must be a non-empty string"),
            ({"command": "run", "args": "--all", "timeout_seconds": 10}, "args must be a list"),
            (
                {"command": "run", "args": [1], "timeout_seconds": 10},
                "args entries must be non-empty strings",
            ),
            (
                {"command": "run", "args": [""], "timeout_seconds": 10},
                "args entries must be non-empty strings",
            ),
            (
                {"command": "run", "timeout_seconds": 0},
                "timeout_seconds must be a positive integer",
            ),
            (
                {"command": "run", "timeout_seconds": True},
                "timeout_seconds must be a positive integer",
            ),
        )
        for value, message in invalid_values:
            with self.subTest(value=value), self.assertRaisesRegex(ConfigError, message):
                self._validate({"measurement": value})

    def test_unknown_fields_and_legacy_shape_are_rejected(self) -> None:
        with self.assertRaisesRegex(ConfigError, "unsupported keys"):
            self._validate({"measurement": command(), "command": "legacy"})
        with self.assertRaisesRegex(ConfigError, "unsupported keys"):
            self._validate(
                {
                    "measurement": {
                        **command(),
                        "shell": True,
                    }
                }
            )

    def test_reserved_output_environment_cannot_be_overridden(self) -> None:
        with self.assertRaisesRegex(ConfigError, "reserved SCX_BENCH_OUT"):
            self._validate(
                {
                    "measurement": command(),
                    "env": {"SCX_BENCH_OUT": "/tmp/other"},
                }
            )


class BenchTimingTest(unittest.TestCase):
    def test_defaults_are_merged_and_bench_values_override_them(self) -> None:
        config = {
            "bench_defaults": {
                "post_warmup_settle_seconds": 2,
                "cooldown_seconds": 1,
            },
            "benches": {
                "test": {
                    "measurement": command(),
                    "post_warmup_settle_seconds": 4,
                }
            },
        }

        merged = _bench_with_defaults(config, "test")

        self.assertEqual(merged["post_warmup_settle_seconds"], 4)
        self.assertEqual(merged["cooldown_seconds"], 1)

    def test_timing_values_must_be_non_negative_integers(self) -> None:
        for key in ("post_warmup_settle_seconds", "cooldown_seconds"):
            for value in (-1, True):
                with self.subTest(key=key, value=value), self.assertRaisesRegex(
                    ConfigError,
                    rf"bench_defaults\.{key} must be a non-negative integer",
                ):
                    _validate_bench_defaults({"bench_defaults": {key: value}})

    def test_expand_plan_keeps_only_the_new_phase_schema(self) -> None:
        measurement = command(["measure", "--seconds", "30"])
        warmup = command(["measure", "--seconds", "5"], 10)
        config = {
            "plans": {
                "test": {
                    "runs": 1,
                    "matrix": [{"machine": "small", "suites": ["suite"]}],
                }
            },
            "machines": {"small": {"vcpus": 1}},
            "suites": {"suite": {"benches": ["bench"], "metric_profile": "metrics"}},
            "metric_profiles": {"metrics": {}},
            "bench_defaults": {"post_warmup_settle_seconds": 2},
            "benches": {
                "bench": {
                    "measurement": measurement,
                    "warmup": warmup,
                }
            },
            "libvirt": {},
        }

        bench = expand_plan(config, "test")[0].bench

        self.assertEqual(bench["measurement"], measurement)
        self.assertEqual(bench["warmup"], warmup)
        self.assertEqual(bench["post_warmup_settle_seconds"], 2)


class SchedulerConfigTest(unittest.TestCase):
    def test_scx_scheduler_uses_settle_seconds(self) -> None:
        _validate_schedulers(
            {
                "schedulers": {
                    "test": {
                        "kind": "scx",
                        "command": "scheduler",
                        "settle_seconds": 2,
                    }
                }
            }
        )

    def test_legacy_scheduler_warmup_is_rejected(self) -> None:
        with self.assertRaisesRegex(ConfigError, "unsupported keys"):
            _validate_schedulers(
                {
                    "schedulers": {
                        "test": {
                            "kind": "scx",
                            "command": "scheduler",
                            "warmup_seconds": 2,
                        }
                    }
                }
            )

    def test_builtin_scheduler_rejects_ignored_fields(self) -> None:
        with self.assertRaisesRegex(ConfigError, "unsupported keys"):
            _validate_schedulers(
                {
                    "schedulers": {
                        "default": {
                            "kind": "builtin",
                            "settle_seconds": 2,
                        }
                    }
                }
            )


if __name__ == "__main__":
    unittest.main()
