// SPDX-License-Identifier: GPL-2.0

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::control::{
    failure as control_failure, require_current, require_rule, require_stats, ControlClient,
    SchedulerControl,
};
use crate::control_wire::{
    ControlResponse, ControlStatus, RuleClass, RuleObservation, RuleSource, RuleState,
};
use crate::journal::{
    Journal, JournalKind, MutationOperationRequest, MutationStatusRequest, MutationVerifyRequest,
    OperationRecord,
};
use crate::rpc::{rpc_error, rpc_result, RpcRequest};
use crate::schema::{
    manifest, tools, MAX_TARGETS, PROVIDER_VERSION, TOOL_MEASUREMENT_CLOSE, TOOL_MEASUREMENT_OPEN,
    TOOL_MEASUREMENT_SAMPLE, TOOL_MEASUREMENT_VALIDATE, TOOL_MUTATION_APPLY,
    TOOL_MUTATION_FINALIZE, TOOL_MUTATION_PREPARE, TOOL_MUTATION_RESTORE, TOOL_MUTATION_STATUS,
    TOOL_MUTATION_VERIFY, TOOL_RULES_SNAPSHOT,
};
use crate::workload::workload_fingerprint;
use crate::{sha256_hex, validate_comm, validate_id};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const CAPABILITIES_URI: &str = "tuning://capabilities/v1";
pub struct Server<C = ControlClient> {
    control: C,
    journal: Journal,
}

