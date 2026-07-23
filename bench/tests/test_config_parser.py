from __future__ import annotations

import unittest
from pathlib import Path

import yaml

from bench.core.config import (
    ConfigError,
    _bench_with_defaults,
    _validate_bench_defaults,
    _validate_benches,
    _validate_metric_profiles,
    _validate_plans,
    _validate_schedulers,
    _validate_suites,
    _validate_treatments,
    expand_plan,
)
from bench.env.manager import _patch_kernel_build_source


def command(argv: list[str] | None = None, timeout_seconds: int = 30) -> dict[str, object]:
    values = argv or ["measure"]
    return {
        "command": values[0],
        "args": values[1:],
        "timeout_seconds": timeout_seconds,
    }


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
                "host_support_files": ["bench/scenarios/example.py"],
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
        for name in ("SCX_BENCH_OUT", "SCX_BENCH_WORKDIR"):
            with self.subTest(name=name), self.assertRaisesRegex(
                ConfigError, f"reserved variables.*{name}"
            ):
                self._validate(
                    {
                        "measurement": command(),
                        "env": {name: "/tmp/other"},
                    }
                )

    def test_local_config_patches_kernel_source_inside_measurement(self) -> None:
        config = {
            "benches": {
                "kernel_build_bzimage": {
                    "measurement": {
                        "args": ["wrapper.py", "--source", None, "--target", "bzImage"]
                    }
                }
            }
        }

        _patch_kernel_build_source(config, Path("/kernel/source"))

        self.assertEqual(
            config["benches"]["kernel_build_bzimage"]["measurement"]["args"][2],
            "/kernel/source",
        )


class TreatmentConfigTest(unittest.TestCase):
    def test_treatment_command_support_files_and_environment_are_valid(self) -> None:
        _validate_treatments(
            {
                "treatments": {
                    "agent_tuned": {
                        **command(["python3", "tune.py"], 600),
                        "host_command": "bench/integrations/tuning_agent/adapter.py",
                        "host_support_files": [
                            "bench/integrations/tuning_agent/mock_llm.py"
                        ],
                        "env": {"MODE": "tune"},
                        "post_treatment_settle_seconds": 5,
                    }
                }
            }
        )

    def test_treatment_requires_a_bounded_command(self) -> None:
        with self.assertRaisesRegex(
            ConfigError,
            r"treatments\.agent_tuned\.timeout_seconds is required",
        ):
            _validate_treatments(
                {
                    "treatments": {
                        "agent_tuned": {
                            "command": "tune",
                        }
                    }
                }
            )

    def test_treatment_rejects_reserved_environment_and_invalid_support_files(self) -> None:
        with self.assertRaisesRegex(ConfigError, "SCX_BENCH_ROLE"):
            _validate_treatments(
                {
                    "treatments": {
                        "agent_tuned": {
                            **command(),
                            "env": {"SCX_BENCH_ROLE": "candidate"},
                        }
                    }
                }
            )

        with self.assertRaisesRegex(ConfigError, "host_support_files must be a string list"):
            _validate_treatments(
                {
                    "treatments": {
                        "agent_tuned": {
                            **command(),
                            "host_support_files": ["ok", ""],
                        }
                    }
                }
            )

    def test_example_cgroup_cpu_matrix_is_internally_consistent(self) -> None:
        config = yaml.safe_load(
            Path("bench/configs/cgroup_cpu_tuning.config").read_text(encoding="utf-8")
        )

        _validate_treatments(config)
        _validate_benches(
            {"benches": {"cgroup_cpu_share": config["benches"]["cgroup_cpu_share"]}}
        )
        _validate_metric_profiles(config)
        _validate_suites(config)
        _validate_plans(config)

        self.assertEqual(config["plans"]["cgroup_cpu_smoke"]["runs"], 1)
        self.assertEqual(config["plans"]["cgroup_cpu"]["runs"], 10)
        self.assertNotIn(
            "--no-commit-disposition",
            config["treatments"]["cgroup_cpu_agent_positive"].get("args", []),
        )
        self.assertEqual(
            config["treatments"]["cgroup_cpu_agent_no_signal"]["args"],
            ["--no-commit-disposition", "proceed"],
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
                        "host_command": "build/scheduler",
                        "host_support_files": ["build/provider"],
                        "settle_seconds": 2,
                    }
                }
            }
        )

    def test_scheduler_support_files_require_a_host_command(self) -> None:
        with self.assertRaisesRegex(
            ConfigError, r"schedulers\.test\.host_support_files requires host_command"
        ):
            _validate_schedulers(
                {
                    "schedulers": {
                        "test": {
                            "kind": "scx",
                            "command": "scheduler",
                            "host_support_files": ["build/provider"],
                        }
                    }
                }
            )

    def test_scheduler_support_files_must_be_a_string_list(self) -> None:
        for value in ("build/provider", ["build/provider", ""]):
            with self.subTest(value=value), self.assertRaisesRegex(
                ConfigError, r"host_support_files must be a string list"
            ):
                _validate_schedulers(
                    {
                        "schedulers": {
                            "test": {
                                "kind": "scx",
                                "command": "scheduler",
                                "host_command": "build/scheduler",
                                "host_support_files": value,
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
