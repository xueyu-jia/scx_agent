use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use serde_json::Value;

use crate::agent::tool_catalog::CatalogKind;
use crate::agent::{AgentCommand, AgentToolInvocation, DecodedToolCall, ToolCatalog};
use crate::domain::ChangeId;
use crate::kernel::evaluation::EvaluationIntentSpec;
use crate::skill::SkillCommand;

pub struct ToolDispatcher<'a> {
    catalog: &'a ToolCatalog,
}

impl<'a> ToolDispatcher<'a> {
    pub fn new(catalog: &'a ToolCatalog) -> Self {
        Self { catalog }
    }

    pub(crate) fn decode(
        &self,
        invocation: &AgentToolInvocation,
    ) -> Result<DecodedToolCall, DispatchError> {
        let entry = self.catalog.entry(&invocation.name).ok_or_else(|| {
            DispatchError::new(format!("unknown agent tool '{}'", invocation.name))
        })?;
        match entry.kind {
            CatalogKind::Probe => Ok(DecodedToolCall::Action(AgentCommand::Probe {
                call_id: invocation.id.clone(),
                capability_id: entry
                    .capability_id
                    .clone()
                    .expect("probe catalog entry must have capability id"),
                arguments: invocation.arguments.clone(),
            })),
            CatalogKind::Mutation => Ok(DecodedToolCall::Action(AgentCommand::Mutation {
                call_id: invocation.id.clone(),
                capability_id: entry
                    .capability_id
                    .clone()
                    .expect("mutation catalog entry must have capability id"),
                arguments: required_value(&invocation.arguments, "arguments")?.clone(),
                reason: required_string(&invocation.arguments, "reason")?,
            })),
            CatalogKind::BeginExperiment => {
                Ok(DecodedToolCall::Action(AgentCommand::BeginExperiment {
                    call_id: invocation.id.clone(),
                    intent: serde_json::from_value::<EvaluationIntentSpec>(
                        invocation.arguments.clone(),
                    )
                    .map_err(|error| {
                        DispatchError::new(format!("invalid evaluation intent: {error}"))
                    })?,
                }))
            }
            CatalogKind::RequestCommit => {
                Ok(DecodedToolCall::Action(AgentCommand::RequestCommit {
                    call_id: invocation.id.clone(),
                    change_ids: parse_change_ids(&invocation.arguments)?,
                    reason: required_string(&invocation.arguments, "reason")?,
                }))
            }
            CatalogKind::Abort => Ok(DecodedToolCall::Action(AgentCommand::Abort {
                call_id: invocation.id.clone(),
                reason: required_string(&invocation.arguments, "reason")?,
            })),
            CatalogKind::LoadSkill => Ok(DecodedToolCall::Context(SkillCommand::LoadSkill {
                name: required_string(&invocation.arguments, "name")?,
            })),
            CatalogKind::LoadSkillReference => {
                Ok(DecodedToolCall::Context(SkillCommand::LoadReference {
                    skill: required_string(&invocation.arguments, "skill")?,
                    path: required_string(&invocation.arguments, "path")?,
                }))
            }
        }
    }
}

fn required_value<'a>(arguments: &'a Value, field: &str) -> Result<&'a Value, DispatchError> {
    arguments
        .get(field)
        .ok_or_else(|| DispatchError::new(format!("{field} is required")))
}

fn required_string(arguments: &Value, field: &str) -> Result<String, DispatchError> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| DispatchError::new(format!("{field} must be a non-empty string")))
}

fn parse_change_ids(arguments: &Value) -> Result<Vec<ChangeId>, DispatchError> {
    let values = arguments
        .get("change_ids")
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
        .ok_or_else(|| DispatchError::new("change_ids must be a non-empty array"))?;
    let mut seen = BTreeSet::new();
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        let raw = value
            .as_str()
            .ok_or_else(|| DispatchError::new("change_ids entries must be strings"))?;
        let id = ChangeId::new(raw).map_err(DispatchError::new)?;
        if !seen.insert(id.clone()) {
            return Err(DispatchError::new(format!(
                "change_ids contains duplicate '{id}'"
            )));
        }
        ids.push(id);
    }
    Ok(ids)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchError {
    message: String,
}

impl DispatchError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DispatchError {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_commit_rejects_duplicate_change_ids() {
        let snapshot = crate::capability::CapabilityRegistry::new(Default::default()).snapshot();
        let catalog = ToolCatalog::from_snapshot(&snapshot);
        let invocation = AgentToolInvocation {
            id: "call-1".to_string(),
            name: "request_commit".to_string(),
            arguments: json!({
                "change_ids": ["change-1", "change-1"],
                "reason": "candidate is ready"
            }),
        };

        assert!(ToolDispatcher::new(&catalog).decode(&invocation).is_err());
    }

    #[test]
    fn begin_experiment_requires_a_structured_contract() {
        let snapshot = crate::capability::CapabilityRegistry::new(Default::default()).snapshot();
        let catalog = ToolCatalog::from_snapshot(&snapshot);
        let invocation = AgentToolInvocation {
            id: "call-2".to_string(),
            name: "begin_experiment".to_string(),
            arguments: json!({
                "objective": "reduce scheduling latency",
                "evaluation_contract": "not-an-object"
            }),
        };

        assert!(ToolDispatcher::new(&catalog).decode(&invocation).is_err());
    }

    #[test]
    fn skill_loader_decodes_as_a_context_command() {
        let snapshot = crate::capability::CapabilityRegistry::new(Default::default()).snapshot();
        let catalog = ToolCatalog::from_snapshots(&snapshot, true);
        let invocation = AgentToolInvocation {
            id: "call-skill".to_string(),
            name: "load_skill".to_string(),
            arguments: json!({"name": "scheduler-guide"}),
        };

        assert_eq!(
            ToolDispatcher::new(&catalog).decode(&invocation).unwrap(),
            DecodedToolCall::Context(SkillCommand::LoadSkill {
                name: "scheduler-guide".into()
            })
        );
    }
}
