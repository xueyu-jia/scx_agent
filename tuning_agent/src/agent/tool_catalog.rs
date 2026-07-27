use std::collections::BTreeMap;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::agent::AgentToolSpec;
use crate::capability::CapabilitySnapshot;
use crate::domain::{CapabilityId, CapabilityKind};
use crate::kernel::evaluation::{
    MAX_OBJECTIVE_STATEMENT_BYTES, MAX_PRIMARY_COMPARISONS, MAX_REGRESSION_GUARDS,
    MAX_WORKLOAD_INVARIANTS,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CatalogKind {
    Probe,
    Mutation,
    BeginExperiment,
    RequestCommit,
    Abort,
    LoadSkill,
    LoadSkillReference,
}

#[derive(Clone, Debug)]
pub(crate) struct CatalogEntry {
    pub kind: CatalogKind,
    pub capability_id: Option<CapabilityId>,
}

#[derive(Clone, Debug)]
pub struct ToolCatalog {
    specs: Vec<AgentToolSpec>,
    entries: BTreeMap<String, CatalogEntry>,
}

impl ToolCatalog {
    pub fn from_snapshot(snapshot: &CapabilitySnapshot) -> Self {
        let mut catalog = Self {
            specs: Vec::new(),
            entries: BTreeMap::new(),
        };

        for meta in snapshot.iter_meta() {
            match meta.kind {
                CapabilityKind::Probe => catalog.add_capability(
                    "probe",
                    CatalogKind::Probe,
                    &meta.id,
                    &meta.description,
                    meta.input_schema.clone(),
                ),
                CapabilityKind::Mutation => catalog.add_capability(
                    "experiment",
                    CatalogKind::Mutation,
                    &meta.id,
                    &meta.description,
                    mutation_schema(meta.input_schema.clone()),
                ),
                CapabilityKind::Measurement | CapabilityKind::Comparison => {}
            }
        }

        catalog.add_builtin(
            "begin_experiment",
            CatalogKind::BeginExperiment,
            "Freeze the episode's only objective and complete evaluation contract before any mutation. Use only exact metric names listed in the selected measurement capability's Output metrics.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["objective", "evaluation_contract"],
                "properties": {
                    "objective": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_OBJECTIVE_STATEMENT_BYTES
                    },
                    "evaluation_contract": evaluation_contract_schema(snapshot)
                }
            }),
        );
        catalog.add_builtin(
            "request_commit",
            CatalogKind::RequestCommit,
            "Request the episode's single deterministic evaluation using only the latest verified change ID for each selected resource. Runtime alone may commit.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["change_ids", "reason"],
                "properties": {
                    "change_ids": {
                        "type": "array",
                        "minItems": 1,
                        "uniqueItems": true,
                        "items": { "type": "string", "minLength": 1 }
                    },
                    "reason": { "type": "string", "minLength": 1 }
                }
            }),
        );
        catalog.add_builtin(
            "abort",
            CatalogKind::Abort,
            "End the current experiment and restore all uncommitted changes.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["reason"],
                "properties": { "reason": { "type": "string", "minLength": 1 } }
            }),
        );
        catalog
    }

    pub fn from_snapshots(snapshot: &CapabilitySnapshot, skills_available: bool) -> Self {
        let mut catalog = Self::from_snapshot(snapshot);
        if skills_available {
            catalog.add_builtin(
                "load_skill",
                CatalogKind::LoadSkill,
                "Load one available Skill's complete SKILL.md instructions. Use it in a context-only tool-call batch before tuning actions.",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name"],
                    "properties": {
                        "name": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 64,
                            "pattern": "^[a-z0-9]+(?:-[a-z0-9]+)*$"
                        }
                    }
                }),
            );
            catalog.add_builtin(
                "load_skill_reference",
                CatalogKind::LoadSkillReference,
                "Load one UTF-8 reference from an already loaded Skill. Use the exact references/... path returned by load_skill and do not mix this call with tuning actions.",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["skill", "path"],
                    "properties": {
                        "skill": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 64,
                            "pattern": "^[a-z0-9]+(?:-[a-z0-9]+)*$"
                        },
                        "path": {
                            "type": "string",
                            "minLength": 12,
                            "maxLength": 4096,
                            "pattern": "^references/"
                        }
                    }
                }),
            );
        }
        catalog
    }

    pub fn specs(&self) -> &[AgentToolSpec] {
        &self.specs
    }

    pub(crate) fn entry(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries.get(name)
    }

    fn add_capability(
        &mut self,
        prefix: &str,
        kind: CatalogKind,
        capability_id: &CapabilityId,
        description: &str,
        input_schema: Value,
    ) {
        let name = capability_tool_name(prefix, capability_id);
        self.specs.push(AgentToolSpec {
            name: name.clone(),
            description: format!("{description} [capability_id={capability_id}]"),
            input_schema,
        });
        self.entries.insert(
            name,
            CatalogEntry {
                kind,
                capability_id: Some(capability_id.clone()),
            },
        );
    }

    fn add_builtin(
        &mut self,
        name: &str,
        kind: CatalogKind,
        description: &str,
        input_schema: Value,
    ) {
        self.specs.push(AgentToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
        });
        self.entries.insert(
            name.to_string(),
            CatalogEntry {
                kind,
                capability_id: None,
            },
        );
    }
}

