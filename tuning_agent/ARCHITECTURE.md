# tuning-agent architecture

This document defines the V2 module boundaries, authority model, and extension contracts.

## Design rule

The architecture follows mechanism/policy separation:

```text
Agent + capability providers
  propose observations, mutations, measurements, and comparison evidence

Trusted Runtime + kernels
  enforce authority, ordering, durability, rollback, evidence validity,
  state transitions, and the final commit decision
```

An extension can add tuning knowledge. It cannot add a second transaction path, skip A/B evaluation, decide `accepted=true`, or invoke rollback/finalize as an Agent tool.

## Source layout

```text
src/
  activation/             activation events, sources, global freeze state
  agent/                  model-visible tool catalog and command decoding
  capability/             provider traits, registry, snapshots, admin policy
  domain/                 IDs and serializable cross-boundary value objects
  kernel/
    transaction/          WAL, recovery, mutation lifecycle, commit seal
    evaluation/           immutable intent/contract, A/B mechanism, central verdict
  adapters/
    local/
      probe/              bounded Linux procfs observation
      mutation/           administrator-bound Linux resource mutation
      measurement/        trusted core system metrics
      comparator/         typed threshold evidence
    mcp/                  MCP stdio transport, manifest loader, providers
    openai/               OpenAI-compatible reasoner adapter
  runtime/
    bootstrap.rs          registry construction
    recovery.rs           startup recovery gate
    daemon.rs             activation loop and composition root
    episode/              state machine and trusted coordinator
  audit/                  structured JSONL evidence, separate from WAL
  config/                 typed TOML model and validation
```

Dependency direction is inward:

```text
domain
  <- capability
  <- kernel
  <- adapters
  <- agent/runtime
  <- daemon composition root
```

`domain` never executes work. Provider traits never change episode state. Adapters never finalize a transaction. The LLM transport never receives kernel authority.

The crate's external API exposes only configuration, the high-level `Runtime`, activation DTOs, and the activation sender. Kernels, WAL seams, coordinator state, provider execution handles, MCP loaders, and concrete adapters are crate-private. A code provider is trusted in-process code and is added through `runtime/bootstrap.rs`; an out-of-process extension uses the MCP boundary.

## Episode state

`EpisodeStateMachine` is the only phase transition authority. Physical phase, frozen intent, and lifecycle are orthogonal:

```text
phase:      Clean | Experimenting | CommitPending | RollingBack | ...
intent:     Draft | Frozen
lifecycle:  Active | Finishing | Finished
```

`Clean + Draft + Active` may Probe or freeze an intent. `Clean + Frozen + Active` may Probe, mutate, or abort. `Clean + Frozen + Finished` means rollback proved the baseline and ended the episode; it never grants authority to replace the target.

```text
Clean
  no episode-scoped transaction or measurement session remains open;
  a FrozenEvaluationIntent may still exist and is never cleared by rollback;
  the Runtime still holds its process-wide WAL-directory ownership lock

Experimenting
  a frozen evaluation intent exists and the transaction Started record is
  durable; only then may a provider perform a mutation effect

CommitPending
  candidate, intent, contract, snapshot generation, and provider pins are frozen;
  AgentAction permissions are empty

RollingBack
  Runtime is restoring the transaction in reverse order

RecoveryRequired
  Runtime cannot prove baseline or committed state; activation freezes

Committed
  provider acknowledgements and the atomic central commit seal completed
```

Probe is deliberately absent from this enum. It is a read-only capability permitted only while the episode lifecycle is `Active` in `Clean` or `Experimenting`.

After any outcome other than `RecoveryRequired`, the global Activation Kernel enters `[safety].cooldown_ms` (default `30000`). It rejects every event, including `Critical`, until the monotonic deadline expires. Cooldown is not an episode phase.

## Authority matrix

| Operation | Agent selects | Provider performs | Trusted owner |
|---|---:|---:|---|
| Probe arguments | yes | yes | Runtime phase/output guard |
| Evaluation intent specification | yes | validates | Runtime + `ContractFreezer` |
| Mutation arguments | yes | prepare/apply/verify | `TransactionKernel` |
| Baseline restore | no | restore/readback | `TransactionKernel` |
| Candidate replay | no | apply/readback | `TransactionKernel` |
| A/B sampling order | no | sample/close | `AbEvaluationProtocol` |
| Comparison evidence | policy selected | compare | `VerdictKernel` |
| Final verdict | no | no | `VerdictKernel` |
| Commit acknowledgement | no | idempotent ack | `TransactionKernel` |
| Commit point | no | no | atomic WAL seal |

## Capability model

`CapabilityRegistry` is the only injection point. Local code and MCP adapters implement the same traits:

