from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from bench.scenarios.cgroup_cpu.common import (
    CgroupCpuScope,
    cgroup_exec_argv,
    read_cpu_stat,
    read_weight,
    scope_state,
    write_weight,
)
from bench.scenarios.cgroup_cpu.workload import build_metrics


MCP_PATH = Path("bench/scenarios/cgroup_cpu/mcp_server.py").resolve()


def load_mcp_module() -> object:
    spec = importlib.util.spec_from_file_location("cgroup_cpu_mcp_test", MCP_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def fake_scope(root: Path, *, target_weight: int = 10, neighbor_weight: int = 100) -> CgroupCpuScope:
    scope = CgroupCpuScope.from_root(root)
    scope.root.mkdir(parents=True)
    (scope.root / "cgroup.procs").write_text("", encoding="utf-8")
    for path, weight in ((scope.target, target_weight), (scope.neighbor, neighbor_weight)):
        path.mkdir()
        (path / "cgroup.procs").write_text("", encoding="utf-8")
        (path / "cpu.weight").write_text(f"{weight}\n", encoding="utf-8")
        (path / "cpu.stat").write_text(
            "usage_usec 100\nuser_usec 60\nsystem_usec 40\n",
            encoding="utf-8",
        )
    return scope


def metric(value: float, unit: str) -> dict[str, object]:
    return {"value": value, "unit": unit, "kind": "gauge"}


class CgroupCpuCommonTest(unittest.TestCase):
    def test_readback_and_scope_state_are_structured(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            scope = fake_scope(Path(temp_dir) / "scope")

            write_weight(scope.target, 100)
            state = scope_state(scope)

            self.assertEqual(read_weight(scope.target), 100)
            self.assertEqual(read_cpu_stat(scope.neighbor)["usage_usec"], 100)
            self.assertEqual(state["target"]["weight"], 100)
            self.assertEqual(state["neighbor"]["weight"], 100)

    def test_exec_helper_enters_group_before_exec(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cgroup = Path(temp_dir) / "group"
            cgroup.mkdir()
            members = cgroup / "cgroup.procs"
            members.write_text("", encoding="utf-8")

            completed = subprocess.run(
                cgroup_exec_argv(cgroup, ["true"]),
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr.decode())
            self.assertTrue(members.read_text(encoding="utf-8").strip())


class CgroupCpuMcpTest(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_mcp_module()

    def _server(self, root: Path, state: Path, scenario: str = "positive") -> object:
        environment = {
            "SCX_CGROUP_CPU_ROOT": str(root),
            "SCX_CGROUP_CPU_STATE_PATH": str(state),
            "SCX_CGROUP_CPU_SCENARIO": scenario,
        }
        with patch.dict(os.environ, environment, clear=False):
            return self.module.CgroupCpuMcp()

    def test_mutation_is_reversible_and_status_survives_restart(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            scope = fake_scope(root)
            state = Path(temp_dir) / "operations.json"
            first = self._server(root, state)
            prepared = first._mutation_prepare(
                {
                    "context": {"operation_id": "transaction/experiment/1"},
                    "arguments": {"value": 100},
                }
            )

            receipt = first._mutation_apply(
                {"operation_id": "transaction/experiment/1", "prepared": prepared}
            )
            second = self._server(root, state)
            status = second._mutation_status({"operation_id": "transaction/experiment/1"})
            restored = second._mutation_restore(
                {"operation_id": "transaction/restore/2", "prepared": prepared}
            )

            self.assertEqual(receipt["state"], "applied")
            self.assertEqual(status["state"], "applied")
            self.assertEqual(restored["state"], "restored")
            self.assertEqual(read_weight(scope.target), 10)

    def test_recovery_fault_never_claims_restore(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            fake_scope(root)
            server = self._server(root, Path(temp_dir) / "operations.json", scenario="recovery")
            prepared = server._mutation_prepare(
                {
                    "context": {"operation_id": "transaction/experiment/1"},
                    "arguments": {"value": 100},
                }
            )
            server._mutation_apply(
                {"operation_id": "transaction/experiment/1", "prepared": prepared}
            )

            with self.assertRaisesRegex(self.module.McpToolError, "restore failure"):
                server._mutation_restore(
                    {"operation_id": "transaction/restore/2", "prepared": prepared}
                )

    def test_fixed_balance_policy_rejects_no_signal_and_unsafe_candidate(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir) / "scope"
            fake_scope(root)
            server = self._server(root, Path(temp_dir) / "operations.json")
            baseline = {
                "metrics": {
                    "target_cpu_share_pct": metric(9.0, "percent"),
                    "neighbor_cpu_share_pct": metric(91.0, "percent"),
                    "aggregate_cpu_rate": metric(2.0, "cpu"),
                }
            }
            positive = {
                "metrics": {
                    "target_cpu_share_pct": metric(50.0, "percent"),
                    "neighbor_cpu_share_pct": metric(50.0, "percent"),
                    "aggregate_cpu_rate": metric(2.0, "cpu"),
                }
            }
            unsafe = {
                "metrics": {
                    "target_cpu_share_pct": metric(99.0, "percent"),
                    "neighbor_cpu_share_pct": metric(1.0, "percent"),
                    "aggregate_cpu_rate": metric(2.0, "cpu"),
                }
            }
            no_signal = {
                "metrics": {
                    "target_cpu_share_pct": metric(50.0, "percent"),
                    "neighbor_cpu_share_pct": metric(50.0, "percent"),
                    "aggregate_cpu_rate": metric(2.0, "cpu"),
                }
            }
            spec = {"policy": "target_share_slo_v1"}

            self.assertEqual(
                server._compare({"specification": spec, "baseline": baseline, "candidate": positive})[
                    "conclusion"
                ],
                "improved",
            )
            self.assertEqual(
                server._compare({"specification": spec, "baseline": baseline, "candidate": unsafe})[
                    "conclusion"
                ],
                "not_improved",
            )
            self.assertEqual(
                server._compare({"specification": spec, "baseline": no_signal, "candidate": no_signal})[
                    "conclusion"
                ],
                "not_improved",
            )


class CgroupCpuShareMetricTest(unittest.TestCase):
    def test_held_out_metrics_keep_target_and_total_separate(self) -> None:
        metrics = build_metrics(
            {"throughput": 50.0},
            {"throughput": 50.0},
            target_cpu_usage_usec=1_000_000,
            neighbor_cpu_usage_usec=1_000_000,
            elapsed_time_sec=1.0,
            target_weight=100,
            neighbor_weight=100,
        )

        self.assertEqual(metrics["target_work_share_pct"], 50.0)
        self.assertEqual(metrics["aggregate_throughput"], 100.0)
        self.assertEqual(metrics["target_cpu_weight"], 100.0)


if __name__ == "__main__":
    unittest.main()
