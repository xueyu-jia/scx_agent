use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::{content_digest, EpisodeId, EvaluationIntentPin};
use crate::kernel::evaluation::{
    EvaluationContractSpec, EvaluationError, EvaluationErrorKind, FrozenEvaluationContract,
};

pub const MAX_OBJECTIVE_STATEMENT_BYTES: usize = 4096;
const EVALUATION_INTENT_DIGEST_DOMAIN_V1: &str = "tuning-agent/evaluation-intent/v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ObjectiveStatement(String);

impl ObjectiveStatement {
    pub fn new(value: impl AsRef<str>) -> Result<Self, String> {
        let normalized = value
            .as_ref()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if normalized.is_empty() {
            return Err("objective statement must not be empty".to_string());
        }
        if normalized.len() > MAX_OBJECTIVE_STATEMENT_BYTES {
            return Err(format!(
                "objective statement must not exceed {MAX_OBJECTIVE_STATEMENT_BYTES} bytes"
            ));
        }
        if normalized.chars().any(char::is_control) {
            return Err("objective statement must not contain control characters".to_string());
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectiveStatement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ObjectiveStatement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<String> for ObjectiveStatement {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ObjectiveStatement {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationIntentSpec {
    pub objective: ObjectiveStatement,
    pub evaluation_contract: EvaluationContractSpec,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FrozenEvaluationIntent {
    pin: EvaluationIntentPin,
    objective: ObjectiveStatement,
    contract: FrozenEvaluationContract,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedFrozenEvaluationIntent {
    pin: EvaluationIntentPin,
    objective: ObjectiveStatement,
    contract: FrozenEvaluationContract,
}

#[derive(Serialize)]
struct IntentDigestPayload<'a> {
    domain: &'static str,
    episode_id: EpisodeId,
    objective: &'a ObjectiveStatement,
    contract_digest: &'a crate::domain::Digest,
}

impl TryFrom<UncheckedFrozenEvaluationIntent> for FrozenEvaluationIntent {
    type Error = String;

    fn try_from(value: UncheckedFrozenEvaluationIntent) -> Result<Self, Self::Error> {
        let expected_pin = value.pin;
        let intent = Self::from_parts(expected_pin.episode_id(), value.objective, value.contract)
            .map_err(|error| error.message)?;
        if intent.pin != expected_pin {
            return Err("evaluation intent pin does not match frozen contents".to_string());
        }
        Ok(intent)
    }
}

impl<'de> Deserialize<'de> for FrozenEvaluationIntent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let unchecked = UncheckedFrozenEvaluationIntent::deserialize(deserializer)?;
        Self::try_from(unchecked).map_err(serde::de::Error::custom)
    }
}

impl FrozenEvaluationIntent {
    pub(crate) fn from_parts(
        episode_id: EpisodeId,
        objective: ObjectiveStatement,
        contract: FrozenEvaluationContract,
    ) -> Result<Self, EvaluationError> {
        let intent_digest = content_digest(&IntentDigestPayload {
            domain: EVALUATION_INTENT_DIGEST_DOMAIN_V1,
            episode_id,
            objective: &objective,
            contract_digest: contract.digest(),
        })
        .map_err(|error| EvaluationError::new(EvaluationErrorKind::InvalidContract, error))?;
        let pin = EvaluationIntentPin::new(episode_id, intent_digest, contract.digest().clone());
        Ok(Self {
            pin,
            objective,
            contract,
        })
    }

    pub fn episode_id(&self) -> EpisodeId {
        self.pin.episode_id()
    }

    pub fn objective(&self) -> &ObjectiveStatement {
        &self.objective
    }

    pub fn contract(&self) -> &FrozenEvaluationContract {
        &self.contract
    }

    pub fn pin(&self) -> &EvaluationIntentPin {
        &self.pin
    }

    pub fn digest(&self) -> &crate::domain::Digest {
        self.pin.intent_digest()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::{
        CapabilityId, ContractId, Digest, ProviderClass, ProviderId, ProviderPin, ProviderVersion,
    };
    use crate::kernel::evaluation::{
        CapabilityBindingPin, ComparisonBinding, MeasurementBinding, SamplingPlan,
    };

    #[test]
    fn objective_statement_normalizes_whitespace_and_enforces_bounds() {
        let objective = ObjectiveStatement::new("  reduce\n scheduling\tlatency  ").unwrap();
        assert_eq!(objective.as_str(), "reduce scheduling latency");
        assert!(ObjectiveStatement::new(" \n\t ").is_err());
        assert!(ObjectiveStatement::new("x".repeat(MAX_OBJECTIVE_STATEMENT_BYTES + 1)).is_err());
        assert!(ObjectiveStatement::new("latency\0target").is_err());
    }

    #[test]
    fn objective_deserialization_cannot_bypass_normalization() {
        let objective: ObjectiveStatement =
            serde_json::from_value(json!("  reduce   latency ")).unwrap();
        assert_eq!(objective.as_str(), "reduce latency");
        assert!(serde_json::from_value::<ObjectiveStatement>(json!(" ")).is_err());
    }

    #[test]
    fn intent_digest_binds_episode_objective_and_contract() {
        let original_contract = contract("contract-a", 1);
        let original = FrozenEvaluationIntent::from_parts(
            EpisodeId::new(1),
            ObjectiveStatement::new("reduce latency").unwrap(),
            original_contract.clone(),
        )
        .unwrap();
        let another_episode = FrozenEvaluationIntent::from_parts(
            EpisodeId::new(2),
            ObjectiveStatement::new("reduce latency").unwrap(),
            original_contract.clone(),
        )
        .unwrap();
        let another_objective = FrozenEvaluationIntent::from_parts(
            EpisodeId::new(1),
            ObjectiveStatement::new("increase throughput").unwrap(),
            original_contract,
        )
        .unwrap();
        let another_contract = FrozenEvaluationIntent::from_parts(
            EpisodeId::new(1),
            ObjectiveStatement::new("reduce latency").unwrap(),
            contract("contract-b", 2),
        )
        .unwrap();

        assert_ne!(original.digest(), another_episode.digest());
        assert_ne!(original.digest(), another_objective.digest());
        assert_ne!(original.digest(), another_contract.digest());
        assert_eq!(original.pin().episode_id(), EpisodeId::new(1));
        assert_eq!(original.pin().intent_digest(), original.digest());
        assert_eq!(
            original.pin().contract_digest(),
            original.contract().digest()
        );
    }

    #[test]
    fn frozen_intent_round_trips_and_rejects_tampering() {
        let intent = FrozenEvaluationIntent::from_parts(
            EpisodeId::new(7),
            ObjectiveStatement::new("reduce latency").unwrap(),
            contract("contract-a", 1),
        )
        .unwrap();
        let encoded = serde_json::to_value(&intent).unwrap();
        assert_eq!(
            serde_json::from_value::<FrozenEvaluationIntent>(encoded.clone()).unwrap(),
            intent
        );

        let mut objective_tampered = encoded.clone();
        objective_tampered["objective"] = json!("increase throughput");
        assert!(serde_json::from_value::<FrozenEvaluationIntent>(objective_tampered).is_err());

        let mut episode_tampered = encoded.clone();
        episode_tampered["pin"]["episode_id"] = json!(8);
        assert!(serde_json::from_value::<FrozenEvaluationIntent>(episode_tampered).is_err());

        let mut contract_digest_tampered = encoded.clone();
        contract_digest_tampered["pin"]["contract_digest"] = json!("sha256:other");
        assert!(
            serde_json::from_value::<FrozenEvaluationIntent>(contract_digest_tampered).is_err()
        );

        let mut unknown = encoded;
        unknown["unexpected"] = json!(true);
        assert!(serde_json::from_value::<FrozenEvaluationIntent>(unknown).is_err());
    }

    #[test]
    fn intent_spec_rejects_unknown_fields() {
        let raw = json!({
            "objective": "reduce latency",
            "evaluation_contract": {
                "measurement": {
                    "capability_id": "measurement/core",
                    "specification": {}
                },
                "primary": [{
                    "capability_id": "comparison/threshold",
                    "specification": {}
                }]
            },
            "unexpected": true
        });

        assert!(serde_json::from_value::<EvaluationIntentSpec>(raw).is_err());
    }

    fn contract(id: &str, sample_count: u32) -> FrozenEvaluationContract {
        let measurement_id = CapabilityId::new("measurement/core").unwrap();
        let comparison_id = CapabilityId::new("comparison/threshold").unwrap();
        FrozenEvaluationContract::from_parts(
            ContractId::new(id).unwrap(),
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
            SamplingPlan {
                settle_ms: 0,
                sample_count,
                sample_interval_ms: 0,
            },
            1,
            vec![pin(measurement_id), pin(comparison_id)],
        )
        .unwrap()
    }

    fn pin(capability_id: CapabilityId) -> CapabilityBindingPin {
        CapabilityBindingPin {
            capability_id,
            provider: ProviderPin {
                provider_id: ProviderId::new("test").unwrap(),
                provider_version: ProviderVersion::new("1").unwrap(),
                provider_class: ProviderClass::Builtin,
                manifest_digest: Digest::new("sha256:test-manifest").unwrap(),
            },
        }
    }
}
