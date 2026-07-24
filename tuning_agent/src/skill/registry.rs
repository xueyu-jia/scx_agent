use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::config::SkillConfig;
use crate::domain::content_digest;
use crate::skill::model::{ReferenceDocument, SkillPackage, SkillSnapshot};
use crate::skill::parser::{parse_skill, read_utf8_bounded};

pub(crate) struct SkillRegistry;

impl SkillRegistry {
    pub(crate) fn load(config: &SkillConfig) -> Result<SkillSnapshot, String> {
        if !config.enabled {
            return Ok(SkillSnapshot::empty());
        }
        let mut packages = BTreeMap::new();
        let mut registry_bytes = 0usize;
        for configured_root in &config.roots {
            let root = configured_root.canonicalize().map_err(|error| {
                format!(
                    "failed to resolve skills root '{}': {error}",
                    configured_root.display()
                )
            })?;
            if !root.is_dir() {
                return Err(format!(
                    "skills root '{}' is not a directory",
                    root.display()
                ));
            }
            for entry_path in sorted_entries(&root)? {
                let entry_metadata = fs::symlink_metadata(&entry_path).map_err(|error| {
                    format!(
                        "failed to inspect skill entry '{}': {error}",
                        entry_path.display()
                    )
                })?;
                if !entry_metadata.is_dir() && !entry_metadata.file_type().is_symlink() {
                    continue;
                }
                let package_root = entry_path.canonicalize().map_err(|error| {
                    format!(
                        "failed to resolve skill entry '{}': {error}",
                        entry_path.display()
                    )
                })?;
                ensure_contained(&package_root, &root, "skill directory")?;
                if !package_root.is_dir() {
                    continue;
                }
                let skill_path = entry_path.join("SKILL.md");
                match fs::symlink_metadata(&skill_path) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(format!(
                            "failed to inspect SKILL.md '{}': {error}",
                            skill_path.display()
                        ));
                    }
                }
                let canonical_skill_path = skill_path.canonicalize().map_err(|error| {
                    format!(
                        "failed to resolve SKILL.md '{}': {error}",
                        skill_path.display()
                    )
                })?;
                ensure_contained(&canonical_skill_path, &package_root, "SKILL.md")?;
                let directory_name = entry_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        format!(
                            "skill directory '{}' is not valid UTF-8",
                            entry_path.display()
                        )
                    })?;
                let parsed =
                    parse_skill(&canonical_skill_path, &package_root, directory_name, config)?;
                if packages.contains_key(&parsed.meta.name) {
                    return Err(format!("duplicate skill name '{}'", parsed.meta.name));
                }
                if packages.len() == config.max_skills {
                    return Err(format!(
                        "skill registry exceeds the configured limit of {} skills",
                        config.max_skills
                    ));
                }
                let references = load_references(&entry_path, &package_root, config)?;
                let package_bytes = parsed.instructions.len()
                    + references
                        .values()
                        .map(|reference| reference.byte_len)
                        .sum::<usize>();
                registry_bytes = registry_bytes
                    .checked_add(package_bytes)
                    .ok_or_else(|| "skill registry content size overflowed usize".to_string())?;
                if registry_bytes > config.max_registry_bytes {
                    return Err(format!(
                        "skill registry exceeds the configured {} byte limit",
                        config.max_registry_bytes
                    ));
                }
                let reference_digests = references
                    .values()
                    .map(|reference| (&reference.path, reference.digest.to_string()))
                    .collect::<Vec<_>>();
                let package_digest = content_digest(&json!({
                    "meta": &parsed.meta,
                    "instructions": parsed.instructions.as_ref(),
                    "references": reference_digests,
                    "allow_implicit_invocation": parsed.allow_implicit_invocation,
                }))?;
                let name = parsed.meta.name.clone();
                packages.insert(
                    name,
                    Arc::new(SkillPackage {
                        meta: parsed.meta,
                        source_path: canonical_skill_path,
                        instructions: parsed.instructions,
                        references,
                        allow_implicit_invocation: parsed.allow_implicit_invocation,
                        digest: package_digest,
                    }),
                );
            }
        }
        SkillSnapshot::new(packages)
    }
}

