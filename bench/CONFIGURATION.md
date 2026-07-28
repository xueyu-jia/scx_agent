# 配置参考

benchmark 配置入口是一个目录，不是单个文件。加载器要求以下三个 YAML 文件同时存在，并校验顶层配置是否位于正确文件。

```text
<config-directory>/
  environment.config  # libvirt / executor / machines
  benches.config      # bench_defaults / metric_profiles / suites / benches
  plan.config         # schedulers / treatments / plans
```

openEuler 24.03 SP4 / 6.6 基础模板位于 `bench/configs/example_config/`。默认 profile 位于 `bench/configs/local_profiles/oe2403sp4_6_6_scx/`，包含真实 LLM 实验定义和机器本地路径。

## environment.config

该文件定义 VM、host 资源分配和机器规格：

```yaml
libvirt:
  uri: qemu:///system
  kernel: null
  kernel_args: root=/dev/vda1 console=ttyS0 systemd.mask=boot-efi.mount psi=1
  kernel_config: null
  kernel_source: null
  sync_kernel_source: false
  initrd: null
  root_image: null
  runtime_dir: /var/lib/libvirt/scx-bench-runs
  network: default
  ssh_user: root
  ssh_key: null
  ssh_port: 22
  workdir: null
  guest_output_dir: /scx_bench_out
  emulator_cpus: null
  iothread_cpus: null
  pin_vhost_threads: true
  boot_timeout_seconds: 90
  vm_settle_seconds: 10
  timeout_extra_seconds: 120
  destroy_on_exit: true
  cpu_mode: host-passthrough

executor:
  parallel: auto
  cpu_source: isolated
  isolated_cpus: null
  irq_cpus: null
  smt_policy: use_all_siblings
  pair_policy: sequential
  memory_guard_gb: 16

machines:
  small_core:
    vcpus: 2
    memory: 4G
    pin_cpus: auto
    exclusive: true
    frequency:
      fixed: true
      governor: performance
      turbo: true
```

关键约束：

- `kernel` 是 guest 使用的 bootable image；`kernel_config` 供 scheduler kconfig 和内核元数据使用，`kernel_source` 供 workload 构建使用；
- `root_image` 是只读 base qcow2，runner 为每次 run 创建 overlay；
- `vm_settle_seconds` 位于 SSH ready 之后、scheduler 启动之前；
- `parallel: auto` 根据隔离 CPU、完整 SMT sibling group、VM 内存和 `memory_guard_gb` 计算并行 comparison pair 数；
- `isolated_cpus` 必须包含完整 SMT sibling group；一个 physical core 的 sibling 不会分给不同 pair；
- `irq_cpus` 不能与 VM pinned CPU 重叠；
- `pin_cpus: auto` 从 `executor.isolated_cpus` 分配 CPU；`exclusive: true` 禁止同一时刻复用这些 CPU；
- `frequency.fixed: true` 要求 runner 在启动 VM 前验证固定频率。

openEuler 6.6 profile 必须在 `kernel_args` 中保留 `psi=1`，否则 `CONFIG_PSI_DEFAULT_DISABLED=y` 会关闭 PSI。

## benches.config

该文件定义阶段默认值、指标、suite 和 workload：

```yaml
bench_defaults:
  post_warmup_settle_seconds: 2
  cooldown_seconds: 1

metric_profiles:
  latency:
    primary:
      - name: p99_latency_us
        direction: lower
        unit: us
        chart: latency_bar
        regression: +5%
      - name: throughput
        direction: higher
        unit: requests/s
        chart: delta_bar
        regression: -3%
    secondary:
      - elapsed_time_sec
      - context_switches
      - migrations

suites:
  latency_only:
    benches: [schbench_latency]
    metric_profile: latency

benches:
  schbench_latency:
    env: {}
    warmup:
      command: python3
      args: [bench/benchmarks/schbench.py, --, -m, "1", -t, "4", -r, "5"]
      timeout_seconds: 20
    measurement:
      command: bench/workloads/bin/perf
      args: [stat, -a, -x, ",", -o, /scx_bench_out/perf_stat.csv, --, python3, bench/benchmarks/schbench.py, --, -m, "1", -t, "4", -r, "30"]
      timeout_seconds: 90
```

`measurement` 必填，`warmup` 可选；两者使用结构化的 `command`、可选 `args` 和必填 `timeout_seconds`。它们共享 bench `env`，但使用不同的 `SCX_BENCH_OUT`，warmup 产物不会进入正式指标。

benchmark 需要额外 host 文件时，使用 `host_support_files`：

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

runner 将这些文件复制到 `/tmp/scx-bench-workload.d/`。benchmark staging 与 treatment staging 相互独立。

### 指标定义

wrapper 在 stdout 输出规范化 JSON；`primary` 指标参与 verdict，`secondary` 指标只进入结果和报告。

- `direction`：`higher` 或 `lower`；
- `unit`：报告显示单位；
- `chart`：报告图表类型；
- `regression`：相对 baseline 的回归阈值，例如 `+5%` 或 `-3%`。

