use serde_json::json;

use crate::audit::{AuditRecord, AuditSink};
use crate::capability::CapabilitySnapshot;
use crate::kernel::transaction::{
    PendingTransaction, TransactionKernel, TransactionSeal, TransactionStore,
};

pub(crate) fn recover_available_before_plugin_bootstrap(
    store: &TransactionStore,
    capabilities: CapabilitySnapshot,
) {
    let Ok(inventory) = store.discover() else {
        return;
    };
    for pending in inventory.pending {
        let all_drivers_available = pending
            .changes
            .iter()
            .all(|change| capabilities.mutation(&change.capability_id).is_some());
        if all_drivers_available {
            // This is an early safety pass. Every failure is retried and audited
            // by the final recovery gate after plugin bootstrap completes.
            let _ = recover_pending(store, &pending, capabilities.clone());
        }
    }
}

pub(crate) fn recover_before_activation(
    store: &TransactionStore,
    capabilities: CapabilitySnapshot,
    audit: &mut dyn AuditSink,
) -> Result<(), String> {
    let inventory = match store.discover() {
        Ok(inventory) => inventory,
        Err(error) => {
            let message = error.to_string();
            let audit_error = audit
                .record(&AuditRecord::runtime(
                    "transaction_recovery_failed",
                    json!({"stage": "discover", "error": message}),
                ))
                .err();
            return Err(match audit_error {
                Some(audit_error) => format!(
                    "transaction recovery discovery failed: {message}; failed to audit discovery error: {audit_error}"
                ),
                None => format!("transaction recovery discovery failed: {message}"),
            });
        }
    };

    let mut failed_items = 0usize;
    let mut audit_failures = Vec::new();
    let mut failure_details = Vec::new();

    for unstarted in inventory.unstarted {
        match store.discard_unstarted(&unstarted) {
            Ok(()) => record_or_collect(
                audit,
                AuditRecord::runtime(
                    "unstarted_transaction_wal_discarded",
                    json!({
                        "transaction_id": unstarted.transaction_id,
                        "path": unstarted.path,
                        "reason": "the WAL had no durable Started record, so the transaction protocol could not have performed a mutation",
                    }),
                ),
                &mut audit_failures,
            ),
            Err(error) => {
                failed_items += 1;
                failure_details.push(format!(
                    "failed to discard unstarted WAL '{}': {error}",
                    unstarted.path.display()
                ));
                record_or_collect(
                    audit,
                    AuditRecord::runtime(
                        "transaction_recovery_failed",
                        json!({
                            "transaction_id": unstarted.transaction_id,
                            "path": unstarted.path,
                            "stage": "discard_unstarted_wal",
                            "error": error.to_string(),
                        }),
                    ),
                    &mut audit_failures,
                );
            }
        }
    }

    for sealed in inventory.sealed {
        match (sealed.outcome, sealed.authorization.as_ref()) {
            (TransactionSeal::Committed, Some(authorization))
                if authorization.intent_pin() == &sealed.intent_pin =>
            {
                record_or_collect(
                    audit,
                    AuditRecord::runtime(
                        "sealed_commit_reconciled",
                        json!({
                            "transaction_id": sealed.transaction_id,
                            "intent_pin": sealed.intent_pin,
                            "episode_id": sealed.intent_pin.episode_id(),
                            "intent_digest": sealed.intent_pin.intent_digest(),
                            "path": sealed.path,
                            "authorization_digest": authorization.authorization_digest(),
                            "contract_digest": authorization.contract_digest(),
                            "candidate_digest": authorization.candidate_digest(),
                            "decision_digest": authorization.decision_digest(),
                            "evaluation_evidence_digest": authorization.evaluation_evidence_digest(),
                        }),
                    ),
                    &mut audit_failures,
                )
            }
            (TransactionSeal::RolledBack, None) => record_or_collect(
                audit,
                AuditRecord::runtime(
                    "sealed_rollback_reconciled",
                    json!({
                        "transaction_id": sealed.transaction_id,
                        "intent_pin": sealed.intent_pin,
                        "episode_id": sealed.intent_pin.episode_id(),
                        "intent_digest": sealed.intent_pin.intent_digest(),
                        "contract_digest": sealed.intent_pin.contract_digest(),
                        "path": sealed.path,
                    }),
                ),
                &mut audit_failures,
            ),
            _ => {
                failed_items += 1;
                failure_details.push(format!(
                    "sealed transaction '{}' has inconsistent authorization metadata",
                    sealed.transaction_id
                ));
                record_or_collect(
                    audit,
                    AuditRecord::runtime(
                        "transaction_recovery_failed",
                        json!({
                            "transaction_id": sealed.transaction_id,
                            "intent_pin": sealed.intent_pin,
                            "episode_id": sealed.intent_pin.episode_id(),
                            "path": sealed.path,
                            "stage": "reconcile_seal",
                            "error": "sealed transaction has inconsistent authorization metadata",
                        }),
                    ),
                    &mut audit_failures,
                );
            }
        }
    }

    for corrupt in inventory.corrupt {
        failed_items += 1;
        failure_details.push(format!(
            "corrupt WAL '{}': {}",
            corrupt.path.display(),
            corrupt.error
        ));
        record_or_collect(
            audit,
            AuditRecord::runtime(
                "transaction_recovery_failed",
                json!({
                    "stage": "inspect_wal",
                    "path": corrupt.path,
                    "error": corrupt.error,
                }),
            ),
            &mut audit_failures,
        );
    }

    for pending in inventory.pending {
        match recover_pending(store, &pending, capabilities.clone()) {
            Ok(restored_change_count) => record_or_collect(
                audit,
                AuditRecord::runtime(
                    "transaction_recovered",
                    json!({
                        "transaction_id": pending.transaction_id,
                        "intent_pin": pending.intent_pin,
                        "episode_id": pending.intent_pin.episode_id(),
                        "intent_digest": pending.intent_pin.intent_digest(),
                        "contract_digest": pending.intent_pin.contract_digest(),
                        "restored_change_count": restored_change_count,
                        "had_applied_unknown": pending.has_applied_unknown,
                        "recorded_capability_generation": pending.capability_generation,
                        "current_capability_generation": capabilities.generation(),
                        "generation_changed": pending.capability_generation != capabilities.generation(),
                    }),
                ),
                &mut audit_failures,
            ),
            Err(error) => {
                failed_items += 1;
                failure_details.push(format!(
                    "transaction '{}' failed during {}: {}",
                    pending.transaction_id, error.stage, error.message
                ));
                record_or_collect(
                    audit,
                    AuditRecord::runtime(
                        "transaction_recovery_failed",
                        json!({
                            "transaction_id": pending.transaction_id,
                            "intent_pin": pending.intent_pin,
                            "episode_id": pending.intent_pin.episode_id(),
                            "intent_digest": pending.intent_pin.intent_digest(),
                            "contract_digest": pending.intent_pin.contract_digest(),
                            "path": pending.path,
                            "stage": error.stage,
                            "error": error.message,
                            "had_applied_unknown": pending.has_applied_unknown,
                            "recorded_capability_generation": pending.capability_generation,
                            "current_capability_generation": capabilities.generation(),
                        }),
                    ),
                    &mut audit_failures,
                );
            }
        }
    }

    if failed_items != 0 || !audit_failures.is_empty() {
        let audit_failure_count_before_summary = audit_failures.len();
        record_or_collect(
            audit,
            AuditRecord::runtime(
                "recovery_gate_blocked",
                json!({
                    "failed_item_count": failed_items,
                    "audit_failure_count": audit_failure_count_before_summary,
                    "failures": failure_details,
                }),
            ),
            &mut audit_failures,
        );
        return Err(format!(
            "transaction recovery gate blocked activation: {failed_items} WAL item(s) failed and {} audit record(s) failed",
            audit_failures.len()
        ));
    }
    Ok(())
}

