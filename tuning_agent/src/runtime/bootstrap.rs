use std::sync::Arc;

use serde_json::{json, Value};

use crate::adapters::local::comparator::ThresholdComparisonPolicy;
use crate::adapters::local::measurement::CoreSystemMeasurementProvider;
use crate::adapters::local::mutation::{BoundLinuxFileMutationDriver, LinuxMutationTarget};
use crate::adapters::local::probe::LinuxProcSnapshotProbe;
use crate::adapters::mcp::{load_server, LoadedMcpCapability};
use crate::capability::{AdminPolicy, CapabilityRegistry};
use crate::config::{CapabilityConfig, LocalMutationTargetConfig, McpConfig};
use crate::domain::{CapabilityId, ProviderClass};

pub(crate) struct RegistryBootstrap {
    pub registry: CapabilityRegistry,
    pub notices: Vec<Value>,
    pub failures: Vec<BootstrapFailure>,
}

pub(crate) struct BootstrapFailure {
    pub component: String,
    pub error: String,
}

pub(crate) fn build_local_registry(
    capabilities: &CapabilityConfig,
    mcp: &McpConfig,
) -> RegistryBootstrap {
    let mut failures = Vec::new();
    let mut provider_classes = vec![ProviderClass::Builtin, ProviderClass::Local];
    if mcp.enabled {
        provider_classes.push(ProviderClass::Mcp);
    }
    let mut policy = AdminPolicy::default().allow_provider_classes(provider_classes);
    if !capabilities.allowed_capabilities.is_empty() {
        let allowed = capabilities
            .allowed_capabilities
            .iter()
            .filter_map(|id| match CapabilityId::new(id.clone()) {
                Ok(id) => Some(id),
                Err(error) => {
                    failures.push(BootstrapFailure {
                        component: "capability_policy".into(),
                        error,
                    });
                    None
                }
            })
            .collect::<Vec<_>>();
        policy = policy.allow_only_capabilities(allowed);
    }
    for id in &capabilities.denied_capabilities {
        match CapabilityId::new(id.clone()) {
            Ok(id) => policy = policy.deny_capability(id),
            Err(error) => failures.push(BootstrapFailure {
                component: "capability_policy".into(),
                error,
            }),
        }
    }

    let mut registry = CapabilityRegistry::new(policy);
    let notices = Vec::new();
    if let Err(error) = registry.register_probe(Arc::new(LinuxProcSnapshotProbe::new())) {
        failures.push(BootstrapFailure {
            component: "builtin/probe.linux-proc-snapshot.v1".into(),
            error: error.to_string(),
        });
    }
    if let Err(error) =
        registry.register_measurement(Arc::new(CoreSystemMeasurementProvider::new()))
    {
        failures.push(BootstrapFailure {
            component: "builtin/measurement.core-system.v1".into(),
            error: error.to_string(),
        });
    }
    if let Err(error) = registry.register_comparison(Arc::new(ThresholdComparisonPolicy::new())) {
        failures.push(BootstrapFailure {
            component: "builtin/comparison.threshold.v1".into(),
            error: error.to_string(),
        });
    }

    for configured in &capabilities.local_mutations {
        let target = match &configured.target {
            LocalMutationTargetConfig::Sysctl { key } => {
                LinuxMutationTarget::Sysctl { key: key.clone() }
            }
            LocalMutationTargetConfig::ProcSys { path } => LinuxMutationTarget::ProcSys {
                path: path.display().to_string(),
            },
            LocalMutationTargetConfig::Sysfs { path } => LinuxMutationTarget::Sysfs {
                path: path.display().to_string(),
            },
            LocalMutationTargetConfig::Cgroup { path } => LinuxMutationTarget::Cgroup {
                path: path.display().to_string(),
            },
        };
        let capability_id = match CapabilityId::new(configured.id.clone()) {
            Ok(id) => id,
            Err(error) => {
                failures.push(BootstrapFailure {
                    component: format!("local_mutation/{}", configured.id),
                    error,
                });
                continue;
            }
        };
        let driver = match BoundLinuxFileMutationDriver::new(
            capability_id,
            configured.description.clone(),
            target,
        ) {
            Ok(driver) => driver,
            Err(error) => {
                failures.push(BootstrapFailure {
                    component: format!("local_mutation/{}", configured.id),
                    error,
                });
                continue;
            }
        };
        if let Err(error) = registry.register_mutation(Arc::new(driver)) {
            failures.push(BootstrapFailure {
                component: format!("local_mutation/{}", configured.id),
                error: error.to_string(),
            });
        }
    }
    RegistryBootstrap {
        registry,
        notices,
        failures,
    }
}

pub(crate) fn extend_registry_with_mcp(bootstrap: &mut RegistryBootstrap, mcp: &McpConfig) {
    if !mcp.enabled {
        return;
    }
    for server in mcp.servers.iter().filter(|server| server.enabled) {
        let loaded = match load_server(server) {
            Ok(loaded) => loaded,
            Err(error) => {
                bootstrap.failures.push(BootstrapFailure {
                    component: format!("mcp_server/{}", server.id),
                    error: error.to_string(),
                });
                continue;
            }
        };
        let loaded_ids = loaded
            .capabilities()
            .iter()
            .map(|capability| capability.meta().id.as_str())
            .collect::<Vec<_>>();
        let skipped = loaded
            .skipped()
            .iter()
            .map(|capability| {
                json!({
                    "capability_id": capability.capability_id,
                    "reason": capability.reason,
                })
            })
            .collect::<Vec<_>>();
        bootstrap.notices.push(json!({
            "server_id": loaded.server_id(),
            "provider": loaded.provider(),
            "loaded_capabilities": loaded_ids,
            "skipped_capabilities": skipped,
        }));
        for capability in loaded.into_capabilities() {
            let capability_id = capability.meta().id.to_string();
            let result = match capability {
                LoadedMcpCapability::Probe(provider) => bootstrap.registry.register_probe(provider),
                LoadedMcpCapability::Mutation(provider) => {
                    bootstrap.registry.register_mutation(provider)
                }
                LoadedMcpCapability::Measurement(provider) => {
                    bootstrap.registry.register_measurement(provider)
                }
                LoadedMcpCapability::Comparison(provider) => {
                    bootstrap.registry.register_comparison(provider)
                }
            };
            if let Err(error) = result {
                bootstrap.failures.push(BootstrapFailure {
                    component: format!("mcp_capability/{capability_id}"),
                    error: error.to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::CapabilityId;
    use crate::kernel::evaluation::TRUSTED_GUARDRAIL_MEASUREMENT_ID;

    #[test]
    fn safe_defaults_always_register_probe_comparison_and_trusted_guardrails() {
        let capabilities = CapabilityConfig::default();
        let mcp = McpConfig::default();
        let mut bootstrap = build_local_registry(&capabilities, &mcp);
        extend_registry_with_mcp(&mut bootstrap, &mcp);
        assert!(bootstrap.notices.is_empty());
        assert!(bootstrap.failures.is_empty());
        let snapshot = bootstrap.registry.snapshot();
        assert!(snapshot
            .probe(&CapabilityId::new("builtin/probe.linux-proc-snapshot.v1").unwrap())
            .is_some());
        assert!(snapshot
            .measurement(&CapabilityId::new(TRUSTED_GUARDRAIL_MEASUREMENT_ID).unwrap())
            .is_some());
        assert!(snapshot
            .comparison(&CapabilityId::new("builtin/comparison.threshold.v1").unwrap())
            .is_some());
    }
}
