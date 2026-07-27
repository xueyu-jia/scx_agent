use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value;

use crate::capability::{CapabilitySnapshot, MutationDriver};
use crate::domain::{
    content_digest, Candidate, CapabilityId, ChangeId, CommitAuthorization, EvaluationIntentPin,
    InvocationContext, MutationApplyRequest, MutationFinalizeRequest, MutationOperationState,
    MutationPrepareRequest, MutationQuery, MutationReceipt, MutationRestoreRequest, MutationState,
    MutationVerifyRequest, OperationId, PreparedMutation, ProviderError, ResourceKey,
};
use crate::kernel::transaction::{
    ChangeRecord, ChangeState, OperationIntentKind, TransactionError, TransactionErrorKind,
    TransactionId, TransactionSeal, TransactionWal, WalEntry, WalEvent,
    MAX_CHANGES_PER_TRANSACTION,
};

pub struct TransactionKernel {
    transaction_id: TransactionId,
    intent_pin: EvaluationIntentPin,
    capabilities: CapabilitySnapshot,
    wal: Box<dyn TransactionWal>,
    changes: BTreeMap<ChangeId, ChangeRecord>,
    resource_heads: BTreeMap<ResourceKey, ChangeId>,
    change_order: Vec<ChangeId>,
    next_sequence: u64,
    operation_counter: u64,
    active_candidate: Option<Candidate>,
    commit_authorization: Option<CommitAuthorization>,
    sealed: Option<TransactionSeal>,
}

impl TransactionKernel {
    pub fn begin(
        transaction_id: TransactionId,
        intent_pin: EvaluationIntentPin,
        capabilities: CapabilitySnapshot,
        wal: Box<dyn TransactionWal>,
    ) -> Result<Self, TransactionError> {
        let mut kernel = Self {
            transaction_id,
            intent_pin,
            capabilities,
            wal,
            changes: BTreeMap::new(),
            resource_heads: BTreeMap::new(),
            change_order: Vec::new(),
            next_sequence: 0,
            operation_counter: 0,
            active_candidate: None,
            commit_authorization: None,
            sealed: None,
        };
        kernel.append_event(WalEvent::Started {
            intent_pin: kernel.intent_pin.clone(),
            capability_generation: kernel.capabilities.generation(),
        })?;
        Ok(kernel)
    }

    pub fn recover(
        transaction_id: TransactionId,
        intent_pin: EvaluationIntentPin,
        capabilities: CapabilitySnapshot,
        wal: Box<dyn TransactionWal>,
    ) -> Result<Self, TransactionError> {
        let entries = wal.load().map_err(wal_error)?;
        let mut changes = BTreeMap::new();
        let mut change_order = Vec::new();
        let mut resource_heads = BTreeMap::new();
        let mut pending_intents = BTreeMap::new();
        let mut sealed = None;
        let mut commit_authorization = None;
        let mut saw_start = false;
        let mut expected_sequence = 0;

        for entry in &entries {
            if entry.transaction_id != transaction_id {
                return Err(TransactionError::new(
                    TransactionErrorKind::CorruptWal,
                    "WAL contains a different transaction id",
                ));
            }
            if entry.sequence != expected_sequence {
                return Err(TransactionError::new(
                    TransactionErrorKind::CorruptWal,
                    format!(
                        "WAL sequence gap: expected {expected_sequence}, got {}",
                        entry.sequence
                    ),
                ));
            }
            expected_sequence += 1;
            match &entry.event {
                WalEvent::Started {
                    intent_pin: stored_intent_pin,
                    ..
                } => {
                    if saw_start || stored_intent_pin != &intent_pin || entry.sequence != 0 {
                        return Err(TransactionError::new(
                            TransactionErrorKind::CorruptWal,
                            "transaction start record does not match the expected evaluation intent",
                        ));
                    }
                    saw_start = true;
                }
                WalEvent::ChangeUpsert { change } => {
                    validate_recovered_change(change, &transaction_id, &capabilities)?;
                    if let Some(previous) = changes.get(&change.change_id) {
                        validate_recovered_transition(previous, change)?;
                    } else {
                        if changes.len() >= MAX_CHANGES_PER_TRANSACTION {
                            return Err(TransactionError::new(
                                TransactionErrorKind::CorruptWal,
                                format!(
                                    "transaction WAL contains more than {MAX_CHANGES_PER_TRANSACTION} changes"
                                ),
                            ));
                        }
                        if change.experiment_verified {
                            return Err(TransactionError::new(
                                TransactionErrorKind::CorruptWal,
                                format!(
                                    "first WAL record for change '{}' is already experiment-verified",
                                    change.change_id
                                ),
                            ));
                        }
                        validate_recovered_revision(change, &changes, &resource_heads)?;
                        resource_heads.insert(change.resource.clone(), change.change_id.clone());
                        change_order.push(change.change_id.clone());
                    }
                    if pending_intents
                        .get(&change.change_id)
                        .is_some_and(|(operation_id, _)| operation_id == &change.last_operation_id)
                    {
                        pending_intents.remove(&change.change_id);
                    }
                    changes.insert(change.change_id.clone(), change.as_ref().clone());
                }
                WalEvent::OperationIntent {
                    change_id,
                    operation_id,
                    operation,
                } => {
                    if !changes.contains_key(change_id) {
                        return Err(TransactionError::new(
                            TransactionErrorKind::CorruptWal,
                            format!("operation intent references unknown change '{change_id}'"),
                        ));
                    }
                    if *operation == OperationIntentKind::Finalize {
                        let change = &changes[change_id];
                        if change.state != ChangeState::CandidateApplied
                            || !change.experiment_verified
                        {
                            return Err(TransactionError::new(
                                TransactionErrorKind::CorruptWal,
                                format!(
                                    "finalize intent references non-candidate change '{change_id}'"
                                ),
                            ));
                        }
                    }
                    pending_intents.insert(change_id.clone(), (operation_id.clone(), *operation));
                }
                WalEvent::Sealed { outcome } => {
                    if sealed.replace(*outcome).is_some()
                        || entry.sequence + 1 != entries.len() as u64
                    {
                        return Err(TransactionError::new(
                            TransactionErrorKind::CorruptWal,
                            "seal must be the final and only terminal WAL record",
                        ));
                    }
                }
                WalEvent::CommitSealed {
                    authorization,
                    changes: terminal_changes,
                } => {
                    if sealed.replace(TransactionSeal::Committed).is_some()
                        || entry.sequence + 1 != entries.len() as u64
                    {
                        return Err(TransactionError::new(
                            TransactionErrorKind::CorruptWal,
                            "commit seal must be the final and only terminal WAL record",
                        ));
                    }
                    if !pending_intents.is_empty()
                        || terminal_changes.is_empty()
                        || terminal_changes.len() != changes.len()
                    {
                        return Err(TransactionError::new(
                            TransactionErrorKind::CorruptWal,
                            "commit seal does not cover a fully acknowledged change set",
                        ));
                    }
                    let mut seen = BTreeSet::new();
                    for terminal in terminal_changes {
                        validate_recovered_change(terminal, &transaction_id, &capabilities)?;
                        let previous = changes.get(&terminal.change_id).ok_or_else(|| {
                            TransactionError::new(
                                TransactionErrorKind::CorruptWal,
                                format!(
                                    "commit seal references unknown change '{}'",
                                    terminal.change_id
                                ),
                            )
                        })?;
                        validate_recovered_transition(previous, terminal)?;
                        if (terminal.state == ChangeState::Finalized
                            && previous.state != ChangeState::CandidateApplied)
                            || (terminal.state == ChangeState::RolledBack
                                && previous.state != ChangeState::BaselineRestored)
                        {
                            return Err(TransactionError::new(
                                TransactionErrorKind::CorruptWal,
                                "commit seal contains an invalid terminal state transition",
                            ));
                        }
                        if !matches!(
                            terminal.state,
                            ChangeState::Finalized | ChangeState::RolledBack
                        ) || !seen.insert(terminal.change_id.clone())
                        {
                            return Err(TransactionError::new(
                                TransactionErrorKind::CorruptWal,
                                "commit seal contains duplicate or non-terminal changes",
                            ));
                        }
                        if terminal.state == ChangeState::Finalized
                            && resource_heads.get(&terminal.resource) != Some(&terminal.change_id)
                        {
                            return Err(TransactionError::new(
                                TransactionErrorKind::CorruptWal,
                                format!(
                                    "commit seal finalized superseded change '{}'",
                                    terminal.change_id
                                ),
                            ));
                        }
                    }
                    let committed_candidate = Candidate::new(
                        terminal_changes
                            .iter()
                            .filter(|change| change.state == ChangeState::Finalized)
                            .map(|change| change.change_id.clone())
                            .collect(),
                    )
                    .map_err(|error| {
                        TransactionError::new(
                            TransactionErrorKind::CorruptWal,
                            format!("commit seal has an invalid candidate: {error}"),
                        )
                    })?;
                    if authorization.candidate_digest() != committed_candidate.digest() {
                        return Err(TransactionError::new(
                            TransactionErrorKind::CorruptWal,
                            "commit authorization does not match the committed candidate",
                        ));
                    }
                    if authorization.intent_pin() != &intent_pin {
                        return Err(TransactionError::new(
                            TransactionErrorKind::CorruptWal,
                            "commit authorization does not match the transaction evaluation intent",
                        ));
                    }
                    commit_authorization = Some(authorization.clone());
                    for terminal in terminal_changes {
                        changes.insert(terminal.change_id.clone(), terminal.clone());
                    }
                }
            }
        }
        if !saw_start {
            return Err(TransactionError::new(
                TransactionErrorKind::CorruptWal,
                "WAL is missing the transaction start record",
            ));
        }
        for (change_id, (operation_id, operation)) in pending_intents {
            if let Some(change) = changes.get_mut(&change_id) {
                if operation != OperationIntentKind::Finalize {
                    change.state = ChangeState::AppliedUnknown;
                }
                change.last_operation_id = operation_id;
                change.message = Some(match operation {
                    OperationIntentKind::Finalize => {
                        "recovered an unacknowledged finalize request; change remains rollbackable"
                            .into()
                    }
                    _ => "recovered an operation intent without a result".into(),
                });
            }
        }
        for change in changes.values_mut() {
            if change.state == ChangeState::IntentDurable {
                change.state = ChangeState::AppliedUnknown;
                change.message = Some("recovered an initial apply intent without a result".into());
            }
        }
        if let Some(outcome) = sealed {
            let terminal = changes.values().all(|change| match outcome {
                TransactionSeal::Committed => {
                    matches!(
                        change.state,
                        ChangeState::Finalized | ChangeState::RolledBack
                    ) && (change.state != ChangeState::Finalized || change.experiment_verified)
                }
                TransactionSeal::RolledBack => change.state == ChangeState::RolledBack,
            });
            if !terminal {
                return Err(TransactionError::new(
                    TransactionErrorKind::CorruptWal,
                    "sealed WAL contains non-terminal or unverified changes",
                ));
            }
        }
        let candidate_ids = change_order
            .iter()
            .filter_map(|change_id| {
                let change = &changes[change_id];
                (change.state == ChangeState::CandidateApplied && change.experiment_verified)
                    .then(|| change_id.clone())
            })
            .collect::<Vec<_>>();
        for change_id in &candidate_ids {
            let change = &changes[change_id];
            if resource_heads.get(&change.resource) != Some(change_id) {
                return Err(TransactionError::new(
                    TransactionErrorKind::CorruptWal,
                    format!("superseded change '{change_id}' is candidate-applied"),
                ));
            }
        }
        let active_candidate =
            if candidate_ids.is_empty() {
                None
            } else {
                Some(Candidate::new(candidate_ids).map_err(|error| {
                    TransactionError::new(TransactionErrorKind::CorruptWal, error)
                })?)
            };

        Ok(Self {
            transaction_id,
            intent_pin,
            capabilities,
            wal,
            changes,
            resource_heads,
            change_order,
            next_sequence: expected_sequence,
            operation_counter: expected_sequence,
            active_candidate,
            commit_authorization,
            sealed,
        })
    }

