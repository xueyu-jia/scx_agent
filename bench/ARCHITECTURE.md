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

  runner.py

  scripts/
    run.py
    isolation.py
    fetch_workloads.py

  collectors/
    guest.py

  benchmarks/
    generic.py

  analysis/
    loader.py
    compare.py
    report.py
    run.py

  configs/
    example.config

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
- generate guest scripts;
- run `vng`;
- enforce host preflight checks;
- save per-run metadata and raw artifacts;
- parse wrapper JSON into `bench_metrics.json`.

It does not parse CLI arguments, choose schedulers, decide execution order, or
run analysis.

### `bench/scripts/run.py`

Main experiment entry point.

Key responsibilities:

- read config;
- select `--baseline` and `--candidate` schedulers;
- expand the selected plan;
- run baseline/candidate in alternating or sequential order;
- write experiment metadata;
- invoke analysis and report generation.

This script is the normal user-facing entry point.

### `bench/scripts/isolation.py`

Prepares or restores host isolation.

Key responsibilities:

- compute target CPUs from config and plan;
- save original host state;
- update GRUB boot parameters;
- configure fixed CPU frequency through a systemd service;
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
perf bench  host perf tool
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

### `bench/collectors/guest.py`

Generates the shell script executed inside the `vng` guest.

The guest script:

- starts the selected scheduler if `kind: scx`;
- collects before/after snapshots;
- runs the workload wrapper;
- stops the scheduler;
- writes `guest_result.json`;
- stores dmesg delta and raw logs.

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
example.config
  -> config parser
  -> RunSpec list
  -> scripts/run.py chooses scheduler order
  -> runner.py executes scheduler + RunSpec batch
  -> vng guest runs generated run_guest.sh
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

`kind: scx` means the generated guest script starts the scheduler before the
workload and stops it after the workload.

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
machines:
  small:
    vcpus: 2
    memory: 8G
    pin_cpus: "2-3"
    exclusive: true
    frequency:
      fixed: true
```

`scripts/isolation.py prepare` configures host boot/runtime state.

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
