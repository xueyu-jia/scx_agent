use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::{
    AgentCommand, AgentReasoner, AgentToolInvocation, AgentToolResult, AgentTurn, ToolCatalog,
    ToolDispatcher,
};
use crate::audit::{AuditRecord, AuditSink};
use crate::capability::CapabilitySnapshot;
use crate::domain::{
    Candidate, ChangeId, CommitId, ContractId, Digest, EpisodeId, EpisodePhase, InvocationContext,
    OperationId, TransactionId,
};
use crate::kernel::evaluation::{
    AbEvaluationEvidence, AbEvaluationProtocol, ContractFreezer, EvaluationDecision,
    EvaluationErrorKind, EvaluationIntentSpec, EvaluationVerdict, FrozenEvaluationIntent,
};
use crate::kernel::transaction::{
    ChangeState, TransactionErrorKind, TransactionKernel, TransactionStore, TransactionWal,
};
use crate::runtime::episode::{AgentAction, CommitStep, EpisodeStateMachine};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EpisodeOutcome {
    pub episode_id: EpisodeId,
    pub phase: EpisodePhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<EvaluationIntentSummary>,
    pub decision: Option<EvaluationDecision>,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EvaluationIntentSummary {
    pub objective: String,
    pub intent_digest: Digest,
    pub contract_id: ContractId,
    pub contract_digest: Digest,
}

impl From<&FrozenEvaluationIntent> for EvaluationIntentSummary {
    fn from(intent: &FrozenEvaluationIntent) -> Self {
        Self {
            objective: intent.objective().as_str().to_string(),
            intent_digest: intent.digest().clone(),
            contract_id: intent.contract().id().clone(),
            contract_digest: intent.contract().digest().clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeError {
    pub message: String,
}

impl RuntimeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeError {}

pub struct EpisodeCoordinator<'a> {
    max_rounds: usize,
    evaluation_timeout: Duration,
    capabilities: CapabilitySnapshot,
    transactions: &'a TransactionStore,
    audit: &'a mut dyn AuditSink,
}

impl<'a> EpisodeCoordinator<'a> {
    pub fn new(
        max_rounds: usize,
        evaluation_timeout: Duration,
        capabilities: CapabilitySnapshot,
        transactions: &'a TransactionStore,
        audit: &'a mut dyn AuditSink,
    ) -> Result<Self, RuntimeError> {
        if max_rounds == 0 {
            return Err(RuntimeError::new(
                "episode max_rounds must be greater than zero",
            ));
        }
        if evaluation_timeout.is_zero() {
            return Err(RuntimeError::new(
                "episode evaluation timeout must be greater than zero",
            ));
        }
        Ok(Self {
            max_rounds,
            evaluation_timeout,
            capabilities,
            transactions,
            audit,
        })
    }

    pub fn run(
        &mut self,
        episode_id: EpisodeId,
        activation: Value,
        reasoner: &mut dyn AgentReasoner,
    ) -> Result<EpisodeOutcome, RuntimeError> {
        let catalog = ToolCatalog::from_snapshot(&self.capabilities);
        let dispatcher = ToolDispatcher::new(&catalog);
        let mut session = EpisodeSession::new(
            episode_id,
            self.evaluation_timeout,
            self.capabilities.clone(),
            self.transactions,
        );
        self.record(AuditRecord::episode(
            "episode_started",
            episode_id,
            session.state.phase(),
            json!({
                "activation": activation,
                "capability_generation": self.capabilities.generation(),
                "capability_count": self.capabilities.len(),
                "evaluation_timeout_ms": self.evaluation_timeout.as_millis(),
            }),
        ))?;

        let context = json!({
            "episode_id": episode_id.get(),
            "activation": activation,
            "capability_generation": self.capabilities.generation(),
            "runtime_policy": {
                "probe_is_a_capability": true,
                "evaluation_protocol": "fixed_ab",
                "evaluation_timeout_ms": self.evaluation_timeout.as_millis(),
                "commit_authority": "runtime_only",
                "commit_pending_blocks_agent_calls": true,
            }
        });
        let first_turn = reasoner.begin(&context, catalog.specs());
        let mut turn = match first_turn {
            Ok(turn) => turn,
            Err(error) => {
                session.fail_closed(format!("reasoner failed before acting: {error}"));
                return self.finish_session(session);
            }
        };

        'rounds: for round in 0..self.max_rounds {
            match turn {
                AgentTurn::Final(content) => {
                    if let Err(error) = self.record(AuditRecord::episode(
                        "agent_final",
                        episode_id,
                        session.state.phase(),
                        json!({"round": round + 1, "content": content}),
                    )) {
                        session.fail_closed(error.to_string());
                        break;
                    }
                    session.summary = "agent ended without a commit request".to_string();
                    break;
                }
                AgentTurn::ToolCalls(calls) => {
                    if calls.is_empty() {
                        session.summary = "agent returned an empty tool-call batch".to_string();
                        break;
                    }
                    let mut results = Vec::with_capacity(calls.len());
                    for invocation in calls {
                        if let Err(error) = self.record(AuditRecord::episode(
                            "agent_command",
                            episode_id,
                            session.state.phase(),
                            json!({
                                "round": round + 1,
                                "call_id": invocation.id,
                                "tool": invocation.name,
                                "arguments": invocation.arguments,
                            }),
                        )) {
                            session.fail_closed(error.to_string());
                            break 'rounds;
                        }
                        let result = match dispatcher.decode(&invocation) {
                            Ok(command) => session.execute(command, &invocation),
                            Err(error) => AgentToolResult::failure(&invocation, error.to_string()),
                        };
                        if let Err(error) = self.record(AuditRecord::episode(
                            "agent_command_result",
                            episode_id,
                            session.state.phase(),
                            json!({
                                "call_id": result.call_id,
                                "tool": result.name,
                                "ok": result.ok,
                                "content": result.content,
                            }),
                        )) {
                            session.fail_closed(error.to_string());
                            break 'rounds;
                        }
                        results.push(result);
                        if session.state.is_reasoning_stopped() {
                            break;
                        }
                    }
                    if session.state.is_reasoning_stopped() {
                        break;
                    }
                    if round + 1 == self.max_rounds {
                        session.summary = format!(
                            "agent reached the configured limit of {} reasoning rounds",
                            self.max_rounds
                        );
                        break;
                    }
                    turn = match reasoner.resume(&results) {
                        Ok(turn) => turn,
                        Err(error) => {
                            session.fail_closed(format!("reasoner failed: {error}"));
                            break;
                        }
                    };
                }
            }
        }

        self.finish_session(session)
    }

    fn finish_session(
        &mut self,
        mut session: EpisodeSession<'_>,
    ) -> Result<EpisodeOutcome, RuntimeError> {
        if !matches!(
            session.state.phase(),
            EpisodePhase::Committed | EpisodePhase::RecoveryRequired
        ) {
            if let Err(error) = session.rollback_open_transaction() {
                session.state.require_recovery();
                session.summary = format!("episode cleanup failed: {error}");
            } else if let Err(error) = session.state.finish_clean() {
                session.state.require_recovery();
                session.summary = format!("episode finalization failed: {error}");
            }
        }
        let intent = session
            .state
            .frozen_intent()
            .map(EvaluationIntentSummary::from);
        let outcome = EpisodeOutcome {
            episode_id: session.episode_id,
            phase: session.state.phase(),
            intent,
            decision: session.decision,
            summary: session.summary,
        };
        self.record(AuditRecord::episode(
            "episode_finished",
            outcome.episode_id,
            outcome.phase,
            serde_json::to_value(&outcome)
                .map_err(|error| RuntimeError::new(format!("failed to encode outcome: {error}")))?,
        ))?;
        Ok(outcome)
    }

    fn record(&mut self, record: AuditRecord) -> Result<(), RuntimeError> {
        self.audit
            .record(&record)
            .map_err(|error| RuntimeError::new(format!("audit write failed: {error}")))
    }
}

struct EpisodeSession<'a> {
    episode_id: EpisodeId,
    evaluation_timeout: Duration,
    capabilities: CapabilitySnapshot,
    transactions: &'a TransactionStore,
    state: EpisodeStateMachine,
    transaction: Option<TransactionKernel>,
    next_transaction: u64,
    next_change: u64,
    next_operation: u64,
    decision: Option<EvaluationDecision>,
    summary: String,
}

impl<'a> EpisodeSession<'a> {
    fn new(
        episode_id: EpisodeId,
        evaluation_timeout: Duration,
        capabilities: CapabilitySnapshot,
        transactions: &'a TransactionStore,
    ) -> Self {
        Self {
            episode_id,
            evaluation_timeout,
            capabilities,
            transactions,
            state: EpisodeStateMachine::new(episode_id),
            transaction: None,
            next_transaction: 0,
            next_change: 0,
            next_operation: 0,
            decision: None,
            summary: "episode ended cleanly without a commit".to_string(),
        }
    }

    fn execute(
        &mut self,
        command: AgentCommand,
        invocation: &AgentToolInvocation,
    ) -> AgentToolResult {
        let result = match command {
            AgentCommand::Probe {
                capability_id,
                arguments,
                ..
            } => self.probe(capability_id, arguments),
            AgentCommand::BeginExperiment { intent, .. } => self.begin_experiment(intent),
            AgentCommand::Mutation {
                capability_id,
                arguments,
                reason,
                ..
            } => self.mutate(capability_id, arguments, reason),
            AgentCommand::RequestCommit {
                change_ids, reason, ..
            } => self.request_commit(change_ids, reason),
            AgentCommand::Abort { reason, .. } => self.abort(reason),
        };
        match result {
            Ok(content) => AgentToolResult::success(invocation, content),
            Err(error) => AgentToolResult::failure(invocation, error),
        }
    }

    fn probe(
        &mut self,
        capability_id: crate::domain::CapabilityId,
        arguments: Value,
    ) -> Result<Value, String> {
        self.state
            .ensure_agent_action(AgentAction::Probe)
            .map_err(|error| error.to_string())?;
        let meta = self
            .capabilities
            .meta(&capability_id)
            .ok_or_else(|| format!("probe capability '{capability_id}' is unavailable"))?;
        ensure_allowed_phase(meta, self.state.phase())?;
        let max_output_bytes = meta.limits.max_output_bytes;
        let provider = self
            .capabilities
            .probe(&capability_id)
            .ok_or_else(|| format!("probe capability '{capability_id}' is unavailable"))?;
        let evidence = provider
            .probe(&crate::domain::ProbeRequest {
                context: self.context("probe")?,
                arguments,
            })
            .map_err(|error| format!("probe '{capability_id}' failed: {error}"))?;
        let evidence = serde_json::to_value(evidence).map_err(|error| error.to_string())?;
        let encoded = serde_json::to_vec(&evidence).map_err(|error| error.to_string())?;
        if encoded.len() > max_output_bytes {
            return Err(format!(
                "probe '{capability_id}' exceeded its {} byte output limit",
                max_output_bytes
            ));
        }
        Ok(evidence)
    }

    fn begin_experiment(&mut self, spec: EvaluationIntentSpec) -> Result<Value, String> {
        self.state
            .ensure_agent_action(AgentAction::BeginExperiment)
            .map_err(|error| error.to_string())?;
        let contract_id = ContractId::new(format!("episode-{}/contract", self.episode_id.get()))?;
        let contract = ContractFreezer::new(self.capabilities.clone())
            .freeze(contract_id.clone(), spec.evaluation_contract)
            .map_err(|error| error.to_string())?;
        AbEvaluationProtocol::new(self.capabilities.clone(), self.evaluation_timeout)
            .map_err(|error| error.to_string())?
            .validate_contract(&contract)
            .map_err(|error| error.to_string())?;
        let intent = FrozenEvaluationIntent::from_parts(self.episode_id, spec.objective, contract)
            .map_err(|error| error.to_string())?;
        let response = json!({
            "objective": intent.objective(),
            "intent_digest": intent.digest(),
            "contract_id": intent.contract().id(),
            "contract_digest": intent.contract().digest(),
            "capability_generation": self.capabilities.generation(),
            "status": "frozen"
        });
        self.state
            .freeze_intent(intent)
            .map_err(|error| error.to_string())?;
        Ok(response)
    }

    fn mutate(
        &mut self,
        capability_id: crate::domain::CapabilityId,
        arguments: Value,
        reason: String,
    ) -> Result<Value, String> {
        self.state
            .ensure_agent_action(AgentAction::Mutation)
            .map_err(|error| error.to_string())?;
        let intent = self
            .state
            .frozen_intent()
            .ok_or_else(|| "frozen evaluation intent is missing".to_string())?;
        AbEvaluationProtocol::ensure_schedule_fits(intent.contract(), self.evaluation_timeout)
            .map_err(|error| error.to_string())?;
        let meta = self
            .capabilities
            .meta(&capability_id)
            .ok_or_else(|| format!("mutation capability '{capability_id}' is unavailable"))?;
        // Every mutation effect runs only after the transaction has entered Experimenting.
        ensure_allowed_phase(meta, EpisodePhase::Experimenting)?;
        if let Err(error) = self.ensure_transaction() {
            self.summary = error.clone();
            return Err(error);
        }
        self.next_change = self.next_change.saturating_add(1);
        let change_id = ChangeId::new(format!(
            "episode-{}/change-{}",
            self.episode_id.get(),
            self.next_change
        ))?;
        let result = self
            .transaction
            .as_mut()
            .expect("transaction was initialized")
            .experiment(change_id.clone(), capability_id.clone(), arguments);
        match result {
            Ok(change) => {
                let transaction_id = change.transaction_id.clone();
                self.state
                    .mutation_applied(&transaction_id)
                    .map_err(|error| error.to_string())?;
                Ok(json!({
                    "change": {
                        "change_id": change.change_id,
                        "capability_id": change.capability_id,
                        "resource": change.resource,
                        "state": change.state,
                    },
                    "reason": reason,
                    "status": "applied_verified"
                }))
            }
            Err(error) => {
                let rollback = self.rollback_open_transaction();
                if let Err(rollback_error) = rollback {
                    let message = format!(
                        "mutation '{capability_id}' failed: {error}; rollback failed: {rollback_error}"
                    );
                    self.state.require_recovery();
                    self.summary = message.clone();
                    Err(message)
                } else {
                    let message = format!(
                        "mutation '{capability_id}' failed and the transaction was rolled back: {error}"
                    );
                    self.summary = message.clone();
                    Err(message)
                }
            }
        }
    }

    fn request_commit(
        &mut self,
        change_ids: Vec<ChangeId>,
        reason: String,
    ) -> Result<Value, String> {
        self.state
            .ensure_agent_action(AgentAction::RequestCommit)
            .map_err(|error| error.to_string())?;
        let candidate = self.canonical_candidate(&change_ids)?;
        self.state
            .request_commit(candidate.digest().clone())
            .map_err(|error| error.to_string())?;
        self.state
            .advance_commit(CommitStep::RestoringBaseline)
            .map_err(|error| error.to_string())?;
        let context = self.context("evaluate")?;
        let intent = self
            .state
            .frozen_intent()
            .ok_or_else(|| "frozen evaluation intent is missing".to_string())?;
        let evidence =
            AbEvaluationProtocol::new(self.capabilities.clone(), self.evaluation_timeout)
                .map_err(|error| error.to_string())?
                .evaluate(
                    self.transaction
                        .as_mut()
                        .ok_or_else(|| "transaction is missing".to_string())?,
                    &context,
                    intent,
                    &candidate,
                );
        let evidence = match evidence {
            Ok(evidence) => evidence,
            Err(error) => {
                let message = format!("A/B evaluation failed: {error}");
                self.summary = message.clone();
                self.rollback_after_commit_failure(&message)?;
                if error.kind == EvaluationErrorKind::Cleanup {
                    self.state.require_recovery();
                    self.summary = message.clone();
                }
                return Err(message);
            }
        };
        for step in [
            CommitStep::MeasuringBaseline,
            CommitStep::ReplayingCandidate,
            CommitStep::MeasuringCandidate,
            CommitStep::Comparing,
        ] {
            self.state
                .advance_commit(step)
                .map_err(|error| error.to_string())?;
        }
        self.decision = Some(evidence.decision.clone());
        match evidence.decision.verdict {
            EvaluationVerdict::Improved => self.finalize(candidate, reason, evidence),
            verdict => {
                let summary = format!("candidate was not committed: {verdict:?}");
                self.summary = summary.clone();
                self.rollback_after_commit_failure(&summary)?;
                Ok(json!({
                    "committed": false,
                    "reason": reason,
                    "evaluation": evidence,
                    "phase": self.state.phase(),
                }))
            }
        }
    }

    fn finalize(
        &mut self,
        candidate: Candidate,
        reason: String,
        evidence: AbEvaluationEvidence,
    ) -> Result<Value, String> {
        let authorization = match evidence.commit_authorization() {
            Ok(authorization) => authorization,
            Err(error) => {
                let message = format!("commit authorization failed: {error}");
                self.summary = message.clone();
                self.rollback_after_commit_failure(&message)?;
                return Err(message);
            }
        };
        self.state
            .advance_commit(CommitStep::Finalizing)
            .map_err(|error| error.to_string())?;
        let finalized = self
            .transaction
            .as_mut()
            .expect("transaction exists during finalize")
            .finalize_candidate(&candidate, &authorization);
        let finalized = match finalized {
            Ok(finalized) => finalized,
            Err(error) if error.kind == TransactionErrorKind::CommitOutcomeUnknown => {
                let message = format!(
                    "candidate finalize outcome could not be established; recovery is required: {error}"
                );
                self.state.require_recovery();
                self.summary = message.clone();
                return Err(message);
            }
            Err(error) => {
                let message = format!("candidate finalize failed: {error}");
                self.summary = message.clone();
                self.rollback_after_commit_failure(&message)?;
                return Err(message);
            }
        };
        let commit_id = CommitId::new(format!(
            "episode-{}/{}",
            self.episode_id.get(),
            candidate.digest()
        ))?;
        self.state
            .commit_completed()
            .map_err(|error| error.to_string())?;
        self.summary = "candidate passed A/B evaluation and was committed".to_string();
        let finalized_changes = finalized
            .iter()
            .map(|change| {
                json!({
                    "change_id": change.change_id,
                    "capability_id": change.capability_id,
                    "resource": change.resource,
                    "state": change.state,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "committed": true,
            "commit_id": commit_id,
            "reason": reason,
            "evaluation": evidence,
            "finalized_changes": finalized_changes,
        }))
    }

    fn abort(&mut self, reason: String) -> Result<Value, String> {
        self.state
            .ensure_agent_action(AgentAction::Abort)
            .map_err(|error| error.to_string())?;
        self.state.stop_reasoning();
        self.rollback_open_transaction()?;
        self.state
            .finish_clean()
            .map_err(|error| error.to_string())?;
        self.summary = format!("agent aborted the experiment: {reason}");
        Ok(json!({"aborted": true, "reason": reason, "phase": self.state.phase()}))
    }

    fn ensure_transaction(&mut self) -> Result<(), String> {
        if self.transaction.is_some() {
            return Ok(());
        }
        self.next_transaction = self.next_transaction.saturating_add(1);
        let transaction_id = TransactionId::new(format!(
            "episode-{}/transaction-{}",
            self.episode_id.get(),
            self.next_transaction
        ))?;
        let intent_pin = self
            .state
            .frozen_intent()
            .ok_or_else(|| "frozen evaluation intent is missing".to_string())?
            .pin()
            .clone();
        let wal: Box<dyn TransactionWal> = match self.transactions.create(&transaction_id) {
            Ok(wal) => Box::new(wal),
            Err(error) => {
                self.state.require_recovery();
                return Err(format!(
                    "transaction WAL creation outcome is unsafe; recovery is required: {error}"
                ));
            }
        };
        let mut transaction = match TransactionKernel::begin(
            transaction_id.clone(),
            intent_pin,
            self.capabilities.clone(),
            wal,
        ) {
            Ok(transaction) => transaction,
            Err(error) => {
                self.state.require_recovery();
                return Err(format!(
                    "transaction Started outcome is unknown; recovery is required: {error}"
                ));
            }
        };
        if let Err(error) = self.state.begin_transaction(transaction_id) {
            return match transaction.rollback_all() {
                Ok(_) => {
                    self.state.stop_reasoning();
                    if let Err(finish_error) = self.state.finish_clean() {
                        self.state.require_recovery();
                        Err(format!(
                            "{error}; rejected transaction was rolled back but episode finalization failed: {finish_error}"
                        ))
                    } else {
                        Err(format!(
                            "{error}; rejected transaction was rolled back and the episode ended"
                        ))
                    }
                }
                Err(rollback_error) => {
                    self.state.require_recovery();
                    Err(format!(
                        "{error}; failed to seal rejected transaction rollback; recovery is required: {rollback_error}"
                    ))
                }
            };
        }
        self.transaction = Some(transaction);
        Ok(())
    }

    fn canonical_candidate(&self, requested: &[ChangeId]) -> Result<Candidate, String> {
        let requested = requested.iter().collect::<BTreeSet<_>>();
        let transaction = self
            .transaction
            .as_ref()
            .ok_or_else(|| "commit requires an active transaction".to_string())?;
        let known = transaction
            .changes()
            .map(|change| &change.change_id)
            .collect::<BTreeSet<_>>();
        if let Some(unknown) = requested.difference(&known).next() {
            return Err(format!("candidate references unknown change '{unknown}'"));
        }
        let canonical = transaction
            .changes()
            .filter(|change| requested.contains(&change.change_id))
            .map(|change| {
                if change.state != ChangeState::AppliedVerified {
                    Err(format!(
                        "change '{}' is not in applied_verified state",
                        change.change_id
                    ))
                } else {
                    Ok(change.change_id.clone())
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Candidate::new(canonical)
    }

    fn rollback_after_commit_failure(&mut self, reason: &str) -> Result<(), String> {
        if let Err(error) = self.rollback_open_transaction() {
            let message = format!("{reason}; rollback failed: {error}");
            self.state.require_recovery();
            self.summary = message.clone();
            return Err(message);
        }
        Ok(())
    }

    fn rollback_open_transaction(&mut self) -> Result<(), String> {
        let phase = self.state.phase();
        let Some(transaction) = self.transaction.as_mut() else {
            if matches!(
                phase,
                EpisodePhase::Experimenting
                    | EpisodePhase::CommitPending
                    | EpisodePhase::RollingBack
            ) {
                return Err(format!(
                    "cannot complete rollback in phase {phase:?}: transaction is missing"
                ));
            }
            return Ok(());
        };
        let starts_rollback = matches!(
            phase,
            EpisodePhase::Experimenting | EpisodePhase::CommitPending
        );
        let completes_rollback = starts_rollback || phase == EpisodePhase::RollingBack;
        if starts_rollback {
            self.state
                .begin_rollback()
                .map_err(|error| error.to_string())?;
        }
        transaction
            .rollback_all()
            .map_err(|error| error.to_string())?;
        if completes_rollback {
            self.state
                .rollback_completed()
                .map_err(|error| error.to_string())?;
        }
        self.transaction = None;
        Ok(())
    }

    fn fail_closed(&mut self, reason: String) {
        self.summary = reason;
        self.state.stop_reasoning();
    }

    fn context(&mut self, label: &str) -> Result<InvocationContext, String> {
        self.next_operation = self.next_operation.saturating_add(1);
        Ok(InvocationContext {
            episode_id: self.episode_id,
            operation_id: OperationId::new(format!(
                "episode-{}/{label}-{}",
                self.episode_id.get(),
                self.next_operation
            ))?,
        })
    }
}

fn ensure_allowed_phase(
    meta: &crate::domain::CapabilityMeta,
    phase: EpisodePhase,
) -> Result<(), String> {
    if meta.is_allowed_in(phase) {
        Ok(())
    } else {
        Err(format!(
            "capability '{}' is not allowed in phase {phase:?}",
            meta.id
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;
    use crate::adapters::local::comparator::ThresholdComparisonPolicy;
    use crate::agent::{AgentToolResult, AgentToolSpec};
    use crate::capability::{AdminPolicy, CapabilityRegistry, MeasurementProvider, MutationDriver};
    use crate::domain::{
        content_digest, CapabilityId, CapabilityKind, CapabilityMeta, CleanupReceipt, Digest,
        EffectClass, MeasurementOpenRequest, MeasurementSampleRequest, MeasurementSession,
        MeasurementSessionId, MetricBatch, MetricKind, MetricQuality, MetricValue,
        MutationApplyRequest, MutationFinalizeRequest, MutationOperationState,
        MutationPrepareRequest, MutationQuery, MutationReceipt, MutationRestoreRequest,
        MutationState, MutationStatus, MutationVerification, MutationVerifyRequest,
        PreparedMutation, ProviderClass, ProviderError, ProviderId, ProviderPin, ProviderVersion,
        ResourceKey,
    };

    struct ScriptedReasoner {
        stage: usize,
        mutation_tool: Option<String>,
        required_improvement: f64,
    }

    impl ScriptedReasoner {
        fn new(required_improvement: f64) -> Self {
            Self {
                stage: 0,
                mutation_tool: None,
                required_improvement,
            }
        }

        fn begin_call(&self) -> AgentTurn {
            AgentTurn::ToolCalls(vec![AgentToolInvocation {
                id: "begin".into(),
                name: "begin_experiment".into(),
                arguments: json!({
                    "objective": "increase synthetic throughput",
                    "evaluation_contract": evaluation_contract(self.required_improvement)
                }),
            }])
        }
    }

    fn evaluation_contract(required_improvement: f64) -> Value {
        json!({
            "measurement": {
                "capability_id": crate::kernel::evaluation::TRUSTED_GUARDRAIL_MEASUREMENT_ID,
                "specification": {}
            },
            "primary": [{
                "capability_id": "builtin/comparison.threshold.v1",
                "specification": {
                    "conditions": [{
                        "metric": "throughput",
                        "op": "increase_percent_ge",
                        "value": required_improvement
                    }]
                }
            }],
            "sampling": {
                "settle_ms": 0,
                "sample_count": 1,
                "sample_interval_ms": 0
            }
        })
    }

    fn intent_spec(objective: &str, evaluation_contract: Value) -> EvaluationIntentSpec {
        serde_json::from_value(json!({
            "objective": objective,
            "evaluation_contract": evaluation_contract,
        }))
        .unwrap()
    }

    impl AgentReasoner for ScriptedReasoner {
        fn begin(
            &mut self,
            _context: &Value,
            tools: &[AgentToolSpec],
        ) -> Result<AgentTurn, String> {
            self.mutation_tool = Some(
                tools
                    .iter()
                    .find(|tool| tool.name.starts_with("experiment_"))
                    .ok_or_else(|| "mutation tool is missing".to_string())?
                    .name
                    .clone(),
            );
            self.stage = 1;
            Ok(self.begin_call())
        }

        fn resume(&mut self, results: &[AgentToolResult]) -> Result<AgentTurn, String> {
            if !results.iter().all(|result| result.ok) {
                return Err("prior tool failed".to_string());
            }
            match self.stage {
                1 => {
                    self.stage = 2;
                    Ok(AgentTurn::ToolCalls(vec![AgentToolInvocation {
                        id: "mutate".into(),
                        name: self.mutation_tool.clone().unwrap(),
                        arguments: json!({
                            "arguments": {"value": "new"},
                            "reason": "synthetic experiment"
                        }),
                    }]))
                }
                2 => {
                    self.stage = 3;
                    let change_id = results[0].content["change"]["change_id"]
                        .as_str()
                        .ok_or_else(|| "mutation result has no change id".to_string())?;
                    Ok(AgentTurn::ToolCalls(vec![AgentToolInvocation {
                        id: "commit".into(),
                        name: "request_commit".into(),
                        arguments: json!({
                            "change_ids": [change_id],
                            "reason": "candidate is ready"
                        }),
                    }]))
                }
                _ => Err("unexpected reasoner resume".to_string()),
            }
        }
    }

    struct MemoryMutation {
        meta: CapabilityMeta,
        value: Arc<Mutex<String>>,
        finalize_error: bool,
        restore_calls: AtomicUsize,
        fail_restore_calls: &'static [usize],
    }

    impl MutationDriver for MemoryMutation {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn prepare(
            &self,
            request: &MutationPrepareRequest,
        ) -> Result<PreparedMutation, ProviderError> {
            let desired = request
                .arguments
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| provider_error("value is required"))?;
            Ok(PreparedMutation {
                capability_id: self.meta.id.clone(),
                provider: self.meta.provider.clone(),
                resource: ResourceKey::new("test/resource").unwrap(),
                baseline: mutation_state(&self.value.lock().unwrap()),
                desired: mutation_state(desired),
                driver_data: json!({}),
            })
        }

        fn apply(&self, request: &MutationApplyRequest) -> Result<MutationReceipt, ProviderError> {
            let desired = request.prepared.desired.value.as_str().unwrap().to_string();
            *self.value.lock().unwrap() = desired;
            Ok(receipt(
                request.operation_id.clone(),
                MutationOperationState::Applied,
                request.prepared.desired.clone(),
            ))
        }

        fn status(&self, query: &MutationQuery) -> Result<MutationStatus, ProviderError> {
            Ok(MutationStatus {
                operation_id: query.operation_id.clone(),
                state: MutationOperationState::Unknown,
                observed: None,
                driver_data: json!({}),
            })
        }

        fn verify(
            &self,
            request: &MutationVerifyRequest,
        ) -> Result<MutationVerification, ProviderError> {
            let observed = mutation_state(&self.value.lock().unwrap());
            Ok(MutationVerification {
                matched: observed == request.expected,
                observed: Some(observed),
                details: json!({}),
            })
        }

        fn restore(
            &self,
            request: &MutationRestoreRequest,
        ) -> Result<MutationReceipt, ProviderError> {
            let call = self.restore_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_restore_calls.contains(&call) {
                return Err(provider_error("injected restore failure"));
            }
            *self.value.lock().unwrap() = request
                .prepared
                .baseline
                .value
                .as_str()
                .unwrap()
                .to_string();
            Ok(receipt(
                request.operation_id.clone(),
                MutationOperationState::Restored,
                request.prepared.baseline.clone(),
            ))
        }

        fn finalize(
            &self,
            request: &MutationFinalizeRequest,
        ) -> Result<MutationReceipt, ProviderError> {
            if self.finalize_error {
                return Err(provider_error("injected finalize acknowledgement failure"));
            }
            Ok(receipt(
                request.operation_id.clone(),
                MutationOperationState::Finalized,
                request.prepared.desired.clone(),
            ))
        }
    }

    struct StateMeasurement {
        meta: CapabilityMeta,
        value: Arc<Mutex<String>>,
        cleanup_error: bool,
    }

    impl MeasurementProvider for StateMeasurement {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn validate_specification(&self, specification: &Value) -> Result<(), ProviderError> {
            if specification == &json!({}) {
                Ok(())
            } else {
                Err(provider_error("specification must be empty"))
            }
        }

        fn open(
            &self,
            request: &MeasurementOpenRequest,
        ) -> Result<MeasurementSession, ProviderError> {
            Ok(MeasurementSession {
                id: MeasurementSessionId::new(format!("session-{}", request.context.operation_id))
                    .unwrap(),
                driver_data: json!({}),
            })
        }

        fn sample(
            &self,
            _request: &MeasurementSampleRequest,
        ) -> Result<MetricBatch, ProviderError> {
            let throughput = if self.value.lock().unwrap().as_str() == "new" {
                120.0
            } else {
                100.0
            };
            let gauge = |value, unit: &str| MetricValue {
                value: json!(value),
                unit: unit.into(),
                kind: MetricKind::Gauge,
            };
            Ok(MetricBatch {
                started_at_ns: 1,
                ended_at_ns: 2,
                quality: MetricQuality::Valid,
                workload_fingerprint: Some("same-workload".into()),
                metrics: BTreeMap::from([
                    ("throughput".into(), gauge(throughput, "ops/s")),
                    ("psi.cpu.full.avg10".into(), gauge(1.0, "percent")),
                    ("psi.io.full.avg10".into(), gauge(1.0, "percent")),
                    ("psi.memory.full.avg10".into(), gauge(1.0, "percent")),
                    ("loadavg.1m".into(), gauge(1.0, "load")),
                ]),
                provenance: json!({"provider": "test"}),
            })
        }

        fn close(&self, session: &MeasurementSession) -> Result<CleanupReceipt, ProviderError> {
            if self.cleanup_error {
                return Err(provider_error("injected cleanup failure"));
            }
            Ok(CleanupReceipt {
                session_id: session.id.clone(),
                cleaned_up: true,
                details: json!({}),
            })
        }
    }

    #[derive(Default)]
    struct MemoryAudit(Vec<AuditRecord>);

    impl AuditSink for MemoryAudit {
        fn record(&mut self, record: &AuditRecord) -> std::io::Result<()> {
            self.0.push(record.clone());
            Ok(())
        }
    }

    #[test]
    fn improved_candidate_is_committed_end_to_end() {
        let (outcome, value) = run_episode(10.0);
        assert_eq!(outcome.phase, EpisodePhase::Committed);
        let intent = outcome.intent.as_ref().expect("intent summary is recorded");
        assert_eq!(intent.objective, "increase synthetic throughput");
        assert_eq!(intent.intent_digest.as_str().len(), 71);
        assert_eq!(
            outcome.decision.unwrap().verdict,
            EvaluationVerdict::Improved
        );
        assert_eq!(*value.lock().unwrap(), "new");
    }

    #[test]
    fn no_signal_candidate_is_rolled_back_end_to_end() {
        let (outcome, value) = run_episode(50.0);
        assert_eq!(outcome.phase, EpisodePhase::Clean);
        assert_eq!(
            outcome.decision.unwrap().verdict,
            EvaluationVerdict::NoSignal
        );
        assert_eq!(*value.lock().unwrap(), "old");
    }

    #[test]
    fn finalize_ack_failure_is_immediately_rolled_back() {
        let (outcome, value) = run_episode_with(
            10.0,
            FailureConfig {
                finalize_error: true,
                ..FailureConfig::default()
            },
        );

        assert_eq!(outcome.phase, EpisodePhase::Clean);
        assert_eq!(*value.lock().unwrap(), "old");
    }

    #[test]
    fn finalize_ack_and_rollback_failure_requires_recovery() {
        let (outcome, value) = run_episode_with(
            10.0,
            FailureConfig {
                finalize_error: true,
                fail_restore_calls: &[2],
                ..FailureConfig::default()
            },
        );

        assert_eq!(outcome.phase, EpisodePhase::RecoveryRequired);
        assert_eq!(*value.lock().unwrap(), "new");
    }

    #[test]
    fn measurement_cleanup_failure_rolls_back_and_requires_recovery() {
        let (outcome, value) = run_episode_with(
            10.0,
            FailureConfig {
                cleanup_error: true,
                ..FailureConfig::default()
            },
        );

        assert_eq!(outcome.phase, EpisodePhase::RecoveryRequired);
        assert_eq!(*value.lock().unwrap(), "old");
    }

    #[test]
    fn episode_cleanup_completes_a_rollback_that_previously_failed() {
        let (outcome, value) = finish_after_failed_abort(&[1]);

        assert_eq!(outcome.phase, EpisodePhase::Clean);
        assert_eq!(*value.lock().unwrap(), "old");
    }

    #[test]
    fn episode_cleanup_requires_recovery_when_rollback_retry_fails() {
        let (outcome, value) = finish_after_failed_abort(&[1, 2]);

        assert_eq!(outcome.phase, EpisodePhase::RecoveryRequired);
        assert!(outcome.summary.contains("episode cleanup failed"));
        assert_eq!(*value.lock().unwrap(), "new");
    }

    #[test]
    fn successful_begin_locks_the_intent_even_after_rollback() {
        let value = Arc::new(Mutex::new("old".to_string()));
        let registry = episode_registry(value.clone(), FailureConfig::default());
        let root = unique_test_root("intent-lock");
        let store = TransactionStore::new(&root).unwrap();
        let mut session = EpisodeSession::new(
            EpisodeId::new(44),
            Duration::from_secs(600),
            registry.snapshot(),
            &store,
        );
        let contract = evaluation_contract(10.0);
        let first = session
            .begin_experiment(intent_spec("  reduce   latency  ", contract.clone()))
            .unwrap();
        let intent_digest = first["intent_digest"].clone();

        let replacement_error = session
            .begin_experiment(intent_spec("increase throughput", contract))
            .unwrap_err();
        assert!(replacement_error.contains("not allowed"));

        session
            .mutate(
                CapabilityId::new("test/mutation").unwrap(),
                json!({"value": "new"}),
                "test frozen intent".into(),
            )
            .unwrap();
        session.abort("finish this episode".into()).unwrap();

        assert_eq!(session.state.phase(), EpisodePhase::Clean);
        assert!(session.state.is_reasoning_stopped());
        let frozen = session.state.frozen_intent().unwrap();
        assert_eq!(frozen.objective().as_str(), "reduce latency");
        assert_eq!(json!(frozen.digest()), intent_digest);
        assert!(session
            .begin_experiment(intent_spec("another objective", evaluation_contract(10.0)))
            .is_err());
        assert_eq!(*value.lock().unwrap(), "old");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejected_begin_does_not_consume_the_single_freeze() {
        let value = Arc::new(Mutex::new("old".to_string()));
        let registry = episode_registry(value, FailureConfig::default());
        let root = unique_test_root("intent-retry");
        let store = TransactionStore::new(&root).unwrap();
        let mut session = EpisodeSession::new(
            EpisodeId::new(45),
            Duration::from_secs(600),
            registry.snapshot(),
            &store,
        );
        let mut invalid = evaluation_contract(10.0);
        invalid["primary"] = json!([]);

        assert!(session
            .begin_experiment(intent_spec("invalid draft", invalid))
            .is_err());
        assert!(session.state.frozen_intent().is_none());

        session
            .begin_experiment(intent_spec("valid objective", evaluation_contract(10.0)))
            .unwrap();
        assert_eq!(
            session.state.frozen_intent().unwrap().objective().as_str(),
            "valid objective"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn transaction_wal_creation_failure_requires_recovery() {
        let value = Arc::new(Mutex::new("old".to_string()));
        let registry = episode_registry(value.clone(), FailureConfig::default());
        let root = unique_test_root("transaction-start-failure");
        let store = TransactionStore::new(&root).unwrap();
        let episode_id = EpisodeId::new(46);
        let transaction_id = TransactionId::new("episode-46/transaction-1").unwrap();
        drop(store.create(&transaction_id).unwrap());
        let mut session = EpisodeSession::new(
            episode_id,
            Duration::from_secs(600),
            registry.snapshot(),
            &store,
        );
        session
            .begin_experiment(intent_spec("test objective", evaluation_contract(10.0)))
            .unwrap();

        let error = session
            .mutate(
                CapabilityId::new("test/mutation").unwrap(),
                json!({"value": "new"}),
                "must not run".into(),
            )
            .unwrap_err();

        assert!(error.contains("recovery is required"));
        assert_eq!(session.state.phase(), EpisodePhase::RecoveryRequired);
        assert!(session.state.is_reasoning_stopped());
        assert!(session.state.frozen_intent().is_some());
        assert_eq!(*value.lock().unwrap(), "old");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invocation_context_ids_are_unique_without_new_changes() {
        let root = unique_test_root("operation-id");
        let store = TransactionStore::new(&root).unwrap();
        let registry = CapabilityRegistry::new(AdminPolicy::default());
        let mut session = EpisodeSession::new(
            EpisodeId::new(7),
            Duration::from_secs(600),
            registry.snapshot(),
            &store,
        );

        let first = session.context("probe").unwrap();
        let second = session.context("probe").unwrap();

        assert_ne!(first.operation_id, second.operation_id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn deterministic_schedule_budget_is_checked_at_freeze_and_before_mutation() {
        let value = Arc::new(Mutex::new("old".to_string()));
        let registry = episode_registry(value.clone(), FailureConfig::default());
        let root = unique_test_root("schedule-budget");
        let store = TransactionStore::new(&root).unwrap();
        let contract = json!({
            "measurement": {
                "capability_id": crate::kernel::evaluation::TRUSTED_GUARDRAIL_MEASUREMENT_ID,
                "specification": {}
            },
            "primary": [{
                "capability_id": "builtin/comparison.threshold.v1",
                "specification": {
                    "conditions": [{
                        "metric": "throughput",
                        "op": "increase_percent_ge",
                        "value": 1.0
                    }]
                }
            }],
            "sampling": {
                "settle_ms": 1,
                "sample_count": 1,
                "sample_interval_ms": 0
            }
        });
        let mut freeze_session = EpisodeSession::new(
            EpisodeId::new(8),
            Duration::from_millis(1),
            registry.snapshot(),
            &store,
        );

        let freeze_error = freeze_session
            .begin_experiment(intent_spec("test", contract.clone()))
            .unwrap_err();

        assert!(freeze_error.contains("deterministic waits"));
        assert!(freeze_session.transaction.is_none());

        let mut mutation_session = EpisodeSession::new(
            EpisodeId::new(9),
            Duration::from_secs(60),
            registry.snapshot(),
            &store,
        );
        mutation_session
            .begin_experiment(intent_spec("test", contract))
            .unwrap();
        mutation_session.evaluation_timeout = Duration::from_millis(1);

        let mutation_error = mutation_session
            .mutate(
                CapabilityId::new("test/mutation").unwrap(),
                json!({"value": "new"}),
                "test".into(),
            )
            .unwrap_err();

        assert!(mutation_error.contains("deterministic waits"));
        assert!(mutation_session.transaction.is_none());
        assert_eq!(*value.lock().unwrap(), "old");
        let _ = fs::remove_dir_all(root);
    }

    #[derive(Clone, Copy, Default)]
    struct FailureConfig {
        finalize_error: bool,
        fail_restore_calls: &'static [usize],
        cleanup_error: bool,
    }

    fn run_episode(required_improvement: f64) -> (EpisodeOutcome, Arc<Mutex<String>>) {
        run_episode_with(required_improvement, FailureConfig::default())
    }

    fn run_episode_with(
        required_improvement: f64,
        failures: FailureConfig,
    ) -> (EpisodeOutcome, Arc<Mutex<String>>) {
        let value = Arc::new(Mutex::new("old".to_string()));
        let registry = episode_registry(value.clone(), failures);
        let root = unique_test_root("episode");
        let store = TransactionStore::new(&root).unwrap();
        let mut audit = MemoryAudit::default();
        let mut reasoner = ScriptedReasoner::new(required_improvement);
        let outcome = EpisodeCoordinator::new(
            3,
            Duration::from_secs(600),
            registry.snapshot(),
            &store,
            &mut audit,
        )
        .unwrap()
        .run(EpisodeId::new(42), json!({"kind": "test"}), &mut reasoner)
        .unwrap();
        assert!(audit
            .0
            .iter()
            .any(|record| record.event == "episode_finished"));
        let _ = fs::remove_dir_all(root);
        (outcome, value)
    }

    fn finish_after_failed_abort(
        fail_restore_calls: &'static [usize],
    ) -> (EpisodeOutcome, Arc<Mutex<String>>) {
        let value = Arc::new(Mutex::new("old".to_string()));
        let registry = episode_registry(
            value.clone(),
            FailureConfig {
                fail_restore_calls,
                ..FailureConfig::default()
            },
        );
        let root = unique_test_root("rollback-retry");
        let store = TransactionStore::new(&root).unwrap();
        let snapshot = registry.snapshot();
        let mut audit = MemoryAudit::default();
        let mut coordinator = EpisodeCoordinator::new(
            3,
            Duration::from_secs(600),
            snapshot.clone(),
            &store,
            &mut audit,
        )
        .unwrap();
        let mut session = EpisodeSession::new(
            EpisodeId::new(43),
            Duration::from_secs(600),
            snapshot,
            &store,
        );
        session
            .begin_experiment(intent_spec(
                "increase synthetic throughput",
                evaluation_contract(10.0),
            ))
            .unwrap();
        session
            .mutate(
                CapabilityId::new("test/mutation").unwrap(),
                json!({"value": "new"}),
                "synthetic experiment".into(),
            )
            .unwrap();

        assert!(session.abort("test abort".into()).is_err());
        assert_eq!(session.state.phase(), EpisodePhase::RollingBack);

        let outcome = coordinator.finish_session(session).unwrap();
        drop(coordinator);
        let _ = fs::remove_dir_all(root);
        (outcome, value)
    }

    fn episode_registry(value: Arc<Mutex<String>>, failures: FailureConfig) -> CapabilityRegistry {
        let mut registry = CapabilityRegistry::new(AdminPolicy::default());
        registry
            .register_mutation(Arc::new(MemoryMutation {
                meta: mutation_meta(),
                value: value.clone(),
                finalize_error: failures.finalize_error,
                restore_calls: AtomicUsize::new(0),
                fail_restore_calls: failures.fail_restore_calls,
            }))
            .unwrap();
        registry
            .register_measurement(Arc::new(StateMeasurement {
                meta: measurement_meta(),
                value: value.clone(),
                cleanup_error: failures.cleanup_error,
            }))
            .unwrap();
        registry
            .register_comparison(Arc::new(ThresholdComparisonPolicy::new()))
            .unwrap();
        registry
    }

    fn unique_test_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tuning-agent-{label}-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn mutation_meta() -> CapabilityMeta {
        let mut meta = CapabilityMeta::new(
            CapabilityId::new("test/mutation").unwrap(),
            CapabilityKind::Mutation,
            EffectClass::ReversibleMutation,
            provider_pin("test-mutation", ProviderClass::Local),
            "test mutation",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
        .with_allowed_phases([EpisodePhase::Clean, EpisodePhase::Experimenting]);
        meta.idempotent = true;
        meta
    }

    fn measurement_meta() -> CapabilityMeta {
        let mut meta = CapabilityMeta::new(
            CapabilityId::new(crate::kernel::evaluation::TRUSTED_GUARDRAIL_MEASUREMENT_ID).unwrap(),
            CapabilityKind::Measurement,
            EffectClass::ReadOnly,
            provider_pin("trusted-test-measurement", ProviderClass::Builtin),
            "trusted test measurement",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
        .with_allowed_phases([EpisodePhase::CommitPending]);
        meta.idempotent = true;
        meta
    }

    fn provider_pin(name: &str, provider_class: ProviderClass) -> ProviderPin {
        ProviderPin {
            provider_id: ProviderId::new(name).unwrap(),
            provider_version: ProviderVersion::new("1").unwrap(),
            provider_class,
            manifest_digest: Digest::new(format!("{name}-manifest")).unwrap(),
        }
    }

    fn mutation_state(value: &str) -> MutationState {
        let value = json!(value);
        MutationState {
            digest: content_digest(&value).unwrap(),
            value,
        }
    }

    fn receipt(
        operation_id: OperationId,
        state: MutationOperationState,
        observed: MutationState,
    ) -> MutationReceipt {
        MutationReceipt {
            operation_id,
            state,
            observed: Some(observed),
            driver_data: json!({}),
        }
    }

    fn provider_error(message: &str) -> ProviderError {
        ProviderError::new(crate::domain::ProviderErrorKind::InvalidRequest, message)
    }
}
