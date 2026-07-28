# 性能测试框架

本框架用于对 `tuning-agent`、`scx_agent_classed` 及其他 Linux 调度器进行可复现的性能测试。默认验证环境为 openEuler 24.03 LTS SP4 的 6.6 内核；Linux 6.18 可作为对照内核。

## 目标

- 在相同内核和机器矩阵上比较 baseline / candidate scheduler-treatment 变体；
- 自动采集 workload、系统、内核和调度器诊断数据；
- 生成机器可读的分析结果和 HTML 报告。

## 当前能力

- 使用 `libvirt + KVM` 创建、运行和清理测试 VM；
- 通过配置定义 machine、suite、bench、metric profile、scheduler 和 treatment；
- 初始化本机配置、workload、base image 和 host 隔离环境；
- 在运行前验证 CPU 隔离、固定频率及 IRQ/RPS/XPS 分配；
- 交替或顺序执行 baseline / candidate comparison pair；
- 保存每次 run 的原始证据，并生成 `analysis.json`、配对统计和 `report.html`。

## 快速开始

以下流程使用已解包并构建好的 openEuler 24.03 SP4 内核。内核构建、依赖安装和 host 隔离说明见 [ENVIRONMENT.md](ENVIRONMENT.md)。

```bash
git clone <repo>
cd scx_agent

KNEVRA=6.6.0-157.0.0.149.20260612.5ba33eb06623.oe2403sp4.x86_64
KPROFILE="$HOME/kernels/oe2403sp4-$KNEVRA"
KSRC="$KPROFILE/source/usr/src/linux-$KNEVRA"
KBUILD="$KPROFILE/build"
CONFIG=bench/configs/local_profiles/oe2403sp4_6_6_scx

python3 -m bench.env init \
  --template "$CONFIG" \
  --config "$CONFIG" \
  --kernel-source "$KSRC" \
  --kernel-image "$KPROFILE/assets/bzImage" \
  --kernel-config "$KBUILD/.config" \
  --kernel-id oe2403sp4_6_6_scx \
  --root-image /var/lib/libvirt/scx-bench-runs/scx-agent-classed-oe2403sp4-6.6-scx-base.qcow2 \
  --sync-kernel-source \
  --force

sudo reboot
python3 -m bench.env verify --config "$CONFIG"
```

### 场景一：调度器与真实 LLM

在本地 profile 的 `plan.config` 中填写
`SCX_TUNING_AGENT_LLM_BASE_URL/API_KEY/MODEL`，再运行 EEVDF A/A 和真实 LLM 实验：

```bash
bash bench/scripts/run_oe2403sp4_real_llm.sh --preflight-only
bash bench/scripts/run_oe2403sp4_real_llm.sh --group-gate
bash bench/scripts/run_oe2403sp4_real_llm.sh
```

脚本按配置直接访问 OpenAI-compatible endpoint。API key
会进入本地 profile、Guest plan 和实验结果；发布结果前应注销该 key。完整说明见
[RUNBOOK.md](RUNBOOK.md)。

`bench.env init` 会根据模板更新本机内核、libvirt、SSH 和 CPU 隔离信息，同时保留 scheduler、treatment 和测试矩阵定义：

```text
bench/configs/local_profiles/oe2403sp4_6_6_scx/
  environment.config
  benches.config
  plan.config
```

### 场景二：Redis CPU 调优

Redis CPU 分为 demo 与 research：demo 从 weight 50 的已知欠配状态开始，用于展示稳定
commit；research 从 100:100 公平状态开始，用于评估真实模型决策。以下默认初始化 demo。

首次运行时，以 Redis 场景配置生成 openEuler profile。这里复用场景一已经生效的 host 隔离，只准备 Redis workload 和独立 base image：

```bash
REDIS_TEMPLATE=bench/configs/redis_cpu_demo
REDIS_CONFIG=bench/configs/local_profiles/oe2403sp4_6_6_redis_cpu_demo
REDIS_PLAN=redis_cpu_demo

cargo build --locked --release \
  --manifest-path tuning_agent/Cargo.toml

python3 -m bench.env init \
  --template "$REDIS_TEMPLATE" \
  --config "$REDIS_CONFIG" \
  --kernel-source "$KSRC" \
  --kernel-image "$KPROFILE/assets/bzImage" \
  --kernel-config "$KBUILD/.config" \
  --kernel-id oe2403sp4_6_6_redis_cpu_demo \
  --root-image /var/lib/libvirt/scx-bench-runs/redis-cpu-oe2403sp4-6.6-base.qcow2 \
  --workloads schbench stress-ng perf redis \
  --skip-isolation \
  --force

python3 -m bench.env verify --config "$REDIS_CONFIG"
```

若 Redis 是该 host 上的首个测试场景，去掉 `--skip-isolation`，并在初始化后重启。

在 Redis 本地 profile 的 `plan.config` 中填写 LLM endpoint、API key 和 model。入口会在
创建 VM 前执行真实 tool-call preflight；性能、置信区间、运行状态和提交次数统一写入 HTML：

```bash
python3 -m bench.scenarios.redis_cpu.run \
  --config "$REDIS_CONFIG" --plan "$REDIS_PLAN"
```

运行 research 时改用 `bench/configs/redis_cpu_tuning`、现有
`oe2403sp4_6_6_redis_cpu` profile 和 `redis_cpu_smoke` plan。两个场景都使用真实模型，
不提供 Mock LLM。完整判定见 [tuning-agent 集成说明](integrations/tuning_agent/README.md#redis-cpu-场景)。

## 环境依赖

- Python 3 和 PyYAML；
- libvirt、QEMU 和 KVM，包括 `virsh`、`qemu-img`、`ssh` 和 `scp`；
- 可由 libvirt 直接启动的内核镜像；
- 可通过 SSH 登录的 base qcow2 guest image；
- `bench/workloads/` 下的 benchmark 程序；
- Cargo 工具链；使用 `kind: scx` 时需先构建相应 scheduler。

`bench.env init` 会检查依赖，并默认尝试通过 apt 安装缺失的系统包。使用 `--no-install-deps` 可关闭自动安装。

构建 tuning-agent、自定义 scheduler 和 MCP adapter：

```bash
cargo build --locked --release \
  --manifest-path tuning_agent/Cargo.toml
cargo build --locked --release \
  --manifest-path schedule/scx_agent_classed/Cargo.toml
cargo build --release \
  --manifest-path schedule/scx_agent_classed_mcp/Cargo.toml
```

## 配置文件

配置入口是一个包含以下三个文件的目录，三个文件缺一不可：

| 文件 | 顶层配置 |
| --- | --- |
| `environment.config` | `libvirt`、`executor`、`machines` |
| `benches.config` | `bench_defaults`、`metric_profiles`、`suites`、`benches` |
| `plan.config` | `schedulers`、`treatments`、`plans` |

所有模板均面向 openEuler 24.03 SP4 / 6.6；Redis demo 与 research 使用独立 profile。
字段格式、执行阶段和完整示例见 [CONFIGURATION.md](CONFIGURATION.md)。

## 文档

- [ENVIRONMENT.md](ENVIRONMENT.md)：内核构建、环境初始化、workload 和 host 隔离；
- [CONFIGURATION.md](CONFIGURATION.md)：三类配置文件的格式与语义；
- [RUNBOOK.md](RUNBOOK.md)：实验运行、结果目录、内核比较和重新分析；
- [integrations/tuning_agent/README.md](integrations/tuning_agent/README.md)：tuning-agent adapter 和真实 LLM 流程；
- [ARCHITECTURE.md](ARCHITECTURE.md)：模块边界、数据流和扩展契约。