Wrapper 的完整输出和扩展契约见 [ARCHITECTURE.md](ARCHITECTURE.md#workload-wrapper-contract)。

## plan.config

该文件定义 scheduler、可选 treatment 和运行矩阵：

```yaml
schedulers:
  default:
    kind: builtin

  scx_agent_classed:
    kind: scx
    command: schedule/scx_agent_classed/target/release/scx_agent_classed
    host_command: schedule/scx_agent_classed/target/release/scx_agent_classed
    host_kconfig: /path/to/linux/.config
    args: [--kconfig, /tmp/scx-bench-kconfig, --default-class, batch]
    env: {}
    settle_seconds: 2

treatments:
  agent_tuned:
    command: /tmp/scx-bench-treatment
    host_command: bench/integrations/tuning_agent/adapter.py
    host_support_files:
      - bench/integrations/tuning_agent/mock_llm.py
      - bench/integrations/tuning_agent/deterministic_mcp.py
    args: [--no-commit-disposition, proceed]
    env:
      MODE: tune
    timeout_seconds: 900
    post_treatment_settle_seconds: 5

plans:
  latency_smoke:
    runs: 1
    matrix:
      - machine: small_core
        suites: [latency_only]
```

### Scheduler

`kind: builtin` 不启动 scheduler 进程。`kind: scx` 由 guest executor 在 treatment、warmup 和 measurement 前启动，并在 cooldown 后停止。

`command` 是 guest 路径。设置 `host_command` 后，runner 会把当前 host 文件复制到 fresh overlay，记录 SHA-256 并执行 staged copy，避免使用 base image 中的旧版本。相对路径从仓库根目录解析。

`host_support_files` 可为 scheduler 携带辅助文件。缺少 `CONFIG_IKCONFIG` 时，可通过 `host_kconfig` 提供 host `.config`，并在 `args` 中传入 staged 路径 `/tmp/scx-bench-kconfig`。

### Treatment

Treatment 是 scheduler settle 后、workload warmup 前运行的有界命令，用来建立正式测量需要的系统状态。baseline 和 candidate 可以选择相同 scheduler，只改变 treatment。

```text
VM settle
  -> scheduler start/settle
  -> optional treatment
  -> post-treatment settle
  -> optional benchmark warmup
  -> post-warmup settle
  -> before snapshot
  -> measurement
  -> after snapshot
  -> cooldown/cleanup
```

Treatment 的 `command`、`host_command` 和 `host_support_files` 与 scheduler staging 语义相同；support files 复制到 `/tmp/scx-bench-treatment.d/`。

真实 LLM scheduler 和 treatment 使用统一配置：

```yaml
env:
  SCX_TUNING_AGENT_LLM_BASE_URL: https://api.example.com/v1
  SCX_TUNING_AGENT_LLM_API_KEY: replace-in-local-profile
  SCX_TUNING_AGENT_LLM_MODEL: model-name
```

`BASE_URL` 是包含版本或自定义前缀的完整 API base，tuning-agent 在其后追加
`/chat/completions`。真实 key 只写入 Git 已忽略的本地 profile。scheduler/treatment 配置
和 `env` 会进入 `guest_plan.json`、manifest 和结果元数据，发布结果前必须注销该 key。

guest executor 只向 treatment 注入角色信息，warmup 和 measurement 不能根据 baseline/candidate 身份分支：

```text
SCX_BENCH_ROLE       baseline | candidate | standalone
SCX_BENCH_VARIANT    <scheduler> 或 <scheduler>__<treatment>
SCX_BENCH_TREATMENT  treatment 名；未配置时为空
SCX_BENCH_OUT        treatment 产物目录
SCX_BENCH_WORKDIR    guest 内仓库目录
```

### Treatment Outcome V2

Treatment 必须在 `SCX_BENCH_TREATMENT_OUTCOME` 指定的路径原子写入不超过 64 KiB 的 JSON：

```json
{
  "version": 2,
  "disposition": "proceed",
  "reason": {
    "code": "tuning_agent.committed",
    "message": "candidate committed and state verified"
  },
  "details": {
    "episode_id": 123,
    "verdict": "improved",
    "candidate_digest": "sha256:..."
  }
}
```

`version`、`disposition`、`reason` 和 `details` 必须存在；`reason` 只能包含非空 `code` 和 `message`，`details` 可以为空对象，未知字段会被拒绝。Bench Core 只解释 `disposition`：

- `proceed`：状态已验证，继续 settle、warmup 和 measurement；
- `stop`：状态安全，但按策略停止本次 run，不进行 measurement；
- `unsafe`：状态不安全或无法验证，必须阻断 measurement。

`stop` 和 `unsafe` 都使 guest executor 以 125 退出，但分别映射为 `TREATMENT_STOPPED` 和 `TREATMENT_UNSAFE_STATE`。非零退出、损坏或缺失的 outcome、残留进程映射为 `TREATMENT_FAILED`；超时映射为 `TREATMENT_TIMEOUT`。即使已写入 `proceed`，命令失败或残留进程仍优先判为失败。

Treatment 启动的训练负载、agent daemon 和 MCP 子进程必须在写 outcome 前全部停止并等待退出。tuning-agent 的领域状态映射见 [integrations/tuning_agent/README.md](integrations/tuning_agent/README.md)。

## 执行计划

runner 为每次 run 生成 `guest_plan.json`，再上传固定的 Python guest executor。treatment、warmup 和 measurement 的 timeout 都在 guest 内执行；超时、非零退出或残留进程会形成明确状态，并清理整个进程组。前置阶段失败时不会开始 measurement。

总 host timeout 包含所有命令、settle、cooldown 和额外余量。dry-run 的 `result.json` 与 `manifest.json` 保存同一份 `execution_plan`，可用于运行前审计。

选择 plan、scheduler 和 treatment 的命令见 [RUNBOOK.md](RUNBOOK.md)。