fn load_references(
    logical_package_root: &Path,
    canonical_package_root: &Path,
    config: &SkillConfig,
) -> Result<BTreeMap<String, ReferenceDocument>, String> {
    let logical_root = logical_package_root.join("references");
    match fs::symlink_metadata(&logical_root) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect references directory '{}': {error}",
                logical_root.display()
            ));
        }
    }
    let canonical_root = logical_root.canonicalize().map_err(|error| {
        format!(
            "failed to resolve references directory '{}': {error}",
            logical_root.display()
        )
    })?;
    ensure_contained(
        &canonical_root,
        canonical_package_root,
        "references directory",
    )?;
    if !canonical_root.is_dir() {
        return Err(format!(
            "references path '{}' is not a directory",
            logical_root.display()
        ));
    }
    let mut references = BTreeMap::new();
    let mut visited_directories = BTreeSet::new();
    visit_references(
        &logical_root,
        Path::new("references"),
        canonical_package_root,
        config,
        &mut visited_directories,
        &mut references,
    )?;
    Ok(references)
}

fn visit_references(
    logical_directory: &Path,
    relative_directory: &Path,
    canonical_package_root: &Path,
    config: &SkillConfig,
    visited_directories: &mut BTreeSet<PathBuf>,
    references: &mut BTreeMap<String, ReferenceDocument>,
) -> Result<(), String> {
    let canonical_directory = logical_directory.canonicalize().map_err(|error| {
        format!(
            "failed to resolve reference directory '{}': {error}",
            logical_directory.display()
        )
    })?;
    ensure_contained(
        &canonical_directory,
        canonical_package_root,
        "reference directory",
    )?;
    if !visited_directories.insert(canonical_directory) {
        return Err(format!(
            "reference directory '{}' creates a symlink cycle or duplicate traversal",
            logical_directory.display()
        ));
    }
    for entry_path in sorted_entries(logical_directory)? {
        let file_name = entry_path
            .file_name()
            .ok_or_else(|| format!("reference path '{}' has no file name", entry_path.display()))?;
        let relative_path = relative_directory.join(file_name);
        let canonical_path = entry_path.canonicalize().map_err(|error| {
            format!(
                "failed to resolve reference '{}': {error}",
                entry_path.display()
            )
        })?;
        ensure_contained(&canonical_path, canonical_package_root, "reference")?;
        let metadata = fs::metadata(&entry_path).map_err(|error| {
            format!(
                "failed to inspect reference '{}': {error}",
                entry_path.display()
            )
        })?;
        if metadata.is_dir() {
            visit_references(
                &entry_path,
                &relative_path,
                canonical_package_root,
                config,
                visited_directories,
                references,
            )?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!(
                "reference '{}' is not a regular file",
                entry_path.display()
            ));
        }
        if references.len() == config.max_references_per_skill {
            return Err(format!(
                "skill references exceed the configured limit of {} files",
                config.max_references_per_skill
            ));
        }
        let logical_path = relative_path.to_str().ok_or_else(|| {
            format!(
                "reference path '{}' is not valid UTF-8",
                relative_path.display()
            )
        })?;
        let content = read_utf8_bounded(
            &canonical_path,
            config.max_reference_bytes,
            "skill reference",
        )?;
        let digest = content_digest(&content)?;
        references.insert(
            logical_path.to_string(),
            ReferenceDocument {
                path: logical_path.to_string(),
                byte_len: content.len(),
                content: Arc::from(content),
                digest,
            },
        );
    }
    Ok(())
}

fn ensure_contained(path: &Path, root: &Path, label: &str) -> Result<(), String> {
    if !path.starts_with(root) {
        return Err(format!(
            "{label} '{}' resolves outside trusted root '{}'",
            path.display(),
            root.display()
        ));
    }
    Ok(())
}