struct RecoveryAttemptError {
    stage: &'static str,
    message: String,
}

fn recover_pending(
    store: &TransactionStore,
    pending: &PendingTransaction,
    capabilities: CapabilitySnapshot,
) -> Result<usize, RecoveryAttemptError> {
    let wal = store
        .open_existing(pending)
        .map_err(|error| RecoveryAttemptError {
            stage: "open_wal",
            message: error.to_string(),
        })?;
    let mut transaction = TransactionKernel::recover(
        pending.transaction_id.clone(),
        pending.intent_pin.clone(),
        capabilities,
        Box::new(wal),
    )
    .map_err(|error| RecoveryAttemptError {
        stage: "rebuild_transaction",
        message: error.to_string(),
    })?;
    transaction
        .rollback_all()
        .map(|restored| restored.len())
        .map_err(|error| RecoveryAttemptError {
            stage: "rollback",
            message: error.to_string(),
        })
}

fn record_or_collect(audit: &mut dyn AuditSink, record: AuditRecord, failures: &mut Vec<String>) {
    let event = record.event.clone();
    if let Err(error) = audit.record(&record) {
        failures.push(format!("failed to record '{event}': {error}"));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::Value;

    use super::*;
    use crate::audit::AuditRecord;
    use crate::capability::{AdminPolicy, CapabilityRegistry};
    use crate::domain::{
        content_digest, Candidate, CapabilityId, ChangeId, CommitAuthorization, Digest, EpisodeId,
        EvaluationIntentPin, MutationState, OperationId, PreparedMutation, ProviderClass,
        ProviderId, ProviderPin, ProviderVersion, ResourceKey, TransactionId,
    };
    use crate::kernel::transaction::{
        ChangeRecord, ChangeState, OperationIntentKind, TransactionSeal, TransactionWal, WalEntry,
        WalEvent,
    };

    #[derive(Default)]
    struct MemoryAudit(Vec<AuditRecord>);

    impl AuditSink for MemoryAudit {
        fn record(&mut self, record: &AuditRecord) -> std::io::Result<()> {
            self.0.push(record.clone());
            Ok(())
        }
    }

    struct FailingAudit;

    impl AuditSink for FailingAudit {
        fn record(&mut self, _record: &AuditRecord) -> std::io::Result<()> {
            Err(std::io::Error::other("audit unavailable"))
        }
    }

    #[test]
    fn corrupt_wal_blocks_activation_recovery_gate() {
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-recovery-gate-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = TransactionStore::new(&root).unwrap();
        fs::write(root.join("tx-deadbeef.jsonl"), "not-json\n").unwrap();
        let snapshot = CapabilityRegistry::new(AdminPolicy::default()).snapshot();
        let mut audit = MemoryAudit::default();

        let error = recover_before_activation(&store, snapshot, &mut audit).unwrap_err();

        assert!(error.contains("recovery gate blocked"));
        assert_eq!(audit.0[0].event, "transaction_recovery_failed");
        assert_eq!(audit.0[0].data["stage"], "inspect_wal");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn early_recovery_pass_seals_transactions_before_plugin_bootstrap() {
        let root = temp_root("early-pass");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("local-first").unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id.clone(), EpisodeId::new(10));
        drop(wal);
        let snapshot = CapabilityRegistry::new(AdminPolicy::default()).snapshot();

        recover_available_before_plugin_bootstrap(&store, snapshot);

        let inventory = store.discover().unwrap();
        assert!(inventory.pending.is_empty());
        assert_eq!(inventory.sealed.len(), 1);
        assert_eq!(inventory.sealed[0].transaction_id, transaction_id);
        assert_eq!(inventory.sealed[0].outcome, TransactionSeal::RolledBack);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_failure_blocks_activation_after_pending_recovery_is_attempted() {
        let root = temp_root("audit-failure");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("pending").unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id.clone(), EpisodeId::new(11));
        drop(wal);

        let snapshot = CapabilityRegistry::new(AdminPolicy::default()).snapshot();
        let error = recover_before_activation(&store, snapshot, &mut FailingAudit).unwrap_err();

        assert!(error.contains("audit record(s) failed"));
        let inventory = store.discover().unwrap();
        assert!(inventory.pending.is_empty());
        assert_eq!(inventory.sealed.len(), 1);
        assert_eq!(inventory.sealed[0].transaction_id, transaction_id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_wal_does_not_prevent_independent_pending_rollback() {
        let root = temp_root("mixed-corrupt-pending");
        let store = TransactionStore::new(&root).unwrap();
        let pending_id = TransactionId::new("pending").unwrap();
        let corrupt_id = TransactionId::new("corrupt").unwrap();
        let mut pending = store.create(&pending_id).unwrap();
        append_start(&mut pending, pending_id.clone(), EpisodeId::new(12));
        drop(pending);
        let corrupt_path = store.wal_path(&corrupt_id).unwrap();
        drop(store.create(&corrupt_id).unwrap());
        fs::write(corrupt_path, "not-json\n").unwrap();
        let snapshot = CapabilityRegistry::new(AdminPolicy::default()).snapshot();
        let mut audit = MemoryAudit::default();

        let error = recover_before_activation(&store, snapshot, &mut audit).unwrap_err();

        assert!(error.contains("1 WAL item(s) failed"));
        let inventory = store.discover().unwrap();
        assert!(inventory.pending.is_empty());
        assert_eq!(inventory.sealed.len(), 1);
        assert_eq!(inventory.sealed[0].transaction_id, pending_id);
        assert_eq!(inventory.sealed[0].outcome, TransactionSeal::RolledBack);
        assert!(audit
            .0
            .iter()
            .any(|record| record.event == "transaction_recovered"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_pending_recovery_does_not_stop_later_transactions() {
        let root = temp_root("multiple-pending");
        let store = TransactionStore::new(&root).unwrap();
        let bad_id = TransactionId::new("a-bad").unwrap();
        let good_id = TransactionId::new("z-good").unwrap();

        let mut bad = store.create(&bad_id).unwrap();
        append_start(&mut bad, bad_id.clone(), EpisodeId::new(21));
        bad.append_durable(&WalEntry {
            sequence: 1,
            transaction_id: bad_id.clone(),
            event: WalEvent::ChangeUpsert {
                change: Box::new(unavailable_change(bad_id.clone())),
            },
        })
        .unwrap();
        let mut good = store.create(&good_id).unwrap();
        append_start(&mut good, good_id.clone(), EpisodeId::new(22));

        let snapshot = CapabilityRegistry::new(AdminPolicy::default()).snapshot();
        let mut audit = MemoryAudit::default();
        let error = recover_before_activation(&store, snapshot, &mut audit).unwrap_err();

        assert!(error.contains("1 WAL item(s) failed"));
        let inventory = store.discover().unwrap();
        assert_eq!(inventory.pending.len(), 1);
        assert_eq!(inventory.pending[0].transaction_id, bad_id);
        assert_eq!(inventory.sealed.len(), 1);
        assert_eq!(inventory.sealed[0].transaction_id, good_id);
        assert!(audit.0.iter().any(|record| {
            record.event == "transaction_recovery_failed"
                && record.data["transaction_id"] == "a-bad"
                && record.data["stage"] == "rebuild_transaction"
        }));
        assert!(audit.0.iter().any(|record| {
            record.event == "transaction_recovered" && record.data["transaction_id"] == "z-good"
        }));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_wal_is_discarded_and_does_not_block_activation() {
        let root = temp_root("unstarted");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("empty").unwrap();
        drop(store.create(&transaction_id).unwrap());
        let snapshot = CapabilityRegistry::new(AdminPolicy::default()).snapshot();
        let mut audit = MemoryAudit::default();

        recover_before_activation(&store, snapshot, &mut audit).unwrap();

        assert_eq!(audit.0.len(), 1);
        assert_eq!(audit.0[0].event, "unstarted_transaction_wal_discarded");
        assert_eq!(audit.0[0].data["transaction_id"], "empty");
        assert!(!store.wal_path(&transaction_id).unwrap().exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sealed_commit_authorization_is_reconciled_from_the_wal() {
        let root = temp_root("commit-reconciliation");
        let store = TransactionStore::new(&root).unwrap();
        let transaction_id = TransactionId::new("committed").unwrap();
        let mut wal = store.create(&transaction_id).unwrap();
        append_start(&mut wal, transaction_id.clone(), EpisodeId::new(31));

        let mut change = unavailable_change(transaction_id.clone());
        append_change(&mut wal, 1, &transaction_id, change.clone());
        change.experiment_verified = true;
        change.state = ChangeState::AppliedVerified;
        append_change(&mut wal, 2, &transaction_id, change.clone());
        change.state = ChangeState::BaselineRestored;
        change.last_operation_id = OperationId::new("operation/restore").unwrap();
        append_change(&mut wal, 3, &transaction_id, change.clone());

        let replay_id = OperationId::new("operation/replay").unwrap();
        append_intent(
            &mut wal,
            4,
            &transaction_id,
            &change.change_id,
            replay_id.clone(),
            OperationIntentKind::Apply,
        );
        change.state = ChangeState::CandidateApplied;
        change.last_operation_id = replay_id;
        append_change(&mut wal, 5, &transaction_id, change.clone());

        let finalize_id = OperationId::new("operation/finalize").unwrap();
        append_intent(
            &mut wal,
            6,
            &transaction_id,
            &change.change_id,
            finalize_id.clone(),
            OperationIntentKind::Finalize,
        );
        change.last_operation_id = finalize_id;
        append_change(&mut wal, 7, &transaction_id, change.clone());

        let candidate = Candidate::new(vec![change.change_id.clone()]).unwrap();
        let authorization = CommitAuthorization::issue(
            intent_pin(EpisodeId::new(31)),
            candidate.digest().clone(),
            content_digest(&"decision").unwrap(),
            content_digest(&"evidence").unwrap(),
        )
        .unwrap();
        let mut terminal = change;
        terminal.state = ChangeState::Finalized;
        wal.seal(&WalEntry {
            sequence: 8,
            transaction_id: transaction_id.clone(),
            event: WalEvent::CommitSealed {
                authorization: authorization.clone(),
                changes: vec![terminal],
            },
        })
        .unwrap();

        let snapshot = CapabilityRegistry::new(AdminPolicy::default()).snapshot();
        let mut audit = MemoryAudit::default();
        recover_before_activation(&store, snapshot, &mut audit).unwrap();

        let record = audit
            .0
            .iter()
            .find(|record| record.event == "sealed_commit_reconciled")
            .unwrap();
        assert_eq!(record.data["transaction_id"], "committed");
        assert_eq!(record.data["episode_id"], 31);
        assert_eq!(
            record.data["intent_digest"],
            authorization.intent_pin().intent_digest().as_str()
        );
        assert_eq!(
            record.data["authorization_digest"],
            authorization.authorization_digest().as_str()
        );
        assert_eq!(
            record.data["evaluation_evidence_digest"],
            authorization.evaluation_evidence_digest().as_str()
        );
        let _ = fs::remove_dir_all(root);
    }

    fn append_start(
        wal: &mut crate::kernel::transaction::FileWal,
        transaction_id: TransactionId,
        episode_id: EpisodeId,
    ) {
        wal.append_durable(&WalEntry {
            sequence: 0,
            transaction_id,
            event: WalEvent::Started {
                intent_pin: intent_pin(episode_id),
                capability_generation: 0,
            },
        })
        .unwrap();
    }

    fn intent_pin(episode_id: EpisodeId) -> EvaluationIntentPin {
        EvaluationIntentPin::new(
            episode_id,
            content_digest(&(episode_id, "intent")).unwrap(),
            content_digest(&(episode_id, "contract")).unwrap(),
        )
    }

    fn unavailable_change(transaction_id: TransactionId) -> ChangeRecord {
        let capability_id = CapabilityId::new("missing/mutation").unwrap();
        let resource = ResourceKey::new("test/resource").unwrap();
        ChangeRecord {
            transaction_id,
            change_id: ChangeId::new("change/missing").unwrap(),
            capability_id: capability_id.clone(),
            resource: resource.clone(),
            prepared: PreparedMutation {
                capability_id,
                provider: ProviderPin {
                    provider_id: ProviderId::new("missing-provider").unwrap(),
                    provider_version: ProviderVersion::new("1").unwrap(),
                    provider_class: ProviderClass::Local,
                    manifest_digest: Digest::new("missing-manifest").unwrap(),
                },
                resource,
                baseline: MutationState {
                    value: Value::String("old".into()),
                    digest: content_digest(&Value::String("old".into())).unwrap(),
                },
                desired: MutationState {
                    value: Value::String("new".into()),
                    digest: content_digest(&Value::String("new".into())).unwrap(),
                },
                driver_data: Value::Null,
            },
            experiment_verified: false,
            state: ChangeState::IntentDurable,
            last_operation_id: OperationId::new("operation/apply").unwrap(),
            last_receipt: None,
            message: None,
        }
    }

    fn append_change(
        wal: &mut crate::kernel::transaction::FileWal,
        sequence: u64,
        transaction_id: &TransactionId,
        change: ChangeRecord,
    ) {
        wal.append_durable(&WalEntry {
            sequence,
            transaction_id: transaction_id.clone(),
            event: WalEvent::ChangeUpsert {
                change: Box::new(change),
            },
        })
        .unwrap();
    }

    fn append_intent(
        wal: &mut crate::kernel::transaction::FileWal,
        sequence: u64,
        transaction_id: &TransactionId,
        change_id: &ChangeId,
        operation_id: OperationId,
        operation: OperationIntentKind,
    ) {
        wal.append_durable(&WalEntry {
            sequence,
            transaction_id: transaction_id.clone(),
            event: WalEvent::OperationIntent {
                change_id: change_id.clone(),
                operation_id,
                operation,
            },
        })
        .unwrap();
    }

    fn temp_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tuning-agent-recovery-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
