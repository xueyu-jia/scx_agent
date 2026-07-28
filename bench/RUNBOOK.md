# 实验运行手册

本文说明预检、实验执行、结果检查和离线分析。首次运行前先按 [ENVIRONMENT.md](ENVIRONMENT.md) 准备 host，并用 `bench.env verify` 验证 profile。

## 运行前检查

不启动 VM，只校验配置并生成执行目录：

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan kernel_migration_smoke \
  --baseline default \
  --candidate scx_agent_classed \
  --dry-run
```

真实运行还会检查 base-image manifest、CPU 隔离、固定频率和 IRQ/RPS/XPS 分配。检查失败时不会启动 VM。

## 真实 LLM 实验

固定脚本运行 EEVDF A/A 和真实 LLM classify 实验。LLM 配置来自本地 profile 中三个
`scx_agent_classed_llm_*` scheduler 的 `SCX_TUNING_AGENT_LLM_*` 环境变量：

```bash
bash bench/scripts/run_oe2403sp4_real_llm.sh --preflight-only
bash bench/scripts/run_oe2403sp4_real_llm.sh --group-gate
bash bench/scripts/run_oe2403sp4_real_llm.sh
```

数据链路如下：

```text
guest tuning-agent
  -> configured OpenAI-compatible API base
  -> /chat/completions
```

`--preflight-only` 对 profile 中配置的 endpoint、key 和 model 执行一次真实
`tool_choice: auto` tool call。每个 Guest 随后对同一配置执行 DNS、TCP 和 TLS readiness，
实验流量不经过 host。API key 会进入本地 profile、Guest plan 和实验结果，发布结果前应注销。

`--group-gate` 分别运行一个 LATENCY、BATCH 和 MIX pair：

- baseline：`default + control`；
- candidate：`scx_agent_classed + DeepSeek classify`；
- candidate 必须只有一个 `committed/improved` classification episode；
- mutation 数必须等于 activation 中的 comm 数；
- baseline 和 candidate measurement 都必须为 `PASS`。

LATENCY 包含 `schbench`；BATCH 包含 `stress-ng` 和 `stress-ng-cpu`；MIX 包含三者。classify 使用共享屏障同时发布目标 comm，要求单个 episode 原子提交全部规则。

完整矩阵包含三组 A/A 各 2 pair，以及 LATENCY 8 pair、BATCH 4 pair、MIX 4 pair，共 22 pair、44 run。control/classify treatment 为 240 秒；classify episode 最多等待 180 秒，随后 settle 5 秒。

脚本逐组校验 run 状态、LLM episode 和 quiet state。单组失败会记录并继续后续组，最终以非零状态报告整体失败。结果写入 `bench/results/oe2403sp4_6_6_scx/real_llm/<timestamp>/`。

adapter、Outcome 和确定性场景说明见 [integrations/tuning_agent/README.md](integrations/tuning_agent/README.md)。

## 通用实验

运行 baseline / candidate 对比：

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan kernel_migration_smoke \
  --baseline default \
  --candidate scx_agent_classed
```

baseline 和 candidate 都可以是 `builtin` 或 `scx`。`--baseline` 只表示比较基准，不要求使用内核默认 scheduler。

比较同一 scheduler 的 control 与 Agent 调优状态时，分别选择 treatment：

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan single_latency_core_priming \
  --baseline default \
  --baseline-treatment llm_latency_control \
  --candidate scx_agent_classed_llm_latency \
  --candidate-treatment llm_latency_classify
```

使用 treatment 后，变体标签为 `<scheduler>__<treatment>`。两侧必须选择不同的 scheduler/treatment 组合。

只运行单个 scheduler：

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan kernel_migration_smoke \
  --scheduler default
```

## Pair 顺序和并行

comparison pair 是同一个 `RunSpec` 下的 baseline 和 candidate。pair 内串行执行，pair 之间可以并行。

默认 `--order alternating` 抵消时间漂移：

