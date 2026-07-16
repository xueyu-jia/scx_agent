use std::error::Error;
use std::fmt;

use crate::domain::{Digest, EpisodeId, EpisodePhase, TransactionId};
use crate::kernel::evaluation::FrozenEvaluationIntent;
use crate::runtime::episode::AgentAction;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitStep {
    InputsFrozen,
    RestoringBaseline,
    MeasuringBaseline,
    ReplayingCandidate,
    MeasuringCandidate,
    Comparing,
    Finalizing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EpisodeLifecycle {
    Active,
    Finishing,
    Finished,
}

#[derive(Clone, Debug)]
pub struct EpisodeStateMachine {
    episode_id: EpisodeId,
    phase: EpisodePhase,
    lifecycle: EpisodeLifecycle,
    intent: Option<FrozenEvaluationIntent>,
    transaction_id: Option<TransactionId>,
    candidate_digest: Option<Digest>,
    commit_step: Option<CommitStep>,
}

impl EpisodeStateMachine {
    pub fn new(episode_id: EpisodeId) -> Self {
        Self {
            episode_id,
            phase: EpisodePhase::Clean,
            lifecycle: EpisodeLifecycle::Active,
            intent: None,
            transaction_id: None,
            candidate_digest: None,
            commit_step: None,
        }
    }

    pub fn phase(&self) -> EpisodePhase {
        self.phase
    }

    #[cfg(test)]
    pub fn lifecycle(&self) -> EpisodeLifecycle {
        self.lifecycle
    }

    pub fn frozen_intent(&self) -> Option<&FrozenEvaluationIntent> {
        self.intent.as_ref()
    }

    #[cfg(test)]
    pub fn transaction_id(&self) -> Option<&TransactionId> {
        self.transaction_id.as_ref()
    }

    pub fn is_reasoning_stopped(&self) -> bool {
        self.lifecycle != EpisodeLifecycle::Active
    }

    #[cfg(test)]
    pub fn candidate_digest(&self) -> Option<&Digest> {
        self.candidate_digest.as_ref()
    }

    #[cfg(test)]
    pub fn commit_step(&self) -> Option<CommitStep> {
        self.commit_step
    }

    pub fn ensure_agent_action(&self, action: AgentAction) -> Result<(), TransitionError> {
        if action.is_allowed_in(
            self.phase,
            self.lifecycle == EpisodeLifecycle::Active,
            self.intent.is_some(),
        ) {
            Ok(())
        } else {
            Err(TransitionError::new(format!(
                "agent action {action:?} is not allowed in phase {:?}",
                self.phase
            )))
        }
    }

    pub fn freeze_intent(&mut self, intent: FrozenEvaluationIntent) -> Result<(), TransitionError> {
        self.require_active()?;
        self.require_phase(EpisodePhase::Clean)?;
        if self.intent.is_some() {
            return Err(TransitionError::new(
                "evaluation intent is already frozen for this episode",
            ));
        }
        if intent.episode_id() != self.episode_id {
            return Err(TransitionError::new(
                "evaluation intent belongs to a different episode",
            ));
        }
        if self.transaction_id.is_some() {
            return Err(TransitionError::new(
                "cannot freeze evaluation intent after a transaction has started",
            ));
        }
        self.intent = Some(intent);
        Ok(())
    }

    pub fn begin_transaction(
        &mut self,
        transaction_id: TransactionId,
    ) -> Result<(), TransitionError> {
        self.require_active()?;
        self.require_phase(EpisodePhase::Clean)?;
        if self.intent.is_none() {
            return Err(TransitionError::new(
                "evaluation intent must be frozen before a transaction starts",
            ));
        }
        if self.transaction_id.is_some() {
            return Err(TransitionError::new(
                "an episode cannot own more than one open transaction",
            ));
        }
        self.transaction_id = Some(transaction_id);
        self.phase = EpisodePhase::Experimenting;
        Ok(())
    }

    pub fn mutation_applied(&self, transaction_id: &TransactionId) -> Result<(), TransitionError> {
        self.require_active()?;
        self.require_phase(EpisodePhase::Experimenting)?;
        if self.transaction_id.as_ref() != Some(transaction_id) {
            return Err(TransitionError::new(
                "mutation result does not belong to the active transaction",
            ));
        }
        Ok(())
    }

    pub fn request_commit(&mut self, candidate_digest: Digest) -> Result<(), TransitionError> {
        self.require_active()?;
        self.require_phase(EpisodePhase::Experimenting)?;
        if self.intent.is_none() || self.transaction_id.is_none() {
            return Err(TransitionError::new(
                "commit requires a frozen evaluation intent and an active transaction",
            ));
        }
        self.candidate_digest = Some(candidate_digest);
        self.commit_step = Some(CommitStep::InputsFrozen);
        self.phase = EpisodePhase::CommitPending;
        self.lifecycle = EpisodeLifecycle::Finishing;
        Ok(())
    }

    pub fn advance_commit(&mut self, step: CommitStep) -> Result<(), TransitionError> {
        self.require_phase(EpisodePhase::CommitPending)?;
        let current = self
            .commit_step
            .ok_or_else(|| TransitionError::new("commit step is missing"))?;
        if commit_step_index(step) != commit_step_index(current) + 1 {
            return Err(TransitionError::new(format!(
                "invalid commit step transition from {current:?} to {step:?}"
            )));
        }
        self.commit_step = Some(step);
        Ok(())
    }

    pub fn begin_rollback(&mut self) -> Result<(), TransitionError> {
        match self.phase {
            EpisodePhase::Experimenting | EpisodePhase::CommitPending => {
                self.phase = EpisodePhase::RollingBack;
                self.lifecycle = EpisodeLifecycle::Finishing;
                self.commit_step = None;
                Ok(())
            }
            other => Err(TransitionError::new(format!(
                "rollback cannot start in phase {other:?}"
            ))),
        }
    }

    pub fn rollback_completed(&mut self) -> Result<(), TransitionError> {
        self.require_phase(EpisodePhase::RollingBack)?;
        self.phase = EpisodePhase::Clean;
        self.lifecycle = EpisodeLifecycle::Finished;
        self.transaction_id = None;
        self.candidate_digest = None;
        self.commit_step = None;
        Ok(())
    }

    pub fn commit_completed(&mut self) -> Result<(), TransitionError> {
        self.require_phase(EpisodePhase::CommitPending)?;
        if self.commit_step != Some(CommitStep::Finalizing) {
            return Err(TransitionError::new(
                "commit cannot complete before the finalizing step",
            ));
        }
        self.phase = EpisodePhase::Committed;
        self.lifecycle = EpisodeLifecycle::Finished;
        Ok(())
    }

    pub fn require_recovery(&mut self) {
        self.phase = EpisodePhase::RecoveryRequired;
        self.lifecycle = EpisodeLifecycle::Finished;
        self.commit_step = None;
    }

    pub fn stop_reasoning(&mut self) {
        if self.lifecycle == EpisodeLifecycle::Active {
            self.lifecycle = EpisodeLifecycle::Finishing;
        }
    }

    pub fn finish_clean(&mut self) -> Result<(), TransitionError> {
        self.require_phase(EpisodePhase::Clean)?;
        if self.transaction_id.is_some()
            || self.candidate_digest.is_some()
            || self.commit_step.is_some()
        {
            return Err(TransitionError::new(
                "clean episode cannot finish with transaction state",
            ));
        }
        self.lifecycle = EpisodeLifecycle::Finished;
        Ok(())
    }

    fn require_active(&self) -> Result<(), TransitionError> {
        if self.lifecycle == EpisodeLifecycle::Active {
            Ok(())
        } else {
            Err(TransitionError::new(format!(
                "episode is not active; lifecycle={:?}",
                self.lifecycle
            )))
        }
    }

    fn require_phase(&self, expected: EpisodePhase) -> Result<(), TransitionError> {
        if self.phase == expected {
            Ok(())
        } else {
            Err(TransitionError::new(format!(
                "expected phase {expected:?}, found {:?}",
                self.phase
            )))
        }
    }
}

fn commit_step_index(step: CommitStep) -> u8 {
    match step {
        CommitStep::InputsFrozen => 0,
        CommitStep::RestoringBaseline => 1,
        CommitStep::MeasuringBaseline => 2,
        CommitStep::ReplayingCandidate => 3,
        CommitStep::MeasuringCandidate => 4,
        CommitStep::Comparing => 5,
        CommitStep::Finalizing => 6,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransitionError {
    message: String,
}

impl TransitionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TransitionError {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::{
        CapabilityId, ContractId, EpisodeId, ProviderClass, ProviderId, ProviderPin,
        ProviderVersion,
    };
    use crate::kernel::evaluation::{
        CapabilityBindingPin, ComparisonBinding, FrozenEvaluationContract, MeasurementBinding,
        ObjectiveStatement, SamplingPlan,
    };

    fn id<T: TryFrom<&'static str, Error = String>>(value: &'static str) -> T {
        T::try_from(value).unwrap()
    }

    fn intent(contract_id: &'static str) -> FrozenEvaluationIntent {
        let measurement_id = CapabilityId::new("measurement/core").unwrap();
        let comparison_id = CapabilityId::new("comparison/threshold").unwrap();
        let pin = |capability_id| CapabilityBindingPin {
            capability_id,
            provider: ProviderPin {
                provider_id: ProviderId::new("test").unwrap(),
                provider_version: ProviderVersion::new("1").unwrap(),
                provider_class: ProviderClass::Builtin,
                manifest_digest: Digest::new("sha256:test-manifest").unwrap(),
            },
        };
        let contract = FrozenEvaluationContract::from_parts(
            ContractId::new(contract_id).unwrap(),
            MeasurementBinding {
                capability_id: measurement_id.clone(),
                specification: json!({}),
            },
            vec![ComparisonBinding {
                capability_id: comparison_id.clone(),
                specification: json!({}),
            }],
            Vec::new(),
            Vec::new(),
            SamplingPlan::default(),
            1,
            vec![pin(measurement_id), pin(comparison_id)],
        )
        .unwrap();
        FrozenEvaluationIntent::from_parts(
            EpisodeId::new(1),
            ObjectiveStatement::new("reduce latency").unwrap(),
            contract,
        )
        .unwrap()
    }

    #[test]
    fn normal_path_freezes_agent_calls_after_commit_request() {
        let mut state = EpisodeStateMachine::new(EpisodeId::new(1));
        state.freeze_intent(intent("contract-1")).unwrap();
        let transaction_id: TransactionId = id("tx-1");
        state.begin_transaction(transaction_id.clone()).unwrap();
        state.mutation_applied(&transaction_id).unwrap();
        state.request_commit(id("candidate-1")).unwrap();

        assert_eq!(state.phase(), EpisodePhase::CommitPending);
        assert!(state.ensure_agent_action(AgentAction::Probe).is_err());
        for step in [
            CommitStep::RestoringBaseline,
            CommitStep::MeasuringBaseline,
            CommitStep::ReplayingCandidate,
            CommitStep::MeasuringCandidate,
            CommitStep::Comparing,
            CommitStep::Finalizing,
        ] {
            state.advance_commit(step).unwrap();
        }
        state.commit_completed().unwrap();
        assert_eq!(state.phase(), EpisodePhase::Committed);
    }

    #[test]
    fn transaction_requires_a_frozen_evaluation_intent() {
        let mut state = EpisodeStateMachine::new(EpisodeId::new(1));
        assert!(state.begin_transaction(id("tx-2")).is_err());
        assert_eq!(state.phase(), EpisodePhase::Clean);
    }

    #[test]
    fn frozen_intent_cannot_be_replaced() {
        let mut state = EpisodeStateMachine::new(EpisodeId::new(1));
        state.freeze_intent(intent("contract-original")).unwrap();

        assert!(state.freeze_intent(intent("contract-replacement")).is_err());
        assert_eq!(
            state.frozen_intent().unwrap().contract().id(),
            &ContractId::new("contract-original").unwrap()
        );
        assert!(state.ensure_agent_action(AgentAction::Mutation).is_ok());
    }

    #[test]
    fn intent_from_another_episode_cannot_be_frozen() {
        let mut state = EpisodeStateMachine::new(EpisodeId::new(1));
        let local_shape = intent("foreign-contract");
        let foreign = FrozenEvaluationIntent::from_parts(
            EpisodeId::new(2),
            local_shape.objective().clone(),
            local_shape.contract().clone(),
        )
        .unwrap();

        assert!(state.freeze_intent(foreign).is_err());
        assert!(state.frozen_intent().is_none());
    }

    #[test]
    fn rollback_returns_to_a_strong_clean_state() {
        let mut state = EpisodeStateMachine::new(EpisodeId::new(1));
        state.freeze_intent(intent("contract-3")).unwrap();
        state.begin_transaction(id("tx-3")).unwrap();
        state.begin_rollback().unwrap();
        state.rollback_completed().unwrap();

        assert_eq!(state.phase(), EpisodePhase::Clean);
        assert_eq!(state.lifecycle(), EpisodeLifecycle::Finished);
        assert!(state.transaction_id().is_none());
        assert!(state.frozen_intent().is_some());
        assert!(state.candidate_digest().is_none());
        assert!(state.freeze_intent(intent("contract-replacement")).is_err());
    }

    #[test]
    fn commit_steps_cannot_be_skipped() {
        let mut state = EpisodeStateMachine::new(EpisodeId::new(1));
        state.freeze_intent(intent("contract-4")).unwrap();
        state.begin_transaction(id("tx-4")).unwrap();
        state.request_commit(id("candidate-4")).unwrap();

        assert!(state.advance_commit(CommitStep::MeasuringBaseline).is_err());
        assert_eq!(state.commit_step(), Some(CommitStep::InputsFrozen));
    }
}
