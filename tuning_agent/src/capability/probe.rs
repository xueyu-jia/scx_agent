use crate::domain::{CapabilityMeta, ProbeEvidence, ProbeRequest, ProviderError};

pub trait ProbeProvider: Send + Sync {
    fn meta(&self) -> &CapabilityMeta;

    fn probe(&self, request: &ProbeRequest) -> Result<ProbeEvidence, ProviderError>;
}