```text
run_index 1: baseline -> candidate
run_index 2: candidate -> baseline
run_index 3: baseline -> candidate
```

需要固定顺序时使用 `--order sequential`。通过 `--parallel N` 限制并行 pair，或使用 `--parallel auto` 根据隔离 CPU、SMT sibling group、VM 内存和 `memory_guard_gb` 自动计算。

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan kernel_migration_smoke \
  --baseline default \
  --candidate scx_agent_classed \
  --order alternating \
  --parallel auto
```

## 正式性能矩阵

先运行迁移门禁，再运行多次测量：

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan kernel_migration_smoke \
  --baseline default \
  --candidate scx_agent_classed \
  --parallel 1

for plan in \
  single_latency_core_measured \
  single_batch_core_measured \
  mixed_fixed_rps_core_measured
do
  python3 bench/scripts/run.py \
    --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
    --plan "$plan" \
    --baseline default \
    --candidate scx_agent_classed \
    --order alternating \
    --parallel 1
done
```

## 比较不同内核

跨内核实验必须保持 scheduler、plan、CPU pinning、VM 规格和运行次数一致。分别运行两个 profile，再离线比较：

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_config \
  --plan single_latency_core_measured \
  --scheduler default \
  --output bench/results/kernel_compare/linux_6_18_default

python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan single_latency_core_measured \
  --scheduler default \
  --output bench/results/kernel_compare/oe2403sp4_6_6_default

python3 -m bench.analysis.run \
  --baseline bench/results/kernel_compare/linux_6_18_default \
  --candidate bench/results/kernel_compare/oe2403sp4_6_6_default \
  --output bench/results/kernel_compare/default_analysis
```

`result.json` 和 `system_metadata.json` 记录 `uname`、`/proc/version`、sched_ext、BTF、关键 config 和 `/proc/config.gz` SHA-256。分析前确认两组 `system.release` 与预期内核一致。

## 结果目录

默认实验目录：

```text
bench/results/experiments/
  <timestamp>__<baseline>_vs_<candidate>/
    metadata.json
    runs/
      <baseline_variant>/
        manifest.json
        run_001__machine_...__suite_...__bench_.../
          result.json
          bench_metrics.json
          guest_plan.json
          guest_result.json
          placement.json
          domain.xml
          stdout.log
          stderr.log
          workload_stdout.log
          workload_stderr.log
          scheduler_stdout.log
          scheduler_stderr.log
          treatment/
          warmup/
          snapshots/
      <candidate_variant>/
        ...
    analysis/
      metadata.json
      analysis.json
      report.html
      paired/
```

`runs/<variant>/run_.../` 保存单次 run 的原始证据；`analysis/analysis.json` 是机器可读的比较结果；`analysis/report.html` 是最终报告。

## 重新分析

已有两组结果时，无需重新运行 workload：

```bash
python3 -m bench.analysis.run \
  --baseline bench/results/experiments/<id>/runs/default \
  --candidate bench/results/experiments/<id>/runs/scx_agent_classed \
  --output /tmp/scx-analysis
```

输出包括：

```text
metadata.json
analysis.json
report.html
paired/pairs.csv
paired/summary.csv
```

交替实验按相同 `run_index` 配对，而不是只比较独立均值。聚合比较、配对统计和 HTML 共用同一个分析模型。分析层只读取 `bench_metrics.json` 中的规范化指标，不重新解析 workload 日志；置信区间为配对变化均值的 95% Student-t 区间。

## 运行约束

- libvirt XML 显式设置 `vcpupin`、`emulatorpin` 和可选 `iothreadpin`；
- 启用 `pin_vhost_threads` 后，runner 记录并尽量 pin host vhost 线程；
- host CPU 必须提前隔离并固定频率；
- managed IRQ 在 pinned CPU 上产生增量时，run 标记为 `INTERRUPT_CONTAMINATED`；
- 资源允许时，`run.py` 以 comparison pair 为单位并发。
