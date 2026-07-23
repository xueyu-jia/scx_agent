# scx 调度器性能测试框架

本项目用于对 Linux `sched_ext`（`scx`）调度器进行可复现的性能测试。

核心目标：

- 使用相同内核和相同机器矩阵运行 baseline / candidate scheduler-treatment 变体；
- 自动收集 workload 指标、系统指标、内核指标和调度器诊断数据；
- 生成 baseline vs candidate 的分析结果和 HTML 报告。

## 当前能力

当前实现支持：

- 基于 `libvirt + KVM` 启动 VM；
- host 隔离环境检查；
- 一键生成本机配置、workload、base image 和 host 隔离配置；
- 配置化的 machine、suite、bench、metric profile 和 scheduler；
- baseline / candidate 变体交替运行；
- guest 内每次 run 的原始数据收集；
- 自动生成 `analysis.json` 和 `report.html`。

## 快速开始

拉取代码后，默认流程是：

```bash
git clone <repo>
cd scx_agent

python3 -m bench.env init --kernel-source ~/linux-6.18
sudo reboot
python3 -m bench.env verify

python3 bench/scripts/run.py \
  --plan smoke \
  --baseline default \
  --candidate scx_agent_classed
```

`bench.env init` 会生成本机专属配置：

```text
bench/configs/local_config/
  environment.config
  benches.config
  plan.config
```

它也会通过 `env/libvirt.py` 备份并修改 `/etc/libvirt/qemu.conf`，让 QEMU
以当前测试用户运行，避免 `run.py` 读取 VM runtime 文件时需要 sudo。
恢复所有由初始化流程管理的 host 设置：

```bash
python3 -m bench.env restore
```

`local_config/` 不提交到 git。`run.py` 和 `bench.env` 默认都使用这个目录入口。

## openEuler 24.03 SP4 / 6.6 迁移

下面的流程对应已验证的构建：

```text
kernel 6.6.0-157.0.0.149.20260612.5ba33eb06623.oe2403sp4.x86_64
runtime 6.6.0-oe2403sp4-157.149-scx
profile bench/configs/local_profiles/oe2403sp4_6_6_scx
```

该 profile 通过 libvirt direct boot 替换内核，但保留现有 Ubuntu 22.04 guest
userspace，以便只改变 kernel 做可比测试。因此结果代表 openEuler kernel 的表现，
不代表完整 openEuler userspace/发行版栈；后者应使用单独的 openEuler root image。

`kernel-*.rpm` 是二进制安装包，不包含完整编译树。迁移时必须同时取得同一
NEVRA 的 `kernel-source-*.rpm`。在 Ubuntu host 上先安装 `rpm2cpio`、`cpio`、
内核构建依赖、`pahole`、libelf 和 LLVM/GCC 工具链，再解包两个 RPM：

```bash
KNEVRA=6.6.0-157.0.0.149.20260612.5ba33eb06623.oe2403sp4.x86_64
PROFILE="$HOME/kernels/oe2403sp4-$KNEVRA"
SOURCE_RPM="$HOME/kernel-source-$KNEVRA.rpm"
BINARY_RPM="$HOME/kernel-$KNEVRA.rpm"

mkdir -p "$PROFILE/source" "$PROFILE/binary" "$PROFILE/build" "$PROFILE/assets"
(cd "$PROFILE/source" && rpm2cpio "$SOURCE_RPM" | cpio -idm --quiet)
(cd "$PROFILE/binary" && rpm2cpio "$BINARY_RPM" | cpio -idm --quiet)

KSRC="$PROFILE/source/usr/src/linux-$KNEVRA"
KBUILD="$PROFILE/build"
cp "$PROFILE/binary/boot/config-$KNEVRA" "$PROFILE/assets/vendor.config"
cp "$PROFILE/assets/vendor.config" "$KBUILD/.config"
```

直接用 libvirt 启动 `bzImage` 时，根文件系统、virtio block/network 不能只编译为
模块。启用 sched_ext 和可复现性所需配置：

