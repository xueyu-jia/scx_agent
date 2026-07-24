use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::agent::{AgentToolInvocation, AgentToolResult};
use crate::config::SkillConfig;
use crate::skill::catalog::SkillCatalogView;
use crate::skill::model::{SkillCommand, SkillPackage, SkillSnapshot};

pub(crate) struct SkillExecution {
    pub result: AgentToolResult,
    pub audit_event: &'static str,
    pub audit_data: Value,
}

pub(crate) struct SkillSession {
    snapshot: SkillSnapshot,
    explicit: BTreeSet<String>,
    loaded: BTreeSet<String>,
    loaded_references: BTreeSet<(String, String)>,
    max_loaded_skills: usize,
    max_reference_reads: usize,
    max_skill_rounds: usize,
    max_catalog_chars: usize,
}

impl SkillSession {
    pub(crate) fn new(
        snapshot: SkillSnapshot,
        config: &SkillConfig,
        requested: &[String],
    ) -> Result<Self, String> {
        let mut explicit = BTreeSet::new();
        for name in requested {
            if !explicit.insert(name.clone()) {
                return Err(format!("activation requested duplicate skill '{name}'"));
            }
            if snapshot.get(name).is_none() {
                return Err(format!("activation requested unavailable skill '{name}'"));
            }
        }
        if explicit.len() > config.max_loaded_skills {
            return Err(format!(
                "activation requested {} skills, exceeding the configured limit of {}",
                explicit.len(),
                config.max_loaded_skills
            ));
        }
        Ok(Self {
            snapshot,
            loaded: explicit.clone(),
            explicit,
            loaded_references: BTreeSet::new(),
            max_loaded_skills: config.max_loaded_skills,
            max_reference_reads: config.max_reference_reads,
            max_skill_rounds: config.max_skill_rounds,
            max_catalog_chars: config.max_catalog_chars,
        })
    }

    pub(crate) fn has_available_skills(&self) -> bool {
        !self.explicit.is_empty()
            || self
                .snapshot
                .iter()
                .any(|package| package.allow_implicit_invocation)
    }

    pub(crate) fn max_skill_rounds(&self) -> usize {
        self.max_skill_rounds
    }

    pub(crate) fn registry_digest(&self) -> String {
        self.snapshot.digest().to_string()
    }

    pub(crate) fn skill_count(&self) -> usize {
        self.snapshot.len()
    }

    pub(crate) fn catalog(&self) -> Result<SkillCatalogView, String> {
        SkillCatalogView::build(&self.snapshot, &self.explicit, self.max_catalog_chars)
    }

    pub(crate) fn explicit_context(&self) -> Vec<Value> {
        self.explicit
            .iter()
            .filter_map(|name| self.snapshot.get(name))
            .map(|package| skill_payload(package, true))
            .collect()
    }

    pub(crate) fn explicit_audit_data(&self) -> Vec<Value> {
        self.explicit
            .iter()
            .filter_map(|name| self.snapshot.get(name))
            .map(|package| {
                json!({
                    "skill": package.meta.name,
                    "path": package.logical_path(),
                    "source_path": package.source_path.display().to_string(),
                    "digest": package.digest.to_string(),
                    "invocation": "explicit",
                    "instruction_bytes": package.instructions.len(),
                    "reference_count": package.references.len(),
                })
            })
            .collect()
    }

    pub(crate) fn execute(
        &mut self,
        command: SkillCommand,
        invocation: &AgentToolInvocation,
    ) -> SkillExecution {
        match command {
            SkillCommand::LoadSkill { name } => self.load_skill(name, invocation),
            SkillCommand::LoadReference { skill, path } => {
                self.load_reference(skill, path, invocation)
            }
        }
    }

