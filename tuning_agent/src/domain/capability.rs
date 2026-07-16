use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{CapabilityId, Digest, EpisodePhase, ProviderId, ProviderVersion};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Probe,
    Mutation,
    Measurement,
    Comparison,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    PureComputation,
    ReadOnly,
    ManagedObservation,
    ReversibleMutation,
    IrreversibleMutation,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderClass {
    Builtin,
    Local,
    Mcp,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderPin {
    pub provider_id: ProviderId,
    pub provider_version: ProviderVersion,
    pub provider_class: ProviderClass,
    pub manifest_digest: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapabilityLimits {
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
}

impl Default for CapabilityLimits {
    fn default() -> Self {
        Self {
            timeout_ms: 30_000,
            max_output_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityMeta {
    pub id: CapabilityId,
    pub kind: CapabilityKind,
    pub effect: EffectClass,
    pub provider: ProviderPin,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub allowed_phases: Vec<EpisodePhase>,
    pub limits: CapabilityLimits,
    pub deterministic: bool,
    pub idempotent: bool,
}

impl CapabilityMeta {
    pub fn new(
        id: CapabilityId,
        kind: CapabilityKind,
        effect: EffectClass,
        provider: ProviderPin,
        description: impl Into<String>,
        input_schema: Value,
        output_schema: Value,
    ) -> Self {
        Self {
            id,
            kind,
            effect,
            provider,
            description: description.into(),
            input_schema,
            output_schema,
            allowed_phases: Vec::new(),
            limits: CapabilityLimits::default(),
            deterministic: false,
            idempotent: false,
        }
    }

    pub fn with_allowed_phases(mut self, phases: impl IntoIterator<Item = EpisodePhase>) -> Self {
        self.allowed_phases = phases.into_iter().collect();
        self.allowed_phases.sort_by_key(|phase| *phase as u8);
        self.allowed_phases.dedup();
        self
    }

    pub fn is_allowed_in(&self, phase: EpisodePhase) -> bool {
        self.allowed_phases.contains(&phase)
    }
}
