# tuning-agent

`tuning-agent` 是一个使用 Rust 编写的 Linux 内核调优 Agent。

它的目标不是维护一套越来越庞大的调优规则，而是让模型像内核专家一样观察系统、提出假设、执行受控实验，并通过确定性的验证流程决定是否保留调优结果。

## 当前能力

- 常驻 daemon
- Unix IPC 激活
- Timer 激活
- eBPF ringbuf 激活接入点
- OpenAI-compatible LLM 接入
- 只读诊断命令
- 结构化内核参数实验写入
- 自动记录写入前 old value
- commit 前确定性验证
- commit 只保留显式声明的 `keep_writes`
- reject / inconclusive / 未 commit 时自动恢复实验写入
- JSONL 审计日志

## 基本概念

### Episode

一次激活会启动一个 episode。

在一个 episode 中，Agent 可以：

1. 读取系统状态
2. 执行实验性内核参数写入
3. 请求 commit
4. 由系统验证是否保留 commit

如果 episode 没有成功 commit，实验写入会被恢复。

### Experiment Write

`experiment_write` 是一次实验性写入。

模型只提交目标和值，例如：

```json
{
  "target": {
    "kind": "sysctl",
    "key": "vm.dirty_ratio"
  },
  "value": "10",
  "reason": "reduce dirty page accumulation"
}
```

ActKernel 会自动：

- 读取写入前的 old value
- 写入新值
- 再次读取确认当前值
- 记录该 target 在本 episode 中实验过的 value

模型不需要，也不能提供 rollback command。

### Commit

commit 不是“保留当前系统状态”。

commit 的含义是：

```text
如果验证通过，只保留 keep_writes 中明确列出的 target/value。
```

如果实验阶段修改了 A、B、C，但 `keep_writes` 只包含 A，那么即使 commit 成功，B 和 C 也会被恢复。

## 安全边界

模型负责提出：

- 需要观察什么
- 要实验哪些参数
- commit 时希望保留哪些写入
- 如何采样验证指标
- 哪些指标应该改善
- 哪些指标不能退化

系统负责强制执行：

- 读写分离
- 结构化写入
- old value 捕获
- 自动恢复
- commit candidate 验证
- 只保留 `keep_writes`
- 系统级 guardrails
- 审计日志

核心不变量：

```text
没有通过 experiment_write 实验过的 target/value 不能出现在 keep_writes 中。
没有出现在 keep_writes 中的实验写入不会被保留。
```

## 运行

构建：

```bash
cargo build
```

复制示例配置：

```bash
cp tuning-agent.example.toml tuning-agent.toml
```

编辑 `tuning-agent.toml` 后启动 daemon：

```bash
./target/debug/tuning-agent --config tuning-agent.toml daemon
```

如果当前目录存在 `tuning-agent.toml`，也可以省略 `--config`：

```bash
./target/debug/tuning-agent daemon
```

发送激活事件：

```bash
cargo run -- --config tuning-agent.toml activate "diagnose current host performance" info cli
```

如果需要 root 权限运行 daemon，建议先构建再运行二进制：

```bash
cargo build
sudo ./target/debug/tuning-agent --config tuning-agent.toml daemon
```

避免直接 `sudo cargo run`，因为 root 环境中的 Cargo 版本可能无法解析当前 `Cargo.lock`。

## 配置

配置只来自 TOML 文件和默认值，不再读取环境变量。

优先级：

```text
显式 --config 文件
  > 当前目录 tuning-agent.toml
  > 默认值
```

如果显式指定 `--config`，但文件不存在或格式错误，程序会直接退出。

示例：

```toml
[llm]
base_url = "http://127.0.0.1:7001"
api_key = "123456"
model = "gpt-5.5"
timeout_ms = 30000

[activation]
socket_path = "/tmp/tuning-agent.sock"
# timer_interval_ms = 60000
# ebpf_ringbuf_pin = "/sys/fs/bpf/tuning_agent_events"

[audit]
path = "logs/audit.jsonl"

[command]
timeout_ms = 30000
output_limit_bytes = 65536
evaluation_output_limit_bytes = 8192

[evaluation]
default_window_seconds = 10
min_window_seconds = 3
max_window_seconds = 60
default_settle_seconds = 3
min_settle_seconds = 0
max_settle_seconds = 10
```

说明：当前 eBPF ringbuf 是接入点，真实 ringbuf reader 还需要明确 BPF object/map contract。

## 模型工具

### probe

执行只读诊断命令。

示例：

```json
{
  "name": "io_pressure",
  "command": "cat /proc/pressure/io",
  "timeout_ms": 1000,
  "working_dir": "/"
}
```

读命令会阻断明显副作用，例如：

- shell 写重定向
- `sysctl -w`
- `/proc/sys` 写入
- `kill` / `pkill`
- `mount` / `umount`
- `tc qdisc`
- `curl` / `wget` / `nc` / `ssh`
- 后台任务

### experiment_write

执行结构化实验写入。

示例：

```json
{
  "target": {
    "kind": "sysctl",
    "key": "vm.dirty_ratio"
  },
  "value": "10",
  "reason": "reduce dirty page accumulation"
}
```

支持的 target：

```json
{ "kind": "sysctl", "key": "vm.dirty_ratio" }
{ "kind": "proc_sys", "path": "/proc/sys/vm/dirty_ratio" }
{ "kind": "sysfs", "path": "/sys/..." }
{ "kind": "cgroup", "path": "/sys/fs/cgroup/..." }
```

### commit

请求验证并保留明确列出的写入。

示例：

```json
{
  "reason": "dirty_ratio reduction should reduce IO pressure",
  "keep_writes": [
    {
      "target": {
        "kind": "sysctl",
        "key": "vm.dirty_ratio"
      },
      "value": "10"
    }
  ],
  "measurement": {
    "command": "io=$(awk '/full/ {for(i=1;i<=NF;i++) if($i ~ /^avg10=/){split($i,a,\"=\"); print a[2]}}' /proc/pressure/io); printf '{\"io_full_avg10\":%s}\\n' \"$io\"",
    "schema": {
      "io_full_avg10": "number"
    },
    "timeout_ms": 1000
  },
  "primary_metrics": [
    {
      "metric": "io_full_avg10",
      "op": "decrease_percent_ge",
      "value": 10
    }
  ],
  "workload_invariants": [
    {
      "metric": "loadavg.1m",
      "op": "change_percent_le",
      "value": 50
    }
  ],
  "regression_guards": [
    {
      "metric": "psi.cpu.full.avg10",
      "op": "increase_abs_le",
      "value": 1
    }
  ],
  "window_seconds": 5,
  "settle_seconds": 1
}
```

`measurement.command` 必须输出单个 JSON object。系统会对 baseline A' 和 candidate B' 使用同一个 measurement command。

`window_seconds` 和 `settle_seconds` 是模型对本次 commit 的建议值。实际使用值会被 `[evaluation]` 中的 min/max 边界裁剪，模型不能绕过配置文件设置的验证时间范围。

## Metric Operator

支持：

```text
decrease_percent_ge
decrease_abs_ge
increase_percent_ge
increase_abs_ge
increase_percent_le
increase_abs_le
decrease_percent_le
decrease_abs_le
change_percent_le
change_abs_le
current_le
current_ge
```

## 内置系统防线

模型可以提供 `regression_guards`，但系统始终额外检查固定 guardrails：

```text
psi.cpu.full.avg10
psi.io.full.avg10
psi.memory.full.avg10
loadavg.1m
```

这些指标由系统采集，不依赖模型提供的 measurement。

## 开发检查

```bash
cargo fmt -- --check
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
```
