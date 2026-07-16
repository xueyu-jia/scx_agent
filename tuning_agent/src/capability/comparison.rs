use serde_json::Value;

use crate::domain::{CapabilityMeta, ComparisonEvidence, ComparisonRequest, ProviderError};

pub trait ComparisonPolicy: Send + Sync {
    fn meta(&self) -> &CapabilityMeta;

    fn validate_specification(&self, specification: &Value) -> Result<(), ProviderError>;

    fn compare(&self, request: &ComparisonRequest) -> Result<ComparisonEvidence, ProviderError>;
}
