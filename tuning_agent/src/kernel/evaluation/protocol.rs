use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::capability::{CapabilitySnapshot, MeasurementProvider};
use crate::domain::{
    content_digest, Candidate, CapabilityId, ChangeId, CommitAuthorization, ComparisonEvidence,
    ComparisonRequest, Digest, EffectClass, EvaluationIntentPin, InvocationContext, OperationId,
    ProviderClass,
};

pub const TRUSTED_GUARDRAIL_MEASUREMENT_ID: &str = "builtin/measurement.core-system.v1";
use crate::kernel::evaluation::{
    collect_measurement, ComparisonBinding, ComparisonEvidenceGroups, EvaluationDecision,
    EvaluationError, EvaluationErrorKind, FrozenEvaluationContract, FrozenEvaluationIntent,
    MeasurementEvidence, VerdictKernel,
};

#[derive(Clone, Debug)]
pub(crate) struct EvaluationDeadline {
    started_at: Instant,
    expires_at: Instant,
    timeout: Duration,
}

impl EvaluationDeadline {
    pub(crate) fn start(timeout: Duration) -> Result<Self, EvaluationError> {
        let started_at = Instant::now();
        let expires_at = started_at.checked_add(timeout).ok_or_else(|| {
            EvaluationError::new(
                EvaluationErrorKind::BudgetExceeded,
                "evaluation timeout cannot be represented by the monotonic clock",
            )
        })?;
        Ok(Self {
            started_at,
            expires_at,
            timeout,
        })
    }

    pub(crate) fn check(&self, stage: &str) -> Result<(), EvaluationError> {
        if Instant::now() >= self.expires_at {
            return Err(self.exceeded(stage));
        }
        Ok(())
    }

    pub(crate) fn ensure_provider_call(
        &self,
        meta: &crate::domain::CapabilityMeta,
        stage: &str,
    ) -> Result<(), EvaluationError> {
        self.check(stage)?;
        let remaining = self.expires_at.saturating_duration_since(Instant::now());
        let declared_timeout = Duration::from_millis(meta.limits.timeout_ms);
        if declared_timeout > remaining {
            return Err(EvaluationError::new(
                EvaluationErrorKind::BudgetExceeded,
                format!(
                    "evaluation budget cannot admit {stage}: capability '{}' declares a {} ms timeout but only {} ms remain",
                    meta.id,
                    meta.limits.timeout_ms,
                    remaining.as_millis()
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn settle(&self, duration: Duration, stage: &str) -> Result<(), EvaluationError> {
        self.check(stage)?;
        let remaining = self.expires_at.saturating_duration_since(Instant::now());
        if duration > remaining {
            return Err(EvaluationError::new(
                EvaluationErrorKind::BudgetExceeded,
                format!(
                    "evaluation budget cannot admit {stage}: {} ms are scheduled but only {} ms remain",
                    duration.as_millis(),
                    remaining.as_millis()
                ),
            ));
        }
        if !duration.is_zero() {
            thread::sleep(duration);
        }
        self.check(stage)
    }

    fn exceeded(&self, stage: &str) -> EvaluationError {
        EvaluationError::new(
            EvaluationErrorKind::BudgetExceeded,
            format!(
                "evaluation budget of {} ms was exceeded {stage} after {} ms",
                self.timeout.as_millis(),
                self.started_at.elapsed().as_millis()
            ),
        )
    }
}

pub trait TransactionPort {
    fn restore_baseline(
        &mut self,
        candidate: &[ChangeId],
    ) -> Result<TransactionStepEvidence, String>;

    fn replay_candidate(
        &mut self,
        candidate: &[ChangeId],
    ) -> Result<TransactionStepEvidence, String>;
}

impl TransactionPort for crate::kernel::transaction::TransactionKernel {
    fn restore_baseline(
        &mut self,
        _candidate: &[ChangeId],
    ) -> Result<TransactionStepEvidence, String> {
        let changes = crate::kernel::transaction::TransactionKernel::restore_baseline(self)
            .map_err(|error| error.to_string())?;
        let details = public_change_details(&changes);
        Ok(TransactionStepEvidence {
            state_digest: crate::domain::content_digest(&details).ok(),
            details,
        })
    }

    fn replay_candidate(
        &mut self,
        candidate: &[ChangeId],
    ) -> Result<TransactionStepEvidence, String> {
        let candidate = Candidate::new(candidate.to_vec())?;
        let changes =
            crate::kernel::transaction::TransactionKernel::replay_candidate(self, &candidate)
                .map_err(|error| error.to_string())?;
        let details = public_change_details(&changes);
        Ok(TransactionStepEvidence {
            state_digest: crate::domain::content_digest(&details).ok(),
            details,
        })
    }
}

fn public_change_details(changes: &[crate::kernel::transaction::ChangeRecord]) -> Value {
    serde_json::json!({
        "changes": changes.iter().map(|change| serde_json::json!({
            "change_id": change.change_id,
            "capability_id": change.capability_id,
            "resource": change.resource,
            "state": change.state,
        })).collect::<Vec<_>>()
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransactionStepEvidence {
    pub state_digest: Option<Digest>,
    pub details: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AbEvaluationEvidence {
    pub intent: EvaluationIntentPin,
    pub contract_id: crate::domain::ContractId,
    pub contract_digest: Digest,
    pub candidate_digest: Digest,
    pub baseline_restore: TransactionStepEvidence,
    pub baseline_system_guardrails: MeasurementEvidence,
    pub baseline_measurement: MeasurementEvidence,
    pub candidate_replay: TransactionStepEvidence,
    pub candidate_system_guardrails: MeasurementEvidence,
    pub candidate_measurement: MeasurementEvidence,
    pub decision: EvaluationDecision,
}

impl AbEvaluationEvidence {
    pub(crate) fn commit_authorization(&self) -> Result<CommitAuthorization, EvaluationError> {
        if self.decision.verdict != crate::kernel::evaluation::EvaluationVerdict::Improved {
            return Err(EvaluationError::new(
                EvaluationErrorKind::Protocol,
                "only an improved evaluation can authorize a commit",
            ));
        }
        if self.intent.contract_digest() != &self.contract_digest {
            return Err(EvaluationError::new(
                EvaluationErrorKind::Protocol,
                "evaluation evidence contract does not match its frozen intent",
            ));
        }
        let decision_digest = content_digest(&self.decision).map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::Protocol,
                format!("failed to digest evaluation decision: {error}"),
            )
        })?;
        let evaluation_evidence_digest = content_digest(self).map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::Protocol,
                format!("failed to digest evaluation evidence: {error}"),
            )
        })?;
        CommitAuthorization::issue(
            self.intent.clone(),
            self.candidate_digest.clone(),
            decision_digest,
            evaluation_evidence_digest,
        )
        .map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::Protocol,
                format!("failed to issue commit authorization: {error}"),
            )
        })
    }
}