```bash
"$KSRC/scripts/config" --file "$KBUILD/.config" \
  --enable SCHED_CLASS_EXT \
  --enable IKHEADERS \
  --enable VIRTIO_BLK \
  --enable VIRTIO_NET \
  --enable SCSI_VIRTIO \
  --enable EXT4_FS \
  --set-str LOCALVERSION -oe2403sp4-157.149-scx \
  --set-str SYSTEM_TRUSTED_KEYS '' \
  --set-str SYSTEM_REVOCATION_KEYS ''

make -C "$KSRC" O="$KBUILD" olddefconfig
make -C "$KSRC" O="$KBUILD" -j"$(nproc)" bzImage
cp "$KBUILD/arch/x86/boot/bzImage" "$PROFILE/assets/bzImage"
cp "$KBUILD/.config" "$PROFILE/assets/kernel.config"
```

`scx_agent_classed` 有一项源码兼容：remote steal 使用普通固定上限循环，避免
6.6 verifier 对 `bpf_for` 展开超过 1,000,000 条处理指令；扫描上限和调度语义不变。

为每个内核使用独立 config、kernel image 和 root image，不能覆盖 Linux 6.18
profile。框架支持外置 build 目录、独立 kernel config 和源码同步：

```bash
python3 -m bench.env init \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --kernel-source "$KSRC" \
  --kernel-image "$PROFILE/assets/bzImage" \
  --kernel-config "$KBUILD/.config" \
  --kernel-id oe2403sp4_6_6_scx \
  --root-image /var/lib/libvirt/scx-bench-runs/scx-bench-oe2403sp4-base.qcow2 \
  --sync-kernel-source

python3 -m bench.env verify \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx
```

先运行单次迁移门禁，再运行正式多次性能测试：

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan kernel_migration_smoke \
  --baseline default \
  --candidate scx_agent_classed \
  --parallel 1

python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan full \
  --baseline default \
  --candidate scx_agent_classed \
  --parallel 1
```

上面的 paired run 比较同一 openEuler 内核下的 scheduler。比较 Linux 6.18 与
openEuler 时，必须在两个 profile 中用相同 scheduler、plan、CPU pinning、VM
规格和运行次数分别执行，再离线比较结果：

```bash
python3 bench/scripts/run.py \
  --config bench/configs/local_config \
  --plan full --scheduler default \
  --output bench/results/kernel_compare/linux_6_18_default

python3 bench/scripts/run.py \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --plan full --scheduler default \
  --output bench/results/kernel_compare/oe2403sp4_6_6_default

python3 -m bench.analysis.run \
  --baseline bench/results/kernel_compare/linux_6_18_default \
  --candidate bench/results/kernel_compare/oe2403sp4_6_6_default \
  --output bench/results/kernel_compare/default_analysis
```

每个 `result.json` 和 `system_metadata.json` 都记录实际 `uname`、`/proc/version`、
sched_ext 状态、BTF 是否存在、关键 config 和 `/proc/config.gz` SHA256。分析前应先
确认两个结果目录的 `system.release` 分别是预期的 6.18 和 openEuler 6.6。

## 依赖

需要：

- Python 3
- PyYAML
- libvirt / QEMU / KVM：`virsh`、`qemu-img`、`ssh`、`scp`
- 可由 libvirt 直接启动的内核镜像
- 可通过 SSH 登录的 base qcow2 guest image
- 放在 `bench/workloads/` 下的 benchmark 程序
- 如果使用 `kind: scx`，需要先构建对应的 Cargo scheduler

`bench.env init` 会检查这些依赖；缺依赖时默认尝试通过 apt 安装。
如果不希望脚本安装系统包，可以加 `--no-install-deps`。

自定义调度器和 MCP adapter 是两个独立的 Cargo 项目，分别构建：

```bash
cargo build --release --manifest-path schedule/scx_agent_classed/Cargo.toml
cargo build --release --manifest-path schedule/scx_agent_classed_mcp/Cargo.toml
```

对应产物位于各自项目的 `target/release/`，`example_config/` 不再从
其他源码目录查找这两个自定义产物。`scx_agent_classed` 使用的
`scx_stats`、`scx_stats_derive`、`scx_utils` 和构建依赖 `scx_cargo` 均在其
`Cargo.toml` 中直接依赖官方 `https://github.com/sched-ext/scx.git`。依赖来源是
`main` 主线，当前固定到 commit
`96e4f928a2d3c84170548f0b552705544f27f2b2`，由 Cargo 下载到用户缓存；仓库不包含
上游 scx 源码副本。固定 SHA 与 `--locked` 确保构建不会随远端 main 自动漂移：

