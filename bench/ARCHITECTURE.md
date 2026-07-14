# Architecture

This document describes the current benchmark framework architecture.

## Design Principles

- Configuration defines stable entities.
- CLI selects experiment variables.
- Runner executes already-expanded specs.
- Scripts orchestrate workflows.
- Collectors gather raw evidence.
- Analysis compares results.
- Reporting only visualizes analysis output.

## Directory Overview

```text
bench/
  config/
    parser.py

  base_image.py
  runner.py

  scripts/
    prepare_env.py
    run.py
    libvirt_env.py
    isolation.py
    fetch_workloads.py

  collectors/
    guest.py
    guest_executor.py

  benchmarks/
    generic.py

  analysis/
    loader.py
    compare.py
    report.py
    run.py

  configs/
    example.config
    local.config  # generated, ignored by git

  workloads/
  schedulers/
  results/
```

## Module Responsibilities

### `bench/config/parser.py`

Owns config loading, validation, and plan expansion.

Key responsibilities:

- validate top-level config sections;
- validate machines, schedulers, suites, benches, and metric profiles;
- expand a plan into `RunSpec` objects.

It does not run benchmarks and does not decide baseline/candidate order.

### `bench/runner.py`

Executes a provided list of `RunSpec` objects for one scheduler.

Key responsibilities:

- create per-run directories;
- build and persist a versioned `guest_plan.json`;
- create a per-run qcow2 overlay;
- generate libvirt domain XML;
- define, start, destroy, and undefine libvirt domains;
- stage the static guest executor and run plan;
- invoke the executor over SSH and copy artifacts back;
- enforce host preflight checks;
- save per-run metadata and raw artifacts;
- parse wrapper JSON into `bench_metrics.json`.

It does not parse CLI arguments, choose schedulers, decide execution order, or
run analysis.

### `bench/scripts/run.py`

Main experiment entry point.

Key responsibilities:

- read config;
- reject stale base images before a real experiment starts;
- select `--baseline` and `--candidate` schedulers;
- expand the selected plan;
- build comparison pairs from expanded `RunSpec` objects;
- allocate host CPU placement for `pin_cpus: auto`;
- run comparison pairs concurrently when isolated resources allow it;
- run baseline/candidate inside each pair in alternating or sequential order;
- write experiment metadata;
- invoke analysis and report generation.

This script is the normal user-facing entry point.

### `bench/scripts/prepare_env.py`

Prepares a machine-specific local environment.

Key responsibilities:

- generate `bench/configs/local.config` from `example.config`;
- derive `libvirt.emulator_cpus` and `executor.isolated_cpus` from host topology;
- generate the SSH key used by the guest;
- call `libvirt_env.py prepare`;
- call `fetch_workloads.py`;
- create the libvirt base image;
- write and verify base-image provenance;
- rebuild only the image through `rebuild-image` without replacing local config;
- call `isolation.py prepare --no-reboot`;
- call `libvirt_env.py restore` and `isolation.py restore` from `restore`;
- verify that the generated environment is usable.

`prepare_env.py` owns machine-local orchestration. It does not run experiments
and does not own subsystem-specific host mutations.

### `bench/base_image.py`

Owns base-image provenance. It hashes every source file under
`bench/benchmarks/`, excluding generated Python caches, and binds that snapshot
to the qcow2 device, inode, size, and modification time. The base initialization
VM recomputes the wrapper hashes after extraction; the manifest is written only
after that check passes and the VM has shut down.

`prepare_env.py verify` and non-dry-run `scripts/run.py` use the same verifier.
The per-run runner does not know which benchmark wrappers exist and does not
copy wrapper source into guests.

### `bench/scripts/libvirt_env.py`

Prepares or restores libvirt/QEMU host settings.

Key responsibilities:

- configure `/etc/libvirt/qemu.conf` so QEMU runs as the benchmark user;
- back up and restore the original qemu.conf;
- prepare the libvirt runtime directory permissions;
- ensure libvirtd and the selected libvirt network are available;
- verify the libvirt/QEMU host environment.

### `bench/scripts/isolation.py`

Prepares or restores host isolation.

Key responsibilities:

- compute target CPUs from config and plan;
- save original host state;
- update GRUB boot parameters;
- configure fixed CPU frequency through a systemd service;
- apply IRQ/RPS/XPS runtime isolation;
- write the runtime isolation report consumed by the runner;
- restore original host settings.

The runner does not modify host isolation. It only checks that isolation is
already active.

### `bench/scripts/fetch_workloads.py`

Fetches and builds community workload programs.

Current workload sources:

```text
hackbench   linux-test-project/ltp
schbench    kernel.googlesource.com/.../mason/schbench
stress-ng   ColinIanKing/stress-ng
fio         axboe/fio
redis       redis/redis
rt-tests    kernel.org rt-tests
will-it-scale antonblanchard/will-it-scale
perf bench  configured kernel source tree tools/perf
kernel build configured kernel source tree
```

The script stores source trees under:

```text
bench/workloads/src/
```

and installs runnable binaries under:

```text
bench/workloads/bin/
```

`perf` is built from `libvirt.kernel_source/tools/perf` and installed as
`bench/workloads/bin/perf`. The `perf bench sched` wrapper uses this binary
before falling back to host `perf`.

### `bench/collectors/guest.py`

Defines the host-side execution-plan model. It converts validated benchmark,
scheduler, and libvirt configuration into a versioned JSON document and
calculates the corresponding host timeout budget.

It does not execute commands or generate shell source.

### `bench/collectors/guest_executor.py`

A standalone, standard-library Python program staged into each guest. It
validates the uploaded JSON plan independently before executing it. Keeping
this boundary static makes argv and environment handling structured and keeps
the guest independent from the host-side `bench` package.

The guest executor:

- waits for the configured VM settle interval after SSH is ready;
- starts the selected scheduler if `kind: scx`;
- waits for the scheduler to settle;
- runs optional workload warmup in an isolated process group and output directory;
- rejects warmup failures, guest-enforced timeouts, leaked processes, or an exited scheduler;
- waits for the post-warmup settle interval;
- collects `before` snapshot;
- runs measurement in its own process group with a guest-enforced timeout;
- collects `after` snapshot;
- verifies that an `scx` scheduler survived the measured workload;
- waits for cooldown;
- stops the scheduler;
- atomically writes a structured `guest_result.json`;
- stores dmesg delta and raw logs.

Warmup, settle, and cooldown are outside the measured snapshot window. Warmup
artifacts are stored below `warmup/`, so its wrapper output and perf files
cannot be consumed as measurement metrics. The measured benchmark wrapper's
metric JSON remains the source of workload performance metrics.

Snapshots currently include:

```text
/proc/stat
/proc/schedstat
/proc/interrupts
/proc/pressure/cpu
/proc/pressure/io
/proc/pressure/memory
dmesg
/sys/kernel/debug/sched_ext/*
```

### `bench/benchmarks/generic.py`

Generic workload wrapper.

It runs a community workload binary, saves raw stdout/stderr, and emits
normalized JSON.

Specialized wrappers are used for tools such as `fio`, `schbench`, `perf bench
sched`, `will-it-scale`, `cyclictest`, `kernel build`, and `redis-benchmark`.

### `bench/metrics.py`

Loads and validates benchmark wrapper output.

Benchmark wrappers convert workload-native output into the framework metric
JSON contract. `bench/metrics.py` reads that JSON from `stdout.log`, preserves
the metrics, and records parse status such as `ok`, `empty_stdout`, or
`non_json_stdout`.

### `bench/analysis/loader.py`

Loads per-run results from scheduler result directories.

Inputs:

```text
result.json
bench_metrics.json
```

Output:

```text
RunMetricSet objects
```

### `bench/analysis/compare.py`

Builds comparison objects.

It does not create bottom-level metric records. Instead, it groups runs by:

```text
machine + suite + bench + metric
```

For each metric listed by the metric profile, it creates:

```json
{
  "machine": "small",
  "suite": "cpu_smoke",
  "bench": "hackbench_smoke",
  "metric": "elapsed_time_sec",
  "role": "primary",
  "direction": "lower",
  "unit": "s",
  "chart": "delta_bar",
  "baseline": {
    "values": [],
    "mean": 0
  },
  "candidate": {
    "values": [],
    "mean": 0
  },
  "delta_pct": 0,
  "verdict": "no_change"
}
```

Verdicts are based on:

- metric direction;
- regression threshold;
- baseline mean;
- candidate mean.

### `bench/analysis/report.py`

Renders `analysis.json` into HTML.

It does not recompute verdicts or metrics.

### `bench/analysis/run.py`

Standalone analysis CLI for re-analyzing existing result directories.

## Data Flow

```text
example.config + prepare_env.py init
  -> local.config
local.config
  -> config parser
  -> base image + benchmark wrapper manifest verification
  -> RunSpec list
  -> scripts/run.py chooses scheduler order
  -> runner.py executes scheduler + RunSpec batch
  -> runner.py uploads guest_executor.py + guest_plan.json
  -> libvirt guest validates and executes the plan
  -> per-run raw artifacts
  -> analysis loader
  -> comparison objects
  -> analysis.json
  -> report.html
```

## Experiment Flow

For an alternating experiment:

```text
round 1:
  baseline run_index=1
  candidate run_index=1

round 2:
  candidate run_index=2
  baseline run_index=2

round 3:
  baseline run_index=3
  candidate run_index=3
```

The execution order is recorded in experiment `metadata.json`.

## Scheduler Model

Schedulers are defined in config:

```yaml
schedulers:
  default:
    kind: builtin

  scx_simple:
    kind: scx
    command: bench/schedulers/scx_simple
    args: []
```

`kind: builtin` means no `scx` scheduler process is started.

`kind: scx` means the guest executor starts the scheduler before warmup and
stops it after measurement and cooldown.

The framework treats baseline and candidate identically. Either can be builtin
or `scx`.

## Result Model

Experiment root:

```text
bench/results/experiments/<timestamp>__<baseline>_vs_<candidate>/
```

Per-run raw data:

```text
runs/<scheduler>/run_.../
```

`guest_result.json` uses its top-level `status` as the authoritative execution
outcome. Scheduler, warmup, and measurement details live below `phases`; valid
failure statuses distinguish scheduler failure, warmup failure/timeout,
measurement failure/timeout, and internal executor errors.

Machine-readable comparison data:

```text
analysis/analysis.json
```

Human-readable report:

```text
analysis/report.html
```

## Metric Model

Workload wrappers emit per-run metrics:

```json
{
  "metrics": {},
  "metadata": {},
  "raw": {}
}
```

Metric profiles define comparison semantics:

```yaml
metric_profiles:
  cpu_throughput:
    primary:
      - name: elapsed_time_sec
        direction: lower
        unit: s
        chart: delta_bar
        regression: +3%
```

The analysis layer consumes wrapper metrics and metric profile definitions to
produce comparison objects.

## Isolation Model

Machine config declares required isolation:

```yaml
executor:
  parallel: auto
  cpu_source: isolated
  isolated_cpus: "2-9"
  smt_policy: use_all_siblings
  pair_policy: sequential

machines:
  small:
    vcpus: 2
    memory: 8G
    pin_cpus: auto
    exclusive: true
    frequency:
      fixed: true
```

`scripts/isolation.py prepare` configures host boot/runtime state.

When `pin_cpus: auto` is used, `executor.isolated_cpus` defines the host CPU
range to isolate. `scripts/run.py` then allocates complete SMT sibling groups
to comparison pairs. A physical core's logical CPU siblings are never split
across different pairs.

`runner.py` checks:

- pinned CPUs exist;
- pinned CPUs are isolated;
- pinned CPUs have fixed frequency.

If checks fail, the run is marked `PREFLIGHT_FAILED` and no VM is started.

## Extension Points

Add new workload:

1. put the binary in `bench/workloads/`;
2. or teach `bench/scripts/fetch_workloads.py` how to fetch/build it;
3. add or reuse a wrapper under `bench/benchmarks/`;
4. add a `benches:` entry;
5. include it in a suite.

Add new scheduler:

1. put the binary in `bench/schedulers/`;
2. add a `schedulers:` entry;
3. pass it with `--baseline` or `--candidate`.

Add new metric:

1. make the wrapper emit it under `metrics`;
2. add it to the relevant `metric_profile`;
3. choose `direction`, `unit`, `chart`, and threshold.

Add new visualization:

1. add a chart type to config validation;
2. teach `analysis/report.py` how to render that chart;
3. keep comparison generation unchanged unless the metric semantics change.
