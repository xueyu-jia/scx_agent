use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::domain::{content_digest, Digest};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SkillMeta {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: BTreeMap<String, String>,
    #[serde(rename = "allowed-tools")]
    pub allowed_tools: Option<String>,
}

impl SkillMeta {
    pub(crate) fn validate(&self, directory_name: &str) -> Result<(), String> {
        validate_skill_name(&self.name)?;
        if self.name != directory_name {
            return Err(format!(
                "skill name '{}' must match parent directory '{directory_name}'",
                self.name
            ));
        }
        let description_chars = self.description.chars().count();
        if description_chars == 0 || description_chars > 1_024 {
            return Err(format!(
                "skill '{}' description must contain between 1 and 1024 characters",
                self.name
            ));
        }
        if self.description.chars().any(char::is_control) {
            return Err(format!(
                "skill '{}' description must not contain control characters",
                self.name
            ));
        }
        validate_optional_text(&self.name, "license", self.license.as_deref(), 4_096)?;
        validate_optional_text(
            &self.name,
            "compatibility",
            self.compatibility.as_deref(),
            500,
        )?;
        validate_optional_text(
            &self.name,
            "allowed-tools",
            self.allowed_tools.as_deref(),
            4_096,
        )?;
        for (key, value) in &self.metadata {
            if key.is_empty()
                || key.len() > 256
                || key.trim() != key
                || key.chars().any(char::is_control)
                || value.len() > 4_096
                || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "skill '{}' contains invalid metadata entry '{key}'",
                    self.name
                ));
            }
        }
        Ok(())
    }
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "skill name '{name}' must contain 1-64 lowercase ASCII letters, digits, or single hyphens"
        ));
    }
    Ok(())
}

fn validate_optional_text(
    skill: &str,
    field: &str,
    value: Option<&str>,
    max_chars: usize,
) -> Result<(), String> {
    if let Some(value) = value {
        let length = value.chars().count();
        if length == 0 || length > max_chars || value.chars().any(char::is_control) {
            return Err(format!(
                "skill '{skill}' field '{field}' must contain between 1 and {max_chars} non-control characters"
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct ReferenceDocument {
    pub path: String,
    pub content: Arc<str>,
    pub byte_len: usize,
    pub digest: Digest,
}

#[derive(Clone, Debug)]
pub(crate) struct SkillPackage {
    pub meta: SkillMeta,
    pub source_path: PathBuf,
    pub instructions: Arc<str>,
    pub references: BTreeMap<String, ReferenceDocument>,
    pub allow_implicit_invocation: bool,
    pub digest: Digest,
}

impl SkillPackage {
    pub(crate) fn logical_path(&self) -> String {
        format!("skill://{}/SKILL.md", self.meta.name)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SkillSnapshot {
    packages: Arc<BTreeMap<String, Arc<SkillPackage>>>,
    digest: Digest,
}

impl SkillSnapshot {
    pub(crate) fn empty() -> Self {
        Self {
            packages: Arc::new(BTreeMap::new()),
            digest: content_digest(&Vec::<String>::new())
                .expect("empty skill snapshot digest must be valid"),
        }
    }

    pub(crate) fn new(packages: BTreeMap<String, Arc<SkillPackage>>) -> Result<Self, String> {
        let digest_input = packages
            .iter()
            .map(|(name, package)| (name.clone(), package.digest.to_string()))
            .collect::<Vec<_>>();
        Ok(Self {
            packages: Arc::new(packages),
            digest: content_digest(&digest_input)?,
        })
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<SkillPackage>> {
        self.packages.get(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<SkillPackage>> {
        self.packages.values()
    }

    pub(crate) fn len(&self) -> usize {
        self.packages.len()
    }

    pub(crate) fn digest(&self) -> &Digest {
        &self.digest
    }
}

impl Default for SkillSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SkillCommand {
    LoadSkill { name: String },
    LoadReference { skill: String, path: String },
}