impl Server<ControlClient> {
    pub fn new(
        control_socket: impl Into<PathBuf>,
        journal_path: impl Into<PathBuf>,
        control_timeout: Duration,
    ) -> Self {
        Self {
            control: ControlClient::new(control_socket.into(), control_timeout),
            journal: Journal::new(journal_path.into()),
        }
    }
}
impl<C: SchedulerControl> Server<C> {
    pub(crate) fn handle_rpc(&self, request: RpcRequest) -> Option<Value> {
        let id = request.id?;
        if request.jsonrpc != "2.0" {
            return Some(rpc_error(id, -32600, "jsonrpc must be '2.0'"));
        }

        let result = match request.method.as_str() {
            "initialize" => Ok(json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {
                    "name": "scx-agent-classed-tuning-mcp",
                    "version": PROVIDER_VERSION
                }
            })),
            "resources/read" => self.resources_read(request.params),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => return Some(rpc_result(id, self.tools_call(request.params))),
            _ => return Some(rpc_error(id, -32601, "method not found")),
        };

        Some(match result {
            Ok(value) => rpc_result(id, value),
            Err(error) => rpc_error(id, -32602, error.to_string()),
        })
    }

    fn resources_read(&self, params: Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            uri: String,
        }

        let request: Request = decode(params, "resources/read parameters")?;
        if request.uri != CAPABILITIES_URI {
            bail!("unsupported resource URI '{}'", request.uri);
        }
        Ok(json!({
            "contents": [{
                "uri": CAPABILITIES_URI,
                "mimeType": "application/json",
                "text": serde_json::to_string(&manifest())?
            }]
        }))
    }

    fn tools_call(&self, params: Value) -> Value {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Request {
            name: String,
            #[serde(default)]
            arguments: Value,
        }

        let outcome = (|| -> Result<Value> {
            let request: Request = decode(params, "tools/call parameters")?;
            match request.name.as_str() {
                TOOL_RULES_SNAPSHOT => self.probe(request.arguments),
                TOOL_MEASUREMENT_VALIDATE => self.measurement_validate(request.arguments),
                TOOL_MEASUREMENT_OPEN => self.measurement_open(request.arguments),
                TOOL_MEASUREMENT_SAMPLE => self.measurement_sample(request.arguments),
                TOOL_MEASUREMENT_CLOSE => self.measurement_close(request.arguments),
                TOOL_MUTATION_PREPARE => self.mutation_prepare(request.arguments),
                TOOL_MUTATION_APPLY => self.mutation_apply(request.arguments),
                TOOL_MUTATION_STATUS => self.mutation_status(request.arguments),
                TOOL_MUTATION_VERIFY => self.mutation_verify(request.arguments),
                TOOL_MUTATION_RESTORE => self.mutation_restore(request.arguments),
                TOOL_MUTATION_FINALIZE => self.mutation_finalize(request.arguments),
                _ => bail!("unknown tool '{}'", request.name),
            }
        })();

        match outcome {
            Ok(structured) => json!({"structuredContent": structured}),
            Err(error) => json!({
                "isError": true,
                "content": [{"type": "text", "text": error.to_string()}]
            }),
        }
    }

    fn probe(&self, input: Value) -> Result<Value> {
        let request: ProbeRequest = decode(input, "rules.snapshot request")?;
        request.context.validate()?;
        let comms = validate_comms(request.arguments.comms)?;
        let response = self.control.snapshot(
            control_request_id(&request.context.operation_id, "snapshot"),
            comms,
        )?;
        let stats = require_stats(&response)?;
        Ok(json!({
            "observed_at_ns": now_ns()?,
            "data": {
                "rules": response.rules,
                "revision": response.revision,
                "rules_seq": response.rules_seq,
                "effective_digest": response.effective_digest,
                "stats": stats
            },
            "warnings": []
        }))
    }

    fn measurement_validate(&self, input: Value) -> Result<Value> {
        let request: ValidateRequest = decode(input, "measurement validate request")?;
        match request.specification.validate() {
            Ok(()) => Ok(json!({"valid": true})),
            Err(error) => Ok(json!({"valid": false, "message": error.to_string()})),
        }
    }

    fn measurement_open(&self, input: Value) -> Result<Value> {
        let request: MeasurementOpenRequest = decode(input, "measurement open request")?;
        request.context.validate()?;
        request.specification.validate()?;
        let targets = canonical_targets(request.specification.targets)?;
        let response = self.control.snapshot(
            control_request_id(&request.context.operation_id, "open"),
            target_comms(&targets),
        )?;
        let stats = require_stats(&response)?;
        let session = IntegritySessionData {
            schema_version: 1,
            targets,
            settle_timeout_ms: request.specification.settle_timeout_ms,
            task_state_errors_at_open: stats.task_state_errors,
            rule_refresh_deferred_at_open: stats.rule_refresh_deferred,
        };
        Ok(json!({
            "id": format!("classification-integrity/{}", short_digest(&request.context.operation_id)),
            "driver_data": session
        }))
    }

    fn measurement_sample(&self, input: Value) -> Result<Value> {
        let request: MeasurementSampleRequest = decode(input, "measurement sample request")?;
        request.context.validate()?;
        request.session.validate()?;
        let started_at_ns = now_ns()?;
        let started = Instant::now();
        let timeout = Duration::from_millis(request.session.driver_data.settle_timeout_ms);

        let response = loop {
            let response = self.control.snapshot(
                control_request_id(&request.context.operation_id, "sample"),
                target_comms(&request.session.driver_data.targets),
            )?;
            let integrity = Integrity::evaluate(&request.session.driver_data.targets, &response)?;
            if integrity.ready() || started.elapsed() >= timeout {
                break (response, integrity);
            }
            thread::sleep(Duration::from_millis(10).min(timeout.saturating_sub(started.elapsed())));
        };

        let (response, integrity) = response;
        let stats = require_stats(&response)?;
        let task_errors_delta = stats
            .task_state_errors
            .checked_sub(request.session.driver_data.task_state_errors_at_open)
            .ok_or_else(|| anyhow!("task_state_errors counter reset during measurement"))?;
        let deferred_delta = stats
            .rule_refresh_deferred
            .checked_sub(request.session.driver_data.rule_refresh_deferred_at_open)
            .ok_or_else(|| anyhow!("rule_refresh_deferred counter reset during measurement"))?;
        let workload_targets = request
            .session
            .driver_data
            .targets
            .iter()
            .map(|target| (target.comm.clone(), target.class))
            .collect::<Vec<_>>();
        let workload = workload_fingerprint(Path::new("/proc"), &workload_targets)?;
        let ended_at_ns = now_ns()?;

        Ok(json!({
            "started_at_ns": started_at_ns,
            "ended_at_ns": ended_at_ns,
            "quality": "valid",
            "workload_fingerprint": workload.digest,
            "metrics": {
                "active_rule_coverage": metric(integrity.active_coverage, "ratio", "gauge"),
                "persisted_rule_coverage": metric(integrity.persisted_coverage, "ratio", "gauge"),
                "active_persisted_consistency": metric(integrity.consistency, "ratio", "gauge"),
                "task_state_errors_delta": metric(task_errors_delta, "errors", "counter"),
                "rule_refresh_deferred_delta": metric(deferred_delta, "tasks", "counter")
            },
            "provenance": {
                "provider": "scx_agent_classed_mcp",
                "version": PROVIDER_VERSION,
                "revision": response.revision,
                "rules_seq": response.rules_seq,
                "effective_digest": response.effective_digest,
                "target_count": request.session.driver_data.targets.len(),
                "matched_task_count": workload.task_count
            }
        }))
    }

    fn measurement_close(&self, input: Value) -> Result<Value> {
        let session: MeasurementSession = decode(input, "measurement close request")?;
        session.validate()?;
        Ok(json!({
            "session_id": session.id,
            "cleaned_up": true,
            "details": {}
        }))
    }

    fn mutation_prepare(&self, input: Value) -> Result<Value> {
        let request: MutationPrepareRequest = decode(input, "mutation prepare request")?;
        request.context.validate()?;
        validate_comm(&request.arguments.comm)?;
        let response = self.control.get_rule(
            control_request_id(&request.context.operation_id, "prepare"),
            request.arguments.comm.clone(),
        )?;
        let baseline = require_current(&response)?;
        let observation = require_rule(&response, &request.arguments.comm)?;
        if observation.source == RuleSource::Base {
            bail!("base rule for '{}' is read-only", request.arguments.comm);
        }
        let desired = RuleState::present(request.arguments.class);
        if baseline == desired {
            bail!(
                "learned rule for '{}' already has the requested class",
                request.arguments.comm
            );
        }
        let resource = format!(
            "learned-rule/{}",
            sha256_hex(request.arguments.comm.as_bytes())
        );
        Ok(json!({
            "resource": resource,
            "baseline": {"value": baseline},
            "desired": {"value": desired},
            "driver_data": {
                "schema_version": 1,
                "comm": request.arguments.comm,
                "baseline": baseline,
                "desired": desired
            }
        }))
    }

    fn mutation_apply(&self, input: Value) -> Result<Value> {
        let request: MutationOperationRequest = decode(input, "mutation apply request")?;
        self.mutate(request, JournalKind::Apply)
    }

    fn mutation_restore(&self, input: Value) -> Result<Value> {
        let request: MutationOperationRequest = decode(input, "mutation restore request")?;
        self.mutate(request, JournalKind::Restore)
    }

    fn mutation_finalize(&self, input: Value) -> Result<Value> {
        let request: MutationOperationRequest = decode(input, "mutation finalize request")?;
        request.validate()?;
        let driver = &request.prepared.driver_data;
        let record = OperationRecord::new(
            JournalKind::Finalize,
            driver.comm.clone(),
            driver.baseline.clone(),
            driver.desired.clone(),
            request.prepared.clone(),
        );
        let existing = self.journal.begin(&request.operation_id, record)?;
        if existing.completed_as("finalized") {
            let observed = self.read_rule(
                &control_request_id(&request.operation_id, "finalize-replay"),
                &driver.comm,
            )?;
            let state = if observed.state == driver.desired
                && observed.integrity_matches(&driver.desired)
            {
                "finalized"
            } else {
                "unknown"
            };
            return Ok(receipt(&request.operation_id, state, &observed.state));
        }
        let observed = self.read_rule(
            &control_request_id(&request.operation_id, "finalize"),
            &driver.comm,
        )?;
        if observed.state != driver.desired || !observed.integrity_matches(&driver.desired) {
            bail!(
                "cannot finalize '{}': desired rule is not active and persisted",
                driver.comm
            );
        }
        self.journal.complete(&request.operation_id, "finalized")?;
        Ok(receipt(&request.operation_id, "finalized", &observed.state))
    }

    fn mutation_status(&self, input: Value) -> Result<Value> {
        let request: MutationStatusRequest = decode(input, "mutation status request")?;
        validate_id("operation_id", &request.operation_id)?;
        let Some(record) = self.journal.get(&request.operation_id)? else {
            return Ok(json!({
                "operation_id": request.operation_id,
                "state": "unknown",
                "driver_data": {}
            }));
        };
        let observed = self.read_rule(
            &control_request_id(&request.operation_id, "status"),
            &record.comm,
        )?;
        let state =
            record.infer_state(&observed.state, observed.integrity_matches(&observed.state));
        if record.is_completion(state) {
            self.journal.complete(&request.operation_id, state)?;
        }
        Ok(receipt(&request.operation_id, state, &observed.state))
    }

    fn mutation_verify(&self, input: Value) -> Result<Value> {
        let request: MutationVerifyRequest = decode(input, "mutation verify request")?;
        request.validate()?;
        let observed = self.read_rule(
            &control_request_id(&request.operation_id, "verify"),
            &request.prepared.driver_data.comm,
        )?;
        let matched = observed.state == request.expected.value
            && observed.integrity_matches(&request.expected.value);
        Ok(json!({
            "matched": matched,
            "observed": {"value": observed.state},
            "details": {
                "active_class": observed.observation.active_class,
                "persisted_class": observed.observation.persisted_class,
                "consistent": observed.observation.consistent,
                "revision": observed.revision,
                "rules_seq": observed.rules_seq
            }
        }))
    }

    fn mutate(&self, request: MutationOperationRequest, kind: JournalKind) -> Result<Value> {
        request.validate()?;
        let driver = &request.prepared.driver_data;
        let (from, to, completed_state) = match kind {
            JournalKind::Apply => (&driver.baseline, &driver.desired, "applied"),
            JournalKind::Restore => (&driver.desired, &driver.baseline, "restored"),
            JournalKind::Finalize => unreachable!(),
        };
        let record = OperationRecord::new(
            kind,
            driver.comm.clone(),
            driver.baseline.clone(),
            driver.desired.clone(),
            request.prepared.clone(),
        );
        let existing = self.journal.begin(&request.operation_id, record)?;
        if existing.completed_as(completed_state) {
            let observed = self.read_rule(
                &control_request_id(&request.operation_id, "replay"),
                &driver.comm,
            )?;
            let state = if observed.state == *to && observed.integrity_matches(to) {
                completed_state
            } else {
                "unknown"
            };
            return Ok(receipt(&request.operation_id, state, &observed.state));
        }

        let current = self.read_rule(
            &control_request_id(&request.operation_id, "preflight"),
            &driver.comm,
        )?;
        if current.state == *to && current.integrity_matches(to) {
            self.journal
                .complete(&request.operation_id, completed_state)?;
            return Ok(receipt(
                &request.operation_id,
                completed_state,
                &current.state,
            ));
        }
        if current.state != *from {
            bail!(
                "rule '{}' drifted: expected {:?}, observed {:?}",
                driver.comm,
                from,
                current.state
            );
        }

        let response = self.control.compare_and_set(
            request.operation_id.clone(),
            driver.comm.clone(),
            from.clone(),
            to.clone(),
        )?;
        if !matches!(
            response.status,
            ControlStatus::Applied | ControlStatus::Noop
        ) {
            bail!(control_failure(&response));
        }
        let observed = self.read_rule(
            &control_request_id(&request.operation_id, "readback"),
            &driver.comm,
        )?;
        if observed.state != *to || !observed.integrity_matches(to) {
            bail!(
                "rule '{}' CAS readback did not match the requested state",
                driver.comm
            );
        }
        self.journal
            .complete(&request.operation_id, completed_state)?;
        Ok(receipt(
            &request.operation_id,
            completed_state,
            &observed.state,
        ))
    }

    fn read_rule(&self, request_id: &str, comm: &str) -> Result<ObservedRule> {
        let response = self
            .control
            .get_rule(request_id.to_string(), comm.to_string())?;
        Ok(ObservedRule {
            state: require_current(&response)?,
            observation: require_rule(&response, comm)?.clone(),
            revision: response.revision,
            rules_seq: response.rules_seq,
        })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvocationContext {
    episode_id: u64,
    operation_id: String,
}

impl InvocationContext {
    fn validate(&self) -> Result<()> {
        let _ = self.episode_id;
        validate_id("operation_id", &self.operation_id)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeRequest {
    context: InvocationContext,
    arguments: SnapshotArguments,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotArguments {
    comms: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    comm: String,
    class: RuleClass,
}

impl Target {
    fn validate(&self) -> Result<()> {
        validate_comm(&self.comm)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegritySpecification {
    targets: Vec<Target>,
    settle_timeout_ms: u64,
}

impl IntegritySpecification {
    fn validate(&self) -> Result<()> {
        if self.settle_timeout_ms > 5_000 {
            bail!("settle_timeout_ms must not exceed 5000");
        }
        canonical_targets(self.targets.clone()).map(|_| ())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidateRequest {
    specification: IntegritySpecification,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeasurementOpenRequest {
    context: InvocationContext,
    specification: IntegritySpecification,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegritySessionData {
    schema_version: u32,
    targets: Vec<Target>,
    settle_timeout_ms: u64,
    task_state_errors_at_open: u64,
    rule_refresh_deferred_at_open: u64,
}

impl IntegritySessionData {
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!("unsupported measurement session schema_version");
        }
        if self.settle_timeout_ms > 5_000 {
            bail!("measurement session settle_timeout_ms exceeds 5000");
        }
        if canonical_targets(self.targets.clone())? != self.targets {
            bail!("measurement session targets are not canonical");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeasurementSession {
    id: String,
    driver_data: IntegritySessionData,
}

impl MeasurementSession {
    fn validate(&self) -> Result<()> {
        validate_id("measurement session id", &self.id)?;
        self.driver_data.validate()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MeasurementSampleRequest {
    context: InvocationContext,
    session: MeasurementSession,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpsertArguments {
    comm: String,
    class: RuleClass,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationPrepareRequest {
    context: InvocationContext,
    arguments: UpsertArguments,
}

struct ObservedRule {
    state: RuleState,
    observation: RuleObservation,
    revision: u64,
    rules_seq: u64,
}

impl ObservedRule {
    fn integrity_matches(&self, expected: &RuleState) -> bool {
        self.observation.consistent
            && self.observation.active_class == expected.class
            && self.observation.persisted_class == expected.class
    }
}

struct Integrity {
    active_coverage: f64,
    persisted_coverage: f64,
    consistency: f64,
}

impl Integrity {
    fn evaluate(targets: &[Target], response: &ControlResponse) -> Result<Self> {
        if targets.is_empty() {
            bail!("measurement has no targets");
        }
        let mut active = 0usize;
        let mut persisted = 0usize;
        let mut consistent = 0usize;
        for target in targets {
            let rule = require_rule(response, &target.comm)?;
            active += usize::from(rule.active_class == Some(target.class));
            persisted += usize::from(rule.persisted_class == Some(target.class));
            consistent += usize::from(rule.consistent && rule.active_class == rule.persisted_class);
        }
        let count = targets.len() as f64;
        Ok(Self {
            active_coverage: active as f64 / count,
            persisted_coverage: persisted as f64 / count,
            consistency: consistent as f64 / count,
        })
    }

    fn ready(&self) -> bool {
        self.active_coverage == 1.0 && self.persisted_coverage == 1.0 && self.consistency == 1.0
    }
}

fn decode<T: DeserializeOwned>(value: Value, label: &str) -> Result<T> {
    serde_json::from_value(value).with_context(|| format!("invalid {label}"))
}

fn validate_comms(comms: Vec<String>) -> Result<Vec<String>> {
    if comms.is_empty() || comms.len() > MAX_TARGETS {
        bail!("comms must contain 1..={MAX_TARGETS} entries");
    }
    let mut unique = BTreeSet::new();
    for comm in comms {
        validate_comm(&comm)?;
        if !unique.insert(comm) {
            bail!("comms contains a duplicate entry");
        }
    }
    Ok(unique.into_iter().collect())
}

fn canonical_targets(mut targets: Vec<Target>) -> Result<Vec<Target>> {
    if targets.is_empty() || targets.len() > MAX_TARGETS {
        bail!("targets must contain 1..={MAX_TARGETS} entries");
    }
    for target in &targets {
        target.validate()?;
    }
    targets.sort_by(|left, right| left.comm.cmp(&right.comm));
    if targets.windows(2).any(|pair| pair[0].comm == pair[1].comm) {
        bail!("targets contains duplicate comm entries");
    }
    Ok(targets)
}

fn target_comms(targets: &[Target]) -> Vec<String> {
    targets.iter().map(|target| target.comm.clone()).collect()
}

fn receipt(operation_id: &str, state: &str, observed: &RuleState) -> Value {
    json!({
        "operation_id": operation_id,
        "state": state,
        "observed": {"value": observed},
        "driver_data": {}
    })
}

fn metric(value: impl Serialize, unit: &str, kind: &str) -> Value {
    json!({"value": value, "unit": unit, "kind": kind})
}

fn now_ns() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos())
}

fn short_digest(value: &str) -> String {
    sha256_hex(value.as_bytes())[..32].to_string()
}

fn control_request_id(operation_id: &str, phase: &str) -> String {
    let candidate = format!("{operation_id}/{phase}");
    if candidate.len() <= 256 {
        candidate
    } else {
        format!("mcp/{phase}/{}", sha256_hex(operation_id.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::BufReader;
    use std::sync::Mutex;

    use super::*;
    use crate::control_wire::{ControlStats, CONTROL_VERSION};
    use crate::journal::{MutationDriverData, MutationState, PreparedMutation, ProviderPin};
    use crate::rpc::{read_bounded_frame, Frame};

    #[test]
    fn manifest_declares_exact_v1_capabilities() {
        let manifest = manifest();
        let capabilities = manifest["capabilities"].as_array().unwrap();
        let ids = capabilities
            .iter()
            .map(|capability| capability["id"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            ids,
            BTreeSet::from([
                "rules.snapshot.v1",
                "rule.upsert.v1",
                "classification.integrity.v1"
            ])
        );
        let mutation = capabilities
            .iter()
            .find(|capability| capability["kind"] == "mutation")
            .unwrap();
        assert_eq!(mutation["effect"], "reversible_mutation");
        assert_eq!(mutation["idempotent"], true);
        assert_eq!(
            mutation["allowed_phases"],
            json!(["clean", "experimenting"])
        );

        let measurement = capabilities
            .iter()
            .find(|capability| capability["id"] == "classification.integrity.v1")
            .unwrap();
        let output = &measurement["output_schema"]["properties"];
        for metric in [
            "active_rule_coverage",
            "persisted_rule_coverage",
            "active_persisted_consistency",
            "task_state_errors_delta",
            "rule_refresh_deferred_delta",
        ] {
            assert!(output[metric]["description"]
                .as_str()
                .is_some_and(|description| !description.is_empty()));
        }
    }

    #[test]
    fn canonical_targets_reject_duplicate_comm() {
        let error = canonical_targets(vec![
            Target {
                comm: "worker".into(),
                class: RuleClass::Latency,
            },
            Target {
                comm: "worker".into(),
                class: RuleClass::Batch,
            },
        ])
        .unwrap_err();
        assert!(error.to_string().contains("duplicate comm"));
    }

    #[test]
    fn bounded_frame_drains_oversized_input() {
        let input = b"123456\n{}\n";
        let mut reader = BufReader::new(&input[..]);
        assert!(matches!(
            read_bounded_frame(&mut reader, 4).unwrap(),
            Some(Frame::TooLarge)
        ));
        let Some(Frame::Data(next)) = read_bounded_frame(&mut reader, 4).unwrap() else {
            panic!("expected the next frame");
        };
        assert_eq!(next, b"{}");
    }

    #[test]
    fn journal_is_durable_and_rejects_operation_id_reuse() {
        let root = temporary_path("journal");
        let journal = Journal::new(root.join("operations.json"));
        let prepared = prepared(
            "worker",
            RuleState::absent(),
            RuleState::present(RuleClass::Batch),
        );
        let record = OperationRecord::new(
            JournalKind::Apply,
            "worker".into(),
            RuleState::absent(),
            RuleState::present(RuleClass::Batch),
            prepared,
        );
        journal.begin("episode-1/apply", record.clone()).unwrap();
        journal.complete("episode-1/apply", "applied").unwrap();

        let loaded = Journal::new(root.join("operations.json"))
            .get("episode-1/apply")
            .unwrap()
            .unwrap();
        assert!(loaded.completed_as("applied"));

        let mut changed = record;
        changed.desired = RuleState::present(RuleClass::Latency);
        assert!(journal.begin("episode-1/apply", changed).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn apply_is_cas_backed_and_idempotent() {
        let root = temporary_path("apply");
        fs::create_dir_all(&root).unwrap();
        let server = Server {
            control: FakeControl::new(RuleState::absent()),
            journal: Journal::new(root.join("journal.json")),
        };
        let prepared = prepared(
            "worker",
            RuleState::absent(),
            RuleState::present(RuleClass::Latency),
        );
        let request = json!({
            "operation_id": "episode-1/apply",
            "prepared": prepared
        });
        let first = server.mutation_apply(request.clone()).unwrap();
        assert_eq!(first["state"], "applied");
        let second = server.mutation_apply(request).unwrap();
        assert_eq!(second, first);
        assert_eq!(
            *server.control.state.lock().unwrap(),
            RuleState::present(RuleClass::Latency)
        );
        assert_eq!(*server.control.cas_calls.lock().unwrap(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepare_rejects_an_existing_target_class() {
        let root = temporary_path("prepare-noop");
        let server = Server {
            control: FakeControl::new(RuleState::present(RuleClass::Latency)),
            journal: Journal::new(root.join("journal.json")),
        };
        let error = server
            .mutation_prepare(json!({
                "context": {"episode_id": 1, "operation_id": "episode-1/prepare"},
                "arguments": {"comm": "worker", "class": "latency"}
            }))
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("already has the requested class"));
    }

    struct FakeControl {
        state: Mutex<RuleState>,
        cas_calls: Mutex<u64>,
    }

    impl FakeControl {
        fn new(state: RuleState) -> Self {
            Self {
                state: Mutex::new(state),
                cas_calls: Mutex::new(0),
            }
        }

        fn response(
            request_id: String,
            comm: &str,
            state: &RuleState,
            status: ControlStatus,
        ) -> ControlResponse {
            ControlResponse {
                version: CONTROL_VERSION,
                request_id,
                status,
                current: Some(state.clone()),
                rules: vec![observation(comm, state)],
                revision: 1,
                rules_seq: 2,
                effective_digest: "sha256:test".into(),
                stats: Some(ControlStats {
                    task_state_errors: 0,
                    rule_refresh_deferred: 0,
                }),
                workload_fingerprint: Some("workload-1".into()),
                message: None,
            }
        }
    }

    impl SchedulerControl for FakeControl {
        fn get_rule(&self, request_id: String, comm: String) -> Result<ControlResponse> {
            let state = self.state.lock().unwrap();
            Ok(Self::response(request_id, &comm, &state, ControlStatus::Ok))
        }

        fn snapshot(&self, request_id: String, comms: Vec<String>) -> Result<ControlResponse> {
            let state = self.state.lock().unwrap();
            let mut response = Self::response(request_id, &comms[0], &state, ControlStatus::Ok);
            response.rules = comms.iter().map(|comm| observation(comm, &state)).collect();
            Ok(response)
        }

        fn compare_and_set(
            &self,
            request_id: String,
            comm: String,
            expected: RuleState,
            desired: RuleState,
        ) -> Result<ControlResponse> {
            *self.cas_calls.lock().unwrap() += 1;
            let mut state = self.state.lock().unwrap();
            let status = if *state != expected {
                ControlStatus::Conflict
            } else if *state == desired {
                ControlStatus::Noop
            } else {
                *state = desired;
                ControlStatus::Applied
            };
            Ok(Self::response(request_id, &comm, &state, status))
        }
    }

    fn observation(comm: &str, state: &RuleState) -> RuleObservation {
        RuleObservation {
            comm: comm.into(),
            class: state.class.unwrap_or(RuleClass::Batch),
            source: if state.present {
                RuleSource::Learned
            } else {
                RuleSource::Default
            },
            active_class: state.class,
            persisted_class: state.class,
            consistent: true,
        }
    }

    fn prepared(comm: &str, baseline: RuleState, desired: RuleState) -> PreparedMutation {
        PreparedMutation {
            capability_id: "mcp/test/rule.upsert.v1".into(),
            provider: ProviderPin {
                provider_id: "mcp/test/scx-agent-classed".into(),
                provider_version: PROVIDER_VERSION.into(),
                provider_class: "mcp".into(),
                manifest_digest: "sha256:test".into(),
            },
            resource: format!(
                "mcp/test/scx-agent-classed/learned-rule/{}",
                sha256_hex(comm.as_bytes())
            ),
            baseline: state_with_digest(baseline.clone()),
            desired: state_with_digest(desired.clone()),
            driver_data: MutationDriverData {
                schema_version: 1,
                comm: comm.into(),
                baseline,
                desired,
            },
        }
    }

    fn state_with_digest(value: RuleState) -> MutationState {
        let canonical = serde_json::to_value(&value).unwrap();
        let digest = format!(
            "sha256:{}",
            sha256_hex(&serde_json::to_vec(&canonical).unwrap())
        );
        MutationState { value, digest }
    }

    fn temporary_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "scx-agent-classed-mcp-{label}-{}-{}",
            std::process::id(),
            now_ns().unwrap()
        ))
    }
}
