use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain::{content_digest, CapabilityId, ContractId, Digest, ProviderPin};
#[cfg(test)]
use crate::kernel::evaluation::MetricCondition;
use crate::kernel::evaluation::{EvaluationError, EvaluationErrorKind};

pub const MAX_PRIMARY_COMPARISONS: usize = 32;
pub const MAX_REGRESSION_GUARDS: usize = 32;
pub const MAX_WORKLOAD_INVARIANTS: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SamplingPlan {
    pub settle_ms: u64,
    pub sample_count: u32,
    pub sample_interval_ms: u64,
}

impl SamplingPlan {
    pub fn validate(&self) -> Result<(), String> {
        if self.sample_count == 0 || self.sample_count > 30 {
            return Err("sampling sample_count must be between 1 and 30".to_string());
        }
        if self.settle_ms > 60_000 {
            return Err("sampling settle_ms must not exceed 60000".to_string());
        }
        if self.sample_interval_ms > 60_000 {
            return Err("sampling sample_interval_ms must not exceed 60000".to_string());
        }
        let scheduled_ms = self.settle_ms.saturating_add(
            u64::from(self.sample_count.saturating_sub(1)).saturating_mul(self.sample_interval_ms),
        );
        if scheduled_ms > 300_000 {
            return Err(
                "sampling settle and interval schedule must not exceed 300000 ms per side"
                    .to_string(),
            );
        }
        Ok(())
    }
}

impl Default for SamplingPlan {
    fn default() -> Self {
        Self {
            settle_ms: 3_000,
            sample_count: 3,
            sample_interval_ms: 1_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasurementBinding {
    pub capability_id: CapabilityId,
    #[serde(default = "empty_object")]
    pub specification: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonBinding {
    pub capability_id: CapabilityId,
    pub specification: Value,
}

impl ComparisonBinding {
    #[cfg(test)]
    pub fn threshold(capability_id: CapabilityId, conditions: Vec<MetricCondition>) -> Self {
        Self {
            capability_id,
            specification: json!({ "conditions": conditions }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationContractSpec {
    pub measurement: MeasurementBinding,
    pub primary: Vec<ComparisonBinding>,
    #[serde(default)]
    pub regression_guards: Vec<ComparisonBinding>,
    #[serde(default)]
    pub workload_invariants: Vec<ComparisonBinding>,
    #[serde(default)]
    pub sampling: SamplingPlan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityBindingPin {
    pub capability_id: CapabilityId,
    pub provider: ProviderPin,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedEvaluationContract")]
pub struct FrozenEvaluationContract {
    id: ContractId,
    measurement: MeasurementBinding,
    primary: Vec<ComparisonBinding>,
    regression_guards: Vec<ComparisonBinding>,
    workload_invariants: Vec<ComparisonBinding>,
    sampling: SamplingPlan,
    capability_generation: u64,
    capability_pins: Vec<CapabilityBindingPin>,
    digest: Digest,
}

#[derive(Deserialize)]
struct UncheckedEvaluationContract {
    id: ContractId,
    measurement: MeasurementBinding,
    primary: Vec<ComparisonBinding>,
    regression_guards: Vec<ComparisonBinding>,
    workload_invariants: Vec<ComparisonBinding>,
    sampling: SamplingPlan,
    capability_generation: u64,
    capability_pins: Vec<CapabilityBindingPin>,
    digest: Digest,
}

impl TryFrom<UncheckedEvaluationContract> for FrozenEvaluationContract {
    type Error = String;

    fn try_from(value: UncheckedEvaluationContract) -> Result<Self, Self::Error> {
        let expected_digest = value.digest;
        let contract = Self::from_parts(
            value.id,
            value.measurement,
            value.primary,
            value.regression_guards,
            value.workload_invariants,
            value.sampling,
            value.capability_generation,
            value.capability_pins,
        )
        .map_err(|error| error.message)?;
        if contract.digest != expected_digest {
            return Err("evaluation contract digest does not match frozen contents".to_string());
        }
        Ok(contract)
    }
}

impl FrozenEvaluationContract {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        id: ContractId,
        measurement: MeasurementBinding,
        primary: Vec<ComparisonBinding>,
        regression_guards: Vec<ComparisonBinding>,
        workload_invariants: Vec<ComparisonBinding>,
        sampling: SamplingPlan,
        capability_generation: u64,
        mut capability_pins: Vec<CapabilityBindingPin>,
    ) -> Result<Self, EvaluationError> {
        if primary.is_empty() || primary.len() > MAX_PRIMARY_COMPARISONS {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                format!(
                    "evaluation contract requires between 1 and {MAX_PRIMARY_COMPARISONS} primary comparisons"
                ),
            ));
        }
        if regression_guards.len() > MAX_REGRESSION_GUARDS {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                format!(
                    "evaluation contract allows at most {MAX_REGRESSION_GUARDS} regression guards"
                ),
            ));
        }
        if workload_invariants.len() > MAX_WORKLOAD_INVARIANTS {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                format!(
                    "evaluation contract allows at most {MAX_WORKLOAD_INVARIANTS} workload invariants"
                ),
            ));
        }
        sampling
            .validate()
            .map_err(|error| EvaluationError::new(EvaluationErrorKind::InvalidContract, error))?;
        for binding in primary
            .iter()
            .chain(regression_guards.iter())
            .chain(workload_invariants.iter())
        {
            if !binding.specification.is_object() {
                return Err(EvaluationError::new(
                    EvaluationErrorKind::InvalidContract,
                    format!(
                        "comparison '{}' specification must be an object",
                        binding.capability_id
                    ),
                ));
            }
        }
        if !measurement.specification.is_object() {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                "measurement specification must be an object",
            ));
        }

        capability_pins.sort_by(|left, right| left.capability_id.cmp(&right.capability_id));
        if capability_pins
            .windows(2)
            .any(|pair| pair[0].capability_id == pair[1].capability_id)
        {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                "evaluation contract contains duplicate capability pins",
            ));
        }
        let referenced = std::iter::once(&measurement.capability_id)
            .chain(
                primary
                    .iter()
                    .chain(regression_guards.iter())
                    .chain(workload_invariants.iter())
                    .map(|binding| &binding.capability_id),
            )
            .cloned()
            .collect::<BTreeSet<_>>();
        let pinned = capability_pins
            .iter()
            .map(|pin| pin.capability_id.clone())
            .collect::<BTreeSet<_>>();
        if referenced != pinned {
            return Err(EvaluationError::new(
                EvaluationErrorKind::InvalidContract,
                "evaluation contract capability pins do not match referenced capabilities",
            ));
        }