```bash
cargo build --locked --release \
  --manifest-path schedule/scx_agent_classed/Cargo.toml
```

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
python3 -m bench.env workloads \
  hackbench schbench stress-ng fio redis rt-tests will-it-scale perf bpftool
```

`perf` 和 `bpftool` 会根据配置文件中的 `libvirt.kernel_source` 从当前内核源码的
`tools/perf`、`tools/bpf/bpftool` 构建：

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
bench/benchmarks/mixed_class.py
```

`mixed_class.py` runs schbench while `stress-ng-cpu` workers saturate the VM.
It reports separate schbench wakeup/request percentiles alongside batch
throughput from the same mixed run, making it useful for validating
workload-class isolation and starvation behavior.

通常不需要手动执行，`bench.env init` 会自动准备 workload。

benchmark wrapper 会随仓库一起固化到 base image，不会在每次 run 时覆盖。
修改、增加或删除 `bench/benchmarks/` 下的文件后，必须重建镜像：

```bash
python3 -m bench.env rebuild-image
python3 -m bench.env verify
```

构建完成后会在 qcow2 旁写入
`<root_image>.scx-bench-manifest.json`，记录镜像 identity 和整个 wrapper
目录的逐文件 SHA256。写入 manifest 前，base-init VM 会在 guest 内重新计算
wrapper 哈希并确认与宿主构建快照一致。`verify` 和非 dry-run 的 `run.py` 都会
比较该 manifest；镜像被替换、manifest 缺失或任一 wrapper 发生变化时，实验会在
创建 VM 前拒绝运行。`rebuild-image` 只使用现有 `local_config/` 重建镜像，不会覆盖
其中的 plan、scheduler 或 machine 配置。

## 配置文件

运行时默认配置入口是：

```text
bench/configs/local_config/
  environment.config  # libvirt / executor / machines
  benches.config      # bench_defaults / metric_profiles / suites / benches
  plan.config         # schedulers / treatments / plans
```

模板配置入口是：

```text
bench/configs/example_config/
```

`example_config/` 不包含个人绝对路径，只作为 `bench.env init` 生成
`local_config/` 的模板。配置加载器只接受目录；三个 part 缺一不可，且顶层 key
必须位于上面标明的职责文件中。

顶层结构：

```text
libvirt         VM 内核、base image、SSH 和 libvirt 设置
bench_defaults  benchmark 默认 post-warmup settle / cooldown 设置
executor         pair 并行、自动 CPU pinning 和 host 资源策略
schedulers       builtin 或 scx 调度器定义
treatments       measurement 前建立实验状态的可选处理命令
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
    host_command: bench/schedulers/scx_simple
    args: []
    settle_seconds: 2
```

`command` is the path used inside the guest. When `host_command` is present,
the runner copies that host executable into each fresh VM overlay and executes
the staged copy instead, ensuring the run uses the current build rather than a
binary baked into the base image. Relative `host_command` paths are resolved
from the repository root.

For schedulers with libbpf kconfig externs on a kernel without
`CONFIG_IKCONFIG`, set `host_kconfig` and pass the staged path explicitly:

```yaml
    host_kconfig: /path/to/linux/.config
    args: [--kconfig, /tmp/scx-bench-kconfig]
```

Treatment 是独立于 scheduler 和 workload warmup 的可选阶段。它在 scheduler
settle 后运行，用于建立随后要测量的系统状态，例如运行训练负载、启动 tuning
agent、等待 episode 结束并验证 committed candidate。普通 warmup 仍在 treatment
之后运行，只负责预热正式 benchmark：

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

