# scx 调度器性能测试框架

本项目用于对 Linux `sched_ext`（`scx`）调度器进行可复现的性能测试。

核心目标：

- 使用相同内核和相同机器矩阵运行 baseline / candidate 调度器；
- 自动收集 workload 指标、系统指标、内核指标和调度器诊断数据；
- 生成 baseline vs candidate 的分析结果和 HTML 报告。

## 当前能力

当前实现支持：

- 基于 `libvirt + KVM` 启动 VM；
- host 隔离环境检查；
- 一键生成本机配置、workload、base image 和 host 隔离配置；
- 配置化的 machine、suite、bench、metric profile 和 scheduler；
- baseline / candidate 交替运行；
- guest 内每次 run 的原始数据收集；
- 自动生成 `analysis.json` 和 `report.html`。

## 快速开始

拉取代码后，默认流程是：

```bash
git clone <repo>
cd scx_agent

python3 bench/scripts/prepare_env.py init --kernel-source ~/linux-6.18
sudo reboot
python3 bench/scripts/prepare_env.py verify

python3 bench/scripts/run.py \
  --plan smoke \
  --baseline default \
  --candidate scx_rlfifo
```

`prepare_env.py init` 会生成本机专属配置：

```text
bench/configs/local.config
```

`local.config` 不提交到 git。`run.py`、`isolation.py` 和
`fetch_workloads.py` 默认都使用它。

## 依赖

需要：

- Python 3
- PyYAML
- libvirt / QEMU / KVM：`virsh`、`qemu-img`、`ssh`、`scp`
- 可由 libvirt 直接启动的内核镜像
- 可通过 SSH 登录的 base qcow2 guest image
- 放在 `bench/workloads/` 下的 benchmark 程序
- 如果使用 `kind: scx`，需要放在 `bench/schedulers/` 下的调度器程序

第一版 `prepare_env.py init` 会检查这些依赖；缺依赖时默认尝试通过 apt 安装。
如果不希望脚本安装系统包，可以加 `--no-install-deps`。

## 拉取和构建 Workload

第一批集成的 workload：

```text
CPU throughput:
  hackbench
  stress-ng
  will-it-scale

IO unblock:
  fio

调度 / IPC:
  perf bench sched pipe
  perf bench sched messaging

tail latency:
  schbench
  cyclictest

综合构建:
  kernel build

真实服务场景:
  redis-server + redis-benchmark
```

拉取并构建：

```bash
python3 bench/scripts/fetch_workloads.py \
  hackbench schbench stress-ng fio redis rt-tests will-it-scale perf
```

`perf` 会根据配置文件中的 `libvirt.kernel_source` 从当前内核源码的
`tools/perf` 构建：

```yaml
libvirt:
  kernel_source: <kernel-source>
```

构建后的二进制会安装到：

```text
bench/workloads/bin/
```

源码会保存在：

```text
bench/workloads/src/
```

当前 wrapper：

```text
bench/benchmarks/hackbench.py
bench/benchmarks/schbench.py
bench/benchmarks/stress_ng.py
bench/benchmarks/fio.py
bench/benchmarks/redis.py
bench/benchmarks/perf_sched.py
bench/benchmarks/will_it_scale.py
bench/benchmarks/cyclictest.py
bench/benchmarks/kernel_build.py
```

通常不需要手动调用该脚本，`prepare_env.py init` 会自动调用。

## 配置文件

运行时默认配置文件是：

```text
bench/configs/local.config
```

模板配置文件是：

```text
bench/configs/example.config
```

`example.config` 不包含个人绝对路径，只作为 `prepare_env.py init` 生成
`local.config` 的模板。

顶层结构：

```text
libvirt         VM 内核、base image、SSH 和 libvirt 设置
executor         pair 并行、自动 CPU pinning 和 host 资源策略
schedulers       builtin 或 scx 调度器定义
plans            smoke / full 等测试计划
machines         VM CPU、内存、pinning、隔离要求
suites           benchmark 分组
metric_profiles  primary / secondary 指标和判定规则
benches          具体 workload wrapper 命令
```

调度器在配置文件中定义：

```yaml
schedulers:
  default:
    kind: builtin

  scx_simple:
    kind: scx
    command: bench/schedulers/scx_simple
    args: []
```

某次实验使用哪个 baseline / candidate 由命令行指定：

```bash
python3 bench/scripts/run.py \
  --plan smoke \
  --baseline default \
  --candidate scx_simple
```

baseline 和 candidate 都可以是 `scx` 调度器。

自动并行和 CPU pinning 由 `executor` 控制：

```yaml
executor:
  parallel: auto
  cpu_source: isolated
  isolated_cpus: "2-9"
  smt_policy: use_all_siblings
  pair_policy: sequential
  memory_guard_gb: 16

machines:
  small:
    vcpus: 2
    memory: 8G
    pin_cpus: auto
    exclusive: true
    frequency:
      fixed: true
```

`isolated_cpus` 必须包含完整的 SMT sibling group。运行时会以
comparison pair 为单位分配 CPU，同一个 physical core 的所有 logical
CPU sibling 只会分配给同一个 pair。

## Host 隔离环境

runner 在真实启动 VM 前会严格检查：