fn evaluation_contract_schema(snapshot: &CapabilitySnapshot) -> Value {
    let measurement = snapshot
        .iter_meta()
        .filter(|meta| meta.kind == CapabilityKind::Measurement && meta.is_agent_selectable())
        .map(binding_schema)
        .collect::<Vec<_>>();
    let comparison = snapshot
        .iter_meta()
        .filter(|meta| meta.kind == CapabilityKind::Comparison)
        .map(binding_schema)
        .collect::<Vec<_>>();
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["measurement", "primary"],
        "properties": {
            "measurement": alternatives(measurement),
            "primary": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_PRIMARY_COMPARISONS,
                "items": alternatives(comparison.clone())
            },
            "regression_guards": {
                "type": "array",
                "maxItems": MAX_REGRESSION_GUARDS,
                "items": alternatives(comparison.clone())
            },
            "workload_invariants": {
                "type": "array",
                "maxItems": MAX_WORKLOAD_INVARIANTS,
                "items": alternatives(comparison)
            },
            "sampling": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "settle_ms": {"type": "integer", "minimum": 0, "maximum": 60000},
                    "sample_count": {"type": "integer", "minimum": 1, "maximum": 30},
                    "sample_interval_ms": {"type": "integer", "minimum": 0, "maximum": 60000}
                }
            }
        }
    })
}

fn binding_schema(meta: &crate::domain::CapabilityMeta) -> Value {
    json!({
        "type": "object",
        "description": binding_description(meta),
        "additionalProperties": false,
        "required": ["capability_id", "specification"],
        "properties": {
            "capability_id": {"const": meta.id.as_str()},
            "specification": meta.input_schema,
        }
    })
}

fn binding_description(meta: &crate::domain::CapabilityMeta) -> String {
    if meta.kind != CapabilityKind::Measurement {
        return meta.description.clone();
    }

    let metrics = meta
        .output_schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, schema)| {
            let meaning = schema
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| schema.get("type").and_then(Value::as_str))
                .unwrap_or("provider-defined value");
            format!("{name}: {meaning}")
        })
        .collect::<Vec<_>>();
    if metrics.is_empty() {
        meta.description.clone()
    } else {
        format!(
            "{}. Output metrics: {}.",
            meta.description.trim_end_matches('.'),
            metrics.join("; ")
        )
    }
}

fn alternatives(variants: Vec<Value>) -> Value {
    if variants.is_empty() {
        json!({"not": {}})
    } else {
        json!({"oneOf": variants})
    }
}

fn mutation_schema(provider_schema: Value) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["arguments", "reason"],
        "properties": {
            "arguments": provider_schema,
            "reason": { "type": "string", "minLength": 1 }
        }
    })
}