Treatment 在顶层独立定义，因而 baseline/candidate 可以使用同一个 scheduler，
只改变 treatment：

```yaml
treatments:
  control:
    command: /usr/local/bin/control-treatment
    args: []
    timeout_seconds: 600
    post_treatment_settle_seconds: 5

  agent_tuned:
    command: /tmp/scx-bench-treatment
    host_command: bench/integrations/tuning_agent/adapter.py
    host_support_files:
      - bench/integrations/tuning_agent/mock_llm.py
      - bench/integrations/tuning_agent/deterministic_mcp.py
    args: [--no-commit-disposition, proceed]
    timeout_seconds: 900
    post_treatment_settle_seconds: 5
    env:
      MODE: tune
```

`command` 是 base image 内的 guest 路径。可选 `host_command` 与 scheduler 的
同名字段语义一致：runner 将当前 host 上的可执行 treatment 文件复制到每个
fresh overlay，记录 SHA-256，并执行 staged copy；相对路径从仓库根目录解析。
这适合频繁修改的自定义 treatment harness，也避免误用 base image 中的旧脚本。
可选 `host_support_files` 会复制到 guest 的 `/tmp/scx-bench-treatment.d/`，
用于随 harness 携带 mock LLM、MCP server 或辅助脚本；它们不会进入 guest
execution plan，避免把 host-only 路径暴露给 guest executor。
Treatment 配置和 `env` 会进入 `guest_plan.json`、manifest 与结果元数据，不能
放置 API key 等秘密；秘密应通过不进入结果归档的受控凭据文件或本地代理提供。

guest executor 只向 treatment 注入 run context，避免 benchmark warmup 或正式
measurement 根据 baseline/candidate 身份分支：

```text
SCX_BENCH_ROLE       baseline | candidate | standalone
SCX_BENCH_VARIANT    <scheduler> 或 <scheduler>__<treatment>
SCX_BENCH_TREATMENT  treatment 名；未配置时为空
SCX_BENCH_OUT        当前阶段的产物目录
SCX_BENCH_WORKDIR    guest 内的仓库工作目录
```

Treatment 还会收到 `SCX_BENCH_TREATMENT_OUTCOME`。命令退出前必须在该路径
原子写入不超过 64 KiB 的 JSON：

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

`version`、`disposition`、`reason`、`details` 都必须存在。`reason` 必须只包含
非空的 `code` 和 `message`，`details` 可以是空对象；未知字段会被拒绝。
Bench Core 只解释 `disposition`，不会根据 `reason.code` 或 `details` 分支：

- `proceed`：treatment 已验证可测量状态，进入 post-treatment settle、warmup 和
  measurement；phase status 记录为 `PROCEEDED`。
- `stop`：系统状态安全，但本次 run 按 treatment 策略不应测量；run status 为
  `TREATMENT_STOPPED`，phase status 为 `STOPPED`。
- `unsafe`：系统状态不安全或无法验证，必须阻断测量；run status 为
  `TREATMENT_UNSAFE_STATE`，phase status 为 `UNSAFE`。

`stop` 和 `unsafe` 都以 125 结束 guest executor，并跳过后续阶段，但语义不同：
前者是有意的安全停止，后者表示无法证明测量安全。命令非零退出、缺少或损坏
outcome、或残留进程产生 `TREATMENT_FAILED`；超时产生 `TREATMENT_TIMEOUT`。
即使命令已经写出 `proceed`，非零退出或残留进程仍优先判为失败。

Treatment 自己启动的训练负载、agent daemon 和 MCP 子进程必须在写 outcome
之前全部停止并等待退出。

### tuning-agent adapter

仓库提供 `bench/integrations/tuning_agent/adapter.py`，负责 Cgroup-scoped
tuning-agent 领域流程和私有状态到通用 outcome 的转换。它会：

