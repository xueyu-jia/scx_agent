# tuning-agent Architecture

This document describes the internal design for developers.

User-facing usage is documented in `README.md`.

## Design Goal

`tuning-agent` is built around this split:

```text
LLM proposes.
Runtime enforces.
```

The LLM may diagnose workloads, choose observations, design experiments, and
submit a commit claim. The runtime owns all deterministic control points:

- activation
- unrestricted shell execution and structured write tracking
- write state tracking
- rollback/restore
- candidate construction
- metric evaluation
- audit logging

The system intentionally avoids a large rule-driven tuning engine. Knowledge
stays mostly in the reasoning loop; safety and state transitions stay in code.

## Module Map

```text
src/
  activation/   wakeup sources and ActivationKernel
  observation/  minimal host snapshot
  reasoning/    OpenAI-compatible LLM protocol
  tools/        model-visible tool schemas
  act/          read execution and structured writes
  evaluate/     commit validation
  runtime/      daemon and episode orchestration
  audit/        JSONL audit journal
  config.rs     TOML configuration and defaults
  types/        shared domain types
```

## Configuration

Configuration is centralized in `src/config.rs`.

The runtime does not read environment variables. Configuration resolution is:

```text
explicit --config path
  > ./tuning-agent.toml when it exists
  > Config::default()
```

If `--config` is provided, the file must exist and parse successfully.

Top-level sections:

```rust
Config {
    llm,
    reasoning,
    activation,
    audit,
    command,
    evaluation,
}
```

Module-specific conversion happens at the boundary:

```text
OpenAiConfig::from_config(&config.llm)
ActKernelConfig::from_config(&config.command)
EvaluationKernelConfig::from_config(&config.evaluation)
TimerSource::new(config.activation.timer_interval_ms)
EbpfRingbufSource::new(config.activation.ebpf_ringbuf_pin)
```

Keep this direction: modules receive typed config, they do not read process
environment.

Evaluation timing config has defaults and hard bounds:

```toml
[evaluation]
default_window_seconds = 10
min_window_seconds = 3
max_window_seconds = 60
default_settle_seconds = 3
min_settle_seconds = 0
max_settle_seconds = 10
```

Commit `window_seconds` and `settle_seconds` are model suggestions. The
EvaluationKernel clamps them to these configured bounds before sleeping or
sampling.

## Runtime Flow

```text
Activation source
  -> ActivationKernel.accept()
  -> Runtime::process_activation_event()
  -> EpisodeController::run()
  -> Observation::core_snapshot()
  -> Reasoning
  -> Tool dispatch loop
  -> finish_episode()
```

The episode loop allows `reasoning.max_rounds` reasoning/tool rounds. The value defaults to 4 and must be greater than zero.

Terminal phases:

```text
Committed
Frozen
```

Episode completion is an audit event, not a phase that overwrites the final outcome.

## Episode Phase

Defined in `src/runtime/episode_state.rs`:

```rust
pub enum EpisodePhase {
    Clean,
    Experimenting,
    CommitPending,
    Committed,
    Frozen,
}
```

Meaning:

```text
Clean
  No uncommitted experiment write is currently tracked.

Experimenting
  At least one experiment_write has modified a target.

CommitPending
  A commit request is being validated.

Committed
  Validation passed and finalize_commit completed.

Frozen
  A deterministic operation failed in a way that stops the episode and freezes
  the global ActivationKernel until the daemon is restarted.
```

Reasoning and acting are not phases. They are activities inside an episode.

## Tool Boundary

The model currently sees three tools:

```text
probe
experiment_write
commit
```

### probe

Maps to:

```rust
ActKernel::execute_command()
```

It executes a non-empty shell script through `/bin/sh -c` without syntax or side-effect restrictions.

### experiment_write

Maps to:

```rust
ActKernel::experiment_write()
```

It accepts structured target/value data. It does not accept shell writes and
does not accept model-provided rollback commands.

### commit

Maps to:

```rust
EvaluationController::validate_commit()
ActKernel::finalize_commit()
```

The commit request must include `keep_writes`.

## ActKernel

Located in:

```text
src/act/
```

Primary responsibilities:

- execute unrestricted shell scripts
- validate structured write targets
- capture original values
- apply experiment writes
- restore baseline state
- apply commit candidate state
- finalize accepted commits
- discard rejected or unfinished experiment writes

ActKernel owns the per-episode target table:

```rust
targets: BTreeMap<WriteTarget, TargetState>
```

`BTreeMap` is used instead of `HashMap` for stable ordering in audit output and
tests.

### WriteTarget

Defined in `src/act/command.rs`:

```rust
pub enum WriteTarget {
    Sysctl { key: String },
    ProcSys { path: PathBuf },
    Sysfs { path: PathBuf },
    Cgroup { path: PathBuf },
}
```

Tests also compile a `/tmp` file target behind `#[cfg(test)]`.

Path mapping:

```text
Sysctl { key: "vm.dirty_ratio" }
  -> /proc/sys/vm/dirty_ratio

ProcSys { path }
  -> exact absolute path under /proc/sys

Sysfs { path }
  -> exact absolute path under /sys

Cgroup { path }
  -> exact absolute path under /sys/fs/cgroup
```

### TargetState

Defined in `src/act/kernel.rs`:

```rust
struct TargetState {
    original_value: String,
    current_value: String,
    experiment_values: BTreeSet<String>,
    write_state: TargetWriteState,
}
```

`write_state` is `Prepared`, `Applied`, or `RecoveryRequired`. A failed write is
re-read before rollback is scheduled: unchanged targets are removed from the
table, while changed or unverifiable targets remain tracked for recovery.

Semantics:

```text
original_value
  Value before the first experiment write to this target in the episode.

current_value
  Last value observed after ActKernel wrote this target.

experiment_values
  All values written to this target by experiment_write in this episode.
```

First write detection is done by map entry:

```rust
self.targets
    .entry(target.clone())
    .or_insert_with(|| TargetState { ... });
```

If the target is not in the map, the current value is captured as
`original_value`. If the target already exists, `original_value` is not changed.

### Experiment Write Flow

```text
experiment_write(target, value)
  -> validate target
  -> read old_value
  -> register Prepared target state
  -> write target=value
  -> re-read current_value
     -> requested value: mark Applied and return WriteReport
     -> old value: remove new Prepared state and return an ordinary failure
     -> changed/unverifiable: mark RecoveryRequired and start recovery
```

This is the only way model-requested writes enter the system.

### Commit Candidate Validation

`keep_writes` is checked before candidate application:

```text
for each keep_write:
  target must exist in ActKernel.targets
  value must exist in target.experiment_values
  duplicate target is rejected
```

This prevents commit from introducing an untested target/value pair.

### Restore / Apply / Finalize

ActKernel exposes these deterministic state transitions:

```rust
restore_to_baseline()
discard_episode_writes()
apply_commit_candidate(keep_writes)
finalize_commit(keep_writes)
```

`restore_to_baseline()`:

```text
write every touched target back to original_value
keep the target table
```

Used before baseline sampling.

`apply_commit_candidate(keep_writes)`:

```text
write only keep_writes values
keep the target table
```

Used before candidate sampling.

`finalize_commit(keep_writes)`:

```text
for every touched target:
  if target is in keep_writes:
    write committed value
  else:
    restore original_value
clear target table
```

Used after accepted evaluation.

`discard_episode_writes()`:

```text
restore_to_baseline()
clear target table
```

Used after rejected/inconclusive validation or unfinished episodes.

## Evaluation

Located in:

```text
src/evaluate/
```

Evaluation is model-claim validation. The model provides what to measure and
what should improve, but code decides the result.

### EvaluationPlan

Parsed from commit arguments:

```rust
pub struct EvaluationPlan {
    pub reason: String,
    pub measurement: MeasurementProgram,
    pub primary: Vec<MetricCondition>,
    pub regression_guards: Vec<MetricCondition>,
    pub workload_invariants: Vec<MetricCondition>,
    pub keep_writes: Vec<CommitWrite>,
    pub window: Option<Duration>,
    pub settle: Option<Duration>,
}
```

### Evaluation Flow

Implemented in `EvaluationController::validate_commit()`:

```text
1. restore_to_baseline()
2. settle
3. sample baseline A'
4. apply_commit_candidate(keep_writes)
5. settle
6. sample candidate B'
7. evaluate baseline vs candidate
8. return Accepted / Rejected / Inconclusive / Frozen
```