pub struct AbEvaluationProtocol {
    capabilities: CapabilitySnapshot,
    guardrail_measurement: Arc<dyn MeasurementProvider>,
    guardrail_binding: crate::kernel::evaluation::MeasurementBinding,
    evaluation_timeout: Duration,
    verdict: VerdictKernel,
}

impl AbEvaluationProtocol {
    pub fn new(
        capabilities: CapabilitySnapshot,
        evaluation_timeout: Duration,
    ) -> Result<Self, EvaluationError> {
        if evaluation_timeout.is_zero() {
            return Err(EvaluationError::new(
                EvaluationErrorKind::BudgetExceeded,
                "evaluation timeout must be greater than zero",
            ));
        }
        let capability_id = CapabilityId::new(TRUSTED_GUARDRAIL_MEASUREMENT_ID)
            .expect("static guardrail capability id is valid");
        let meta = capabilities.meta(&capability_id).ok_or_else(|| {
            EvaluationError::new(
                EvaluationErrorKind::MissingCapability,
                format!("trusted system guardrail measurement '{capability_id}' is unavailable"),
            )
        })?;
        if meta.provider.provider_class != ProviderClass::Builtin
            || meta.effect != EffectClass::ReadOnly
            || !meta.is_allowed_in(crate::domain::EpisodePhase::CommitPending)
        {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                "trusted system guardrail measurement has unsafe metadata",
            ));
        }
        let guardrail_measurement = capabilities.measurement(&capability_id).ok_or_else(|| {
            EvaluationError::new(
                EvaluationErrorKind::MissingCapability,
                "trusted system guardrail measurement provider is unavailable",
            )
        })?;
        let guardrail_binding = crate::kernel::evaluation::MeasurementBinding {
            capability_id,
            specification: serde_json::json!({}),
        };
        guardrail_measurement
            .validate_specification(&guardrail_binding.specification)
            .map_err(|error| {
                EvaluationError::new(
                    EvaluationErrorKind::InvalidContract,
                    format!("trusted system guardrail measurement is invalid: {error}"),
                )
            })?;
        Ok(Self {
            capabilities,
            guardrail_measurement,
            guardrail_binding,
            evaluation_timeout,
            verdict: VerdictKernel,
        })
    }

    pub fn ensure_schedule_fits(
        contract: &FrozenEvaluationContract,
        evaluation_timeout: Duration,
    ) -> Result<(), EvaluationError> {
        let intervals_per_collection = u128::from(contract.sampling().sample_count - 1)
            * u128::from(contract.sampling().sample_interval_ms);
        let collections_per_side = if contract.measurement().capability_id.as_str()
            == TRUSTED_GUARDRAIL_MEASUREMENT_ID
            && contract.measurement().specification == serde_json::json!({})
        {
            1_u128
        } else {
            2_u128
        };
        let scheduled_ms = u128::from(contract.sampling().settle_ms) * 2
            + intervals_per_collection * collections_per_side * 2;
        if scheduled_ms > evaluation_timeout.as_millis() {
            return Err(EvaluationError::new(
                EvaluationErrorKind::BudgetExceeded,
                format!(
                    "evaluation contract schedules {scheduled_ms} ms of deterministic waits, exceeding the {} ms evaluation budget",
                    evaluation_timeout.as_millis()
                ),
            ));
        }
        Ok(())
    }

    pub fn validate_contract(
        &self,
        contract: &FrozenEvaluationContract,
    ) -> Result<(), EvaluationError> {
        Self::ensure_schedule_fits(contract, self.evaluation_timeout)?;
        self.validate_contract_with_deadline(contract, None)
    }

    fn validate_contract_with_deadline(
        &self,
        contract: &FrozenEvaluationContract,
        deadline: Option<&EvaluationDeadline>,
    ) -> Result<(), EvaluationError> {
        if contract.capability_generation() != self.capabilities.generation() {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                format!(
                    "contract pins capability generation {}, runtime has {}",
                    contract.capability_generation(),
                    self.capabilities.generation()
                ),
            ));
        }
        for pin in contract.capability_pins() {
            let meta = self.capabilities.meta(&pin.capability_id).ok_or_else(|| {
                EvaluationError::new(
                    EvaluationErrorKind::MissingCapability,
                    format!("pinned capability '{}' is unavailable", pin.capability_id),
                )
            })?;
            if meta.provider != pin.provider {
                return Err(EvaluationError::new(
                    EvaluationErrorKind::InvalidContract,
                    format!(
                        "provider pin changed for capability '{}'",
                        pin.capability_id
                    ),
                ));
            }
        }
        let measurement = self
            .capabilities
            .measurement(&contract.measurement().capability_id)
            .ok_or_else(|| {
                EvaluationError::new(
                    EvaluationErrorKind::MissingCapability,
                    format!(
                        "measurement capability '{}' is unavailable",
                        contract.measurement().capability_id
                    ),
                )
            })?;
        if let Some(deadline) = deadline {
            deadline.ensure_provider_call(
                measurement.meta(),
                "before frozen measurement specification validation",
            )?;
        }
        let measurement_validation =
            measurement.validate_specification(&contract.measurement().specification);
        if let Some(deadline) = deadline {
            deadline.check("after frozen measurement specification validation")?;
        }
        measurement_validation.map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                format!(
                    "measurement '{}' rejected its frozen specification: {error}",
                    contract.measurement().capability_id
                ),
            )
        })?;
        self.validate_comparison_capabilities(contract, deadline)
    }

    pub fn evaluate(
        &self,
        transaction: &mut dyn TransactionPort,
        context: &InvocationContext,
        intent: &FrozenEvaluationIntent,
        candidate: &Candidate,
    ) -> Result<AbEvaluationEvidence, EvaluationError> {
        if context.episode_id != intent.episode_id() {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                "evaluation intent belongs to a different episode",
            ));
        }
        let contract = intent.contract();
        let deadline = EvaluationDeadline::start(self.evaluation_timeout)?;
        Self::ensure_schedule_fits(contract, self.evaluation_timeout)?;
        self.validate_contract_with_deadline(contract, Some(&deadline))?;
        let measurement = self
            .capabilities
            .measurement(&contract.measurement().capability_id)
            .ok_or_else(|| {
                EvaluationError::new(
                    EvaluationErrorKind::MissingCapability,
                    format!(
                        "measurement capability '{}' is unavailable",
                        contract.measurement().capability_id
                    ),
                )
            })?;
        deadline.check("before restoring the baseline")?;
        let baseline_restore = transaction.restore_baseline(candidate.change_ids());
        deadline.check("after restoring the baseline")?;
        let baseline_restore = baseline_restore.map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::Transaction,
                format!("failed to restore evaluation baseline: {error}"),
            )
        })?;
        deadline.settle(
            Duration::from_millis(contract.sampling().settle_ms),
            "while settling the baseline",
        )?;
        let baseline_context = side_context(context, "baseline")?;
        let baseline_system_guardrails = collect_measurement(
            self.guardrail_measurement.clone(),
            &self.guardrail_binding,
            &baseline_context,
            contract.sampling(),
            &deadline,
        )?;
        let baseline_measurement = if contract.measurement().capability_id
            == self.guardrail_binding.capability_id
            && contract.measurement().specification == self.guardrail_binding.specification
        {
            baseline_system_guardrails.clone()
        } else {
            collect_measurement(
                measurement.clone(),
                contract.measurement(),
                &baseline_context,
                contract.sampling(),
                &deadline,
            )?
        };

        deadline.check("before replaying the candidate")?;
        let candidate_replay = transaction.replay_candidate(candidate.change_ids());
        deadline.check("after replaying the candidate")?;
        let candidate_replay = candidate_replay.map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::Transaction,
                format!("failed to replay evaluation candidate: {error}"),
            )
        })?;
        deadline.settle(
            Duration::from_millis(contract.sampling().settle_ms),
            "while settling the candidate",
        )?;
        let candidate_context = side_context(context, "candidate")?;
        let candidate_system_guardrails = collect_measurement(
            self.guardrail_measurement.clone(),
            &self.guardrail_binding,
            &candidate_context,
            contract.sampling(),
            &deadline,
        )?;
        let candidate_measurement = if contract.measurement().capability_id
            == self.guardrail_binding.capability_id
            && contract.measurement().specification == self.guardrail_binding.specification
        {
            candidate_system_guardrails.clone()
        } else {
            collect_measurement(
                measurement,
                contract.measurement(),
                &candidate_context,
                contract.sampling(),
                &deadline,
            )?
        };

        deadline.check("before checking measurement comparability")?;
        let schemas_are_comparable = self.verdict.schemas_are_comparable(
            &baseline_system_guardrails.batch,
            &candidate_system_guardrails.batch,
            &baseline_measurement.batch,
            &candidate_measurement.batch,
        );
        let policy_evidence = if schemas_are_comparable {
            ComparisonEvidenceGroups {
                primary: self.compare_all(
                    context,
                    contract,
                    contract.primary(),
                    &baseline_measurement.batch,
                    &candidate_measurement.batch,
                    &deadline,
                )?,
                regression_guards: self.compare_all(
                    context,
                    contract,
                    contract.regression_guards(),
                    &baseline_measurement.batch,
                    &candidate_measurement.batch,
                    &deadline,
                )?,
                workload_invariants: self.compare_all(
                    context,
                    contract,
                    contract.workload_invariants(),
                    &baseline_measurement.batch,
                    &candidate_measurement.batch,
                    &deadline,
                )?,
            }
        } else {
            ComparisonEvidenceGroups::default()
        };
        deadline.check("before producing the central evaluation verdict")?;
        let decision = self.verdict.decide(
            &baseline_system_guardrails.batch,
            &candidate_system_guardrails.batch,
            &baseline_measurement.batch,
            &candidate_measurement.batch,
            policy_evidence,
        );
        deadline.check("after producing the central evaluation verdict")?;

        Ok(AbEvaluationEvidence {
            intent: intent.pin().clone(),
            contract_id: contract.id().clone(),
            contract_digest: contract.digest().clone(),
            candidate_digest: candidate.digest().clone(),
            baseline_restore,
            baseline_system_guardrails,
            baseline_measurement,
            candidate_replay,
            candidate_system_guardrails,
            candidate_measurement,
            decision,
        })
    }

    fn compare_all(
        &self,
        context: &InvocationContext,
        contract: &FrozenEvaluationContract,
        bindings: &[ComparisonBinding],
        baseline: &crate::domain::MetricBatch,
        candidate: &crate::domain::MetricBatch,
        deadline: &EvaluationDeadline,
    ) -> Result<Vec<ComparisonEvidence>, EvaluationError> {
        bindings
            .iter()
            .map(|binding| {
                let policy = self
                    .capabilities
                    .comparison(&binding.capability_id)
                    .ok_or_else(|| {
                        EvaluationError::new(
                            EvaluationErrorKind::MissingCapability,
                            format!(
                                "comparison capability '{}' is unavailable",
                                binding.capability_id
                            ),
                        )
                    })?;
                let stage = format!("before comparison '{}'", binding.capability_id);
                deadline.ensure_provider_call(policy.meta(), &stage)?;
                let evidence = policy.compare(&ComparisonRequest {
                    context: context.clone(),
                    contract_id: contract.id().clone(),
                    specification: binding.specification.clone(),
                    baseline: baseline.clone(),
                    candidate: candidate.clone(),
                });
                let stage = format!("after comparison '{}'", binding.capability_id);
                deadline.check(&stage)?;
                let evidence = evidence.map_err(|error| {
                    EvaluationError::new(
                        EvaluationErrorKind::Comparison,
                        format!("comparison '{}' failed: {error}", binding.capability_id),
                    )
                })?;
                let encoded = serde_json::to_vec(&evidence).map_err(|error| {
                    EvaluationError::new(
                        EvaluationErrorKind::Comparison,
                        format!("comparison evidence encoding failed: {error}"),
                    )
                })?;
                if encoded.len() > policy.meta().limits.max_output_bytes {
                    return Err(EvaluationError::new(
                        EvaluationErrorKind::Comparison,
                        format!(
                            "comparison '{}' exceeded its {} byte output limit",
                            binding.capability_id,
                            policy.meta().limits.max_output_bytes
                        ),
                    ));
                }
                Ok(evidence)
            })
            .collect()
    }

    fn validate_comparison_capabilities(
        &self,
        contract: &FrozenEvaluationContract,
        deadline: Option<&EvaluationDeadline>,
    ) -> Result<(), EvaluationError> {
        for binding in contract
            .primary()
            .iter()
            .chain(contract.regression_guards().iter())
            .chain(contract.workload_invariants().iter())
        {
            let policy = self
                .capabilities
                .comparison(&binding.capability_id)
                .ok_or_else(|| {
                    EvaluationError::new(
                        EvaluationErrorKind::MissingCapability,
                        format!(
                            "comparison capability '{}' is unavailable",
                            binding.capability_id
                        ),
                    )
                })?;
            if let Some(deadline) = deadline {
                let stage = format!(
                    "before frozen comparison '{}' specification validation",
                    binding.capability_id
                );
                deadline.ensure_provider_call(policy.meta(), &stage)?;
            }
            let validation = policy.validate_specification(&binding.specification);
            if let Some(deadline) = deadline {
                let stage = format!(
                    "after frozen comparison '{}' specification validation",
                    binding.capability_id
                );
                deadline.check(&stage)?;
            }
            validation.map_err(|error| {
                EvaluationError::new(
                    EvaluationErrorKind::InvalidContract,
                    format!(
                        "comparison '{}' rejected its frozen specification: {error}",
                        binding.capability_id
                    ),
                )
            })?;
        }
        Ok(())
    }
}

