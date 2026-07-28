# tuning-agent 集成

该目录把 tuning-agent 的领域流程适配为 benchmark 通用 Treatment Outcome V2。Bench Core 只解释 `proceed`、`stop` 和 `unsafe`，不依赖 tuning-agent 私有状态。

通用 treatment 配置和 Outcome 格式见 [../../CONFIGURATION.md](../../CONFIGURATION.md#treatment)，实验命令见 [../../RUNBOOK.md](../../RUNBOOK.md#真实-llm-实验)。

## Adapter 流程

`adapter.py` 负责 cgroup-scoped tuning：

1. 创建临时 cgroup，并把可选训练负载加入该 cgroup；
2. 检查配置的 LLM endpoint 在 Guest 中可达；
3. 启动 support process 和 tuning-agent daemon；
4. 等待并验证可选的 training readiness 文件；
5. 执行 `tuning-agent activate --wait --json <cgroup_path>`；
6. 停止并等待辅助进程，验证保留的 cgroup 和 CPU 状态；
7. 原子写入 Outcome V2。

私有状态映射：

```text
committed + state verified                 -> proceed
no_commit + baseline verified + ITT policy -> proceed
no_commit + baseline verified + strict     -> stop
no_commit + baseline unverified             -> unsafe
recovery_required                           -> unsafe
```

`--no-commit-disposition proceed|stop` 控制已验证 `no_commit` 的策略；`recovery_required -> unsafe` 不可配置。activation 响应、验证证据和私有状态写入 `details`，Bench Core 不据此分支。

## Readiness 和超时

训练负载可配置：

```text
SCX_TUNING_AGENT_TRAINING_READY_PATH
SCX_TUNING_AGENT_TRAINING_READY_TIMEOUT_SECONDS
```

相对 ready path 以 `SCX_BENCH_OUT` 为基准。配置后，adapter 按 V1 readiness 格式验证 PID、start time 和 executable；未配置时使用 settle sleep。

真实模型场景通过 `SCX_TUNING_AGENT_LLM_BASE_URL/API_KEY/MODEL` 指定完整 API base、
凭据和模型。adapter 归档的 tuning-agent 配置会隐藏 key，但 Treatment `env` 仍会进入
Guest plan 和实验结果；使用短期 key 时应在发布结果前注销。

## 确定性测试

`mock_llm.py` 和 `deterministic_mcp.py` 提供不依赖真实模型的协议与安全测试。`SCX_DETERMINISTIC_SCENARIO` 支持：

- `positive`：应 commit；
- `no_signal`：应 rollback 为 `no_commit`；
- `unsafe`：应 rollback 为 `no_commit`；
- `recovery`：应阻断 warmup 和 measurement。

这些场景验证 provider-independent 的运行时契约，不用于统计真实模型决策质量。

## 真实 LLM scheduler

`scx_real_llm_scheduler.py` 和 `scx_perf_treatment.py` 组成 openEuler 正式实验链路。
scheduler 从配置读取统一的 `SCX_TUNING_AGENT_LLM_*`，Guest tuning-agent 直接访问相应
OpenAI-compatible endpoint。Host 只在启动 VM 前执行一次协议 preflight。

classify treatment 通过共享屏障发布 workload 的全部目标 `comm`，要求 `scx_agent_classed` 在一条 activation 和一个 LLM episode 中处理它们。episode 必须为每个目标生成 mutation，并通过一次 `request_commit` 原子提交。

固定脚本会验证：

- LLM episode 为 `committed/improved`；
- mutation 数等于 activation comm 数；
- measurement 状态为 `PASS`；
- episode 结束后系统处于 quiet state。

完整的 preflight、group gate、矩阵和结果路径见 [../../RUNBOOK.md](../../RUNBOOK.md#真实-llm-实验)。

## Held-out 测量边界

Agent 内部训练负载和 A/B evaluation 只决定是否 commit；benchmark warmup 和 measurement 是独立的 held-out 外部测量。

benchmark 自有的 `host_support_files` 复制到 `/tmp/scx-bench-workload.d/`，不能依赖 treatment 的 `/tmp/scx-bench-treatment.d/`：

```yaml
benches:
  redis_cpu:
    host_support_files:
      - bench/scenarios/redis_cpu/workload.py
      - bench/scenarios/redis_cpu/common.py
    measurement:
      command: python3
      args: [/tmp/scx-bench-workload.d/workload.py]
      timeout_seconds: 180
```

## Redis CPU 场景

Redis CPU 提供两个独立配置：

- `bench/configs/redis_cpu_demo/`：Redis 从 weight 50 的已知欠配状态开始，仅允许
  100/200/400/800，用于稳定展示诊断、mutation、固定 A/B 和 commit；
- `bench/configs/redis_cpu_tuning/`：Redis 与 batch 从 100:100 公平状态开始，模型自行
  冻结评价契约，用于研究真实决策质量，允许合理的 no-commit。

两者都使用两个 Redis shard、两个独立 driver CPU 和竞争性 batch workload；训练阶段
通过严格 readiness 文件固定 workload identity，正式测量使用独立的 staged workload。

VM 使用 6 个 vCPU：Redis server 和 batch 共享 CPU 0-1；两个
`redis-benchmark` driver 分别固定到 CPU 2、CPU 4。CPU 3、CPU 5 是同组
预留的 SMT sibling，不运行 driver。

demo activation 固定要求恢复至少 5% 的 p99，并保持 `batch_cpu_rate >= 0.5`；候选集合
由 MCP schema 限定。research activation 只要求降低 p99 并维持 batch 进度，不预设
百分比。Bench 的通用分析报告展示 held-out p99、QPS、batch CPU rate、置信区间、
运行状态和 `tuning_agent.committed` 次数，不对模型合同结构做场景级判定。

Redis 入口从 `redis_cpu_agent` treatment 读取 LLM 配置，并在创建 VM 前执行真实
tool-call preflight：

```bash
python3 -m bench.scenarios.redis_cpu.run \
  --config bench/configs/local_profiles/oe2403sp4_6_6_redis_cpu_demo \
  --plan redis_cpu_demo_smoke
```

该场景不提供 Mock LLM。入口只负责 preflight 和调用通用 runner；MCP restore、回滚和
cgroup 清理等安全不变量由确定性测试覆盖。