fn sorted_entries(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "failed to read directory '{}': {error}",
                directory.display()
            )
        })?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| format!("failed to read directory entry: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn registry_loads_standard_skill_and_utf8_references() {
        let root = unique_root("load");
        let skill = root.join("scheduler-guide");
        fs::create_dir_all(skill.join("references/nested")).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: scheduler-guide\ndescription: Diagnose scheduler pressure. Use for PSI CPU and run-queue events.\nlicense: Apache-2.0\ncompatibility: Requires Linux observation capabilities\nmetadata:\n  author: performance-team\n  version: \"1.0\"\nallowed-tools: Read\n---\nRead references/signals.md when needed.\n",
        )
        .unwrap();
        fs::write(skill.join("references/signals.md"), "# Signals\nPSI CPU\n").unwrap();
        fs::write(skill.join("references/nested/details.txt"), "details\n").unwrap();
        fs::create_dir_all(skill.join("scripts")).unwrap();
        fs::write(skill.join("scripts/ignored.sh"), "exit 1\n").unwrap();
        let snapshot = SkillRegistry::load(&config(&root)).unwrap();

        assert_eq!(snapshot.len(), 1);
        let package = snapshot.get("scheduler-guide").unwrap();
        assert_eq!(package.meta.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(package.meta.metadata["version"], "1.0");
        assert_eq!(package.meta.allowed_tools.as_deref(), Some("Read"));
        assert_eq!(package.references.len(), 2);
        assert!(package.references.contains_key("references/signals.md"));
        assert!(package
            .references
            .contains_key("references/nested/details.txt"));
        assert!(!package.references.contains_key("scripts/ignored.sh"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn openai_policy_disables_implicit_invocation() {
        let root = unique_root("policy");
        let skill = root.join("explicit-only");
        fs::create_dir_all(skill.join("agents")).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: explicit-only\ndescription: Run only when an activation explicitly requests this skill.\n---\nInstructions.\n",
        )
        .unwrap();
        fs::write(
            skill.join("agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: false\n",
        )
        .unwrap();

        let snapshot = SkillRegistry::load(&config(&root)).unwrap();
        assert!(
            !snapshot
                .get("explicit-only")
                .unwrap()
                .allow_implicit_invocation
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn registry_rejects_directory_name_mismatch_and_binary_reference() {
        let root = unique_root("invalid");
        let skill = root.join("wrong-directory");
        fs::create_dir_all(skill.join("references")).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: another-name\ndescription: Invalid mismatch used by a parser test.\n---\n",
        )
        .unwrap();
        assert!(SkillRegistry::load(&config(&root))
            .unwrap_err()
            .contains("must match"));

        fs::write(
            skill.join("SKILL.md"),
            "---\nname: wrong-directory\ndescription: Valid metadata used by a binary reference test.\n---\n",
        )
        .unwrap();
        fs::write(skill.join("references/binary"), [0xff, 0xfe]).unwrap();
        assert!(SkillRegistry::load(&config(&root))
            .unwrap_err()
            .contains("UTF-8"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reference_only_mode_rejects_openai_tool_dependencies() {
        let root = unique_root("dependency");
        let skill = root.join("dependent-skill");
        fs::create_dir_all(skill.join("agents")).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: dependent-skill\ndescription: Requires an unsupported tool dependency for this validation test.\n---\n",
        )
        .unwrap();
        fs::write(
            skill.join("agents/openai.yaml"),
            "dependencies:\n  tools:\n    - type: mcp\n      value: example\n",
        )
        .unwrap();

        assert!(SkillRegistry::load(&config(&root))
            .unwrap_err()
            .contains("tool dependencies"));
        let _ = fs::remove_dir_all(root);
    }

    fn config(root: &Path) -> SkillConfig {
        SkillConfig {
            enabled: true,
            roots: vec![root.to_path_buf()],
            ..SkillConfig::default()
        }
    }

    fn unique_root(label: &str) -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-skill-{label}-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }
}
