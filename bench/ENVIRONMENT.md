# 测试环境

本文说明默认 openEuler 环境的内核准备、host 初始化、workload 构建和隔离管理。配置字段见 [CONFIGURATION.md](CONFIGURATION.md)，实验命令见 [RUNBOOK.md](RUNBOOK.md)。

## 已验证环境

```text
kernel  6.6.0-157.0.0.149.20260612.5ba33eb06623.oe2403sp4.x86_64
runtime 6.6.0-oe2403sp4-157.149-scx
profile bench/configs/local_profiles/oe2403sp4_6_6_scx
```

该 profile 通过 libvirt direct boot 替换内核，但沿用 Ubuntu 22.04 guest userspace，以便只改变 kernel。测试结果代表 openEuler kernel，不代表完整的 openEuler 发行版栈；测试后者需要独立的 openEuler root image。

每个内核必须使用独立的 config、kernel image 和 root image，不能在不同 profile 间共享这些可变资产。

## 依赖

Host 需要：

- Python 3、PyYAML；
- libvirt、QEMU、KVM、`virsh` 和 `qemu-img`；
- SSH 和 SCP；
- 内核构建工具链，包括 `rpm2cpio`、`cpio`、`pahole`、libelf 和 LLVM/GCC；
- Cargo 工具链，用于构建 `scx` scheduler 和 MCP adapter。

`bench.env init` 会检查依赖，并默认尝试通过 apt 安装缺失的系统包。使用 `--no-install-deps` 可禁用自动安装。

## 准备 openEuler 内核

`kernel-*.rpm` 不包含完整编译树，必须同时取得相同 NEVRA 的 `kernel-source-*.rpm`：

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

libvirt direct boot 要求根文件系统和 virtio block/network 驱动内建到内核。启用 `sched_ext` 和实验所需配置后构建：

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

openEuler 配置包含 `CONFIG_PSI_DEFAULT_DISABLED=y`，因此 `libvirt.kernel_args` 必须包含 `psi=1`。否则 guest 不会创建 `/proc/pressure/`，tuning-agent A/B evaluation 会在 commit 前回滚。

`scx_agent_classed` 针对 6.6 verifier 使用固定上限的 remote-steal 循环，避免 `bpf_for` 展开超过 1,000,000 条处理指令；扫描上限和调度语义不变。

## 初始化

准备好内核后初始化 profile：

```bash
CONFIG=bench/configs/local_profiles/oe2403sp4_6_6_scx

python3 -m bench.env init \
  --template "$CONFIG" \
  --config "$CONFIG" \
  --kernel-source "$KSRC" \
  --kernel-image "$PROFILE/assets/bzImage" \
  --kernel-config "$KBUILD/.config" \
  --kernel-id oe2403sp4_6_6_scx \
  --root-image /var/lib/libvirt/scx-bench-runs/scx-agent-classed-oe2403sp4-6.6-scx-base.qcow2 \
  --sync-kernel-source \
  --force

sudo reboot
python3 -m bench.env verify --config "$CONFIG"
```

初始化会更新 profile 中的本机路径、SSH 和 CPU 分配，准备 workload、base image 和 host 隔离，并保留 scheduler、treatment 与 plan 定义。

`env/libvirt.py` 会备份并修改 `/etc/libvirt/qemu.conf`，让 QEMU 以当前测试用户运行。这样 `run.py` 读取 VM runtime 文件时不需要 sudo。

## 构建 scheduler

自定义 scheduler 和 MCP adapter 是独立的 Cargo 项目：

```bash
cargo build --locked --release \
  --manifest-path schedule/scx_agent_classed/Cargo.toml
cargo build --release \
  --manifest-path schedule/scx_agent_classed_mcp/Cargo.toml
```

`scx_agent_classed` 的 `scx_stats`、`scx_stats_derive`、`scx_utils` 和 `scx_cargo` 直接来自 sched-ext/scx，固定在 commit `96e4f928a2d3c84170548f0b552705544f27f2b2`。`--locked` 确保构建不会随远端 `main` 漂移。

## Workload 和 base image

默认实验使用：

```text
batch       stress-ng
latency     schbench
measurement perf stat
```

手动拉取并构建：

```bash
python3 -m bench.env workloads \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  schbench stress-ng perf
```

源码保存在 `bench/workloads/src/`，二进制安装到 `bench/workloads/bin/`。`perf` 从 `libvirt.kernel_source/tools/perf` 构建。通常无需手动执行该命令，`bench.env init` 会准备 profile 所需的 workload。

benchmark wrapper 会固化到 base image，不在每次 run 时覆盖。修改 `bench/benchmarks/` 后必须重建并验证镜像：

```bash
python3 -m bench.env rebuild-image \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx
python3 -m bench.env verify \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx
```

构建会在 qcow2 旁生成 `<root_image>.scx-bench-manifest.json`，记录镜像 identity 和 wrapper SHA-256。base-init VM、`verify` 和真实 `run.py` 会交叉验证该 manifest；镜像被替换、manifest 缺失或 wrapper 变化时，实验在创建 VM 前终止。`rebuild-image` 不修改 plan、scheduler 或 machine 配置。

Workload Wrapper 的输出和扩展契约见 [ARCHITECTURE.md](ARCHITECTURE.md#workload-wrapper-contract)。

## Host 隔离

真实运行前，runner 要求：

- pinned CPU 存在且已隔离；
- pinned CPU 使用固定频率；
- 可迁移 IRQ、RPS 和 XPS 不包含 pinned CPU。

查看状态或预览修改：

```bash
python3 -m bench.env isolation status \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx

python3 -m bench.env isolation prepare \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --dry-run
```

手动准备隔离环境：

```bash
sudo python3 -m bench.env isolation prepare \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx \
  --no-reboot
```

`bench.env init` 默认执行该步骤，随后需要重启。隔离状态保存在 `/var/lib/scx-bench/isolation-state.json`；脚本会更新 GRUB 参数，并安装 systemd service，在重启后设置 pinned CPU 频率。

当 machine 使用 `pin_cpus: auto` 时，`executor.isolated_cpus` 决定隔离范围。无法迁移的 managed IRQ 会记为 `unmovable`；若它在测量期间向 pinned CPU 产生中断，run 标记为 `INTERRUPT_CONTAMINATED`。

## 恢复环境

恢复初始化流程管理的 libvirt 和隔离设置：

```bash
python3 -m bench.env restore \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx
```

只恢复隔离设置并自动重启：

```bash
sudo python3 -m bench.env isolation restore \
  --config bench/configs/local_profiles/oe2403sp4_6_6_scx
```