```rust
trait ProbeProvider {
    fn meta(&self) -> &CapabilityMeta;
    fn probe(&self, request: &ProbeRequest) -> Result<ProbeEvidence, ProviderError>;
}

trait MutationDriver {
    fn prepare(&self, request: &MutationPrepareRequest) -> Result<PreparedMutation, ProviderError>;
    fn apply(&self, request: &MutationApplyRequest) -> Result<MutationReceipt, ProviderError>;
    fn status(&self, query: &MutationQuery) -> Result<MutationStatus, ProviderError>;
    fn verify(&self, request: &MutationVerifyRequest) -> Result<MutationVerification, ProviderError>;
    fn restore(&self, request: &MutationRestoreRequest) -> Result<MutationReceipt, ProviderError>;
    fn finalize(&self, request: &MutationFinalizeRequest) -> Result<MutationReceipt, ProviderError>;
}

trait MeasurementProvider {
    fn validate_specification(&self, specification: &Value) -> Result<(), ProviderError>;
    fn open(&self, request: &MeasurementOpenRequest) -> Result<MeasurementSession, ProviderError>;
    fn sample(&self, request: &MeasurementSampleRequest) -> Result<MetricBatch, ProviderError>;
    fn close(&self, session: &MeasurementSession) -> Result<CleanupReceipt, ProviderError>;
}

trait ComparisonPolicy {
    fn validate_specification(&self, specification: &Value) -> Result<(), ProviderError>;
    fn compare(&self, request: &ComparisonRequest) -> Result<ComparisonEvidence, ProviderError>;
}
```

All interfaces also expose `CapabilityMeta`. Registry validation enforces:

```text
Probe        ReadOnly              Clean | Experimenting
Mutation     ReversibleMutation    Clean | Experimenting, idempotent=true
Measurement ReadOnly              CommitPending
Comparison  PureComputation        CommitPending, deterministic=true
```

Empty `allowed_phases` is fail-closed. `ManagedObservation` and `IrreversibleMutation` are currently rejected. Internal restore/replay/finalize calls are kernel authority and are not derived from model-visible phase metadata.

### Snapshot semantics

The mutable Registry exists only at bootstrap. Each episode receives an immutable `CapabilitySnapshot` containing metadata and `Arc` provider handles. Tool names and the evaluation-contract schema are generated from that snapshot. An active episode cannot observe provider hot-reload.

## Evaluation intent freezing

The Agent submits one `EvaluationIntentSpec` before the first mutation:

```text
bounded, normalized objective statement
measurement binding
primary comparison bindings
regression guard bindings
workload invariant bindings
sampling plan
```

Runtime then:

1. validates and normalizes the human-readable objective;
2. resolves every Contract ID through the typed snapshot partition;
3. checks `CommitPending` authority;
4. asks each provider to validate its specification before mutation;
5. records the exact `ProviderPin` for every referenced capability;
6. validates sampling bounds, non-empty primary policy, and total A/B budget;
7. computes the Contract SHA-256;
8. constructs `FrozenEvaluationIntent` and computes a second SHA-256 over the domain version, `EpisodeId`, objective, and contract digest;
9. installs the Intent into episode state only after every preceding step succeeds.

`FrozenEvaluationIntent` owns the only `FrozenEvaluationContract` used by the episode. Neither type has a public unchecked constructor; deserialization reruns validation and digest computation. The episode state permits only `None -> Some` and exposes no clear, replace, or take operation. A failed freeze may be corrected and retried; a successful freeze cannot be replaced even after rollback.

The objective is audit semantics. The Contract remains the machine-executable definition of success because Runtime cannot prove that natural-language prose agrees with arbitrary provider specifications. A single objective may therefore contain multiple primary conditions and guardrails without becoming multiple episode targets.

The current episode policy permits one transaction and one final A/B evaluation per episode. A complete rollback sets `Clean + Finished` while retaining the frozen Intent. Changing objective, workload, metrics, thresholds, Measurement, Comparison, or sampling requires a new `EpisodeId`. Future multi-trial support must add explicit `TrialId` and a total trial budget while sharing the same Intent.

The administrator also fixes a total monotonic evaluation budget with `[safety].evaluation_timeout_ms` (default `600000`). Contract freeze, and every later mutation admission, conservatively account for all deterministic A/B waits: both settle periods plus every guardrail/domain sampling interval on both sides. A schedule that already exceeds the budget is rejected before any mutation.

## Transaction mechanism

### Prepare and experiment

