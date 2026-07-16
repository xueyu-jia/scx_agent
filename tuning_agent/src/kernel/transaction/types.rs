use serde::{Deserialize, Serialize};

use crate::domain::{
    CapabilityId, ChangeId, MutationReceipt, OperationId, PreparedMutation, ResourceKey,
};

pub use crate::domain::TransactionId;

pub const MAX_CHANGES_PER_TRANSACTION: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    IntentDurable,
    AppliedUnknown,
    AppliedVerified,
    BaselineRestored,
    CandidateApplied,
    Finalized,
    RolledBack,
    FailedNotApplied,
    DriftDetected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationIntentKind {
    Apply,
    Restore,
    Finalize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub transaction_id: TransactionId,
    pub change_id: ChangeId,
    pub capability_id: CapabilityId,
    pub resource: ResourceKey,
    pub prepared: PreparedMutation,
    #[serde(default)]
    pub experiment_verified: bool,
    pub state: ChangeState,
    pub last_operation_id: OperationId,
    pub last_receipt: Option<MutationReceipt>,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionSeal {
    Committed,
    RolledBack,
}
