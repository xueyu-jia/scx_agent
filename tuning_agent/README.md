# tuning-agent

`tuning-agent` 是一个以 Agent 进行 Linux 性能调优、由可信 Runtime 强制执行安全边界的 Rust 服务。

核心原则：

```text
LLM 和 capability provider 决定：观察什么、实验什么、哪些证据表示改善。
可信 Runtime 决定：何时允许、如何记账、如何恢复、证据是否有效、能否提交。
```

## 工作流

Probe 不再是 episode 阶段，而是一类只读能力。一次 episode 的状态只有：

```text
Clean
  -> Experimenting
  -> CommitPending
       -> Committed
       -> RollingBack -> Clean
       -> RecoveryRequired
```

物理 phase 与 `Active / Finishing / Finished` 生命周期正交。图中 rollback 后的 `Clean` 是 `Clean + Finished`；只有 episode 初始的 `Clean + Active` 才能继续调用 Agent 工具。

- `Clean`：没有 episode 范围的未提交 transaction 或 measurement session。它不表示目标可以重新定义；Runtime 生命周期内仍始终持有 WAL 目录的进程级独占锁。
- `Experimenting`：已冻结 evaluation intent，且 Transaction Kernel 已在 WAL 中持久化当前 transaction 的 intent pin；此后才允许 mutation effect。
- `CommitPending`：candidate、evaluation intent、contract 和 provider 版本均已冻结；所有 Agent tool call 被阻止。
- `Committed`：固定 A/B 流程判定改善，provider acknowledgement 与中央 commit seal 完成。
- `RecoveryRequired`：状态无法确定或恢复失败，Activation Kernel 冻结，不再接受新 episode。

episode 以非 `RecoveryRequired` 结果结束后，Activation Kernel 会进入管理员配置的 cooldown；默认 30 秒内拒绝所有新事件，包括 `Critical`。Cooldown 是全局 activation 状态，不是 episode phase。

Agent 可见的工具来自当前 episode 的 `CapabilitySnapshot`：

- `probe_*`：调用一个注册的只读 `ProbeProvider`。
- `begin_experiment`：在任何 mutation 前一次性冻结 objective 和完整 evaluation contract。
- `experiment_*`：调用一个注册的可回滚 `MutationDriver`。
- `request_commit`：提交 Runtime 生成的 `change_id`，请求可信 Runtime 评估。
- `abort`：恢复全部未提交修改并结束当前实验。

Measurement 和 Comparison capability 不直接暴露为 Agent tool。Agent 只能在 `begin_experiment` 的 schema 中选择它们，Runtime 在 `CommitPending` 内按固定 A/B 协议调用。

### 一个 episode 一个目标

`begin_experiment` 成功后生成不可变的 `FrozenEvaluationIntent`：

```text
EpisodeId
  + normalized ObjectiveStatement
  + FrozenEvaluationContract
  -> EvaluationIntentPin(episode_id, intent_digest, contract_digest)
```

Objective 是面向人类和审计的目标陈述；Contract 中的 primary comparison、guardrail、workload invariant 和 sampling plan 是 Runtime 使用的机器可执行成功标准。Runtime 无法判断自然语言与任意插件 specification 是否语义一致，因此最终判定始终以冻结的 Contract 为准。

同一 `EpisodeId` 只允许一次成功冻结。Contract 校验失败不消耗冻结机会；成功后 Objective 或 Contract 都不能替换。根因假设、Probe 选择和同一 transaction 内的 Mutation 方案仍可调整。完整 rollback 只清除 transaction/candidate，不清除 Intent，并结束当前 episode。需要改变 workload、指标、阈值、Measurement、Comparison 或 sampling 时必须创建新 episode。

## 安全边界

### Transaction Kernel

所有 mutation 都必须经过 Transaction Kernel：

1. provider `prepare` 捕获 resource、baseline、desired state 和 provider pin，且不得产生副作用；
2. WAL 先持久化 intent，再允许 `apply`；
3. apply 后必须 readback verify，成功后生成 `change_id`；同一 resource 再次试值会恢复原始 baseline，并生成指向上一版的新 revision；
4. baseline restore 按修改顺序的逆序执行，所有 revision 都回到首版捕获的 baseline；
5. candidate replay 只接受本 episode 中每个 resource 最新且曾成功实验并验证过的 change；
6. 外部 drift、丢失响应或 WAL 写入失败均 fail-closed；
7. provider finalize 只是幂等、无系统副作用的 commit acknowledgement；中央 WAL seal 才是 commit point；
8. `Started` WAL 在任何 mutation effect 前持久化完整 `EvaluationIntentPin`；
9. `CommitSealed` 原子保存 terminal changes 与 Runtime 签发的 `CommitAuthorization`，后者绑定同一 intent pin、candidate、decision 和完整 evaluation evidence digest。