    fn load_skill(&mut self, name: String, invocation: &AgentToolInvocation) -> SkillExecution {
        let Some(package) = self.snapshot.get(&name) else {
            return failure(
                invocation,
                "skill_load_failed",
                json!({"skill": name}),
                format!("skill '{name}' is unavailable"),
            );
        };
        if self.loaded.contains(&name) {
            return SkillExecution {
                result: AgentToolResult::success(
                    invocation,
                    json!({
                        "skill": name,
                        "already_loaded": true,
                        "digest": package.digest.to_string(),
                    }),
                ),
                audit_event: "skill_loaded",
                audit_data: json!({
                    "skill": name,
                    "digest": package.digest.to_string(),
                    "invocation": if self.explicit.contains(&name) { "explicit" } else { "implicit" },
                    "already_loaded": true,
                }),
            };
        }
        if !package.allow_implicit_invocation {
            return failure(
                invocation,
                "skill_load_failed",
                json!({"skill": name}),
                format!("skill '{name}' requires explicit invocation"),
            );
        }
        if self.loaded.len() == self.max_loaded_skills {
            return failure(
                invocation,
                "skill_load_failed",
                json!({"skill": name}),
                format!(
                    "loaded skill limit of {} has been reached",
                    self.max_loaded_skills
                ),
            );
        }
        self.loaded.insert(name.clone());
        SkillExecution {
            result: AgentToolResult::success(invocation, skill_payload(package, false)),
            audit_event: "skill_loaded",
            audit_data: json!({
                "skill": name,
                "path": package.logical_path(),
                "source_path": package.source_path.display().to_string(),
                "digest": package.digest.to_string(),
                "invocation": "implicit",
                "already_loaded": false,
                "instruction_bytes": package.instructions.len(),
                "reference_count": package.references.len(),
            }),
        }
    }

    fn load_reference(
        &mut self,
        skill: String,
        path: String,
        invocation: &AgentToolInvocation,
    ) -> SkillExecution {
        if !self.loaded.contains(&skill) {
            return failure(
                invocation,
                "skill_reference_load_failed",
                json!({"skill": skill, "path": path}),
                format!("skill '{skill}' must be loaded before its references"),
            );
        }
        let Some(package) = self.snapshot.get(&skill) else {
            return failure(
                invocation,
                "skill_reference_load_failed",
                json!({"skill": skill, "path": path}),
                format!("skill '{skill}' is unavailable"),
            );
        };
        let Some(reference) = package.references.get(&path) else {
            return failure(
                invocation,
                "skill_reference_load_failed",
                json!({"skill": skill, "path": path}),
                format!("reference '{path}' is unavailable for skill '{skill}'"),
            );
        };
        let key = (skill.clone(), path.clone());
        if self.loaded_references.contains(&key) {
            return SkillExecution {
                result: AgentToolResult::success(
                    invocation,
                    json!({
                        "skill": skill,
                        "path": path,
                        "already_loaded": true,
                        "digest": reference.digest.to_string(),
                    }),
                ),
                audit_event: "skill_reference_loaded",
                audit_data: json!({
                    "skill": skill,
                    "path": path,
                    "digest": reference.digest.to_string(),
                    "already_loaded": true,
                }),
            };
        }
        if self.loaded_references.len() == self.max_reference_reads {
            return failure(
                invocation,
                "skill_reference_load_failed",
                json!({"skill": skill, "path": path}),
                format!(
                    "skill reference read limit of {} has been reached",
                    self.max_reference_reads
                ),
            );
        }
        self.loaded_references.insert(key);
        SkillExecution {
            result: AgentToolResult::success(
                invocation,
                json!({
                    "skill": skill,
                    "path": path,
                    "already_loaded": false,
                    "digest": reference.digest.to_string(),
                    "byte_len": reference.byte_len,
                    "content": reference.content.as_ref(),
                }),
            ),
            audit_event: "skill_reference_loaded",
            audit_data: json!({
                "skill": skill,
                "path": path,
                "digest": reference.digest.to_string(),
                "byte_len": reference.byte_len,
                "already_loaded": false,
            }),
        }
    }
}