The settle/window values used in steps 2, 3, 5, and 6 are effective values:

```text
effective = clamp(model value or configured default, configured min, configured max)
```

The old model of:

```text
rollback all experiment commands
replay all successful experiment commands
```

has been removed.

Evaluation now replays only the explicit commit candidate.

### Measurement Program

The model provides an unrestricted shell script that must print one JSON object:

```json
{
  "command": "...",
  "schema": {
    "metric": "number"
  }
}
```

Rules:

```text
stdout must be one JSON object
same command is used for A' and B'
numeric and bool fields become EvaluationSample values
schema fields are validated when present
```

### Metrics

`EvaluationSample` is a `BTreeMap<String, f64>` plus raw measurement JSON.

It merges:

```text
model measurement JSON
system core guardrail metrics
```

System guardrail metrics come from `CoreSnapshot`:

```text
loadavg.1m
psi.cpu.full.avg10
psi.io.full.avg10
psi.memory.full.avg10
psi.cpu.some.avg10
psi.io.some.avg10
psi.memory.some.avg10
```

### Decision Ordering

Implemented in `EvaluationKernel::evaluate()`:

```text
if regression_guards fail:
  Unsafe

else if system_guardrails fail:
  Unsafe

else if workload_invariants fail:
  Inconclusive

else if primary_metrics pass:
  Improved

else:
  NoSignal
```

Only `Improved` is accepted.

## Reasoning

Located in:

```text
src/reasoning/
```

The current reasoner is OpenAI-compatible:

```text
POST {base_url}/v1/chat/completions
Authorization: Bearer {api_key}
```

Configuration:

```text
[llm]
base_url
api_key
model
timeout_ms
```

Protocol responsibilities:

```text
encode messages
encode tools
parse assistant content
parse tool calls
append tool results
```

Protocol does not enforce system safety policy.

## Activation

Located in:

```text
src/activation/
```

`ActivationKernel` owns:

```text
Sleeping / Active / Cooldown / Frozen state
dedupe window
event acceptance
```

Sources:

```text
UnixIpcSource
TimerSource
EbpfRingbufSource
```

`EbpfRingbufSource` is currently a hook. It needs a concrete ringbuf map
contract before it becomes a real reader.

## Observation

Located in:

```text
src/observation/
```

Observation is intentionally thin. It collects only enough initial facts to
bootstrap reasoning:

```text
/proc/loadavg
/proc/stat
/proc/meminfo
/proc/pressure/cpu
/proc/pressure/memory
/proc/pressure/io
/proc/net/snmp
/proc/net/softnet_stat
```

Everything else should be requested through `probe`.

## Audit

Located in:

```text
src/audit/
```

Audit output is JSONL:

```text
logs/audit.jsonl
```

Audit records include:

- episode started
- activation rejected
- core snapshot
- reasoning output
- tool result
- act result
- episode finished

Act results record `rollback_required`, `rollback_attempted`,
`rollback_succeeded`, and `rollback_error` separately. Episode-finished records
contain both the final episode phase and the resulting global agent state.

Future improvement: add first-class phase-change and evaluation-subphase records
instead of only embedding them in tool results.

## Development Invariants

Do not reintroduce model-provided rollback commands.

```text
rollback must be generated by code from captured original_value
```

Do not let commit mean "keep whatever state exists now".

```text
commit must only keep explicit keep_writes
```

Do not let commit introduce untested writes.

```text
keep_writes target/value must exist in experiment_values
```

Do not put authorization into the OpenAI protocol layer.

```text
protocol parses tool calls
ActKernel executes shell scripts and tracks structured write state
EpisodeController enforces phase policy
```

Do not expand Observation into a heavy collector.

```text
Observation stays thin
probe handles demand-driven diagnostics
```

## Test Notes

Normal checks:

```bash
cargo fmt -- --check
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
```

The Unix IPC test binds a Unix socket. It may fail in restricted sandboxes even
when the code is correct.

Important tests:

```text
ActKernel unrestricted shell command execution
ActKernel keep-only finalization
Unix IPC activation
OpenAI-compatible protocol parsing
accepted commit candidate flow
rejected commit restore flow
inconclusive commit restore flow
```