每个 transaction 使用独立 JSONL WAL。Runtime 在启动 MCP、写 audit 或扫描恢复状态前先独占 WAL 目录。随后先用 builtin/local Registry 尽早恢复当前可处理的 transaction，再 best-effort 加载 MCP，最后执行完整 recovery gate。损坏日志和各个 pending transaction 会逐项处理并汇总失败；任何未恢复状态、MCP bootstrap 失败或 audit 失败最终都会阻止绑定 activation source，但无关插件故障不会延迟已经可执行的本地 rollback。已封口 transaction 会在启动时重放 reconciliation audit。

当前 V2 `Started` record 强制要求 intent pin，不会通过默认值接受旧 WAL。升级前应先用生成旧 WAL 的版本恢复全部 pending transaction；若未来需要在线兼容，必须实现显式的 rollback-only legacy reader，旧记录不能进入新 commit 流程。

### Evaluation Kernel

当前 evaluation mechanism 固定为：

```text
restore baseline
  -> settle
  -> trusted system guardrail measurement A
  -> selected domain measurement A
  -> replay exact candidate
  -> settle
  -> trusted system guardrail measurement B
  -> selected domain measurement B
  -> comparison evidence
  -> central verdict
  -> finalize or rollback
```

固定 PSI/loadavg guardrail 始终由内置、受信任的 core measurement 采集。MCP measurement 即使返回同名指标，也不能覆盖系统 guardrail 证据。Comparison plugin 只返回结构化 evidence，最终 verdict 仍由 `VerdictKernel` 产生。

domain measurement 的 A/B 两侧必须都提供存在且完全相同的可信 `workload_fingerprint`。任一侧缺失或两侧变化都会得到 `Inconclusive`，不能进入 finalize。

Measurement/Comparison specification 在第一个 mutation 之前完成 provider 预验证。Contract 记录所有 provider pin 并计算 SHA-256；外层 Intent 再绑定 EpisodeId、规范化 Objective 和 contract digest。两层对象在反序列化时都会重新计算，不能由 LLM 伪造或跨 episode 重用。

管理员通过 `[safety].evaluation_timeout_ms` 为整个 A/B 流程设置单调时钟总预算，默认 `600000` ms。Runtime 在 contract 冻结时和每次 mutation 前计算两侧 settle、guardrail/domain sampling interval 的全部确定性等待；计划本身超预算时，在任何系统修改前拒绝实验。进入 A/B 后，baseline restore、candidate replay、settle、measurement open/sample/close、comparison 和中央 verdict 的前后都检查同一个 deadline。provider 声明的单次 timeout 无法装入剩余预算时不会启动调用；超预算会触发正常的 transaction rollback。

成功的 measurement `open` 始终优先执行一次 `close`，即使 deadline 已经过期。MCP transport 能强制其请求 timeout；进程内同步 provider 无法被 Rust 安全地抢占，必须合作遵守声明 timeout，Runtime 只能在调用返回后检测越界。不要把可能无限阻塞的本地实现注册为 capability。

```toml
[safety]
evaluation_timeout_ms = 600000
cooldown_ms = 30000
```

`cooldown_ms` 是成功恢复或提交后的 activation 间隔；设为 `0` 可由管理员显式关闭 cooldown。

### Capability Policy

Registry 将能力分成四个强类型接口：

```text
ProbeProvider        read-only，Clean / Experimenting
MutationDriver       reversible + idempotent，Clean / Experimenting
MeasurementProvider read-only，CommitPending
ComparisonPolicy     pure + deterministic，CommitPending
```

空 `allowed_phases` 不代表“全部允许”，而是无权限。当前不接受 irreversible mutation，也不接受 managed-observation provider；后者需要独立 session WAL 和崩溃恢复协议后才能启用。

任意 shell 不是默认 capability。需要命令的观测或 measurement 必须由内部代码或 MCP provider 预先实现、声明 schema，并通过同一 Registry 策略。

crate 对外只暴露高层 `Config`、`Runtime`、activation DTO 和发送函数。Transaction Kernel、WAL、Coordinator、provider execution handle 与具体 adapter 都是 crate-private，避免嵌入方绕过 recovery、A/B 或 commit authority。代码型 capability 作为受信任代码在仓库内实现 SPI，并且只能由 `runtime/bootstrap.rs` 注册；进程外扩展使用 MCP。

## Skills 与 References

Runtime 支持标准 Agent Skill 目录中的 `SKILL.md` 和 `references/`。Skill 只扩展推理上下文，不是 capability，也不会获得新的执行权限：

```text
skills-root/
  scheduler-guide/
    SKILL.md
    references/
      signals.md
    agents/
      openai.yaml
```

启动时 Runtime 严格解析并有界读入全部 Skill 指令与 UTF-8 Reference，计算内容 digest 后生成不可变 snapshot。首轮上下文只包含 `name`、`description` 和逻辑路径；显式请求会预加载完整 `SKILL.md`，隐式匹配则由 Agent 单独调用 `load_skill`。选中 Skill 后可用 `load_skill_reference` 按精确的 `references/...` 路径加载资料。

```toml
[skills]
enabled = true
roots = ["/etc/tuning-agent/skills"]
max_skills = 64
max_catalog_chars = 8000
max_loaded_skills = 4
max_skill_rounds = 4
max_reference_reads = 8
max_skill_bytes = 131072
max_reference_bytes = 262144
max_references_per_skill = 128
max_registry_bytes = 16777216
```