fn skill_payload(package: &SkillPackage, explicitly_loaded: bool) -> Value {
    json!({
        "skill": package.meta.name,
        "path": package.logical_path(),
        "digest": package.digest.to_string(),
        "explicitly_loaded": explicitly_loaded,
        "instructions": package.instructions.as_ref(),
        "references": package.references.values().map(|reference| json!({
            "path": reference.path,
            "byte_len": reference.byte_len,
            "digest": reference.digest.to_string(),
        })).collect::<Vec<_>>(),
    })
}

fn failure(
    invocation: &AgentToolInvocation,
    event: &'static str,
    mut audit_data: Value,
    error: String,
) -> SkillExecution {
    if let Some(data) = audit_data.as_object_mut() {
        data.insert("error".to_string(), Value::String(error.clone()));
    }
    SkillExecution {
        result: AgentToolResult::failure(invocation, error),
        audit_event: event,
        audit_data,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::domain::content_digest;
    use crate::skill::model::{ReferenceDocument, SkillMeta};

    #[test]
    fn reference_requires_loaded_skill_and_is_returned_once() {
        let snapshot = snapshot(false);
        let config = SkillConfig::default();
        let mut session = SkillSession::new(snapshot, &config, &[]).unwrap();
        let reference = invocation("ref", "load_skill_reference");
        let before = session.execute(
            SkillCommand::LoadReference {
                skill: "guide".into(),
                path: "references/signals.md".into(),
            },
            &reference,
        );
        assert!(!before.result.ok);

        let load = invocation("load", "load_skill");
        assert!(
            session
                .execute(
                    SkillCommand::LoadSkill {
                        name: "guide".into()
                    },
                    &load
                )
                .result
                .ok
        );
        let first = session.execute(
            SkillCommand::LoadReference {
                skill: "guide".into(),
                path: "references/signals.md".into(),
            },
            &reference,
        );
        assert_eq!(first.result.content["content"], "PSI signals");
        let repeated = session.execute(
            SkillCommand::LoadReference {
                skill: "guide".into(),
                path: "references/signals.md".into(),
            },
            &reference,
        );
        assert_eq!(repeated.result.content["already_loaded"], true);
        assert!(repeated.result.content.get("content").is_none());
    }

    #[test]
    fn implicit_disabled_skill_can_only_be_preloaded_explicitly() {
        let snapshot = snapshot(true);
        let config = SkillConfig::default();
        let invocation = invocation("load", "load_skill");
        let mut implicit = SkillSession::new(snapshot.clone(), &config, &[]).unwrap();
        assert!(
            !implicit
                .execute(
                    SkillCommand::LoadSkill {
                        name: "guide".into()
                    },
                    &invocation,
                )
                .result
                .ok
        );

        let explicit = SkillSession::new(snapshot, &config, &["guide".into()]).unwrap();
        assert_eq!(explicit.explicit_context().len(), 1);
    }

    fn snapshot(explicit_only: bool) -> SkillSnapshot {
        let reference_content: Arc<str> = Arc::from("PSI signals");
        let reference = ReferenceDocument {
            path: "references/signals.md".into(),
            byte_len: reference_content.len(),
            digest: content_digest(&reference_content.as_ref()).unwrap(),
            content: reference_content,
        };
        let package = SkillPackage {
            meta: SkillMeta {
                name: "guide".into(),
                description: "Guide scheduler diagnosis. Use for PSI events.".into(),
                ..SkillMeta::default()
            },
            source_path: PathBuf::from("/guide/SKILL.md"),
            instructions: Arc::from("instructions"),
            references: BTreeMap::from([(reference.path.clone(), reference)]),
            allow_implicit_invocation: !explicit_only,
            digest: content_digest(&"guide").unwrap(),
        };
        SkillSnapshot::new(BTreeMap::from([("guide".into(), Arc::new(package))])).unwrap()
    }

    fn invocation(id: &str, name: &str) -> AgentToolInvocation {
        AgentToolInvocation {
            id: id.into(),
            name: name.into(),
            arguments: json!({}),
        }
    }
}