fn capability_tool_name(prefix: &str, capability_id: &CapabilityId) -> String {
    let digest = Sha256::digest(capability_id.as_str().as_bytes());
    let suffix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{prefix}_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CapabilityKind, CapabilityMeta, Digest, EffectClass, ProviderClass, ProviderId,
        ProviderPin, ProviderVersion,
    };

    #[test]
    fn generated_names_are_openai_safe_and_stable() {
        let id = CapabilityId::new("mcp/scxtop:scheduler.stats@v1").unwrap();
        let first = capability_tool_name("probe", &id);
        let second = capability_tool_name("probe", &id);

        assert_eq!(first, second);
        assert!(first
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_'));
    }

    #[test]
    fn skill_context_tools_are_only_exposed_when_skills_are_available() {
        let registry =
            crate::capability::CapabilityRegistry::new(crate::capability::AdminPolicy::default());
        let snapshot = registry.snapshot();
        let without_skills = ToolCatalog::from_snapshots(&snapshot, false);
        let with_skills = ToolCatalog::from_snapshots(&snapshot, true);

        assert!(!without_skills
            .specs()
            .iter()
            .any(|tool| tool.name == "load_skill"));
        assert!(with_skills
            .specs()
            .iter()
            .any(|tool| tool.name == "load_skill"));
        assert!(with_skills
            .specs()
            .iter()
            .any(|tool| tool.name == "load_skill_reference"));
    }

    #[test]
    fn evaluation_contract_schema_matches_runtime_comparison_limits() {
        let registry =
            crate::capability::CapabilityRegistry::new(crate::capability::AdminPolicy::default());
        let schema = evaluation_contract_schema(&registry.snapshot());

        assert_eq!(
            schema["properties"]["primary"]["maxItems"],
            json!(MAX_PRIMARY_COMPARISONS)
        );
        assert_eq!(
            schema["properties"]["regression_guards"]["maxItems"],
            json!(MAX_REGRESSION_GUARDS)
        );
        assert_eq!(
            schema["properties"]["workload_invariants"]["maxItems"],
            json!(MAX_WORKLOAD_INVARIANTS)
        );
    }

    #[test]
    fn runtime_guardrail_measurement_is_absent_from_agent_contract_schema() {
        let mut registry =
            crate::capability::CapabilityRegistry::new(crate::capability::AdminPolicy::default());
        registry
            .register_measurement(std::sync::Arc::new(
                crate::adapters::local::measurement::CoreSystemMeasurementProvider::new(),
            ))
            .unwrap();

        let schema = evaluation_contract_schema(&registry.snapshot());
        let measurement = &schema["properties"]["measurement"];

        assert_eq!(measurement, &json!({"not": {}}));
        assert!(!schema
            .to_string()
            .contains("builtin/measurement.core-system.v1"));
    }

    #[test]
    fn measurement_binding_describes_provider_output_metrics() {
        let meta = CapabilityMeta::new(
            CapabilityId::new("measurement/classification-integrity").unwrap(),
            CapabilityKind::Measurement,
            EffectClass::ReadOnly,
            ProviderPin {
                provider_id: ProviderId::new("test-provider").unwrap(),
                provider_version: ProviderVersion::new("1").unwrap(),
                provider_class: ProviderClass::Mcp,
                manifest_digest: Digest::new("test-manifest").unwrap(),
            },
            "Measure classification integrity",
            json!({"type": "object"}),
            json!({
                "type": "object",
                "properties": {
                    "active_rule_coverage": {
                        "type": "number",
                        "description": "Fraction of expected rules active in BPF"
                    },
                    "task_state_errors_delta": {
                        "type": "number",
                        "description": "Task-state errors since measurement open"
                    }
                }
            }),
        );

        let binding = binding_schema(&meta);
        let description = binding["description"].as_str().unwrap();
        assert!(description.contains("active_rule_coverage"));
        assert!(description.contains("Fraction of expected rules active in BPF"));
        assert!(description.contains("task_state_errors_delta"));
        assert_eq!(
            binding["properties"]["capability_id"]["const"],
            "measurement/classification-integrity"
        );
    }
}
