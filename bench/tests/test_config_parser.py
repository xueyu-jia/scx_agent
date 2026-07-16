from __future__ import annotations

import unittest

from bench.config.parser import (
    ConfigError,
    _bench_with_defaults,
    _validate_bench_defaults,
    _validate_benches,
    _validate_schedulers,
    _validate_treatments,
    expand_plan,
)


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
        with self.assertRaisesRegex(ConfigError, "reserved variables.*SCX_BENCH_OUT"):
            self._validate(
                {
                    "measurement": command(),
                    "env": {"SCX_BENCH_OUT": "/tmp/other"},
                }
            )


class TreatmentConfigTest(unittest.TestCase):
    def test_treatment_command_policy_and_environment_are_valid(self) -> None:
        _validate_treatments(
            {
                "treatments": {
                    "agent_tuned": {
                        **command(["python3", "tune.py"], 600),
                        "host_command": "bench/treatments/tune.py",
                        "host_support_files": ["bench/treatments/mock_openai_llm.py"],
                        "env": {"MODE": "tune"},
                        "post_treatment_settle_seconds": 5,
                        "allow_no_commit": True,
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

    def test_treatment_rejects_invalid_policy_and_reserved_environment(self) -> None:
        with self.assertRaisesRegex(ConfigError, "allow_no_commit must be a boolean"):
            _validate_treatments(
                {
                    "treatments": {
                        "agent_tuned": {
                            **command(),
                            "allow_no_commit": "yes",
                        }
                    }
                }
            )

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
