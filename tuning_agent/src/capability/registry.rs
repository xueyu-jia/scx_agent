use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::capability::{
    AdminPolicy, CapabilitySnapshot, ComparisonPolicy, MeasurementProvider, MutationDriver,
    ProbeProvider,
};
use crate::domain::{CapabilityId, CapabilityKind, CapabilityMeta, EffectClass};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryErrorKind {
    Duplicate,
    InvalidMetadata,
    KindMismatch,
    PolicyDenied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryError {
    pub kind: RegistryErrorKind,
    pub message: String,
}

impl RegistryError {
    pub(crate) fn new(kind: RegistryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RegistryError {}

pub struct CapabilityRegistry {
    policy: AdminPolicy,
    generation: u64,
    metadata: BTreeMap<CapabilityId, CapabilityMeta>,
    probes: BTreeMap<CapabilityId, Arc<dyn ProbeProvider>>,
    mutations: BTreeMap<CapabilityId, Arc<dyn MutationDriver>>,
    measurements: BTreeMap<CapabilityId, Arc<dyn MeasurementProvider>>,
    comparisons: BTreeMap<CapabilityId, Arc<dyn ComparisonPolicy>>,
}

impl CapabilityRegistry {
    pub fn new(policy: AdminPolicy) -> Self {
        Self {
            policy,
            generation: 0,
            metadata: BTreeMap::new(),
            probes: BTreeMap::new(),
            mutations: BTreeMap::new(),
            measurements: BTreeMap::new(),
            comparisons: BTreeMap::new(),
        }
    }

    pub fn register_probe(
        &mut self,
        provider: Arc<dyn ProbeProvider>,
    ) -> Result<(), RegistryError> {
        let meta = provider.meta().clone();
        self.validate_registration(&meta, CapabilityKind::Probe, &[EffectClass::ReadOnly])?;
        self.probes.insert(meta.id.clone(), provider);
        self.finish_registration(meta);
        Ok(())
    }

    pub fn register_mutation(
        &mut self,
        provider: Arc<dyn MutationDriver>,
    ) -> Result<(), RegistryError> {
        let meta = provider.meta().clone();
        self.validate_registration(
            &meta,
            CapabilityKind::Mutation,
            &[EffectClass::ReversibleMutation],
        )?;
        self.mutations.insert(meta.id.clone(), provider);
        self.finish_registration(meta);
        Ok(())
    }

    pub fn register_measurement(
        &mut self,
        provider: Arc<dyn MeasurementProvider>,
    ) -> Result<(), RegistryError> {
        let meta = provider.meta().clone();
        self.validate_registration(&meta, CapabilityKind::Measurement, &[EffectClass::ReadOnly])?;
        self.measurements.insert(meta.id.clone(), provider);
        self.finish_registration(meta);
        Ok(())
    }

    pub fn register_comparison(
        &mut self,
        provider: Arc<dyn ComparisonPolicy>,
    ) -> Result<(), RegistryError> {
        let meta = provider.meta().clone();
        self.validate_registration(
            &meta,
            CapabilityKind::Comparison,
            &[EffectClass::PureComputation],
        )?;
        self.comparisons.insert(meta.id.clone(), provider);
        self.finish_registration(meta);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.metadata.len()
    }

    pub fn snapshot(&self) -> CapabilitySnapshot {
        CapabilitySnapshot::new(
            self.generation,
            self.metadata.clone(),
            self.probes.clone(),
            self.mutations.clone(),
            self.measurements.clone(),
            self.comparisons.clone(),
        )
    }

    fn validate_registration(
        &self,
        meta: &CapabilityMeta,
        expected_kind: CapabilityKind,
        allowed_effects: &[EffectClass],
    ) -> Result<(), RegistryError> {
        if self.metadata.contains_key(&meta.id) {
            return Err(RegistryError::new(
                RegistryErrorKind::Duplicate,
                format!("capability '{}' is already registered", meta.id),
            ));
        }
        if meta.kind != expected_kind {
            return Err(RegistryError::new(
                RegistryErrorKind::KindMismatch,
                format!(
                    "capability '{}' declares {:?}, expected {:?}",
                    meta.id, meta.kind, expected_kind
                ),
            ));
        }
        if !allowed_effects.contains(&meta.effect) {
            return Err(RegistryError::new(
                RegistryErrorKind::InvalidMetadata,
                format!(
                    "capability '{}' has invalid {:?} effect for {:?}",
                    meta.id, meta.effect, expected_kind
                ),
            ));
        }
        if meta.description.trim().is_empty() {
            return Err(RegistryError::new(
                RegistryErrorKind::InvalidMetadata,
                format!("capability '{}' has an empty description", meta.id),
            ));
        }
        if meta.allowed_phases.is_empty() {
            return Err(RegistryError::new(
                RegistryErrorKind::InvalidMetadata,
                format!("capability '{}' must declare allowed phases", meta.id),
            ));
        }
        let valid_phases: &[crate::domain::EpisodePhase] = match expected_kind {
            CapabilityKind::Probe | CapabilityKind::Mutation => &[
                crate::domain::EpisodePhase::Clean,
                crate::domain::EpisodePhase::Experimenting,
            ],
            CapabilityKind::Measurement | CapabilityKind::Comparison => {
                &[crate::domain::EpisodePhase::CommitPending]
            }
        };
        if meta
            .allowed_phases
            .iter()
            .any(|phase| !valid_phases.contains(phase))
        {
            return Err(RegistryError::new(
                RegistryErrorKind::InvalidMetadata,
                format!(
                    "capability '{}' declares a phase outside its authority boundary",
                    meta.id
                ),
            ));
        }
        if expected_kind == CapabilityKind::Mutation && !meta.idempotent {
            return Err(RegistryError::new(
                RegistryErrorKind::InvalidMetadata,
                format!("mutation capability '{}' must be idempotent", meta.id),
            ));
        }
        if expected_kind == CapabilityKind::Comparison && !meta.deterministic {
            return Err(RegistryError::new(
                RegistryErrorKind::InvalidMetadata,
                format!("comparison capability '{}' must be deterministic", meta.id),
            ));
        }
        if meta.input_schema.is_null() || meta.output_schema.is_null() {
            return Err(RegistryError::new(
                RegistryErrorKind::InvalidMetadata,
                format!("capability '{}' must declare input/output schemas", meta.id),
            ));
        }
        if meta.limits.timeout_ms == 0 || meta.limits.max_output_bytes == 0 {
            return Err(RegistryError::new(
                RegistryErrorKind::InvalidMetadata,
                format!("capability '{}' has invalid execution limits", meta.id),
            ));
        }
        self.policy.validate(meta, self.metadata.len())
    }

    fn finish_registration(&mut self, meta: CapabilityMeta) {
        self.metadata.insert(meta.id.clone(), meta);
        self.generation = self.generation.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::{
        CapabilityMeta, ComparisonEvidence, ComparisonRequest, Digest, MeasurementOpenRequest,
        MeasurementSampleRequest, MeasurementSession, MutationApplyRequest,
        MutationFinalizeRequest, MutationPrepareRequest, MutationQuery, MutationReceipt,
        MutationRestoreRequest, MutationStatus, MutationVerification, MutationVerifyRequest,
        PreparedMutation, ProbeEvidence, ProbeRequest, ProviderClass, ProviderError, ProviderId,
        ProviderPin, ProviderVersion,
    };

    struct DummyProbe {
        meta: CapabilityMeta,
    }

    impl ProbeProvider for DummyProbe {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn probe(&self, _request: &ProbeRequest) -> Result<ProbeEvidence, ProviderError> {
            unreachable!()
        }
    }

    struct DummyMutation {
        meta: CapabilityMeta,
    }

    impl MutationDriver for DummyMutation {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn prepare(
            &self,
            _request: &MutationPrepareRequest,
        ) -> Result<PreparedMutation, ProviderError> {
            unreachable!()
        }

        fn apply(&self, _request: &MutationApplyRequest) -> Result<MutationReceipt, ProviderError> {
            unreachable!()
        }

        fn status(&self, _query: &MutationQuery) -> Result<MutationStatus, ProviderError> {
            unreachable!()
        }

        fn verify(
            &self,
            _request: &MutationVerifyRequest,
        ) -> Result<MutationVerification, ProviderError> {
            unreachable!()
        }

        fn restore(
            &self,
            _request: &MutationRestoreRequest,
        ) -> Result<MutationReceipt, ProviderError> {
            unreachable!()
        }

        fn finalize(
            &self,
            _request: &MutationFinalizeRequest,
        ) -> Result<MutationReceipt, ProviderError> {
            unreachable!()
        }
    }

    struct DummyMeasurement {
        meta: CapabilityMeta,
    }

    impl MeasurementProvider for DummyMeasurement {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn validate_specification(
            &self,
            _specification: &serde_json::Value,
        ) -> Result<(), ProviderError> {
            unreachable!()
        }

        fn open(
            &self,
            _request: &MeasurementOpenRequest,
        ) -> Result<MeasurementSession, ProviderError> {
            unreachable!()
        }

        fn sample(
            &self,
            _request: &MeasurementSampleRequest,
        ) -> Result<crate::domain::MetricBatch, ProviderError> {
            unreachable!()
        }

        fn close(
            &self,
            _session: &MeasurementSession,
        ) -> Result<crate::domain::CleanupReceipt, ProviderError> {
            unreachable!()
        }
    }

    struct DummyComparison {
        meta: CapabilityMeta,
    }

    impl ComparisonPolicy for DummyComparison {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn validate_specification(
            &self,
            _specification: &serde_json::Value,
        ) -> Result<(), ProviderError> {
            unreachable!()
        }

        fn compare(
            &self,
            _request: &ComparisonRequest,
        ) -> Result<ComparisonEvidence, ProviderError> {
            unreachable!()
        }
    }

    #[test]
    fn registry_is_strongly_partitioned_and_snapshot_is_frozen() {
        let mut registry = CapabilityRegistry::new(AdminPolicy::default());
        let probe_id = CapabilityId::new("builtin/probe.psi").unwrap();
        registry
            .register_probe(Arc::new(DummyProbe {
                meta: meta(
                    probe_id.clone(),
                    CapabilityKind::Probe,
                    EffectClass::ReadOnly,
                    ProviderClass::Builtin,
                ),
            }))
            .unwrap();

        let snapshot = registry.snapshot();
        assert!(snapshot.probe(&probe_id).is_some());
        assert!(snapshot.mutation(&probe_id).is_none());

        let mutation_id = CapabilityId::new("local/mutate.sysctl").unwrap();
        registry
            .register_mutation(Arc::new(DummyMutation {
                meta: meta(
                    mutation_id.clone(),
                    CapabilityKind::Mutation,
                    EffectClass::ReversibleMutation,
                    ProviderClass::Local,
                ),
            }))
            .unwrap();

        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.meta(&mutation_id).is_none());
        assert_eq!(registry.snapshot().len(), 2);
    }

    #[test]
    fn duplicate_ids_are_rejected_across_provider_kinds() {
        let mut registry = CapabilityRegistry::new(AdminPolicy::default());
        let id = CapabilityId::new("builtin/shared").unwrap();
        registry
            .register_probe(Arc::new(DummyProbe {
                meta: meta(
                    id.clone(),
                    CapabilityKind::Probe,
                    EffectClass::ReadOnly,
                    ProviderClass::Builtin,
                ),
            }))
            .unwrap();
        let error = registry
            .register_measurement(Arc::new(DummyMeasurement {
                meta: meta(
                    id,
                    CapabilityKind::Measurement,
                    EffectClass::ReadOnly,
                    ProviderClass::Builtin,
                ),
            }))
            .unwrap_err();
        assert_eq!(error.kind, RegistryErrorKind::Duplicate);
    }

    #[test]
    fn default_admin_policy_rejects_mcp_until_explicitly_enabled() {
        let id = CapabilityId::new("mcp/probe.scheduler").unwrap();
        let provider = Arc::new(DummyProbe {
            meta: meta(
                id.clone(),
                CapabilityKind::Probe,
                EffectClass::ReadOnly,
                ProviderClass::Mcp,
            ),
        });
        let mut registry = CapabilityRegistry::new(AdminPolicy::default());
        assert_eq!(
            registry.register_probe(provider.clone()).unwrap_err().kind,
            RegistryErrorKind::PolicyDenied
        );

        let policy = AdminPolicy::default().allow_provider_classes([
            ProviderClass::Builtin,
            ProviderClass::Local,
            ProviderClass::Mcp,
        ]);
        let mut registry = CapabilityRegistry::new(policy);
        registry.register_probe(provider).unwrap();
        assert!(registry.snapshot().probe(&id).is_some());
    }

    #[test]
    fn irreversible_mutation_is_rejected_even_if_admin_allows_the_effect() {
        let policy = AdminPolicy::default().allow_effects([
            EffectClass::PureComputation,
            EffectClass::ReadOnly,
            EffectClass::ManagedObservation,
            EffectClass::ReversibleMutation,
            EffectClass::IrreversibleMutation,
        ]);
        let mut registry = CapabilityRegistry::new(policy);
        let error = registry
            .register_mutation(Arc::new(DummyMutation {
                meta: meta(
                    CapabilityId::new("local/mutate.irreversible").unwrap(),
                    CapabilityKind::Mutation,
                    EffectClass::IrreversibleMutation,
                    ProviderClass::Local,
                ),
            }))
            .unwrap_err();
        assert_eq!(error.kind, RegistryErrorKind::InvalidMetadata);
    }

    #[test]
    fn all_provider_traits_are_object_safe() {
        fn accepts_probe(_: &dyn ProbeProvider) {}
        fn accepts_mutation(_: &dyn MutationDriver) {}
        fn accepts_measurement(_: &dyn MeasurementProvider) {}
        fn accepts_comparison(_: &dyn ComparisonPolicy) {}

        let probe = DummyProbe {
            meta: meta(
                CapabilityId::new("builtin/probe").unwrap(),
                CapabilityKind::Probe,
                EffectClass::ReadOnly,
                ProviderClass::Builtin,
            ),
        };
        let mutation = DummyMutation {
            meta: meta(
                CapabilityId::new("local/mutation").unwrap(),
                CapabilityKind::Mutation,
                EffectClass::ReversibleMutation,
                ProviderClass::Local,
            ),
        };
        let measurement = DummyMeasurement {
            meta: meta(
                CapabilityId::new("builtin/measurement").unwrap(),
                CapabilityKind::Measurement,
                EffectClass::ReadOnly,
                ProviderClass::Builtin,
            ),
        };
        let comparison = DummyComparison {
            meta: meta(
                CapabilityId::new("builtin/comparison").unwrap(),
                CapabilityKind::Comparison,
                EffectClass::PureComputation,
                ProviderClass::Builtin,
            ),
        };

        accepts_probe(&probe);
        accepts_mutation(&mutation);
        accepts_measurement(&measurement);
        accepts_comparison(&comparison);
    }

    fn meta(
        id: CapabilityId,
        kind: CapabilityKind,
        effect: EffectClass,
        provider_class: ProviderClass,
    ) -> CapabilityMeta {
        let mut meta = CapabilityMeta::new(
            id,
            kind,
            effect,
            ProviderPin {
                provider_id: ProviderId::new("test-provider").unwrap(),
                provider_version: ProviderVersion::new("1.0.0").unwrap(),
                provider_class,
                manifest_digest: Digest::new("sha256:test").unwrap(),
            },
            "test capability",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
        .with_allowed_phases(match kind {
            CapabilityKind::Probe | CapabilityKind::Mutation => vec![
                crate::domain::EpisodePhase::Clean,
                crate::domain::EpisodePhase::Experimenting,
            ],
            CapabilityKind::Measurement | CapabilityKind::Comparison => {
                vec![crate::domain::EpisodePhase::CommitPending]
            }
        });
        if kind == CapabilityKind::Mutation {
            meta.idempotent = true;
        }
        if kind == CapabilityKind::Comparison {
            meta.deterministic = true;
        }
        meta
    }
}