        let digest = content_digest(&(
            &id,
            &measurement,
            &primary,
            &regression_guards,
            &workload_invariants,
            &sampling,
            capability_generation,
            &capability_pins,
        ))
        .map_err(|error| EvaluationError::new(EvaluationErrorKind::InvalidContract, error))?;

        Ok(Self {
            id,
            measurement,
            primary,
            regression_guards,
            workload_invariants,
            sampling,
            capability_generation,
            capability_pins,
            digest,
        })
    }

    pub fn id(&self) -> &ContractId {
        &self.id
    }

    pub fn measurement(&self) -> &MeasurementBinding {
        &self.measurement
    }

    pub fn primary(&self) -> &[ComparisonBinding] {
        &self.primary
    }

    pub fn regression_guards(&self) -> &[ComparisonBinding] {
        &self.regression_guards
    }

    pub fn workload_invariants(&self) -> &[ComparisonBinding] {
        &self.workload_invariants
    }

    pub fn sampling(&self) -> &SamplingPlan {
        &self.sampling
    }

    pub fn capability_generation(&self) -> u64 {
        self.capability_generation
    }

    pub fn capability_pins(&self) -> &[CapabilityBindingPin] {
        &self.capability_pins
    }

    pub fn digest(&self) -> &Digest {
        &self.digest
    }
}

fn empty_object() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ProviderClass, ProviderId, ProviderVersion};
    use crate::kernel::evaluation::MetricOperator;

    fn id(value: &str) -> CapabilityId {
        CapabilityId::new(value).unwrap()
    }

    fn pin(capability_id: CapabilityId) -> CapabilityBindingPin {
        CapabilityBindingPin {
            capability_id,
            provider: ProviderPin {
                provider_id: ProviderId::new("test").unwrap(),
                provider_version: ProviderVersion::new("1").unwrap(),
                provider_class: ProviderClass::Builtin,
                manifest_digest: Digest::new("test-manifest").unwrap(),
            },
        }
    }

    #[test]
    fn contract_rejects_empty_primary_policy() {
        let measurement_id = id("measurement/core");
        let result = FrozenEvaluationContract::from_parts(
            ContractId::new("contract-1").unwrap(),
            MeasurementBinding {
                capability_id: measurement_id.clone(),
                specification: json!({}),
            },
            Vec::new(),
            Vec::new(),
            Vec::new(),
            SamplingPlan::default(),
            1,
            vec![pin(measurement_id)],
        );

        assert_eq!(
            result.unwrap_err().kind,
            EvaluationErrorKind::InvalidContract
        );
    }

    #[test]
    fn contract_rejects_comparison_fanout_above_limits() {
        let measurement_id = id("measurement/core");
        let comparison_id = id("comparison/threshold");
        let binding = || ComparisonBinding {
            capability_id: comparison_id.clone(),
            specification: json!({}),
        };
        let make = |primary: usize, guards: usize, invariants: usize| {
            FrozenEvaluationContract::from_parts(
                ContractId::new("contract-limits").unwrap(),
                MeasurementBinding {
                    capability_id: measurement_id.clone(),
                    specification: json!({}),
                },
                (0..primary).map(|_| binding()).collect(),
                (0..guards).map(|_| binding()).collect(),
                (0..invariants).map(|_| binding()).collect(),
                SamplingPlan::default(),
                1,
                vec![pin(measurement_id.clone()), pin(comparison_id.clone())],
            )
        };

        assert!(make(MAX_PRIMARY_COMPARISONS + 1, 0, 0).is_err());
        assert!(make(1, MAX_REGRESSION_GUARDS + 1, 0).is_err());
        assert!(make(1, 0, MAX_WORKLOAD_INVARIANTS + 1).is_err());
        assert!(make(
            MAX_PRIMARY_COMPARISONS,
            MAX_REGRESSION_GUARDS,
            MAX_WORKLOAD_INVARIANTS
        )
        .is_ok());
    }

    #[test]
    fn threshold_binding_serializes_typed_condition() {
        let binding = ComparisonBinding::threshold(
            id("comparison/threshold"),
            vec![MetricCondition::new(
                "latency.p99",
                MetricOperator::DecreasePercentGe,
                10.0,
            )],
        );

        assert_eq!(
            binding.specification["conditions"][0]["op"],
            "decrease_percent_ge"
        );
    }

    #[test]
    fn deserialization_cannot_bypass_frozen_contract_validation() {
        let raw = json!({
            "id": "contract-1",
            "measurement": {
                "capability_id": "measurement/core",
                "specification": {}
            },
            "primary": [],
            "regression_guards": [],
            "workload_invariants": [],
            "sampling": {
                "settle_ms": 0,
                "sample_count": 0,
                "sample_interval_ms": 0
            },
            "capability_generation": 1,
            "digest": "digest-1"
        });

        assert!(serde_json::from_value::<FrozenEvaluationContract>(raw).is_err());
    }
}