    pub fn changes(&self) -> impl Iterator<Item = &ChangeRecord> {
        self.change_order
            .iter()
            .filter_map(|change_id| self.changes.get(change_id))
    }

    #[cfg(test)]
    pub fn change(&self, change_id: &ChangeId) -> Option<&ChangeRecord> {
        self.changes.get(change_id)
    }

    #[cfg(test)]
    pub fn seal_state(&self) -> Option<TransactionSeal> {
        self.sealed
    }

    #[cfg(test)]
    pub fn commit_authorization(&self) -> Option<&CommitAuthorization> {
        self.commit_authorization.as_ref()
    }

    pub fn experiment(
        &mut self,
        change_id: ChangeId,
        capability_id: CapabilityId,
        arguments: Value,
    ) -> Result<ChangeRecord, TransactionError> {
        self.ensure_open()?;
        if self.changes.contains_key(&change_id) {
            return Err(TransactionError::new(
                TransactionErrorKind::DuplicateChange,
                format!("change '{change_id}' already exists"),
            ));
        }
        if self.changes.len() >= MAX_CHANGES_PER_TRANSACTION {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidState,
                format!(
                    "transaction cannot contain more than {MAX_CHANGES_PER_TRANSACTION} changes"
                ),
            ));
        }
        let driver = self.driver(&capability_id)?;
        let mut operation_id = self.next_operation_id("experiment")?;
        let mut prepared = driver
            .prepare(&MutationPrepareRequest {
                context: InvocationContext {
                    episode_id: self.intent_pin.episode_id(),
                    operation_id: operation_id.clone(),
                },
                arguments: arguments.clone(),
            })
            .map_err(provider_error)?;
        self.validate_prepared(&capability_id, &prepared)?;
        let supersedes = self.resource_heads.get(&prepared.resource).cloned();
        if let Some(previous_id) = &supersedes {
            let previous = self.changes.get(previous_id).cloned().ok_or_else(|| {
                TransactionError::new(
                    TransactionErrorKind::InvalidState,
                    format!("resource head references unknown change '{previous_id}'"),
                )
            })?;
            if previous.state != ChangeState::AppliedVerified || !previous.experiment_verified {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidState,
                    format!(
                        "resource '{}' cannot be revised from change '{}' in state {:?}",
                        previous.resource, previous.change_id, previous.state
                    ),
                ));
            }
            if previous.capability_id != capability_id
                || previous.prepared.provider != prepared.provider
            {
                return Err(TransactionError::new(
                    TransactionErrorKind::PinMismatch,
                    format!(
                        "resource '{}' must be revised through the same pinned capability",
                        previous.resource
                    ),
                ));
            }
            self.ensure_expected(previous_id, ExpectedState::Desired)?;
            if prepared.baseline != previous.prepared.desired {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidState,
                    format!(
                        "revision prepare for resource '{}' did not observe the latest desired state",
                        previous.resource
                    ),
                ));
            }
            self.restore_one(previous_id, ChangeState::BaselineRestored)?;

            let initially_prepared = prepared;
            operation_id = self.next_operation_id("revision")?;
            prepared = driver
                .prepare(&MutationPrepareRequest {
                    context: InvocationContext {
                        episode_id: self.intent_pin.episode_id(),
                        operation_id: operation_id.clone(),
                    },
                    arguments,
                })
                .map_err(provider_error)?;
            self.validate_prepared(&capability_id, &prepared)?;
            if prepared.resource != previous.resource
                || prepared.baseline != previous.prepared.baseline
                || prepared.desired != initially_prepared.desired
            {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidState,
                    format!(
                        "resource '{}' revision changed identity, original baseline, or desired state during prepare",
                        previous.resource
                    ),
                ));
            }
        }

        let record = ChangeRecord {
            transaction_id: self.transaction_id.clone(),
            change_id: change_id.clone(),
            supersedes,
            capability_id,
            resource: prepared.resource.clone(),
            prepared,
            experiment_verified: false,
            state: ChangeState::IntentDurable,
            last_operation_id: operation_id.clone(),
            last_receipt: None,
            message: None,
        };
        // This durable record is the write-ahead intent for the first apply.
        self.append_event(WalEvent::ChangeUpsert {
            change: Box::new(record.clone()),
        })?;
        self.resource_heads
            .insert(record.resource.clone(), change_id.clone());
        self.change_order.push(change_id.clone());
        self.changes.insert(change_id.clone(), record);

        self.apply_and_verify(
            &change_id,
            operation_id,
            ChangeState::AppliedVerified,
            false,
        )?;
        Ok(self.changes[&change_id].clone())
    }

    pub fn restore_baseline(&mut self) -> Result<Vec<ChangeRecord>, TransactionError> {
        self.ensure_open()?;
        let ids = self.change_order.iter().rev().cloned().collect::<Vec<_>>();
        for change_id in ids {
            self.restore_one(&change_id, ChangeState::BaselineRestored)?;
        }
        self.active_candidate = None;
        Ok(self.changes().cloned().collect())
    }

    pub fn replay_candidate(
        &mut self,
        candidate: &Candidate,
    ) -> Result<Vec<ChangeRecord>, TransactionError> {
        self.ensure_open()?;
        self.validate_candidate(candidate)?;
        for change_id in &self.change_order {
            let change = &self.changes[change_id];
            if change.state != ChangeState::BaselineRestored {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidState,
                    format!(
                        "change '{}' must be restored before candidate replay; state={:?}",
                        change_id, change.state
                    ),
                ));
            }
        }
        let ids = self.change_order.clone();
        for change_id in &ids {
            self.ensure_expected(change_id, ExpectedState::Baseline)?;
        }

        self.active_candidate = Some(candidate.clone());
        for change_id in candidate.change_ids() {
            let operation_id = self.next_operation_id("replay")?;
            self.append_operation_intent(change_id, &operation_id, OperationIntentKind::Apply)?;
            self.apply_and_verify(change_id, operation_id, ChangeState::CandidateApplied, true)?;
        }
        Ok(candidate
            .change_ids()
            .iter()
            .filter_map(|id| self.changes.get(id).cloned())
            .collect())
    }

    pub fn finalize_candidate(
        &mut self,
        candidate: &Candidate,
        authorization: &CommitAuthorization,
    ) -> Result<Vec<ChangeRecord>, TransactionError> {
        self.ensure_open()?;
        self.validate_candidate(candidate)?;
        if authorization.intent_pin() != &self.intent_pin {
            return Err(TransactionError::new(
                TransactionErrorKind::PinMismatch,
                "commit authorization does not match the transaction evaluation intent",
            ));
        }
        if authorization.candidate_digest() != candidate.digest() {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidCandidate,
                "commit authorization does not match the candidate",
            ));
        }
        if self.active_candidate.as_ref() != Some(candidate) {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidCandidate,
                "candidate does not match the replayed candidate",
            ));
        }

        let ids = self.change_order.clone();
        // Complete all drift checks before the first irreversible finalize call.
        for change_id in &ids {
            let change = &self.changes[change_id];
            if self.resource_heads.get(&change.resource) != Some(change_id) {
                continue;
            }
            if candidate.contains(change_id) {
                if change.state != ChangeState::CandidateApplied {
                    return Err(TransactionError::new(
                        TransactionErrorKind::InvalidState,
                        format!("candidate change '{change_id}' is not applied"),
                    ));
                }
                self.ensure_expected(change_id, ExpectedState::Desired)?;
            } else {
                self.ensure_expected(change_id, ExpectedState::Baseline)?;
            }
        }

        for change_id in candidate.change_ids() {
            let operation_id = self.next_operation_id("finalize")?;
            self.append_operation_intent(change_id, &operation_id, OperationIntentKind::Finalize)?;
            let record = self.changes[change_id].clone();
            let driver = self.driver_for_record(&record)?;
            let result = driver.finalize(&MutationFinalizeRequest {
                operation_id: operation_id.clone(),
                prepared: record.prepared.clone(),
            });
            let receipt = match result {
                Ok(receipt) if receipt.state == MutationOperationState::Finalized => receipt,
                Ok(receipt) => {
                    self.update_change(
                        change_id,
                        ChangeState::CandidateApplied,
                        operation_id,
                        Some(receipt),
                        Some("provider did not acknowledge finalize".into()),
                    )?;
                    return Err(TransactionError::new(
                        TransactionErrorKind::Provider,
                        format!("provider did not acknowledge finalize for change '{change_id}'"),
                    ));
                }
                Err(error) => {
                    let message = format!("provider finalize acknowledgement failed: {error}");
                    self.update_change(
                        change_id,
                        ChangeState::CandidateApplied,
                        operation_id,
                        None,
                        Some(message.clone()),
                    )?;
                    return Err(TransactionError::new(
                        TransactionErrorKind::Provider,
                        format!("change '{change_id}': {message}"),
                    ));
                }
            };
            // Finalize is an idempotent acknowledgement with no system-state
            // side effect. Persist the ack while retaining rollback authority.
            self.update_change(
                change_id,
                ChangeState::CandidateApplied,
                operation_id,
                Some(receipt),
                None,
            )?;
            self.ensure_expected(change_id, ExpectedState::Desired)?;
        }

        // Ack collection can take time. Recheck the complete candidate/baseline
        // boundary immediately before making the commit decision durable.
        for change_id in &ids {
            let change = &self.changes[change_id];
            if self.resource_heads.get(&change.resource) != Some(change_id) {
                continue;
            }
            self.ensure_expected(
                change_id,
                if candidate.contains(change_id) {
                    ExpectedState::Desired
                } else {
                    ExpectedState::Baseline
                },
            )?;
        }

        let mut terminal_changes = Vec::with_capacity(ids.len());
        for change_id in &ids {
            let mut terminal = self.changes[change_id].clone();
            terminal.state = if candidate.contains(change_id) {
                ChangeState::Finalized
            } else {
                ChangeState::RolledBack
            };
            terminal.message = None;
            terminal_changes.push(terminal);
        }
        self.commit_seal(authorization.clone(), terminal_changes)?;
        Ok(self.changes().cloned().collect())
    }

    pub fn rollback_all(&mut self) -> Result<Vec<ChangeRecord>, TransactionError> {
        self.ensure_open()?;
        if self
            .changes
            .values()
            .any(|change| change.state == ChangeState::Finalized)
        {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidState,
                "cannot roll back a transaction with finalized changes",
            ));
        }
        let ids = self.change_order.iter().rev().cloned().collect::<Vec<_>>();
        for change_id in ids {
            self.restore_one(&change_id, ChangeState::RolledBack)?;
        }
        self.active_candidate = None;
        self.seal(TransactionSeal::RolledBack)?;
        Ok(self.changes().cloned().collect())
    }

    fn apply_and_verify(
        &mut self,
        change_id: &ChangeId,
        operation_id: OperationId,
        success_state: ChangeState,
        intent_already_appended: bool,
    ) -> Result<(), TransactionError> {
        if !intent_already_appended && self.changes[change_id].state != ChangeState::IntentDurable {
            self.append_operation_intent(change_id, &operation_id, OperationIntentKind::Apply)?;
        }
        let record = self.changes[change_id].clone();
        let driver = self.driver_for_record(&record)?;
        let apply = driver.apply(&MutationApplyRequest {
            operation_id: operation_id.clone(),
            prepared: record.prepared.clone(),
        });
        let receipt = match apply {
            Ok(receipt) if receipt.state == MutationOperationState::Applied => Some(receipt),
            Ok(receipt) if receipt.state == MutationOperationState::NotApplied => {
                self.update_change(
                    change_id,
                    ChangeState::FailedNotApplied,
                    operation_id,
                    Some(receipt),
                    Some("driver reported not applied".into()),
                )?;
                return Err(TransactionError::new(
                    TransactionErrorKind::Provider,
                    format!("change '{change_id}' was not applied"),
                ));
            }
            Ok(receipt) => {
                return self.mark_apply_unknown(
                    change_id,
                    operation_id,
                    Some(receipt),
                    "driver returned an ambiguous apply state",
                );
            }
            Err(error) => match driver.status(&MutationQuery {
                operation_id: operation_id.clone(),
            }) {
                Ok(status) if status.state == MutationOperationState::Applied => None,
                Ok(status) if status.state == MutationOperationState::NotApplied => {
                    self.update_change(
                        change_id,
                        ChangeState::FailedNotApplied,
                        operation_id,
                        None,
                        Some(format!("apply failed and status is not-applied: {error}")),
                    )?;
                    return Err(provider_error(error));
                }
                _ => {
                    return self.mark_apply_unknown(
                        change_id,
                        operation_id,
                        None,
                        &format!("apply failed and status could not prove the outcome: {error}"),
                    );
                }
            },
        };

        let verification = driver.verify(&MutationVerifyRequest {
            operation_id: operation_id.clone(),
            prepared: record.prepared.clone(),
            expected: record.prepared.desired.clone(),
        });
        match verification {
            Ok(verification) if verification.matched => self.update_after_effect(
                change_id,
                success_state,
                operation_id,
                receipt,
                None,
                success_state == ChangeState::AppliedVerified,
            ),
            Ok(_) => self.mark_apply_unknown(
                change_id,
                operation_id,
                receipt,
                "applied value did not match the prepared desired state",
            ),
            Err(error) => self.mark_apply_unknown(
                change_id,
                operation_id,
                receipt,
                &format!("applied value could not be verified: {error}"),
            ),
        }
    }

    fn restore_one(
        &mut self,
        change_id: &ChangeId,
        restored_state: ChangeState,
    ) -> Result<(), TransactionError> {
        let state = self.changes[change_id].state;
        if state == ChangeState::Finalized {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidState,
                format!("finalized change '{change_id}' cannot be restored"),
            ));
        }
        if state == ChangeState::RolledBack && restored_state == ChangeState::RolledBack {
            self.ensure_expected(change_id, ExpectedState::Baseline)?;
            return Ok(());
        }

        if self.matches_expected(change_id, ExpectedState::Baseline)? {
            if state != restored_state {
                let operation_id = self.next_operation_id("baseline-observed")?;
                self.update_change(change_id, restored_state, operation_id, None, None)?;
            }
            return Ok(());
        }
        if !self.matches_expected(change_id, ExpectedState::Desired)? {
            return self.mark_drift(
                change_id,
                "resource matches neither baseline nor desired state",
            );
        }

        let operation_id = self.next_operation_id("restore")?;
        self.append_operation_intent(change_id, &operation_id, OperationIntentKind::Restore)?;
        let record = self.changes[change_id].clone();
        let driver = self.driver_for_record(&record)?;
        let receipt = match driver.restore(&MutationRestoreRequest {
            operation_id: operation_id.clone(),
            prepared: record.prepared.clone(),
        }) {
            Ok(receipt) if receipt.state == MutationOperationState::Restored => receipt,
            Ok(receipt) => {
                self.set_unknown(
                    change_id,
                    operation_id.clone(),
                    Some(receipt),
                    "restore returned an ambiguous state",
                )?;
                return Err(TransactionError::new(
                    TransactionErrorKind::AppliedUnknown,
                    format!("restore outcome for change '{change_id}' is unknown"),
                ));
            }
            Err(error) => {
                let known_restored = driver
                    .status(&MutationQuery {
                        operation_id: operation_id.clone(),
                    })
                    .is_ok_and(|status| status.state == MutationOperationState::Restored);
                if !known_restored {
                    return Err(self.unknown_after_provider_error(change_id, operation_id, error));
                }
                MutationReceipt {
                    operation_id: operation_id.clone(),
                    state: MutationOperationState::Restored,
                    observed: Some(record.prepared.baseline.clone()),
                    driver_data: Value::Null,
                }
            }
        };
        if !self.matches_expected(change_id, ExpectedState::Baseline)? {
            self.set_unknown(
                change_id,
                operation_id.clone(),
                Some(receipt),
                "restore completed but baseline readback did not match",
            )?;
            return Err(TransactionError::new(
                TransactionErrorKind::AppliedUnknown,
                format!("restored state of change '{change_id}' could not be verified"),
            ));
        }
        self.update_after_effect(
            change_id,
            restored_state,
            operation_id,
            Some(receipt),
            None,
            false,
        )
    }

    fn ensure_expected(
        &mut self,
        change_id: &ChangeId,
        expected: ExpectedState,
    ) -> Result<(), TransactionError> {
        if self.matches_expected(change_id, expected)? {
            Ok(())
        } else {
            self.mark_drift(change_id, "resource changed outside the transaction")
        }
    }

    fn matches_expected(
        &self,
        change_id: &ChangeId,
        expected: ExpectedState,
    ) -> Result<bool, TransactionError> {
        let record = self.changes[change_id].clone();
        let driver = self.driver_for_record(&record)?;
        let expected = match expected {
            ExpectedState::Baseline => record.prepared.baseline.clone(),
            ExpectedState::Desired => record.prepared.desired.clone(),
        };
        let operation_id = OperationId::new(format!(
            "{}/verify/{}",
            self.transaction_id, self.operation_counter
        ))
        .map_err(|error| TransactionError::new(TransactionErrorKind::InvalidState, error))?;
        driver
            .verify(&MutationVerifyRequest {
                operation_id,
                prepared: record.prepared,
                expected,
            })
            .map(|verification| verification.matched)
            .map_err(provider_error)
    }

    fn mark_drift<T>(
        &mut self,
        change_id: &ChangeId,
        message: &str,
    ) -> Result<T, TransactionError> {
        let operation_id = self.next_operation_id("drift")?;
        self.update_change(
            change_id,
            ChangeState::DriftDetected,
            operation_id,
            None,
            Some(message.into()),
        )?;
        Err(TransactionError::new(
            TransactionErrorKind::ExternalDrift,
            format!("external drift detected for change '{change_id}': {message}"),
        ))
    }

    fn mark_apply_unknown(
        &mut self,
        change_id: &ChangeId,
        operation_id: OperationId,
        receipt: Option<MutationReceipt>,
        message: &str,
    ) -> Result<(), TransactionError> {
        self.set_unknown(change_id, operation_id, receipt, message)?;
        Err(TransactionError::new(
            TransactionErrorKind::AppliedUnknown,
            format!("apply outcome for change '{change_id}' is unknown: {message}"),
        ))
    }

    fn unknown_after_provider_error(
        &mut self,
        change_id: &ChangeId,
        operation_id: OperationId,
        error: ProviderError,
    ) -> TransactionError {
        let message = format!("provider operation outcome is unknown: {error}");
        let persist_error = self
            .set_unknown(change_id, operation_id, None, &message)
            .err();
        persist_error.unwrap_or_else(|| {
            TransactionError::new(
                TransactionErrorKind::AppliedUnknown,
                format!("change '{change_id}': {message}"),
            )
        })
    }

    fn set_unknown(
        &mut self,
        change_id: &ChangeId,
        operation_id: OperationId,
        receipt: Option<MutationReceipt>,
        message: &str,
    ) -> Result<(), TransactionError> {
        self.update_change(
            change_id,
            ChangeState::AppliedUnknown,
            operation_id,
            receipt,
            Some(message.into()),
        )
    }

    fn append_operation_intent(
        &mut self,
        change_id: &ChangeId,
        operation_id: &OperationId,
        operation: OperationIntentKind,
    ) -> Result<(), TransactionError> {
        self.append_event(WalEvent::OperationIntent {
            change_id: change_id.clone(),
            operation_id: operation_id.clone(),
            operation,
        })
    }

    fn update_change(
        &mut self,
        change_id: &ChangeId,
        state: ChangeState,
        operation_id: OperationId,
        receipt: Option<MutationReceipt>,
        message: Option<String>,
    ) -> Result<(), TransactionError> {
        self.persist_change(change_id, state, operation_id, receipt, message, false)
    }

    fn persist_change(
        &mut self,
        change_id: &ChangeId,
        state: ChangeState,
        operation_id: OperationId,
        receipt: Option<MutationReceipt>,
        message: Option<String>,
        mark_experiment_verified: bool,
    ) -> Result<(), TransactionError> {
        let mut snapshot = self.changes.get(change_id).cloned().ok_or_else(|| {
            TransactionError::new(
                TransactionErrorKind::InvalidState,
                format!("unknown change '{change_id}'"),
            )
        })?;
        snapshot.state = state;
        snapshot.last_operation_id = operation_id;
        snapshot.last_receipt = receipt;
        snapshot.message = message;
        if mark_experiment_verified {
            snapshot.experiment_verified = true;
        }
        self.append_event(WalEvent::ChangeUpsert {
            change: Box::new(snapshot.clone()),
        })?;
        self.changes.insert(change_id.clone(), snapshot);
        Ok(())
    }

    fn update_after_effect(
        &mut self,
        change_id: &ChangeId,
        state: ChangeState,
        operation_id: OperationId,
        receipt: Option<MutationReceipt>,
        message: Option<String>,
        mark_experiment_verified: bool,
    ) -> Result<(), TransactionError> {
        match self.persist_change(
            change_id,
            state,
            operation_id.clone(),
            receipt,
            message,
            mark_experiment_verified,
        ) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Some(change) = self.changes.get_mut(change_id) {
                    change.state = ChangeState::AppliedUnknown;
                    change.last_operation_id = operation_id;
                    change.message =
                        Some("effect completed but its result was not durable in the WAL".into());
                }
                Err(error)
            }
        }
    }

    fn validate_candidate(&self, candidate: &Candidate) -> Result<(), TransactionError> {
        let mut resources = BTreeSet::new();
        for change_id in candidate.change_ids() {
            let change = self.changes.get(change_id).ok_or_else(|| {
                TransactionError::new(
                    TransactionErrorKind::InvalidCandidate,
                    format!("candidate references unknown change '{change_id}'"),
                )
            })?;
            if !change.experiment_verified {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidCandidate,
                    format!("candidate change '{change_id}' has no durable successful experiment"),
                ));
            }
            if self.resource_heads.get(&change.resource) != Some(change_id) {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidCandidate,
                    format!("candidate change '{change_id}' has been superseded"),
                ));
            }
            if !resources.insert(change.resource.clone()) {
                return Err(TransactionError::new(
                    TransactionErrorKind::DuplicateResource,
                    format!("candidate repeats resource '{}'", change.resource),
                ));
            }
        }
        Ok(())
    }

    fn validate_prepared(
        &self,
        capability_id: &CapabilityId,
        prepared: &PreparedMutation,
    ) -> Result<(), TransactionError> {
        if &prepared.capability_id != capability_id {
            return Err(TransactionError::new(
                TransactionErrorKind::PinMismatch,
                "driver prepared a different capability id",
            ));
        }
        let meta = self.capabilities.meta(capability_id).ok_or_else(|| {
            TransactionError::new(
                TransactionErrorKind::CapabilityUnavailable,
                format!("capability '{capability_id}' is absent from the snapshot"),
            )
        })?;
        if prepared.provider != meta.provider {
            return Err(TransactionError::new(
                TransactionErrorKind::PinMismatch,
                format!("provider pin changed while preparing '{capability_id}'"),
            ));
        }
        validate_state_digest(
            &prepared.baseline,
            "prepared baseline",
            TransactionErrorKind::InvalidState,
        )?;
        validate_state_digest(
            &prepared.desired,
            "prepared desired state",
            TransactionErrorKind::InvalidState,
        )?;
        if prepared.baseline.value == prepared.desired.value {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidState,
                "mutation baseline and desired states are identical",
            ));
        }
        Ok(())
    }

    fn driver(
        &self,
        capability_id: &CapabilityId,
    ) -> Result<Arc<dyn MutationDriver>, TransactionError> {
        self.capabilities.mutation(capability_id).ok_or_else(|| {
            TransactionError::new(
                TransactionErrorKind::CapabilityUnavailable,
                format!("mutation capability '{capability_id}' is unavailable"),
            )
        })
    }

    fn driver_for_record(
        &self,
        record: &ChangeRecord,
    ) -> Result<Arc<dyn MutationDriver>, TransactionError> {
        let driver = self.driver(&record.capability_id)?;
        if driver.meta().provider != record.prepared.provider {
            return Err(TransactionError::new(
                TransactionErrorKind::PinMismatch,
                format!(
                    "provider pin for change '{}' no longer matches",
                    record.change_id
                ),
            ));
        }
        Ok(driver)
    }

    fn ensure_open(&self) -> Result<(), TransactionError> {
        if let Some(seal) = self.sealed {
            return Err(TransactionError::new(
                TransactionErrorKind::Sealed,
                format!("transaction is already sealed as {seal:?}"),
            ));
        }
        Ok(())
    }

    fn next_operation_id(&mut self, label: &str) -> Result<OperationId, TransactionError> {
        self.operation_counter = self.operation_counter.saturating_add(1);
        OperationId::new(format!(
            "{}/{label}/{}",
            self.transaction_id, self.operation_counter
        ))
        .map_err(|error| TransactionError::new(TransactionErrorKind::InvalidState, error))
    }

    fn append_event(&mut self, event: WalEvent) -> Result<(), TransactionError> {
        let entry = WalEntry {
            sequence: self.next_sequence,
            transaction_id: self.transaction_id.clone(),
            event,
        };
        self.wal.append_durable(&entry).map_err(wal_error)?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(())
    }

    fn seal(&mut self, outcome: TransactionSeal) -> Result<(), TransactionError> {
        let entry = WalEntry {
            sequence: self.next_sequence,
            transaction_id: self.transaction_id.clone(),
            event: WalEvent::Sealed { outcome },
        };
        self.wal.seal(&entry).map_err(wal_error)?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.sealed = Some(outcome);
        Ok(())
    }

    fn commit_seal(
        &mut self,
        authorization: CommitAuthorization,
        terminal_changes: Vec<ChangeRecord>,
    ) -> Result<(), TransactionError> {
        let entry = WalEntry {
            sequence: self.next_sequence,
            transaction_id: self.transaction_id.clone(),
            event: WalEvent::CommitSealed {
                authorization: authorization.clone(),
                changes: terminal_changes.clone(),
            },
        };
        self.wal.seal(&entry).map_err(|error| {
            TransactionError::new(
                TransactionErrorKind::CommitOutcomeUnknown,
                format!("commit seal outcome is unknown: {error}"),
            )
        })?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        for change in terminal_changes {
            self.changes.insert(change.change_id.clone(), change);
        }
        self.commit_authorization = Some(authorization);
        self.sealed = Some(TransactionSeal::Committed);
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ExpectedState {
    Baseline,
    Desired,
}

fn validate_recovered_change(
    change: &ChangeRecord,
    transaction_id: &TransactionId,
    capabilities: &CapabilitySnapshot,
) -> Result<(), TransactionError> {
    if &change.transaction_id != transaction_id
        || change.resource != change.prepared.resource
        || change.capability_id != change.prepared.capability_id
        || change.supersedes.as_ref() == Some(&change.change_id)
    {
        return Err(TransactionError::new(
            TransactionErrorKind::CorruptWal,
            format!(
                "change '{}' has inconsistent persisted identity",
                change.change_id
            ),
        ));
    }
    validate_state_digest(
        &change.prepared.baseline,
        "persisted baseline",
        TransactionErrorKind::CorruptWal,
    )?;
    if change.prepared.baseline == change.prepared.desired {
        return Err(TransactionError::new(
            TransactionErrorKind::CorruptWal,
            format!(
                "change '{}' has identical baseline and desired states",
                change.change_id
            ),
        ));
    }
    validate_state_digest(
        &change.prepared.desired,
        "persisted desired state",
        TransactionErrorKind::CorruptWal,
    )?;
    if matches!(
        change.state,
        ChangeState::AppliedVerified | ChangeState::CandidateApplied | ChangeState::Finalized
    ) && !change.experiment_verified
    {
        return Err(TransactionError::new(
            TransactionErrorKind::CorruptWal,
            format!(
                "change '{}' claims a verified state without durable experiment evidence",
                change.change_id
            ),
        ));
    }
    let meta = capabilities.meta(&change.capability_id).ok_or_else(|| {
        TransactionError::new(
            TransactionErrorKind::CapabilityUnavailable,
            format!(
                "capability '{}' is unavailable during recovery",
                change.capability_id
            ),
        )
    })?;
    if meta.provider != change.prepared.provider
        || capabilities.mutation(&change.capability_id).is_none()
    {
        return Err(TransactionError::new(
            TransactionErrorKind::PinMismatch,
            format!(
                "provider pin mismatch for recovered change '{}'",
                change.change_id
            ),
        ));
    }
    Ok(())
}

fn validate_recovered_revision(
    change: &ChangeRecord,
    changes: &BTreeMap<ChangeId, ChangeRecord>,
    resource_heads: &BTreeMap<ResourceKey, ChangeId>,
) -> Result<(), TransactionError> {
    if change.state != ChangeState::IntentDurable {
        return Err(TransactionError::new(
            TransactionErrorKind::CorruptWal,
            format!(
                "first WAL record for change '{}' is not an apply intent",
                change.change_id
            ),
        ));
    }
    match &change.supersedes {
        None => {
            if let Some(owner) = resource_heads.get(&change.resource) {
                return Err(TransactionError::new(
                    TransactionErrorKind::CorruptWal,
                    format!(
                        "resource '{}' is already owned by change '{}' but '{}' has no revision link",
                        change.resource, owner, change.change_id
                    ),
                ));
            }
        }
        Some(previous_id) => {
            if resource_heads.get(&change.resource) != Some(previous_id) {
                return Err(TransactionError::new(
                    TransactionErrorKind::CorruptWal,
                    format!(
                        "change '{}' does not supersede the latest revision of resource '{}'",
                        change.change_id, change.resource
                    ),
                ));
            }
            let previous = changes.get(previous_id).ok_or_else(|| {
                TransactionError::new(
                    TransactionErrorKind::CorruptWal,
                    format!(
                        "change '{}' supersedes unknown change '{}'",
                        change.change_id, previous_id
                    ),
                )
            })?;
            if previous.state != ChangeState::BaselineRestored
                || !previous.experiment_verified
                || previous.capability_id != change.capability_id
                || previous.prepared.provider != change.prepared.provider
                || previous.prepared.baseline != change.prepared.baseline
            {
                return Err(TransactionError::new(
                    TransactionErrorKind::CorruptWal,
                    format!(
                        "change '{}' has an invalid predecessor '{}'",
                        change.change_id, previous_id
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn validate_state_digest(
    state: &MutationState,
    label: &str,
    kind: TransactionErrorKind,
) -> Result<(), TransactionError> {
    let expected = content_digest(&state.value).map_err(|error| {
        TransactionError::new(kind, format!("failed to compute {label} digest: {error}"))
    })?;
    if state.digest != expected {
        return Err(TransactionError::new(
            kind,
            format!("{label} digest does not match its value"),
        ));
    }
    Ok(())
}

fn validate_recovered_transition(
    previous: &ChangeRecord,
    next: &ChangeRecord,
) -> Result<(), TransactionError> {
    if previous.transaction_id != next.transaction_id
        || previous.change_id != next.change_id
        || previous.supersedes != next.supersedes
        || previous.capability_id != next.capability_id
        || previous.resource != next.resource
        || previous.prepared != next.prepared
    {
        return Err(TransactionError::new(
            TransactionErrorKind::CorruptWal,
            format!(
                "immutable identity changed for WAL change '{}'",
                previous.change_id
            ),
        ));
    }
    if previous.experiment_verified && !next.experiment_verified {
        return Err(TransactionError::new(
            TransactionErrorKind::CorruptWal,
            format!(
                "experiment verification was cleared for change '{}'",
                previous.change_id
            ),
        ));
    }
    if !previous.experiment_verified
        && next.experiment_verified
        && next.state != ChangeState::AppliedVerified
    {
        return Err(TransactionError::new(
            TransactionErrorKind::CorruptWal,
            format!(
                "change '{}' gained experiment verification outside initial apply",
                previous.change_id
            ),
        ));
    }
    Ok(())
}

fn provider_error(error: ProviderError) -> TransactionError {
    TransactionError::new(TransactionErrorKind::Provider, error.to_string())
}

fn wal_error(error: crate::kernel::transaction::WalError) -> TransactionError {
    TransactionError::new(TransactionErrorKind::Wal, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::capability::{AdminPolicy, CapabilityRegistry};
    use crate::domain::{
        CapabilityKind, CapabilityMeta, Digest, EffectClass, EpisodeId, EpisodePhase,
        EvaluationIntentPin, MutationState, MutationStatus, MutationVerification, ProviderClass,
        ProviderErrorKind, ProviderId, ProviderPin, ProviderVersion,
    };
    use crate::kernel::transaction::{WalError, WalEvent};

    #[derive(Clone, Copy)]
    enum ApplyMode {
        Normal,
        ApplyThenUnknown,
    }

    struct FakeDriver {
        meta: CapabilityMeta,
        current: Arc<Mutex<BTreeMap<String, String>>>,
        statuses: Mutex<BTreeMap<OperationId, MutationStatus>>,
        apply_mode: ApplyMode,
        finalize_calls: AtomicUsize,
        fail_finalize_at: Option<usize>,
        wal_entries: Arc<Mutex<Vec<WalEntry>>>,
        saw_durable_intent: Arc<Mutex<bool>>,
    }

    impl MutationDriver for FakeDriver {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn prepare(
            &self,
            request: &MutationPrepareRequest,
        ) -> Result<PreparedMutation, ProviderError> {
            let resource = request.arguments["resource"].as_str().unwrap();
            let desired = request.arguments["value"].as_str().unwrap();
            let baseline = self.current.lock().unwrap()[resource].clone();
            Ok(PreparedMutation {
                capability_id: self.meta.id.clone(),
                provider: self.meta.provider.clone(),
                resource: ResourceKey::new(resource).unwrap(),
                baseline: state(&baseline),
                desired: state(desired),
                driver_data: json!({"resource": resource}),
            })
        }

        fn apply(&self, request: &MutationApplyRequest) -> Result<MutationReceipt, ProviderError> {
            let intent_is_durable = self
                .wal_entries
                .lock()
                .unwrap()
                .last()
                .is_some_and(|entry| {
                    matches!(
                        entry.event,
                        WalEvent::ChangeUpsert { .. } | WalEvent::OperationIntent { .. }
                    )
                });
            *self.saw_durable_intent.lock().unwrap() = intent_is_durable;
            let resource = request.prepared.resource.as_str().to_string();
            let desired = request.prepared.desired.value.as_str().unwrap().to_string();
            self.current.lock().unwrap().insert(resource, desired);
            if matches!(self.apply_mode, ApplyMode::ApplyThenUnknown) {
                return Err(
                    ProviderError::new(ProviderErrorKind::Timeout, "response lost").retryable(true),
                );
            }
            let receipt = receipt(
                request.operation_id.clone(),
                MutationOperationState::Applied,
                request.prepared.desired.clone(),
            );
            self.statuses
                .lock()
                .unwrap()
                .insert(request.operation_id.clone(), status_from(&receipt));
            Ok(receipt)
        }

        fn status(&self, query: &MutationQuery) -> Result<MutationStatus, ProviderError> {
            Ok(self
                .statuses
                .lock()
                .unwrap()
                .get(&query.operation_id)
                .cloned()
                .unwrap_or(MutationStatus {
                    operation_id: query.operation_id.clone(),
                    state: MutationOperationState::Unknown,
                    observed: None,
                    driver_data: Value::Null,
                }))
        }

        fn verify(
            &self,
            request: &MutationVerifyRequest,
        ) -> Result<MutationVerification, ProviderError> {
            let resource = request.prepared.resource.as_str();
            let current = self.current.lock().unwrap()[resource].clone();
            let observed = state(&current);
            Ok(MutationVerification {
                matched: observed == request.expected,
                observed: Some(observed),
                details: Value::Null,
            })
        }

        fn restore(
            &self,
            request: &MutationRestoreRequest,
        ) -> Result<MutationReceipt, ProviderError> {
            let resource = request.prepared.resource.as_str().to_string();
            let baseline = request
                .prepared
                .baseline
                .value
                .as_str()
                .unwrap()
                .to_string();
            self.current.lock().unwrap().insert(resource, baseline);
            let receipt = receipt(
                request.operation_id.clone(),
                MutationOperationState::Restored,
                request.prepared.baseline.clone(),
            );
            self.statuses
                .lock()
                .unwrap()
                .insert(request.operation_id.clone(), status_from(&receipt));
            Ok(receipt)
        }

        fn finalize(
            &self,
            request: &MutationFinalizeRequest,
        ) -> Result<MutationReceipt, ProviderError> {
            let call = self.finalize_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_finalize_at == Some(call) {
                return Err(ProviderError::new(
                    ProviderErrorKind::Internal,
                    "injected finalize acknowledgement failure",
                ));
            }
            let receipt = receipt(
                request.operation_id.clone(),
                MutationOperationState::Finalized,
                request.prepared.desired.clone(),
            );
            self.statuses
                .lock()
                .unwrap()
                .insert(request.operation_id.clone(), status_from(&receipt));
            Ok(receipt)
        }
    }

    struct MemoryWal {
        entries: Arc<Mutex<Vec<WalEntry>>>,
        sealed: bool,
        fail_at_sequence: Option<u64>,
    }

    impl TransactionWal for MemoryWal {
        fn append_durable(&mut self, entry: &WalEntry) -> Result<(), WalError> {
            if self.sealed {
                return Err(WalError::new("sealed"));
            }
            if self.fail_at_sequence == Some(entry.sequence) {
                self.fail_at_sequence = None;
                return Err(WalError::new("injected durable append failure"));
            }
            self.entries.lock().unwrap().push(entry.clone());
            Ok(())
        }

        fn load(&self) -> Result<Vec<WalEntry>, WalError> {
            Ok(self.entries.lock().unwrap().clone())
        }

        fn seal(&mut self, entry: &WalEntry) -> Result<(), WalError> {
            self.append_durable(entry)?;
            self.sealed = true;
            Ok(())
        }
    }

    #[test]
    fn full_baseline_replay_finalize_lifecycle_is_transactional() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/a").unwrap();
        kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        assert!(kernel.change(&change_id).unwrap().experiment_verified);
        assert_eq!(fixture.current("resource/a"), "new");
        assert!(*fixture.saw_durable_intent.lock().unwrap());

        kernel.restore_baseline().unwrap();
        assert_eq!(fixture.current("resource/a"), "old");
        let candidate = Candidate::new(vec![change_id.clone()]).unwrap();
        kernel.replay_candidate(&candidate).unwrap();
        assert_eq!(fixture.current("resource/a"), "new");
        let authorization = authorization_for(&candidate);
        kernel
            .finalize_candidate(&candidate, &authorization)
            .unwrap();
        assert_eq!(kernel.seal_state(), Some(TransactionSeal::Committed));
        assert_eq!(kernel.commit_authorization(), Some(&authorization));
        assert_eq!(
            kernel.change(&change_id).unwrap().state,
            ChangeState::Finalized
        );
        let entries = fixture.wal_entries.lock().unwrap();
        assert!(matches!(
            &entries.last().unwrap().event,
            WalEvent::CommitSealed {
                authorization: sealed_authorization,
                ..
            } if sealed_authorization == &authorization
        ));
        assert!(!entries.iter().any(|entry| {
            matches!(
                &entry.event,
                WalEvent::ChangeUpsert { change }
                    if change.state == ChangeState::Finalized
            )
        }));
    }

    #[test]
    fn apply_with_lost_response_becomes_applied_unknown() {
        let mut fixture = fixture(ApplyMode::ApplyThenUnknown);
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/unknown").unwrap();
        let error = kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::AppliedUnknown);
        assert_eq!(
            kernel.change(&change_id).unwrap().state,
            ChangeState::AppliedUnknown
        );
        assert!(!kernel.change(&change_id).unwrap().experiment_verified);
        assert_eq!(fixture.current("resource/a"), "new");
        assert!(*fixture.saw_durable_intent.lock().unwrap());
    }

    #[test]
    fn later_resource_revision_supersedes_the_previous_change() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let first = ChangeId::new("change/one").unwrap();
        let second = ChangeId::new("change/two").unwrap();
        let first_record = kernel
            .experiment(
                first.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        assert_eq!(first_record.supersedes, None);

        let second_record = kernel
            .experiment(
                second.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "other"}),
            )
            .unwrap();

        assert_eq!(second_record.supersedes, Some(first.clone()));
        assert_eq!(second_record.prepared.baseline, state("old"));
        assert_eq!(fixture.current("resource/a"), "other");
        assert_eq!(
            kernel.change(&first).unwrap().state,
            ChangeState::BaselineRestored
        );

        kernel.restore_baseline().unwrap();
        assert_eq!(fixture.current("resource/a"), "old");
        let superseded = Candidate::new(vec![first.clone()]).unwrap();
        let error = kernel.replay_candidate(&superseded).unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::InvalidCandidate);
        assert!(error.message.contains("superseded"));

        let candidate = Candidate::new(vec![second.clone()]).unwrap();
        kernel.replay_candidate(&candidate).unwrap();
        assert_eq!(fixture.current("resource/a"), "other");
        kernel
            .finalize_candidate(&candidate, &authorization_for(&candidate))
            .unwrap();
        assert_eq!(
            kernel.change(&first).unwrap().state,
            ChangeState::RolledBack
        );
        assert_eq!(
            kernel.change(&second).unwrap().state,
            ChangeState::Finalized
        );
    }

    #[test]
    fn recovery_preserves_a_linear_revision_chain_and_rolls_back_to_original_baseline() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let first = ChangeId::new("change/revision-1").unwrap();
        let second = ChangeId::new("change/revision-2").unwrap();
        kernel
            .experiment(
                first.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        kernel
            .experiment(
                second.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "other"}),
            )
            .unwrap();
        let entries = fixture.wal_entries.lock().unwrap().clone();
        drop(kernel);

        let mut recovered = TransactionKernel::recover(
            TransactionId::new("transaction/test").unwrap(),
            intent_pin(),
            fixture.capability_snapshot.clone(),
            Box::new(MemoryWal {
                entries: Arc::new(Mutex::new(entries.clone())),
                sealed: false,
                fail_at_sequence: None,
            }),
        )
        .unwrap();
        assert_eq!(recovered.change(&second).unwrap().supersedes, Some(first));
        recovered.rollback_all().unwrap();
        assert_eq!(fixture.current("resource/a"), "old");

        let mut tampered = entries;
        let revision = tampered
            .iter_mut()
            .find_map(|entry| match &mut entry.event {
                WalEvent::ChangeUpsert { change }
                    if change.change_id == second && change.state == ChangeState::IntentDurable =>
                {
                    Some(change)
                }
                _ => None,
            })
            .expect("revision apply intent must exist");
        revision.supersedes = None;
        let error = TransactionKernel::recover(
            TransactionId::new("transaction/test").unwrap(),
            intent_pin(),
            fixture.capability_snapshot,
            Box::new(MemoryWal {
                entries: Arc::new(Mutex::new(tampered)),
                sealed: false,
                fail_at_sequence: None,
            }),
        )
        .err()
        .expect("a duplicate resource without a revision link must be rejected");
        assert_eq!(error.kind, TransactionErrorKind::CorruptWal);
    }

    #[test]
    fn resource_revision_refuses_to_overwrite_external_drift() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let first = ChangeId::new("change/drifted-revision").unwrap();
        kernel
            .experiment(
                first.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        fixture
            .current
            .lock()
            .unwrap()
            .insert("resource/a".into(), "administrator-value".into());

        let error = kernel
            .experiment(
                ChangeId::new("change/must-not-apply").unwrap(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "other"}),
            )
            .unwrap_err();

        assert_eq!(error.kind, TransactionErrorKind::ExternalDrift);
        assert_eq!(fixture.current("resource/a"), "administrator-value");
        assert_eq!(
            kernel.change(&first).unwrap().state,
            ChangeState::DriftDetected
        );
        assert_eq!(kernel.changes().count(), 1);
    }

    #[test]
    fn no_op_mutation_is_rejected_before_wal_or_apply() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();

        let error = kernel
            .experiment(
                ChangeId::new("change/no-op").unwrap(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "old"}),
            )
            .unwrap_err();

        assert_eq!(error.kind, TransactionErrorKind::InvalidState);
        assert!(error.message.contains("identical"));
        assert_eq!(kernel.changes().count(), 0);
        assert_eq!(fixture.wal_entries.lock().unwrap().len(), 1);
        assert_eq!(fixture.current("resource/a"), "old");
    }

    #[test]
    fn transaction_rejects_changes_above_the_runtime_limit() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        for index in 0..MAX_CHANGES_PER_TRANSACTION {
            let resource = format!("resource/limit-{index}");
            fixture
                .current
                .lock()
                .unwrap()
                .insert(resource.clone(), "old".to_string());
            kernel
                .experiment(
                    ChangeId::new(format!("change/limit-{index}")).unwrap(),
                    fixture.capability_id.clone(),
                    json!({"resource": resource, "value": "new"}),
                )
                .unwrap();
        }

        let error = kernel
            .experiment(
                ChangeId::new("change/over-limit").unwrap(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/over-limit", "value": "new"}),
            )
            .unwrap_err();

        assert_eq!(error.kind, TransactionErrorKind::InvalidState);
        assert_eq!(kernel.changes().count(), MAX_CHANGES_PER_TRANSACTION);
        kernel.rollback_all().unwrap();
    }

    #[test]
    fn external_drift_blocks_restore_without_overwriting_the_resource() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/drift").unwrap();
        kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        fixture
            .current
            .lock()
            .unwrap()
            .insert("resource/a".into(), "administrator-value".into());

        let error = kernel.restore_baseline().unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::ExternalDrift);
        assert_eq!(fixture.current("resource/a"), "administrator-value");
        assert_eq!(
            kernel.change(&change_id).unwrap().state,
            ChangeState::DriftDetected
        );
    }

    #[test]
    fn wal_failure_after_apply_keeps_live_state_applied_unknown() {
        let mut fixture = fixture_with_wal_failure(ApplyMode::Normal, Some(2));
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/wal-failure").unwrap();
        let error = kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap_err();

        assert_eq!(error.kind, TransactionErrorKind::Wal);
        assert_eq!(fixture.current("resource/a"), "new");
        assert_eq!(
            kernel.change(&change_id).unwrap().state,
            ChangeState::AppliedUnknown
        );
        let entries = fixture.wal_entries.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            entries.last().unwrap().event,
            WalEvent::ChangeUpsert { .. }
        ));
    }

    #[test]
    fn unverified_change_cannot_be_replayed_as_a_candidate() {
        let mut fixture = fixture(ApplyMode::ApplyThenUnknown);
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/unverified").unwrap();
        kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap_err();
        kernel.restore_baseline().unwrap();
        assert!(!kernel.change(&change_id).unwrap().experiment_verified);

        let candidate = Candidate::new(vec![change_id]).unwrap();
        let error = kernel.replay_candidate(&candidate).unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::InvalidCandidate);
    }

    #[test]
    fn finalize_ack_failure_keeps_every_change_rollbackable() {
        let mut fixture = fixture_with_finalize_failure(2);
        let mut kernel = fixture.kernel.take().unwrap();
        let first = ChangeId::new("change/finalize-a").unwrap();
        let second = ChangeId::new("change/finalize-b").unwrap();
        kernel
            .experiment(
                first.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new-a"}),
            )
            .unwrap();
        kernel
            .experiment(
                second.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/b", "value": "new-b"}),
            )
            .unwrap();
        kernel.restore_baseline().unwrap();
        let candidate = Candidate::new(vec![first.clone(), second.clone()]).unwrap();
        kernel.replay_candidate(&candidate).unwrap();

        let error = kernel
            .finalize_candidate(&candidate, &authorization_for(&candidate))
            .unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::Provider);
        assert_eq!(
            kernel.change(&first).unwrap().state,
            ChangeState::CandidateApplied
        );
        assert_eq!(
            kernel.change(&second).unwrap().state,
            ChangeState::CandidateApplied
        );
        assert!(!fixture
            .wal_entries
            .lock()
            .unwrap()
            .iter()
            .any(|entry| matches!(&entry.event, WalEvent::CommitSealed { .. })));

        kernel.rollback_all().unwrap();
        assert_eq!(fixture.current("resource/a"), "old");
        assert_eq!(fixture.current("resource/b"), "old-b");
        assert_eq!(kernel.seal_state(), Some(TransactionSeal::RolledBack));
    }

    #[test]
    fn finalize_rejects_authorization_for_another_candidate() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/authorization-mismatch").unwrap();
        kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        kernel.restore_baseline().unwrap();
        let candidate = Candidate::new(vec![change_id]).unwrap();
        kernel.replay_candidate(&candidate).unwrap();
        let authorization = CommitAuthorization::issue(
            intent_pin(),
            Digest::new("different-candidate-digest").unwrap(),
            Digest::new("decision-digest").unwrap(),
            Digest::new("evaluation-evidence-digest").unwrap(),
        )
        .unwrap();

        let error = kernel
            .finalize_candidate(&candidate, &authorization)
            .unwrap_err();

        assert_eq!(error.kind, TransactionErrorKind::InvalidCandidate);
        assert_eq!(kernel.seal_state(), None);
        assert_eq!(fixture.current("resource/a"), "new");
        kernel.rollback_all().unwrap();
    }

    #[test]
    fn finalize_rejects_authorization_for_another_evaluation_intent() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/intent-mismatch").unwrap();
        kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        kernel.restore_baseline().unwrap();
        let candidate = Candidate::new(vec![change_id]).unwrap();
        kernel.replay_candidate(&candidate).unwrap();
        let authorization = CommitAuthorization::issue(
            different_intent_pin(),
            candidate.digest().clone(),
            Digest::new("decision-digest").unwrap(),
            Digest::new("evaluation-evidence-digest").unwrap(),
        )
        .unwrap();

        let error = kernel
            .finalize_candidate(&candidate, &authorization)
            .unwrap_err();

        assert_eq!(error.kind, TransactionErrorKind::PinMismatch);
        assert_eq!(kernel.seal_state(), None);
        assert_eq!(fixture.current("resource/a"), "new");
        kernel.rollback_all().unwrap();
    }

    #[test]
    fn recovery_rejects_experiment_verification_downgrade() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/downgrade").unwrap();
        kernel
            .experiment(
                change_id,
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        kernel.restore_baseline().unwrap();
        let mut entries = fixture.wal_entries.lock().unwrap().clone();
        let WalEvent::ChangeUpsert { change } = &mut entries.last_mut().unwrap().event else {
            panic!("expected restored change record");
        };
        assert!(change.experiment_verified);
        change.experiment_verified = false;

        let error = TransactionKernel::recover(
            TransactionId::new("transaction/test").unwrap(),
            intent_pin(),
            fixture.capability_snapshot,
            Box::new(MemoryWal {
                entries: Arc::new(Mutex::new(entries)),
                sealed: false,
                fail_at_sequence: None,
            }),
        )
        .err()
        .expect("verification downgrade must reject recovery");
        assert_eq!(error.kind, TransactionErrorKind::CorruptWal);
    }

    #[test]
    fn recovery_rejects_a_different_evaluation_intent_pin() {
        let fixture = fixture(ApplyMode::Normal);
        let entries = fixture.wal_entries.lock().unwrap().clone();

        let error = TransactionKernel::recover(
            TransactionId::new("transaction/test").unwrap(),
            different_intent_pin(),
            fixture.capability_snapshot,
            Box::new(MemoryWal {
                entries: Arc::new(Mutex::new(entries)),
                sealed: false,
                fail_at_sequence: None,
            }),
        )
        .err()
        .expect("a transaction cannot be recovered under another evaluation intent");

        assert_eq!(error.kind, TransactionErrorKind::CorruptWal);
        assert!(error.message.contains("evaluation intent"));
    }

    #[test]
    fn commit_seal_failure_is_reported_as_an_unknown_commit_outcome() {
        // One change produces records 0..=8 before the central commit seal.
        let mut fixture = fixture_with_wal_failure(ApplyMode::Normal, Some(9));
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/seal-failure").unwrap();
        kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        kernel.restore_baseline().unwrap();
        let candidate = Candidate::new(vec![change_id.clone()]).unwrap();
        kernel.replay_candidate(&candidate).unwrap();

        let error = kernel
            .finalize_candidate(&candidate, &authorization_for(&candidate))
            .unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::CommitOutcomeUnknown);
        assert_eq!(kernel.seal_state(), None);
        assert_eq!(
            kernel.change(&change_id).unwrap().state,
            ChangeState::CandidateApplied
        );
        assert!(!fixture
            .wal_entries
            .lock()
            .unwrap()
            .iter()
            .any(|entry| matches!(&entry.event, WalEvent::CommitSealed { .. })));

        assert_eq!(fixture.current("resource/a"), "new");
    }

    #[test]
    fn recovery_applies_atomic_commit_seal_batch() {
        let mut fixture = fixture(ApplyMode::Normal);
        let mut kernel = fixture.kernel.take().unwrap();
        let change_id = ChangeId::new("change/recover-commit").unwrap();
        kernel
            .experiment(
                change_id.clone(),
                fixture.capability_id.clone(),
                json!({"resource": "resource/a", "value": "new"}),
            )
            .unwrap();
        kernel.restore_baseline().unwrap();
        let candidate = Candidate::new(vec![change_id.clone()]).unwrap();
        kernel.replay_candidate(&candidate).unwrap();
        kernel
            .finalize_candidate(&candidate, &authorization_for(&candidate))
            .unwrap();
        let entries = fixture.wal_entries.lock().unwrap().clone();
        drop(kernel);

        let mut tampered_entries = entries.clone();
        let WalEvent::CommitSealed { authorization, .. } =
            &mut tampered_entries.last_mut().unwrap().event
        else {
            panic!("expected commit seal");
        };
        *authorization = CommitAuthorization::issue(
            intent_pin(),
            Digest::new("different-candidate-digest").unwrap(),
            Digest::new("decision-digest").unwrap(),
            Digest::new("evaluation-evidence-digest").unwrap(),
        )
        .unwrap();
        let tampered = TransactionKernel::recover(
            TransactionId::new("transaction/test").unwrap(),
            intent_pin(),
            fixture.capability_snapshot.clone(),
            Box::new(MemoryWal {
                entries: Arc::new(Mutex::new(tampered_entries)),
                sealed: true,
                fail_at_sequence: None,
            }),
        )
        .err()
        .expect("tampered commit authorization must reject recovery");
        assert_eq!(tampered.kind, TransactionErrorKind::CorruptWal);

        let recovered = TransactionKernel::recover(
            TransactionId::new("transaction/test").unwrap(),
            intent_pin(),
            fixture.capability_snapshot,
            Box::new(MemoryWal {
                entries: Arc::new(Mutex::new(entries)),
                sealed: true,
                fail_at_sequence: None,
            }),
        )
        .unwrap();
        assert_eq!(recovered.seal_state(), Some(TransactionSeal::Committed));
        let change = recovered.change(&change_id).unwrap();
        assert_eq!(change.state, ChangeState::Finalized);
        assert!(change.experiment_verified);
    }

    struct Fixture {
        kernel: Option<TransactionKernel>,
        capability_id: CapabilityId,
        capability_snapshot: CapabilitySnapshot,
        current: Arc<Mutex<BTreeMap<String, String>>>,
        saw_durable_intent: Arc<Mutex<bool>>,
        wal_entries: Arc<Mutex<Vec<WalEntry>>>,
    }

    fn authorization_for(candidate: &Candidate) -> CommitAuthorization {
        CommitAuthorization::issue(
            intent_pin(),
            candidate.digest().clone(),
            Digest::new("decision-digest").unwrap(),
            Digest::new("evaluation-evidence-digest").unwrap(),
        )
        .unwrap()
    }

    fn intent_pin() -> EvaluationIntentPin {
        EvaluationIntentPin::new(
            EpisodeId::new(1),
            Digest::new("intent-digest").unwrap(),
            Digest::new("contract-digest").unwrap(),
        )
    }

    fn different_intent_pin() -> EvaluationIntentPin {
        EvaluationIntentPin::new(
            EpisodeId::new(2),
            Digest::new("other-intent-digest").unwrap(),
            Digest::new("contract-digest").unwrap(),
        )
    }

    impl Fixture {
        fn current(&self, resource: &str) -> String {
            self.current.lock().unwrap()[resource].clone()
        }
    }

    fn fixture(mode: ApplyMode) -> Fixture {
        fixture_with_failures(mode, None, None)
    }

    fn fixture_with_wal_failure(mode: ApplyMode, fail_at_sequence: Option<u64>) -> Fixture {
        fixture_with_failures(mode, fail_at_sequence, None)
    }

    fn fixture_with_finalize_failure(fail_finalize_at: usize) -> Fixture {
        fixture_with_failures(ApplyMode::Normal, None, Some(fail_finalize_at))
    }

    fn fixture_with_failures(
        mode: ApplyMode,
        fail_at_sequence: Option<u64>,
        fail_finalize_at: Option<usize>,
    ) -> Fixture {
        let capability_id = CapabilityId::new("test/mutation").unwrap();
        let current = Arc::new(Mutex::new(BTreeMap::from([
            ("resource/a".to_string(), "old".to_string()),
            ("resource/b".to_string(), "old-b".to_string()),
        ])));
        let entries = Arc::new(Mutex::new(Vec::new()));
        let saw_durable_intent = Arc::new(Mutex::new(false));
        let driver = Arc::new(FakeDriver {
            meta: mutation_meta(capability_id.clone()),
            current: current.clone(),
            statuses: Mutex::new(BTreeMap::new()),
            apply_mode: mode,
            finalize_calls: AtomicUsize::new(0),
            fail_finalize_at,
            wal_entries: entries.clone(),
            saw_durable_intent: saw_durable_intent.clone(),
        });
        let mut registry = CapabilityRegistry::new(AdminPolicy::default());
        registry.register_mutation(driver).unwrap();
        let capability_snapshot = registry.snapshot();
        let kernel = TransactionKernel::begin(
            TransactionId::new("transaction/test").unwrap(),
            intent_pin(),
            capability_snapshot.clone(),
            Box::new(MemoryWal {
                entries: entries.clone(),
                sealed: false,
                fail_at_sequence,
            }),
        )
        .unwrap();
        Fixture {
            kernel: Some(kernel),
            capability_id,
            capability_snapshot,
            current,
            saw_durable_intent,
            wal_entries: entries,
        }
    }

    fn mutation_meta(id: CapabilityId) -> CapabilityMeta {
        let mut meta = CapabilityMeta::new(
            id,
            CapabilityKind::Mutation,
            EffectClass::ReversibleMutation,
            ProviderPin {
                provider_id: ProviderId::new("fake-driver").unwrap(),
                provider_version: ProviderVersion::new("1").unwrap(),
                provider_class: ProviderClass::Local,
                manifest_digest: Digest::new("fake-manifest").unwrap(),
            },
            "fake mutation",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
        .with_allowed_phases([EpisodePhase::Clean, EpisodePhase::Experimenting]);
        meta.idempotent = true;
        meta
    }

    fn state(value: &str) -> MutationState {
        let value = Value::String(value.into());
        MutationState {
            digest: content_digest(&value).unwrap(),
            value,
        }
    }

    fn receipt(
        operation_id: OperationId,
        operation_state: MutationOperationState,
        observed: MutationState,
    ) -> MutationReceipt {
        MutationReceipt {
            operation_id,
            state: operation_state,
            observed: Some(observed),
            driver_data: Value::Null,
        }
    }

    fn status_from(receipt: &MutationReceipt) -> MutationStatus {
        MutationStatus {
            operation_id: receipt.operation_id.clone(),
            state: receipt.state,
            observed: receipt.observed.clone(),
            driver_data: receipt.driver_data.clone(),
        }
    }
}
