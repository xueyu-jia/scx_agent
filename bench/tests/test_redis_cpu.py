from __future__ import annotations

import importlib.util
import os
import tempfile
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from bench.scenarios.redis_cpu import common, loadgen
from bench.scenarios.redis_cpu.run import (
    _preflight_llm,
    _validate_outputs,
    main as redis_run_main,
)


MCP_PATH = Path("bench/scenarios/redis_cpu/mcp_server.py").resolve()


def load_mcp_module() -> object:
    spec = importlib.util.spec_from_file_location(
        "bench.scenarios.redis_cpu.redis_cpu_mcp_test",
        MCP_PATH,
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fake_scope(root: Path) -> common.RedisCpuScope:
    scope = common.RedisCpuScope.from_root(root)
    scope.root.mkdir(parents=True)
    (scope.root / "cgroup.procs").write_text("", encoding="utf-8")
    (scope.root / "cpu.pressure").write_text(
        "some avg10=1.00 avg60=1.00 avg300=1.00 total=100\n",
        encoding="utf-8",
    )
    for path in (scope.redis, scope.batch, scope.driver):
        path.mkdir()
        (path / "cgroup.procs").write_text("", encoding="utf-8")
        (path / "cpu.weight").write_text("100\n", encoding="utf-8")
        (path / "cpu.max").write_text("max 100000\n", encoding="utf-8")
        (path / "cpu.stat").write_text(
            "usage_usec 100\nuser_usec 60\nsystem_usec 40\n",
            encoding="utf-8",
        )
    return scope


def full_metrics(weight: float = 100.0) -> dict[str, float]:
    return {
        "redis_p50_latency_us": 100.0,
        "redis_p95_latency_us": 200.0,
        "redis_p99_latency_us": 300.0,
        "redis_qps": 10_000.0,
        "redis_cpu_rate": 1.0,
        "batch_cpu_rate": 1.0,
        "redis_cpu_share_pct": 50.0,
        "batch_cpu_share_pct": 50.0,
        "target_cpu_weight": weight,
        "cpu_pressure_some_pct": 10.0,
    }


class RedisCpuCommonTest(unittest.TestCase):
    def test_run_passes_absolute_config_and_output_paths_to_runner(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            config = root / "config"
            config.mkdir()
            original_cwd = Path.cwd()
            try:
                os.chdir(root)
                with patch(
                    "bench.scenarios.redis_cpu.run._preflight_llm",
                    return_value=True,
                ) as preflight, patch(
                    "bench.scenarios.redis_cpu.run.subprocess.run",
                    return_value=SimpleNamespace(returncode=0),
                ) as run, patch(
                    "bench.scenarios.redis_cpu.run._validate_outputs"
                ) as validate:
                    status = redis_run_main(
                        [
                            "--config",
                            "config",
                            "--output",
                            "results",
                            "--plan",
                            "redis_cpu_demo_smoke",
                        ]
                    )
            finally:
                os.chdir(original_cwd)

        self.assertEqual(status, 0)
        preflight.assert_called_once_with(config)
        command = run.call_args.args[0]
        self.assertEqual(command[command.index("--config") + 1], str(config))
        self.assertEqual(
            command[command.index("--output") + 1],
            str(root / "results"),
        )
        self.assertEqual(
            command[command.index("--candidate-treatment") + 1],
            "redis_cpu_agent",
        )
        validate.assert_called_once_with(root / "results", "default")

    def test_run_preflights_the_agent_treatment_configuration(self) -> None:
        config = {
            "treatments": {
                "agent": {
                    "env": {
                        "SCX_TUNING_AGENT_LLM_BASE_URL": "https://llm.example/v1",
                        "SCX_TUNING_AGENT_LLM_API_KEY": "test-api-key",
                        "SCX_TUNING_AGENT_LLM_MODEL": "test-model",
                    }
                }
            }
        }
        with patch(
            "bench.scenarios.redis_cpu.run.load_config_data",
            return_value=config,
        ), patch(
            "bench.scenarios.redis_cpu.run.preflight_protocol"
        ) as preflight:
            with patch(
                "bench.scenarios.redis_cpu.run.CANDIDATE_TREATMENT",
                "agent",
            ):
                self.assertTrue(_preflight_llm(Path("config")))

        preflight.assert_called_once()

    def test_run_requires_analysis_report_and_both_result_sets(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            output = Path(temp_dir)
            analysis_dir = output / "analysis"
            analysis_dir.mkdir()
            (analysis_dir / "analysis.json").write_text("{}\n", encoding="utf-8")
            (analysis_dir / "report.html").write_text("<html></html>\n", encoding="utf-8")
            for treatment in ("redis_cpu_control", "redis_cpu_agent"):
                result_dir = (
                    output
                    / "runs"
                    / f"default__{treatment}"
                    / "run_001"
                )
                result_dir.mkdir(parents=True)
                (result_dir / "result.json").write_text("{}\n", encoding="utf-8")

            _validate_outputs(output, "default")

            (analysis_dir / "report.html").unlink()
            with self.assertRaisesRegex(ValueError, "report is missing or empty"):
                _validate_outputs(output, "default")

    def test_each_redis_shard_uses_a_dedicated_driver_cpu(self) -> None:
        args = SimpleNamespace(
            redis_benchmark_binary="/opt/redis-benchmark",
            requests=20_000,
            clients=64,
        )
        process_specs = loadgen._benchmark_process_specs(args)
        commands = [command for _, command in process_specs]

        self.assertEqual([cpu for cpu, _ in process_specs], [2, 4])
        self.assertEqual(
            [command[command.index("-p") + 1] for command in commands],
            ["16379", "16380"],
        )

    def test_scoped_exec_allows_empty_command_arguments(self) -> None:
        command = ["redis-server", "--save", ""]

        scoped = common.scoped_exec_argv(Path("/sys/fs/cgroup/test"), (0, 1), command)

        self.assertEqual(scoped[-len(command) :], command)
        with self.assertRaisesRegex(common.RedisCpuError, "non-empty argv"):
            common.scoped_exec_argv(Path("/sys/fs/cgroup/test"), (0, 1), [])
        with self.assertRaisesRegex(common.RedisCpuError, "non-empty argv"):
            common.scoped_exec_argv(Path("/sys/fs/cgroup/test"), (0, 1), [""])

    def test_redis_output_and_two_shard_aggregation(self) -> None:
        output = """
        Latency summary (msec):
                avg       min       p50       p95       p99       max
              1.000     0.100     0.500     2.000     3.000     4.000
        throughput summary: 12345.67 requests per second
        """
        parsed = common.parse_redis_benchmark(output)
        metrics = common.aggregate_shards(
            parsed,
            {**parsed, "p99_latency_us": 4_000.0, "qps": 10_000.0},
            redis_usage_usec=1_000_000,
            batch_usage_usec=1_000_000,
            pressure_usec=500_000,
            elapsed_seconds=1.0,
            weight=273,
        )

        self.assertEqual(metrics["redis_p50_latency_us"], 500.0)
        self.assertEqual(metrics["redis_p99_latency_us"], 4_000.0)
        self.assertEqual(metrics["redis_qps"], 22_345.67)
        self.assertEqual(metrics["target_cpu_weight"], 273.0)
        self.assertEqual(set(metrics), set(common.METRIC_NAMES))

    def test_workload_fingerprint_excludes_weight_but_detects_identity_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            scope = fake_scope(Path(temp_dir) / "scope")
            identities = {
                pid: {
                    "pid": pid,
                    "start_time_ticks": pid * 10,
                    "executable": (
                        "/bin/redis-benchmark" if pid in (32, 33) else f"/bin/process-{pid}"
                    ),
                    "affinity": (
                        [2, 4]
                        if pid == 31
                        else [2]
                        if pid == 32
                        else [4]
                        if pid == 33
                        else [0, 1]
                    ),
                }
                for pid in (11, 12, 21, 31, 32, 33)
            }
            (scope.redis / "cgroup.procs").write_text("11\n12\n", encoding="utf-8")
            (scope.batch / "cgroup.procs").write_text("21\n22\n23\n", encoding="utf-8")
            (scope.driver / "cgroup.procs").write_text("31\n", encoding="utf-8")
            runtime = {
                "version": 1,
                "run_id": "run",
                "scope": str(scope.root),
                "cgroups": {
                    name: {"path": str(path), "inode": path.stat().st_ino}
                    for name, path in (
                        ("redis", scope.redis),
                        ("batch", scope.batch),
                        ("driver", scope.driver),
                    )
                },
                "processes": {
                    "redis": [identities[11], identities[12]],
                    "batch": identities[21],
                    "loadgen": identities[31],
                },
                "redis": {"ports": [16379, 16380], "config_digest": "sha256:" + "a" * 64},
                "loadgen": {
                    "parameters_digest": "sha256:" + "b" * 64,
                    "benchmark_executable": "/bin/redis-benchmark",
                },
                "workload_digest": "sha256:" + "c" * 64,
            }
            with patch.object(
                common, "process_identity", side_effect=lambda pid: identities[pid]
            ), patch.object(common, "_is_descendant", return_value=True):
                before = common.validate_runtime_identity(runtime, scope)
                common.write_weight(scope.redis, 273)
                after = common.validate_runtime_identity(runtime, scope)
                self.assertEqual(before, after)
                (scope.driver / "cgroup.procs").write_text("31\n32\n", encoding="utf-8")
                with_benchmark = common.validate_runtime_identity(runtime, scope)
                self.assertEqual(before, with_benchmark)
                (scope.driver / "cgroup.procs").write_text(
                    "31\n32\n33\n",
                    encoding="utf-8",
                )
                with_both_benchmarks = common.validate_runtime_identity(runtime, scope)
                self.assertEqual(before, with_both_benchmarks)
                identities[11] = {**identities[11], "start_time_ticks": 999}
                with self.assertRaisesRegex(common.RedisCpuError, "identity changed"):
                    common.validate_runtime_identity(runtime, scope)

class RedisCpuMcpTest(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_mcp_module()

    def _server(
        self,
        root: Path,
        output: Path,
        *,
        restore_failure: str = "never",
        allowed_weights: str | None = None,
    ) -> object:
        environment = {
            "SCX_REDIS_CPU_ROOT": str(root),
            "SCX_REDIS_CPU_OUTPUT_DIR": str(output),
            "SCX_REDIS_CPU_RESTORE_FAILURE": restore_failure,
        }
        if allowed_weights is not None:
            environment["SCX_REDIS_CPU_ALLOWED_WEIGHTS"] = allowed_weights
        with patch.dict(os.environ, environment, clear=False):
            server = self.module.RedisCpuMcp()
        runtime = {
            "version": 1,
            "run_id": "run-1",
            "workload_digest": "sha256:" + "a" * 64,
        }
        server._runtime = lambda: (runtime, "sha256:" + "b" * 64)
        server._runtime_static = lambda: runtime
        return server

    def test_manifest_has_only_generic_probe_measurement_and_continuous_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            fake_scope(root)
            server = self._server(root, Path(temp_dir))
            manifest = server._manifest()
            capabilities = manifest["capabilities"]

            self.assertEqual(
                [item["id"] for item in capabilities],
                ["redis.snapshot.v1", "redis.window.v1", "redis.target-cpu-weight.v1"],
            )
            self.assertEqual([item["kind"] for item in capabilities], ["probe", "measurement", "mutation"])
            schema = capabilities[-1]["input_schema"]["properties"]["value"]
            self.assertEqual(schema, {"type": "integer", "minimum": 1, "maximum": 10_000})
            self.assertNotIn("enum", schema)
            self.assertIn("called repeatedly in one episode", capabilities[-1]["description"])

    def test_demo_manifest_restricts_weight_choices(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            scope = fake_scope(root)
            server = self._server(
                root,
                Path(temp_dir) / "output",
                allowed_weights="[100,200,400,800]",
            )
            schema = server._manifest()["capabilities"][-1]["input_schema"]

            self.assertEqual(
                schema["properties"]["value"],
                {"type": "integer", "enum": [100, 200, 400, 800]},
            )
            with self.assertRaisesRegex(self.module.McpToolError, "must be one of"):
                server._mutation_prepare(
                    {
                        "context": {"operation_id": "prepare/invalid"},
                        "arguments": {"value": 300},
                    }
                )
            prepared = server._mutation_prepare(
                {
                    "context": {"operation_id": "prepare/valid"},
                    "arguments": {"value": 200},
                }
            )
            self.assertEqual(prepared["baseline"]["value"], 100)
            self.assertEqual(prepared["desired"]["value"], 200)
            self.assertEqual(common.read_weight(scope.redis), 100)

    def test_mutation_readback_persistence_drift_and_restore(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            scope = fake_scope(root)
            output = Path(temp_dir) / "output"
            server = self._server(root, output)
            prepared = server._mutation_prepare(
                {"context": {"operation_id": "prepare/1"}, "arguments": {"value": 273}}
            )
            applied = server._mutation_apply(
                {"operation_id": "apply/1", "prepared": prepared}
            )
            self.assertEqual(applied["state"], "applied")
            self.assertEqual(common.read_weight(scope.redis), 273)

            restarted = self._server(root, output)
            status = restarted._mutation_status({"operation_id": "apply/1"})
            self.assertEqual(status["state"], "applied")
            (scope.redis / "cpu.weight").write_text("999\n", encoding="utf-8")
            self.assertEqual(
                restarted._mutation_status({"operation_id": "apply/1"})["state"],
                "unknown",
            )
            (scope.redis / "cpu.weight").write_text("273\n", encoding="utf-8")
            restarted._runtime = lambda: (_ for _ in ()).throw(
                common.RedisCpuError("training workload exited")
            )
            restored = restarted._mutation_restore(
                {"operation_id": "restore/1", "prepared": prepared}
            )
            self.assertEqual(restored["state"], "restored")
            self.assertEqual(common.read_weight(scope.redis), 100)

    def test_restore_failure_is_persistent_and_never_claims_success(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            fake_scope(root)
            output = Path(temp_dir) / "output"
            server = self._server(root, output, restore_failure="always")
            prepared = server._mutation_prepare(
                {"context": {"operation_id": "prepare/1"}, "arguments": {"value": 273}}
            )
            server._mutation_apply({"operation_id": "apply/1", "prepared": prepared})
            with self.assertRaisesRegex(self.module.McpToolError, "restore failure"):
                server._mutation_restore({"operation_id": "restore/1", "prepared": prepared})
            with self.assertRaisesRegex(self.module.McpToolError, "restore failure"):
                server._mutation_restore({"operation_id": "restore/1", "prepared": prepared})
            recovered = self._server(root, output, restore_failure="never")
            first = recovered._mutation_restore(
                {"operation_id": "restore/1", "prepared": prepared}
            )
            second = recovered._mutation_restore(
                {"operation_id": "restore/1", "prepared": prepared}
            )
            self.assertEqual(first["state"], "restored")
            self.assertEqual(second["state"], "restored")

    def test_measurement_consumes_post_open_sequence_and_marks_stale_sample_invalid(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            fake_scope(root)
            server = self._server(root, Path(temp_dir) / "output")
            initial = {"version": 1, "run_id": "run-1", "next_sequence": 4, "samples": []}
            server._loadgen_state = lambda _runtime: initial
            opened = server._measurement_open(
                {
                    "context": {"operation_id": "measure/1"},
                    "specification": {"max_sample_age_ms": 1_000, "wait_timeout_ms": 500},
                }
            )
            session = server.sessions[opened["id"]]
            fresh = {
                "sequence": 4,
                "started_at_ns": 10,
                "ended_at_ns": 20,
                "monotonic_started_ns": session["opened_monotonic_ns"] + 1,
                "monotonic_ended_ns": time.monotonic_ns() - 2_000_000_000,
                "quality": "valid",
                "workload_fingerprint": session["workload_fingerprint"],
                "weight_at_start": 100,
                "weight_at_end": 100,
                "metrics": full_metrics(),
                "errors": [],
            }
            state = {"version": 1, "run_id": "run-1", "next_sequence": 5, "samples": [fresh]}
            server._loadgen_state = lambda _runtime: state
            sampled = server._measurement_sample(
                {"session": {"id": opened["id"], "driver_data": opened["driver_data"]}}
            )

            self.assertEqual(sampled["quality"], "invalid")
            self.assertEqual(set(sampled["metrics"]), set(common.METRIC_NAMES))
            self.assertIn("freshness", sampled["provenance"]["errors"][0])

    def test_measurement_skips_pre_open_sequence_and_detects_fingerprint_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            fake_scope(root)
            server = self._server(root, Path(temp_dir) / "output")
            initial = {"version": 1, "run_id": "run-1", "next_sequence": 8, "samples": []}
            server._loadgen_state = lambda _runtime: initial
            opened = server._measurement_open(
                {
                    "context": {"operation_id": "measure/2"},
                    "specification": {"max_sample_age_ms": 30_000, "wait_timeout_ms": 500},
                }
            )
            session = server.sessions[opened["id"]]
            before_open = {
                "sequence": 8,
                "started_at_ns": 10,
                "ended_at_ns": 20,
                "monotonic_started_ns": session["opened_monotonic_ns"] - 1,
                "monotonic_ended_ns": time.monotonic_ns(),
                "quality": "valid",
                "workload_fingerprint": session["workload_fingerprint"],
                "weight_at_start": 100,
                "weight_at_end": 100,
                "metrics": full_metrics(),
                "errors": [],
            }
            drifted = {
                **before_open,
                "sequence": 9,
                "monotonic_started_ns": session["opened_monotonic_ns"] + 1,
                "workload_fingerprint": "sha256:" + "f" * 64,
            }
            state = {
                "version": 1,
                "run_id": "run-1",
                "next_sequence": 10,
                "samples": [before_open, drifted],
            }
            server._loadgen_state = lambda _runtime: state
            sampled = server._measurement_sample(
                {"session": {"id": opened["id"], "driver_data": opened["driver_data"]}}
            )

            self.assertEqual(sampled["quality"], "invalid")
            self.assertEqual(sampled["provenance"]["sequence"], 9)
            self.assertEqual(sampled["provenance"]["skipped"][0]["sequence"], 8)
            self.assertIn("fingerprint", " ".join(sampled["provenance"]["errors"]))


if __name__ == "__main__":
    unittest.main()