- pinned CPU 是否存在；
- pinned CPU 是否已经隔离；
- pinned CPU 是否固定频率。

如果检查失败，runner 会拒绝启动 VM。

查看当前隔离状态：

```bash
python3 bench/scripts/isolation.py status
```

预览将要修改的 host 设置：

```bash
python3 bench/scripts/isolation.py prepare \
  --dry-run
```

准备隔离环境：

```bash
sudo python3 bench/scripts/isolation.py prepare --no-reboot
```

通常不需要手动执行，`prepare_env.py init` 会调用它，然后用户手动
`sudo reboot`。

恢复原始 host 设置并自动重启：

```bash
sudo python3 bench/scripts/isolation.py restore
```

隔离脚本会保存状态到：

```text
/var/lib/scx-bench/isolation-state.json
```

它会修改 GRUB 启动参数，并安装一个 systemd service，用于重启后设置 pinned CPU 的固定频率。

当 machine 使用 `pin_cpus: auto` 时，隔离脚本使用
`executor.isolated_cpus` 作为需要隔离和固定频率的 CPU 范围。

## 运行实验

不启动 VM，仅检查流程和生成目录：

```bash
python3 bench/scripts/run.py \
  --plan smoke \
  --baseline default \
  --candidate scx_simple \
  --dry-run
```

运行真实实验：

```bash
python3 bench/scripts/run.py \
  --plan smoke \
  --baseline default \
  --candidate scx_simple
```

默认以 comparison pair 为基本单位运行：

```text
pair = 同一个 RunSpec 下的 baseline + candidate
```

pair 内 baseline/candidate 串行执行，pair 之间可以并行。默认执行顺序是交替：

```text
run_index 1: baseline -> candidate
run_index 2: candidate -> baseline
run_index 3: baseline -> candidate
```

也可以使用顺序运行：

```bash
python3 bench/scripts/run.py \
  --plan smoke \
  --baseline default \
  --candidate scx_simple \
  --order sequential
```

控制 pair 并行度：

```bash
python3 bench/scripts/run.py \
  --plan smoke \
  --baseline default \
  --candidate scx_simple \
  --parallel auto
```

`--parallel auto` 会根据已隔离 CPU、SMT sibling group、VM 内存和
`memory_guard_gb` 自动决定哪些 pair 可以同时运行。

## 结果目录

完整实验默认保存到：

```text
bench/results/experiments/
  <timestamp>__<baseline>_vs_<candidate>/
```

目录结构：

```text
metadata.json

runs/
  <baseline_scheduler>/
    manifest.json
    run_001__machine_...__suite_...__bench_.../
      result.json
      bench_metrics.json
      stdout.log
      stderr.log
      workload_stdout.log
      workload_stderr.log
      scheduler_stdout.log
      scheduler_stderr.log
      libvirt_stdout.log
      libvirt_stderr.log
      domain.xml
      placement.json
      disk.qcow2
      guest_result.json
      run_guest.sh
      snapshots/

  <candidate_scheduler>/
    manifest.json
    run_001__machine_...__suite_...__bench_.../
      ...

analysis/
  metadata.json
  analysis.json
  report.html
```

数据类型对应关系：

```text
每次 run 的原始数据：
  runs/<scheduler>/run_.../

profile 后的 comparison 数据：
  analysis/analysis.json

最终 HTML 报告：
  analysis/report.html
```

## Workload Wrapper

社区 benchmark 程序放在：

```text
bench/workloads/
```

框架实际运行的是 wrapper：

```text
bench/benchmarks/
```

当前通用 wrapper 是：

```text
bench/benchmarks/generic.py
```

它会运行真实 workload，保存原始输出，并输出统一 JSON：

```json
{
  "metrics": {
    "elapsed_time_sec": 1.23
  },
  "metadata": {
    "wrapper": "generic",
    "returncode": 0
  },
  "raw": {
    "stdout_path": "...",
    "stderr_path": "..."
  }
}
```

对于正式测试，建议为不同工具编写专用 wrapper，例如：

```text
bench/benchmarks/fio.py
bench/benchmarks/schbench.py
bench/benchmarks/cyclictest.py
```

专用 wrapper 应解析工具原生输出，并输出稳定的指标名，例如：

```text
iops
throughput
p99_latency_us
p999_latency_us
elapsed_time_sec
```

## 单独重新分析

如果已有两组结果目录，可以只重新运行分析：

```bash
python3 -m bench.analysis.run \
  --baseline bench/results/experiments/<id>/runs/default \
  --candidate bench/results/experiments/<id>/runs/scx_simple \
  --output /tmp/scx-analysis
```

输出：

```text
metadata.json
analysis.json
report.html
```

## 注意事项

- `--baseline` 只是比较基准，不代表必须是内核默认调度器。
- baseline 和 candidate 都可以是 `builtin` 或 `scx`。
- libvirt XML 会显式设置 `vcpupin` 和 `emulatorpin`。
- host CPU 隔离和固定频率需要先通过 `bench/scripts/isolation.py` 准备。
- runner 会在真实运行前做严格 preflight 检查。
- run.py 以 comparison pair 为单位调度，资源允许时可以并发运行多个 VM。
