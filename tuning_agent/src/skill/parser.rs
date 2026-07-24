use std::fs;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;

use crate::config::SkillConfig;
use crate::skill::model::SkillMeta;

pub(crate) struct ParsedSkill {
    pub meta: SkillMeta,
    pub instructions: Arc<str>,
    pub allow_implicit_invocation: bool,
}

pub(crate) fn parse_skill(
    skill_path: &Path,
    package_root: &Path,
    directory_name: &str,
    config: &SkillConfig,
) -> Result<ParsedSkill, String> {
    let content = read_utf8_bounded(skill_path, config.max_skill_bytes, "SKILL.md")?;
    let frontmatter = split_frontmatter(&content)
        .map_err(|error| format!("failed to parse skill '{}': {error}", skill_path.display()))?;
    let meta: SkillMeta = serde_yaml::from_str(frontmatter).map_err(|error| {
        format!(
            "failed to parse skill metadata '{}': {error}",
            skill_path.display()
        )
    })?;
    meta.validate(directory_name)?;
    let allow_implicit_invocation = parse_openai_metadata(
        &skill_path
            .parent()
            .expect("SKILL.md must have a parent")
            .join("agents/openai.yaml"),
        config.max_skill_bytes,
        &meta.name,
        package_root,
    )?;
    Ok(ParsedSkill {
        meta,
        instructions: Arc::from(content),
        allow_implicit_invocation,
    })
}

pub(crate) fn read_utf8_bounded(
    path: &Path,
    max_bytes: usize,
    label: &str,
) -> Result<String, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("failed to inspect {label} '{}': {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{label} '{}' is not a regular file",
            path.display()
        ));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(format!(
            "{label} '{}' exceeds the {max_bytes} byte limit",
            path.display()
        ));
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("failed to read {label} '{}': {error}", path.display()))?;
    if bytes.contains(&0) {
        return Err(format!("{label} '{}' contains a NUL byte", path.display()));
    }
    String::from_utf8(bytes).map_err(|_| format!("{label} '{}' is not valid UTF-8", path.display()))
}

fn split_frontmatter(content: &str) -> Result<&str, String> {
    let mut lines = content.split_inclusive('\n');
    let first = lines
        .next()
        .ok_or_else(|| "SKILL.md is empty".to_string())?;
    if trim_line_ending(first) != "---" {
        return Err("SKILL.md must begin with YAML frontmatter delimiter '---'".to_string());
    }
    let frontmatter_start = first.len();
    let mut offset = frontmatter_start;
    for line in lines {
        if trim_line_ending(line) == "---" {
            return Ok(&content[frontmatter_start..offset]);
        }
        offset += line.len();
    }
    Err("SKILL.md has no closing YAML frontmatter delimiter".to_string())
}

fn trim_line_ending(line: &str) -> &str {
    line.strip_suffix('\n')
        .unwrap_or(line)
        .strip_suffix('\r')
        .unwrap_or_else(|| line.strip_suffix('\n').unwrap_or(line))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OpenAiMetadata {
    interface: Option<serde_yaml::Value>,
    dependencies: OpenAiDependencies,
    policy: OpenAiPolicy,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OpenAiDependencies {
    tools: Vec<serde_yaml::Value>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct OpenAiPolicy {
    allow_implicit_invocation: bool,
}

impl Default for OpenAiPolicy {
    fn default() -> Self {
        Self {
            allow_implicit_invocation: true,
        }
    }
}

fn parse_openai_metadata(
    path: &Path,
    max_bytes: usize,
    skill_name: &str,
    package_root: &Path,
) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(format!(
                "failed to inspect OpenAI metadata for skill '{skill_name}': {error}"
            ));
        }
    }
    let canonical_path = path.canonicalize().map_err(|error| {
        format!("failed to resolve OpenAI metadata for skill '{skill_name}': {error}")
    })?;
    if !canonical_path.starts_with(package_root) {
        return Err(format!(
            "agents/openai.yaml for skill '{skill_name}' resolves outside its skill directory"
        ));
    }
    let content = read_utf8_bounded(
        &canonical_path,
        max_bytes.min(64 * 1024),
        "agents/openai.yaml",
    )?;
    let metadata: OpenAiMetadata = serde_yaml::from_str(&content).map_err(|error| {
        format!("failed to parse OpenAI metadata for skill '{skill_name}': {error}")
    })?;
    let _ = metadata.interface;
    if !metadata.dependencies.tools.is_empty() {
        return Err(format!(
            "skill '{skill_name}' declares tool dependencies, which are unsupported in reference-only skill mode"
        ));
    }
    Ok(metadata.policy.allow_implicit_invocation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_supports_lf_and_crlf() {
        assert_eq!(
            split_frontmatter("---\nname: test\n---\nbody").unwrap(),
            "name: test\n"
        );
        assert_eq!(
            split_frontmatter("---\r\nname: test\r\n---\r\nbody").unwrap(),
            "name: test\r\n"
        );
    }

    #[test]
    fn frontmatter_requires_delimiters() {
        assert!(split_frontmatter("name: test")
            .unwrap_err()
            .contains("begin"));
        assert!(split_frontmatter("---\nname: test")
            .unwrap_err()
            .contains("closing"));
    }
}