- 创建临时 cgroup，并把可选训练负载放入该 cgroup；
- 启动 mock/真实 LLM 代理等 support process；
- 启动 tuning-agent daemon；
- 执行 `tuning-agent activate --wait --json ... <cgroup_path>`；
- 停止并等待全部辅助进程，验证保留的 cgroup 与 CPU 状态；
- 原子写入 Outcome V2。

私有状态映射如下：

```text
committed + state verified                 -> proceed
no_commit + baseline verified + ITT policy -> proceed
no_commit + baseline verified + strict     -> stop
no_commit + baseline unverified             -> unsafe
recovery_required                           -> unsafe
```

`--no-commit-disposition proceed|stop` 是 adapter 的领域策略；
`recovery_required -> unsafe` 不可配置。激活响应、验证证据和私有状态保存在
`details` 中供审计，Bench Core 不解释这些内容。

确定性测试可以使用 `bench/integrations/tuning_agent/mock_llm.py` 和
`bench/integrations/tuning_agent/deterministic_mcp.py`。场景由
`SCX_DETERMINISTIC_SCENARIO` 控制：
`positive` 应 commit，`no_signal` 和 `unsafe` 应 rollback 为 no_commit，
`recovery` 应阻断正式 warmup/measurement。

真实 workload MCP 接入时，保持同一边界：Agent 内部训练负载和 A/B evaluate
只用于决定是否 commit；bench 的正式 warmup/measurement 是 held-out 外部测量。
benchmark 自有的 `host_support_files` 会独立复制到
`/tmp/scx-bench-workload.d/`，不能依赖 treatment staging：

```yaml
benches:
  cgroup_cpu_share:
    host_support_files:
      - bench/scenarios/cgroup_cpu/workload.py
      - bench/scenarios/cgroup_cpu/common.py
    measurement:
      command: python3
      args: [/tmp/scx-bench-workload.d/workload.py]
      timeout_seconds: 30
```

完整的 cgroup CPU 场景位于 `bench/configs/cgroup_cpu_tuning/`，本地配置
完成初始化后可运行 paired matrix：

```bash
python3 -m bench.scenarios.cgroup_cpu.matrix \
  --config bench/configs/local_config \
  --plan cgroup_cpu_smoke
```

VM 与调度器准备阶段使用独立的 settle 时间；workload warmup 必须显式配置
命令。warmup 成功后等待 `post_warmup_settle_seconds`，再采集 before
snapshot 并启动正式测量：

```yaml
libvirt:
  vm_settle_seconds: 10

bench_defaults:
  post_warmup_settle_seconds: 2
  cooldown_seconds: 1

benches:
  schbench_latency:
    env: {}
    warmup:
      command: python3
      args: [bench/benchmarks/schbench.py, --, -m, "4", -t, "16", -r, "10"]
      timeout_seconds: 30
    measurement:
      command: python3
      args: [bench/benchmarks/schbench.py, --, -m, "4", -t, "16", -r, "60"]
      timeout_seconds: 120
```

`measurement` 必填，`warmup` 可选；两者都使用结构化的 `command`、可选
`args` 和必填 `timeout_seconds`。warmup 与正式测量共享 benchmark `env`，
但 warmup 的 `SCX_BENCH_OUT` 指向独立的 `warmup/` 目录，因此它生成的
wrapper 输出和 `perf_stat.csv` 不会进入正式测量结果。

runner 为每次 run 生成 `guest_plan.json`，再上传固定的 Python guest
executor。treatment、warmup 与 measurement 的 timeout 都在 guest 内执行；
超时、非零退出或残留进程会被记录为明确状态并清理整个进程组。前置阶段失败
时不会启动 measurement。总 host timeout 包含所有已启用命令、settle/cooldown
阶段及额外余量。dry-run 的 `result.json` 与 `manifest.json` 都保存同一份
`execution_plan`。

某次实验使用哪个 baseline / candidate 由命令行指定：

```bash
python3 bench/scripts/run.py \
  --plan smoke \
  --baseline default \
  --candidate scx_simple
```

baseline 和 candidate 都可以是 `scx` 调度器。

要比较同一个 scheduler 在 control 与 Agent 调优后的表现：