fn side_context(
    context: &InvocationContext,
    side: &str,
) -> Result<InvocationContext, EvaluationError> {
    let mut prefix = context.operation_id.as_str();
    while prefix.len() + side.len() + 1 > 256 {
        prefix = &prefix[..previous_char_boundary(prefix, prefix.len() - 1)];
    }
    let operation_id = OperationId::new(format!("{prefix}-{side}")).map_err(|error| {
        EvaluationError::new(
            EvaluationErrorKind::Protocol,
            format!("failed to derive measurement operation id: {error}"),
        )
    })?;
    Ok(InvocationContext {
        episode_id: context.episode_id,
        operation_id,
    })
}

fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::capability::{
        AdminPolicy, CapabilityRegistry, ComparisonPolicy, MeasurementProvider,
    };
    use crate::domain::{
        CapabilityId, CapabilityKind, CapabilityMeta, CleanupReceipt, ComparisonConclusion,
        ConditionEvidence, EffectClass, EpisodeId, MeasurementOpenRequest,
        MeasurementSampleRequest, MeasurementSession, MeasurementSessionId, MetricBatch,
        MetricKind, MetricQuality, MetricValue, ProviderClass, ProviderError, ProviderId,
        ProviderPin, ProviderVersion,
    };
    use crate::kernel::evaluation::{
        ComparisonBinding, ContractFreezer, EvaluationContractSpec, EvaluationVerdict,
        MeasurementBinding, SamplingPlan,
    };

    struct QueueMeasurement {
        meta: CapabilityMeta,
        samples: Mutex<VecDeque<MetricBatch>>,
        opens: AtomicUsize,
        closes: AtomicUsize,
    }

    impl MeasurementProvider for QueueMeasurement {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn validate_specification(&self, _specification: &Value) -> Result<(), ProviderError> {
            Ok(())
        }

        fn open(
            &self,
            _request: &MeasurementOpenRequest,
        ) -> Result<MeasurementSession, ProviderError> {
            let index = self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(MeasurementSession {
                id: MeasurementSessionId::new(format!("session-{index}")).unwrap(),
                driver_data: json!({}),
            })
        }

        fn sample(
            &self,
            _request: &MeasurementSampleRequest,
        ) -> Result<MetricBatch, ProviderError> {
            Ok(self.samples.lock().unwrap().pop_front().unwrap())
        }

        fn close(&self, session: &MeasurementSession) -> Result<CleanupReceipt, ProviderError> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(CleanupReceipt {
                session_id: session.id.clone(),
                cleaned_up: true,
                details: json!({}),
            })
        }
    }

    struct PassingComparison {
        meta: CapabilityMeta,
    }

    impl ComparisonPolicy for PassingComparison {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn validate_specification(&self, _specification: &Value) -> Result<(), ProviderError> {
            Ok(())
        }

        fn compare(
            &self,
            _request: &ComparisonRequest,
        ) -> Result<ComparisonEvidence, ProviderError> {
            Ok(ComparisonEvidence {
                conclusion: ComparisonConclusion::Improved,
                conditions: vec![ConditionEvidence {
                    name: "primary".to_string(),
                    passed: true,
                    details: json!({}),
                }],
                details: json!({}),
            })
        }
    }

    #[derive(Default)]
    struct FakeTransaction {
        calls: Vec<&'static str>,
    }

    impl TransactionPort for FakeTransaction {
        fn restore_baseline(
            &mut self,
            _candidate: &[ChangeId],
        ) -> Result<TransactionStepEvidence, String> {
            self.calls.push("restore");
            Ok(TransactionStepEvidence {
                state_digest: None,
                details: json!({}),
            })
        }

        fn replay_candidate(
            &mut self,
            _candidate: &[ChangeId],
        ) -> Result<TransactionStepEvidence, String> {
            self.calls.push("replay");
            Ok(TransactionStepEvidence {
                state_digest: None,
                details: json!({}),
            })
        }
    }

    fn provider_pin(name: &str) -> ProviderPin {
        ProviderPin {
            provider_id: ProviderId::new(name).unwrap(),
            provider_version: ProviderVersion::new("1").unwrap(),
            provider_class: ProviderClass::Builtin,
            manifest_digest: Digest::new(format!("{name}-digest")).unwrap(),
        }
    }

    fn meta(id: &str, kind: CapabilityKind, effect: EffectClass) -> CapabilityMeta {
        let mut meta = CapabilityMeta::new(
            CapabilityId::new(id).unwrap(),
            kind,
            effect,
            provider_pin(id),
            "test provider",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
        .with_allowed_phases([crate::domain::EpisodePhase::CommitPending]);
        if kind == CapabilityKind::Comparison {
            meta.deterministic = true;
        }
        meta
    }

    fn batch(primary: f64, psi: f64) -> MetricBatch {
        let value = |number, unit: &str| MetricValue {
            value: json!(number),
            unit: unit.to_string(),
            kind: MetricKind::Gauge,
        };
        MetricBatch {
            started_at_ns: 1,
            ended_at_ns: 2,
            quality: MetricQuality::Valid,
            workload_fingerprint: Some("same".to_string()),
            metrics: BTreeMap::from([
                ("throughput".to_string(), value(primary, "req/s")),
                ("psi.cpu.full.avg10".to_string(), value(psi, "percent")),
                ("psi.io.full.avg10".to_string(), value(psi, "percent")),
                ("psi.memory.full.avg10".to_string(), value(psi, "percent")),
                ("loadavg.1m".to_string(), value(1.0, "load")),
            ]),
            provenance: json!({}),
        }
    }

    fn freeze_test_contract(
        snapshot: &CapabilitySnapshot,
        measurement_id: CapabilityId,
        comparison_id: CapabilityId,
        sampling: SamplingPlan,
        suffix: &str,
    ) -> FrozenEvaluationContract {
        ContractFreezer::new(snapshot.clone())
            .freeze(
                crate::domain::ContractId::new(format!("contract-{suffix}")).unwrap(),
                EvaluationContractSpec {
                    measurement: MeasurementBinding {
                        capability_id: measurement_id,
                        specification: json!({}),
                    },
                    primary: vec![ComparisonBinding {
                        capability_id: comparison_id,
                        specification: json!({}),
                    }],
                    regression_guards: Vec::new(),
                    workload_invariants: Vec::new(),
                    sampling,
                },
            )
            .unwrap()
    }

    fn freeze_test_intent(
        episode_id: EpisodeId,
        contract: FrozenEvaluationContract,
    ) -> FrozenEvaluationIntent {
        FrozenEvaluationIntent::from_parts(
            episode_id,
            crate::kernel::evaluation::ObjectiveStatement::new("test objective").unwrap(),
            contract,
        )
        .unwrap()
    }

    #[test]
    fn deterministic_waits_are_rejected_before_evaluation_starts() {
        let measurement = Arc::new(QueueMeasurement {
            meta: meta(
                TRUSTED_GUARDRAIL_MEASUREMENT_ID,
                CapabilityKind::Measurement,
                EffectClass::ReadOnly,
            ),
            samples: Mutex::new(VecDeque::new()),
            opens: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        });
        let comparison = Arc::new(PassingComparison {
            meta: meta(
                "test/comparison-budget-schedule",
                CapabilityKind::Comparison,
                EffectClass::PureComputation,
            ),
        });
        let mut registry = CapabilityRegistry::new(AdminPolicy::default());
        registry.register_measurement(measurement.clone()).unwrap();
        registry.register_comparison(comparison.clone()).unwrap();
        let snapshot = registry.snapshot();
        let contract = freeze_test_contract(
            &snapshot,
            measurement.meta.id.clone(),
            comparison.meta.id.clone(),
            SamplingPlan {
                settle_ms: 10,
                sample_count: 1,
                sample_interval_ms: 0,
            },
            "budget-schedule",
        );
        let protocol = AbEvaluationProtocol::new(snapshot, Duration::from_millis(10)).unwrap();

        let error = protocol.validate_contract(&contract).unwrap_err();

        assert_eq!(error.kind, EvaluationErrorKind::BudgetExceeded);
        assert!(error.message.contains("schedules 20 ms"));
        assert_eq!(measurement.opens.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_call_is_rejected_when_declared_timeout_exceeds_remaining_budget() {
        let mut measurement_meta = meta(
            TRUSTED_GUARDRAIL_MEASUREMENT_ID,
            CapabilityKind::Measurement,
            EffectClass::ReadOnly,
        );
        measurement_meta.limits.timeout_ms = 100;
        let measurement = Arc::new(QueueMeasurement {
            meta: measurement_meta,
            samples: Mutex::new(VecDeque::new()),
            opens: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        });
        let comparison = Arc::new(PassingComparison {
            meta: meta(
                "test/comparison-budget-provider",
                CapabilityKind::Comparison,
                EffectClass::PureComputation,
            ),
        });
        let mut registry = CapabilityRegistry::new(AdminPolicy::default());
        registry.register_measurement(measurement.clone()).unwrap();
        registry.register_comparison(comparison.clone()).unwrap();
        let snapshot = registry.snapshot();
        let contract = freeze_test_contract(
            &snapshot,
            measurement.meta.id.clone(),
            comparison.meta.id.clone(),
            SamplingPlan {
                settle_ms: 0,
                sample_count: 1,
                sample_interval_ms: 0,
            },
            "budget-provider",
        );
        let candidate = Candidate::new(vec![ChangeId::new("change-budget").unwrap()]).unwrap();
        let context = InvocationContext {
            episode_id: EpisodeId::new(3),
            operation_id: OperationId::new("evaluate-budget").unwrap(),
        };
        let intent = freeze_test_intent(context.episode_id, contract);
        let mut transaction = FakeTransaction::default();

        let error = AbEvaluationProtocol::new(snapshot, Duration::from_millis(10))
            .unwrap()
            .evaluate(&mut transaction, &context, &intent, &candidate)
            .unwrap_err();

        assert_eq!(error.kind, EvaluationErrorKind::BudgetExceeded);
        assert!(error.message.contains("declares a 100 ms timeout"));
        assert!(transaction.calls.is_empty());
        assert_eq!(measurement.opens.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn protocol_runs_ab_and_never_has_finalize_authority() {
        let measurement = Arc::new(QueueMeasurement {
            meta: meta(
                TRUSTED_GUARDRAIL_MEASUREMENT_ID,
                CapabilityKind::Measurement,
                EffectClass::ReadOnly,
            ),
            samples: Mutex::new(VecDeque::from([batch(100.0, 1.0), batch(120.0, 1.0)])),
            opens: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        });
        let comparison = Arc::new(PassingComparison {
            meta: meta(
                "test/comparison",
                CapabilityKind::Comparison,
                EffectClass::PureComputation,
            ),
        });
        let measurement_id = measurement.meta.id.clone();
        let comparison_id = comparison.meta.id.clone();
        let mut registry = CapabilityRegistry::new(AdminPolicy::default());
        registry.register_measurement(measurement.clone()).unwrap();
        registry.register_comparison(comparison).unwrap();
        let snapshot = registry.snapshot();
        let contract = ContractFreezer::new(snapshot.clone())
            .freeze(
                crate::domain::ContractId::new("contract-1").unwrap(),
                EvaluationContractSpec {
                    measurement: MeasurementBinding {
                        capability_id: measurement_id,
                        specification: json!({}),
                    },
                    primary: vec![ComparisonBinding {
                        capability_id: comparison_id,
                        specification: json!({}),
                    }],
                    regression_guards: Vec::new(),
                    workload_invariants: Vec::new(),
                    sampling: SamplingPlan {
                        settle_ms: 0,
                        sample_count: 1,
                        sample_interval_ms: 0,
                    },
                },
            )
            .unwrap();
        let mut forged_contract = serde_json::to_value(&contract).unwrap();
        forged_contract["sampling"]["sample_count"] = json!(2);
        assert!(serde_json::from_value::<FrozenEvaluationContract>(forged_contract).is_err());
        let candidate = Candidate::new(vec![ChangeId::new("change-1").unwrap()]).unwrap();
        let context = InvocationContext {
            episode_id: EpisodeId::new(1),
            operation_id: OperationId::new("evaluation").unwrap(),
        };
        let intent = freeze_test_intent(context.episode_id, contract);
        let mut transaction = FakeTransaction::default();
        let protocol = AbEvaluationProtocol::new(snapshot, Duration::from_secs(600)).unwrap();
        let foreign_context = InvocationContext {
            episode_id: EpisodeId::new(99),
            operation_id: OperationId::new("foreign-evaluation").unwrap(),
        };

        let foreign_error = protocol
            .evaluate(&mut transaction, &foreign_context, &intent, &candidate)
            .unwrap_err();
        assert_eq!(foreign_error.kind, EvaluationErrorKind::InvalidContract);
        assert!(transaction.calls.is_empty());

        let evidence = protocol
            .evaluate(&mut transaction, &context, &intent, &candidate)
            .unwrap();

        assert_eq!(transaction.calls, vec!["restore", "replay"]);
        assert_eq!(measurement.opens.load(Ordering::SeqCst), 2);
        assert_eq!(measurement.closes.load(Ordering::SeqCst), 2);
        assert_eq!(&evidence.intent, intent.pin());
        assert_eq!(evidence.decision.verdict, EvaluationVerdict::Improved);

        let mut inconsistent = evidence.clone();
        inconsistent.contract_digest = content_digest(&"another contract").unwrap();
        assert!(inconsistent.commit_authorization().is_err());
    }

    #[test]
    fn trusted_system_measurement_remains_separate_with_fixed_guards_disabled() {
        let trusted = Arc::new(QueueMeasurement {
            meta: meta(
                TRUSTED_GUARDRAIL_MEASUREMENT_ID,
                CapabilityKind::Measurement,
                EffectClass::ReadOnly,
            ),
            samples: Mutex::new(VecDeque::from([batch(100.0, 1.0), batch(100.0, 3.0)])),
            opens: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        });
        let mut injected_meta = meta(
            "mcp/domain-measurement",
            CapabilityKind::Measurement,
            EffectClass::ReadOnly,
        );
        injected_meta.provider.provider_class = ProviderClass::Mcp;
        let injected = Arc::new(QueueMeasurement {
            meta: injected_meta,
            samples: Mutex::new(VecDeque::from([batch(100.0, 1.0), batch(120.0, 1.0)])),
            opens: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
        });
        let comparison = Arc::new(PassingComparison {
            meta: meta(
                "test/comparison-spoof",
                CapabilityKind::Comparison,
                EffectClass::PureComputation,
            ),
        });
        let mut registry =
            CapabilityRegistry::new(AdminPolicy::default().allow_provider_classes([
                ProviderClass::Builtin,
                ProviderClass::Local,
                ProviderClass::Mcp,
            ]));
        registry.register_measurement(trusted).unwrap();
        registry.register_measurement(injected.clone()).unwrap();
        registry.register_comparison(comparison.clone()).unwrap();
        let snapshot = registry.snapshot();
        let contract = ContractFreezer::new(snapshot.clone())
            .freeze(
                crate::domain::ContractId::new("contract-spoof").unwrap(),
                EvaluationContractSpec {
                    measurement: MeasurementBinding {
                        capability_id: injected.meta.id.clone(),
                        specification: json!({}),
                    },
                    primary: vec![ComparisonBinding {
                        capability_id: comparison.meta.id.clone(),
                        specification: json!({}),
                    }],
                    regression_guards: Vec::new(),
                    workload_invariants: Vec::new(),
                    sampling: SamplingPlan {
                        settle_ms: 0,
                        sample_count: 1,
                        sample_interval_ms: 0,
                    },
                },
            )
            .unwrap();
        let candidate = Candidate::new(vec![ChangeId::new("change-spoof").unwrap()]).unwrap();
        let context = InvocationContext {
            episode_id: EpisodeId::new(2),
            operation_id: OperationId::new("evaluate-spoof").unwrap(),
        };
        let intent = freeze_test_intent(context.episode_id, contract);
        let mut transaction = FakeTransaction::default();

        let evidence = AbEvaluationProtocol::new(snapshot, Duration::from_secs(600))
            .unwrap()
            .evaluate(&mut transaction, &context, &intent, &candidate)
            .unwrap();

        assert_eq!(evidence.decision.verdict, EvaluationVerdict::Improved);
        assert!(evidence.decision.system_guardrails.is_empty());
        assert_eq!(
            evidence.candidate_measurement.batch.metrics["psi.cpu.full.avg10"].value,
            json!(1.0)
        );
        assert_eq!(
            evidence.candidate_system_guardrails.batch.metrics["psi.cpu.full.avg10"].value,
            json!(3.0)
        );
    }
}
