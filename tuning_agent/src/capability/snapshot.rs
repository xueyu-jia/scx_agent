use std::collections::BTreeMap;
use std::sync::Arc;

use crate::capability::{ComparisonPolicy, MeasurementProvider, MutationDriver, ProbeProvider};
use crate::domain::{CapabilityId, CapabilityMeta};

#[derive(Clone)]
pub struct CapabilitySnapshot {
    generation: u64,
    metadata: BTreeMap<CapabilityId, CapabilityMeta>,
    probes: BTreeMap<CapabilityId, Arc<dyn ProbeProvider>>,
    mutations: BTreeMap<CapabilityId, Arc<dyn MutationDriver>>,
    measurements: BTreeMap<CapabilityId, Arc<dyn MeasurementProvider>>,
    comparisons: BTreeMap<CapabilityId, Arc<dyn ComparisonPolicy>>,
}

impl CapabilitySnapshot {
    pub(crate) fn new(
        generation: u64,
        metadata: BTreeMap<CapabilityId, CapabilityMeta>,
        probes: BTreeMap<CapabilityId, Arc<dyn ProbeProvider>>,
        mutations: BTreeMap<CapabilityId, Arc<dyn MutationDriver>>,
        measurements: BTreeMap<CapabilityId, Arc<dyn MeasurementProvider>>,
        comparisons: BTreeMap<CapabilityId, Arc<dyn ComparisonPolicy>>,
    ) -> Self {
        Self {
            generation,
            metadata,
            probes,
            mutations,
            measurements,
            comparisons,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.metadata.len()
    }

    pub fn meta(&self, id: &CapabilityId) -> Option<&CapabilityMeta> {
        self.metadata.get(id)
    }

    pub fn iter_meta(&self) -> impl Iterator<Item = &CapabilityMeta> {
        self.metadata.values()
    }

    pub fn probe(&self, id: &CapabilityId) -> Option<Arc<dyn ProbeProvider>> {
        self.probes.get(id).cloned()
    }

    pub fn mutation(&self, id: &CapabilityId) -> Option<Arc<dyn MutationDriver>> {
        self.mutations.get(id).cloned()
    }

    pub fn measurement(&self, id: &CapabilityId) -> Option<Arc<dyn MeasurementProvider>> {
        self.measurements.get(id).cloned()
    }

    pub fn comparison(&self, id: &CapabilityId) -> Option<Arc<dyn ComparisonPolicy>> {
        self.comparisons.get(id).cloned()
    }
}
