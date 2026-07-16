use std::collections::BTreeSet;

use crate::capability::CapabilitySnapshot;
use crate::domain::{CapabilityId, ContractId, EpisodePhase};
use crate::kernel::evaluation::{
    CapabilityBindingPin, EvaluationContractSpec, EvaluationError, EvaluationErrorKind,
    FrozenEvaluationContract,
};

pub struct ContractFreezer {
    capabilities: CapabilitySnapshot,
}

impl ContractFreezer {
    pub fn new(capabilities: CapabilitySnapshot) -> Self {
        Self { capabilities }
    }

    pub fn freeze(
        &self,
        id: ContractId,
        spec: EvaluationContractSpec,
    ) -> Result<FrozenEvaluationContract, EvaluationError> {
        let measurement = self
            .capabilities
            .measurement(&spec.measurement.capability_id)
            .ok_or_else(|| missing("measurement", &spec.measurement.capability_id))?;
        ensure_commit_pending(measurement.meta())?;
        measurement
            .validate_specification(&spec.measurement.specification)
            .map_err(|error| {
                EvaluationError::new(
                    EvaluationErrorKind::InvalidContract,
                    format!(
                        "measurement '{}' rejected its specification: {error}",
                        spec.measurement.capability_id
                    ),
                )
            })?;

        let mut capability_ids = BTreeSet::from([spec.measurement.capability_id.clone()]);
        for binding in spec
            .primary
            .iter()
            .chain(spec.regression_guards.iter())
            .chain(spec.workload_invariants.iter())
        {
            let policy = self
                .capabilities
                .comparison(&binding.capability_id)
                .ok_or_else(|| missing("comparison", &binding.capability_id))?;
            ensure_commit_pending(policy.meta())?;
            policy
                .validate_specification(&binding.specification)
                .map_err(|error| {
                    EvaluationError::new(
                        EvaluationErrorKind::InvalidContract,
                        format!(
                            "comparison '{}' rejected its specification: {error}",
                            binding.capability_id
                        ),
                    )
                })?;
            capability_ids.insert(binding.capability_id.clone());
        }
        let pins = capability_ids
            .into_iter()
            .map(|capability_id| {
                let meta = self
                    .capabilities
                    .meta(&capability_id)
                    .expect("typed provider has matching capability metadata");
                CapabilityBindingPin {
                    capability_id,
                    provider: meta.provider.clone(),
                }
            })
            .collect();

        FrozenEvaluationContract::from_parts(
            id,
            spec.measurement,
            spec.primary,
            spec.regression_guards,
            spec.workload_invariants,
            spec.sampling,
            self.capabilities.generation(),
            pins,
        )
    }
}

fn ensure_commit_pending(meta: &crate::domain::CapabilityMeta) -> Result<(), EvaluationError> {
    if meta.is_allowed_in(EpisodePhase::CommitPending) {
        Ok(())
    } else {
        Err(EvaluationError::new(
            EvaluationErrorKind::InvalidContract,
            format!(
                "evaluation capability '{}' is not allowed in commit_pending",
                meta.id
            ),
        ))
    }
}

fn missing(kind: &str, capability_id: &CapabilityId) -> EvaluationError {
    EvaluationError::new(
        EvaluationErrorKind::MissingCapability,
        format!("{kind} capability '{capability_id}' is unavailable"),
    )
}
