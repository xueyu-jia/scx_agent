use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{
    content_digest, CapabilityId, Digest, InvocationContext, OperationId, ProviderPin, ResourceKey,
};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MutationState {
    pub value: Value,
    pub digest: Digest,
}

#[derive(Deserialize)]
struct UncheckedMutationState {
    value: Value,
    digest: Digest,
}

impl<'de> Deserialize<'de> for MutationState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let unchecked = UncheckedMutationState::deserialize(deserializer)?;
        let expected = content_digest(&unchecked.value).map_err(serde::de::Error::custom)?;
        if unchecked.digest != expected {
            return Err(serde::de::Error::custom(
                "mutation state digest does not match its value",
            ));
        }
        Ok(Self {
            value: unchecked.value,
            digest: unchecked.digest,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationPrepareRequest {
    pub context: InvocationContext,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreparedMutation {
    pub capability_id: CapabilityId,
    pub provider: ProviderPin,
    pub resource: ResourceKey,
    pub baseline: MutationState,
    pub desired: MutationState,
    pub driver_data: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationApplyRequest {
    pub operation_id: OperationId,
    pub prepared: PreparedMutation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationRestoreRequest {
    pub operation_id: OperationId,
    pub prepared: PreparedMutation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationFinalizeRequest {
    pub operation_id: OperationId,
    pub prepared: PreparedMutation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MutationQuery {
    pub operation_id: OperationId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationVerifyRequest {
    pub operation_id: OperationId,
    pub prepared: PreparedMutation,
    pub expected: MutationState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationOperationState {
    NotApplied,
    Applied,
    Restored,
    Finalized,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationReceipt {
    pub operation_id: OperationId,
    pub state: MutationOperationState,
    pub observed: Option<MutationState>,
    pub driver_data: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationStatus {
    pub operation_id: OperationId,
    pub state: MutationOperationState,
    pub observed: Option<MutationState>,
    pub driver_data: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MutationVerification {
    pub matched: bool,
    pub observed: Option<MutationState>,
    pub details: Value,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn mutation_state_deserialization_rejects_a_forged_digest() {
        let value = json!("baseline");
        let digest = content_digest(&value).unwrap();
        let valid = json!({
            "value": value,
            "digest": digest,
        });
        assert!(serde_json::from_value::<MutationState>(valid).is_ok());

        let forged = json!({
            "value": "baseline",
            "digest": "sha256:forged",
        });
        assert!(serde_json::from_value::<MutationState>(forged).is_err());
    }
}