显式调用使用可重复的 `--skill`：

```bash
cargo run -- --config tuning-agent.toml activate --wait --json \
  --skill scheduler-guide \
  "diagnose scheduler pressure" warning cli
```

`agents/openai.yaml` 当前只应用 `policy.allow_implicit_invocation`。声明 tool dependency 的 Skill 在 reference-only 模式下会被拒绝。`allowed-tools` 不授予权限；`scripts/` 和 `assets/` 不被索引、读取或执行。Skill/Reference 调用不能和 Probe、Experiment、Commit 或 Abort 混在同一 tool-call batch。

## 内置能力

- `builtin/probe.linux-proc-snapshot.v1`：有界读取 loadavg、meminfo 摘要、PSI、CPU 和 scheduler 计数。调用者不能指定路径或命令。
- `builtin/measurement.core-system.v1`：采集 loadavg 与 CPU/IO/memory PSI，同时作为固定系统 guardrail 数据源。
- `builtin/comparison.threshold.v1`：执行 typed metric condition，例如 `decrease_percent_ge`、`increase_abs_le`。

本地 mutation 不提供通用路径写入工具。管理员配置的每个条目会生成一个绑定到单一资源的 capability；Agent 只能提供 `value`：

```toml
[[capabilities.local_mutations]]
id = "local/vm-dirty-ratio"
description = "Experiment with vm.dirty_ratio"
kind = "sysctl"
key = "vm.dirty_ratio"
```

还支持显式绑定的 `proc_sys`、`sysfs` 和 `cgroup` 绝对路径。provider 会 canonicalize 路径并确认其位于对应 Linux 根目录下。

## MCP 扩展

MCP Server 通过固定 resource `tuning://capabilities/v1` 发布 tuning capability manifest。普通 `tools/list` 描述或 MCP annotations 不会被当作安全授权。

Runtime 在启动时：

1. 以 stdio 初始化 MCP Server；子进程环境默认清空，只传递配置中的显式 `env`；
2. 读取并严格解析 manifest，拒绝未知字段和不支持的 schema；
3. 对照 `tools/list` 验证 manifest 引用的 operation；
4. 强制将 provider class 设为 `Mcp`，应用全局和 per-server allowlist；
5. 默认拒绝 MCP mutation，只有 server 配置 `allow_mutations = true` 才可注册；
6. 将四类 provider 注入同一个 `CapabilityRegistry`。

示例配置：

```toml
[mcp]
enabled = true

[[mcp.servers]]
id = "scheduler-observer"
enabled = true
command = "/usr/local/libexec/scheduler-observer-mcp"
args = []
request_timeout_ms = 30000
allowed_capabilities = []
allow_mutations = false

[mcp.servers.env]
RUST_LOG = "warn"
```

Registry 在 episode 开始时生成 immutable snapshot。provider 不会在存在未恢复 mutation 时热更新或卸载。

MCP manifest 是协议与授权输入，不是对恶意进程的 sandbox。Server 仍应以完成其能力所需的最低 OS 用户、capability、cgroup 和文件权限运行；`allow_mutations` 只应授予受审计的 provider。

MCP Server 作者需要遵守的 manifest、operation DTO、schema 与恢复契约见 [`MCP_CAPABILITIES.md`](MCP_CAPABILITIES.md)。

## 运行

```bash
cd tuning_agent
cargo build
cp tuning-agent.example.toml tuning-agent.toml
./target/debug/tuning-agent --config tuning-agent.toml daemon
```

发送一次激活：

```bash
cargo run -- --config tuning-agent.toml activate \
  "diagnose current host performance" info cli
```

等待 episode 完成并输出结构化结果：

```bash
cargo run -- --config tuning-agent.toml activate \
  --wait --json --timeout-seconds 900 \
  "bench treatment" info scx-bench /sys/fs/cgroup/my-workload
```

`--wait --json` 返回 `ActivationResponse`，其中 `status` 只表达 runtime 结果分类：
`committed`、`no_commit`、`recovery_required`、`rejected` 或 `error`。它不会授予
调用方 commit 权限；commit 仍只能由 Runtime 在固定 A/B evaluate 后完成。

配置优先级：

```text
显式 --config 文件
  > 当前目录 tuning-agent.toml
  > 安全默认值
```

配置使用 `deny_unknown_fields`。V1 的 `[command]`、`[evaluation]`、`experiment_write`、shell measurement 等配置和协议已删除，出现时会直接报错，避免静默使用无效安全设置。

## 验证

```bash
cargo fmt --check
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
```

部分受限 sandbox 会禁止 Unix socket `bind(2)`；这种环境下可以只跳过对应 IPC 测试，其余 transaction、evaluation、provider 和端到端 episode 测试仍应全部运行：

```bash
cargo test --offline -- --skip activation::source::unix::tests::unix_ipc_source_receives_activation_event
```

开发者模块边界和扩展契约见 [`ARCHITECTURE.md`](ARCHITECTURE.md)。
