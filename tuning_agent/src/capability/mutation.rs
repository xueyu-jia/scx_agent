use crate::domain::{
    CapabilityMeta, MutationApplyRequest, MutationFinalizeRequest, MutationPrepareRequest,
    MutationQuery, MutationReceipt, MutationRestoreRequest, MutationStatus, MutationVerification,
    MutationVerifyRequest, PreparedMutation, ProviderError,
};

pub trait MutationDriver: Send + Sync {
    fn meta(&self) -> &CapabilityMeta;

    fn prepare(&self, request: &MutationPrepareRequest) -> Result<PreparedMutation, ProviderError>;

    fn apply(&self, request: &MutationApplyRequest) -> Result<MutationReceipt, ProviderError>;

    fn status(&self, query: &MutationQuery) -> Result<MutationStatus, ProviderError>;

    fn verify(
        &self,
        request: &MutationVerifyRequest,
    ) -> Result<MutationVerification, ProviderError>;

    fn restore(&self, request: &MutationRestoreRequest) -> Result<MutationReceipt, ProviderError>;

    /// A retry-safe commit acknowledgement. It must not change the tuned resource and
    /// must not discard rollback material before the Transaction Kernel seals the WAL.
    fn finalize(&self, request: &MutationFinalizeRequest)
        -> Result<MutationReceipt, ProviderError>;
}
