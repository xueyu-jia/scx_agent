use std::collections::BTreeSet;

use crate::domain::{CapabilityId, CapabilityMeta, EffectClass, ProviderClass};

use super::{RegistryError, RegistryErrorKind};

#[derive(Clone, Debug)]
pub struct AdminPolicy {
    allowed_capabilities: Option<BTreeSet<CapabilityId>>,
    denied_capabilities: BTreeSet<CapabilityId>,
    allowed_provider_classes: BTreeSet<ProviderClass>,
    allowed_effects: BTreeSet<EffectClass>,
}

const MAX_CAPABILITIES: usize = 256;

impl Default for AdminPolicy {
    fn default() -> Self {
        Self {
            allowed_capabilities: None,
            denied_capabilities: BTreeSet::new(),
            allowed_provider_classes: BTreeSet::from([
                ProviderClass::Builtin,
                ProviderClass::Local,
            ]),
            allowed_effects: BTreeSet::from([
                EffectClass::PureComputation,
                EffectClass::ReadOnly,
                EffectClass::ReversibleMutation,
            ]),
        }
    }
}

impl AdminPolicy {
    pub fn allow_only_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = CapabilityId>,
    ) -> Self {
        self.allowed_capabilities = Some(capabilities.into_iter().collect());
        self
    }

    pub fn deny_capability(mut self, capability: CapabilityId) -> Self {
        self.denied_capabilities.insert(capability);
        self
    }

    pub fn allow_provider_classes(
        mut self,
        classes: impl IntoIterator<Item = ProviderClass>,
    ) -> Self {
        self.allowed_provider_classes = classes.into_iter().collect();
        self
    }

    #[cfg(test)]
    pub fn allow_effects(mut self, effects: impl IntoIterator<Item = EffectClass>) -> Self {
        self.allowed_effects = effects.into_iter().collect();
        self
    }

    pub(crate) fn validate(
        &self,
        meta: &CapabilityMeta,
        registered_count: usize,
    ) -> Result<(), RegistryError> {
        if registered_count >= MAX_CAPABILITIES {
            return Err(RegistryError::new(
                RegistryErrorKind::PolicyDenied,
                format!("capability limit {MAX_CAPABILITIES} has been reached"),
            ));
        }
        if !self
            .allowed_provider_classes
            .contains(&meta.provider.provider_class)
        {
            return Err(policy_denied(meta, "provider class is not allowed"));
        }
        if let Some(allowed) = &self.allowed_capabilities {
            if !allowed.contains(&meta.id) {
                return Err(policy_denied(meta, "capability is not allowlisted"));
            }
        }
        if self.denied_capabilities.contains(&meta.id) {
            return Err(policy_denied(meta, "capability is denied"));
        }
        if !self.allowed_effects.contains(&meta.effect) {
            return Err(policy_denied(meta, "effect class is not allowed"));
        }
        Ok(())
    }
}

fn policy_denied(meta: &CapabilityMeta, reason: &str) -> RegistryError {
    RegistryError::new(
        RegistryErrorKind::PolicyDenied,
        format!("capability '{}' rejected: {reason}", meta.id),
    )
}
