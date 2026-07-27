from __future__ import annotations

import importlib.util
import os
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from bench.scenarios.redis_cpu import common
from bench.scenarios.redis_cpu.matrix import _evaluation_fingerprints, _has_batch_guard


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
                    "executable": f"/bin/process-{pid}",
                    "affinity": [0, 1] if pid not in (31, 32) else [2],
                }
                for pid in (11, 12, 21, 31, 32)
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
                    "benchmark_executable": "/bin/process-32",
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
                identities[11] = {**identities[11], "start_time_ticks": 999}
                with self.assertRaisesRegex(common.RedisCpuError, "identity changed"):
                    common.validate_runtime_identity(runtime, scope)

    def test_real_llm_matrix_reads_frozen_batch_guard_and_ab_fingerprints(self) -> None:
        contract = {
            "evaluation_contract": {
                "regression_guards": [
                    {
                        "capability_id": "builtin/comparison.threshold.v1",
                        "specification": {
                            "conditions": [
                                {
                                    "metric": "batch_cpu_rate",
                                    "op": "decrease_percent_le",
                                    "value": 20,
                                }
                            ]
                        },
                    }
                ]
            }
        }
        audit = [
            {
                "event": "agent_command_result",
                "data": {
                    "tool": "request_commit",
                    "content": {
                        "evaluation": {
                            "baseline_measurement": {
                                "batch": {"workload_fingerprint": "same"}
                            },
                            "candidate_measurement": {
                                "batch": {"workload_fingerprint": "same"}
                            },
                        }
                    },
                },
            }
        ]

        self.assertTrue(_has_batch_guard(contract))
        self.assertEqual(_evaluation_fingerprints(audit), ("same", "same"))


class RedisCpuMcpTest(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_mcp_module()

    def _server(self, root: Path, output: Path, *, restore_failure: str = "never") -> object:
        environment = {
            "SCX_REDIS_CPU_ROOT": str(root),
            "SCX_REDIS_CPU_OUTPUT_DIR": str(output),
            "SCX_REDIS_CPU_RESTORE_FAILURE": restore_failure,
        }
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