```bash
python3 bench/scripts/run.py \
  --plan smoke \
  --baseline scx_agent_classed \
  --candidate scx_agent_classed \
  --baseline-treatment control \
  --candidate-treatment agent_tuned
```

没有 treatment 时，原有 scheduler 对比命令和结果目录保持不变。指定
treatment 后，运行变体标签为 `<scheduler>__<treatment>`；两侧必须选择不同
的 scheduler/treatment 组合。

自动并行和 CPU pinning 由 `executor` 控制：

```yaml
executor:
  parallel: auto
  cpu_source: isolated
  isolated_cpus: "2-9"
  irq_cpus: "0-1"
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

`irq_cpus` 是 host 设备 IRQ、RPS 和 XPS 的目标 CPU 集合，必须和
VM pinned CPU 不重叠。`isolation.py prepare/apply-runtime` 会把
`/proc/irq/*/smp_affinity_list` 和 `/sys/class/net/*/queues/*/*ps_cpus`
写到这组 CPU，并在 restore 时恢复原值。

如果内核拒绝迁移某个 IRQ，例如 managed IRQ，`apply-runtime` 会把它
记录为 `unmovable`，不会在启动 VM 前直接失败。runner 会在 guest
脚本执行前后比较 host `/proc/interrupts`；只要 unmovable IRQ 在 VM
pinned CPU 上产生增量，该 run 会被标记为 `INTERRUPT_CONTAMINATED`。
当前 managed IRQ 策略固定为 `fail_on_delta`。

## Host 隔离环境

runner 在真实启动 VM 前会严格检查：

- pinned CPU 是否存在；
- pinned CPU 是否已经隔离；
- pinned CPU 是否固定频率。
- 可迁移 IRQ、RPS 和 XPS 是否仍包含 pinned CPU。

如果检查失败，runner 会拒绝启动 VM。

查看当前隔离状态：

```bash
python3 -m bench.env isolation status
```

预览将要修改的 host 设置：

```bash
python3 -m bench.env isolation prepare \
  --dry-run
```

准备隔离环境：

```bash
sudo python3 -m bench.env isolation prepare --no-reboot
```

通常不需要手动执行，`bench.env init` 会调用它，然后用户手动
`sudo reboot`。

恢复原始 host 设置并自动重启：

```bash
sudo python3 -m bench.env isolation restore
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
  <baseline_variant>/
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
      treatment/
        stdout.log
        stderr.log
        outcome.json
      warmup/
        stdout.log
        stderr.log
      libvirt_stdout.log
      libvirt_stderr.log
      domain.xml
      placement.json
      disk.qcow2
      guest_plan.json
      guest_result.json
      snapshots/

  <candidate_variant>/
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

输出同时包含 HTML 比较和按 `run_index` 的配对统计：

```text
metadata.json
analysis.json
report.html
paired/pairs.csv
paired/summary.csv
```

正式的交替顺序实验会按相同 `run_index` 配对，而不是只比较两组独立均值。
聚合比较与配对比较复用同一组 result loader、metric profile 和 comparison group，
并统一写入 `analysis.json` 与 HTML；`pairs.csv` 和 `summary.csv` 是同一分析模型的
表格化导出。分析层只读取 `bench_metrics.json` 中已经规范化的指标，不解析 workload
日志。置信区间是配对变化均值的 95% Student-t 区间。

## 注意事项

- `--baseline` 只是比较基准，不代表必须是内核默认调度器。
- baseline 和 candidate 都可以是 `builtin` 或 `scx`。
- 两侧可以选择同一个 scheduler，但必须通过不同 treatment 形成不同运行变体。
- libvirt XML 会显式设置 `vcpupin`、`emulatorpin` 和可选的
  `iothreadpin`；启用 `pin_vhost_threads` 时，runner 会记录并尽量 pin
  host vhost 线程。
- host CPU 隔离和固定频率需要先通过 `python3 -m bench.env isolation` 准备。
- runner 会在真实运行前做严格 preflight 检查。
- run.py 以 comparison pair 为单位调度，资源允许时可以并发运行多个 VM。