```text
Agent Mutation command
  -> write durable Started record with EvaluationIntentPin
  -> transition episode to Experimenting
  -> Runtime allocates trusted change_id
  -> driver.prepare()                    no side effects
  -> validate capability/provider/resource identity
  -> durable ChangeUpsert intent         fsync
  -> driver.apply(operation_id)
  -> driver.verify(desired)
  -> durable AppliedVerified record      fsync
```

`experiment_verified` is a persistent monotonic fact. Restore or replay cannot create it, clear it, or infer it from current state. A candidate may contain only changes with this fact.

One transaction owns each canonical `ResourceKey` at most once. This prevents ambiguous rollback ordering and conflicting candidate entries.

### Lost responses and drift

Every effect has a durable intent before invocation. If the provider response or result WAL update is lost, recovery marks the change `AppliedUnknown`. Runtime reconciles with provider status and readback; it never assumes success from a timeout.

Before overwrite, restore, replay, and commit, readback must match the recorded baseline or desired state. A third value is external drift. Runtime will not overwrite it and enters `RecoveryRequired` if it cannot restore safely.

### Restore and candidate

Baseline restore runs in reverse change order. Candidate IDs are canonicalized back to original transaction order; LLM ordering is not trusted. Candidate digest is computed by `Candidate::new` from the canonical IDs and validated on deserialization.

### Commit protocol

Provider `finalize` is narrowly defined as retry-safe acknowledgement. It must not modify the tuned resource or discard rollback material before the central seal.

```text
for every selected change:
  durable finalize intent
  provider acknowledgement
  durable CandidateApplied result (still rollback-capable)

verify complete selected candidate + unselected baseline
write one atomic CommitSealed event containing all terminal records
  and the Runtime-issued CommitAuthorization
fsync
```

`CommitAuthorization` binds the complete `EvaluationIntentPin` (episode, intent, and contract digests), canonical candidate digest, central decision digest, and complete evaluation-evidence digest. `TransactionKernel` rejects authorization whose pin differs from its durable `Started` record before invoking any finalize acknowledgement. It is an internal permit, not a provider or public-library authority.

No record becomes `Finalized` until every acknowledgement succeeds. An acknowledgement failure retains rollback authority and triggers immediate rollback. A central seal write failure is different: its durable outcome may be unknown, so Runtime must not blindly rollback; it enters `RecoveryRequired` and lets startup reconciliation establish the terminal state.

### WAL store and recovery

`TransactionStore` maps each `TransactionId` to its own collision-free `tx-<hex>.jsonl` file. It holds a non-blocking exclusive directory lock for the Runtime lifetime, secures the directory to `0700`, uses `0600` create-new files, rejects symlinks/non-files, and binds discovery to recovery with device/inode identity. It never deletes or repairs corrupt or non-empty evidence. A strictly empty file created before the durable `Started` record may be discarded only after path, ID, type, length, and identity revalidation followed by directory fsync.

Every non-empty V2 WAL must contain an `EvaluationIntentPin` in its first `Started` event. Store discovery, kernel recovery, evaluation evidence, commit authorization, and `CommitSealed` all compare the complete pin. Missing legacy fields are not defaulted: an older pending WAL must be recovered before upgrade or handled by an explicit rollback-only legacy reader.

At startup:

```text
acquire exclusive WAL-directory ownership
  -> build builtin/local Registry
  -> early best-effort recovery for transactions whose drivers are available
  -> load each MCP server best-effort
  -> run the final complete recovery inventory gate
       -> reconcile sealed commit/rollback audit records
       -> process corrupt logs and pending transactions independently
       -> aggregate every recovery, plugin, and audit failure
  -> bind activation sources only when the aggregate gate is clear
```

An unrelated MCP or audit failure cannot prevent an already-available local rollback, but it still prevents activation after recovery. WAL is recovery state. `AuditSink` is operational evidence. They are intentionally separate.

## Evaluation mechanism

V2 fixes the mechanism to A/B while keeping measurement and policy extensible:

```text
restore baseline
settle
trusted guardrail sample A
domain sample A
replay candidate
settle
trusted guardrail sample B
domain sample B
comparison evidence
central verdict
```

Any successful measurement `open` is paired with exactly one `close`, including sample errors. Samples are type/unit checked and median-aggregated. Invalid/partial batches or missing fixed metrics are fail-closed. Domain A/B batches must both contain the same non-empty trusted workload fingerprint; a missing or changed fingerprint is `Inconclusive`.

One monotonic deadline starts at entry to `AbEvaluationProtocol::evaluate`. The Runtime checks it before and after baseline restore, candidate replay, settle, every measurement operation, every comparison, and the central verdict. Before a provider call, its declared capability timeout must fit in the remaining total budget. `BudgetExceeded` is an ordinary evaluation failure and therefore rolls the transaction back. Once measurement `open` succeeds, cleanup has priority: `close` is attempted even when the deadline or timeout-admission check has already failed.

This is a hard orchestration boundary but only a cooperative execution boundary for in-process synchronous providers: Rust cannot safely interrupt an arbitrary blocking local call. MCP request timeouts are enforced by their transport; local providers must return within their declared timeout. Runtime detects a local overrun immediately after the call returns and rejects its evidence.

System guardrails use a dedicated builtin measurement handle and separate evidence fields. Domain or MCP measurement cannot spoof protected metric names. Central verdict precedence is:

```text
fixed system or regression failure -> Unsafe
invalid/partial/incomparable data   -> Inconclusive
all primary policies improved      -> Improved
otherwise                           -> NoSignal
```

Only `Improved` enters transaction finalize. Every other verdict rolls back.

## MCP injection

MCP is a provider transport, not an authority transport.

The server publishes a strict manifest at `tuning://capabilities/v1`. Runtime also reads `tools/list` and requires each declared operation to exist. It does not infer effect class, reversibility, phases, or idempotence from tool names, descriptions, or generic MCP annotations.

Security properties:

- standard newline-delimited JSON-RPC stdio transport;
- `initialize`/`initialized` handshake and request ID correlation;
- bounded frames, timeouts, stdout protocol isolation, concurrent stderr drain;
- child environment cleared, then explicit configured variables applied;
- strict manifest and supported JSON Schema subset;
- local input validation before `tools/call`, structured output validation after it;
- ProviderClass forced to `Mcp` regardless of server data;
- global and per-server capability allowlists;
- MCP mutation disabled unless `allow_mutations=true` for that server;
- provider handles are pinned in the episode snapshot and recovery WAL.

An MCP server disconnect during a Probe is an ordinary failed observation. Disconnect during a transaction effect is reconciled through status/readback; if state cannot be proven, activation freezes.

The manifest is not an OS sandbox. Deployment must still run each server with least-privilege credentials, Linux capabilities, filesystem access, and cgroup limits appropriate to its declared operations.

The complete provider-author contract is documented in [`MCP_CAPABILITIES.md`](MCP_CAPABILITIES.md).

## Runtime coordinator

`EpisodeCoordinator` owns, for one episode:

```text
EpisodeStateMachine
  -> EpisodeLifecycle
  -> Option<FrozenEvaluationIntent> (single owner, monotonic)
CapabilitySnapshot
ToolCatalog + ToolDispatcher
Option<TransactionKernel>
evaluation decision and terminal outcome
```

The dispatcher only decodes. The coordinator checks permissions and invokes providers/kernels. It does not hold a second Contract pointer; every mutation and evaluation reads `state.frozen_intent().contract()`. `request_commit` moves the lifecycle to `Finishing`: after phase transition to `CommitPending`, evaluation runs synchronously under Runtime control and no result can trigger another Agent call.

Reasoner failure, final text, empty tool batches, audit failure, abort, mutation failure after transaction start, or round exhaustion all pass through the same cleanup path. If rollback succeeds, the outcome is `Clean + Finished` with the Intent retained in `EpisodeOutcome`; otherwise it is `RecoveryRequired`.

## Adding capability code

1. Implement exactly one provider trait as trusted in-process code.
2. Declare bounded JSON input/output schemas, limits, phases, effect class, provider version, and manifest digest.
3. Make mutation operations idempotent by `OperationId`; make `prepare` side-effect free and resource identity canonical.
4. Implement explicit specification validation for Measurement/Comparison.
5. Register only in `runtime/bootstrap.rs` under `AdminPolicy`; do not expose a second public registrar or raw provider handle.
6. Test invalid inputs, output bounds, timeout/lost-response behavior, cleanup, drift, and rollback.
7. Add an end-to-end episode test when the capability changes candidate or evaluation behavior.

Do not add an alternate shell dispatcher, direct write helper, provider-controlled final verdict, or model-visible restore/finalize tool.

## Future extension points

The current A/B protocol is intentionally fixed. A future evaluation mechanism should be introduced as a trusted Runtime strategy with the same transaction and verdict authority boundaries. It must not be injected as an ordinary MCP operation.

Multiple evaluated candidates in one episode are intentionally unsupported. A future design must introduce explicit `TrialId`, immutable sharing of the episode Intent, an episode-wide trial budget, and statistical controls for adaptive repeated testing. Clearing or replacing Intent is never a trial mechanism.

`ManagedObservation` may be enabled only after adding measurement-session intent/receipt WAL, idempotent status/close, and startup cleanup. `IrreversibleMutation` remains outside autonomous tuning and requires a separate manual approval model.
