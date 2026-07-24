use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::skill::model::{SkillPackage, SkillSnapshot};

#[derive(Clone, Debug)]
pub(crate) struct SkillCatalogView {
    pub entries: Vec<Value>,
    pub truncated: bool,
    pub omitted: usize,
}

impl SkillCatalogView {
    pub(crate) fn build(
        snapshot: &SkillSnapshot,
        explicit: &BTreeSet<String>,
        max_chars: usize,
    ) -> Result<Self, String> {
        let mut candidates = Vec::new();
        for name in explicit {
            if let Some(package) = snapshot.get(name) {
                candidates.push((package.as_ref(), true));
            }
        }
        candidates.extend(
            snapshot
                .iter()
                .filter(|package| {
                    package.allow_implicit_invocation && !explicit.contains(&package.meta.name)
                })
                .map(|package| (package.as_ref(), false)),
        );
        let candidate_count = candidates.len();
        let mut description_limit = 1_024;
        let mut entries = render(&candidates, description_limit);
        for limit in [512, 256, 128, 64] {
            if encoded_chars(&entries)? <= max_chars {
                break;
            }
            description_limit = limit;
            entries = render(&candidates, description_limit);
        }
        while encoded_chars(&entries)? > max_chars {
            let Some(index) = candidates.iter().rposition(|(_, is_explicit)| !is_explicit) else {
                return Err(format!(
                    "explicit skill metadata exceeds the configured {max_chars} character catalog limit"
                ));
            };
            candidates.remove(index);
            entries = render(&candidates, description_limit);
        }
        let omitted = candidate_count.saturating_sub(candidates.len());
        Ok(Self {
            entries,
            truncated: description_limit < 1_024 || omitted > 0,
            omitted,
        })
    }
}

fn render(candidates: &[(&SkillPackage, bool)], description_limit: usize) -> Vec<Value> {
    candidates
        .iter()
        .map(|(package, _)| {
            json!({
                "name": package.meta.name,
                "description": truncate_chars(&package.meta.description, description_limit),
                "path": package.logical_path(),
            })
        })
        .collect()
}

fn encoded_chars(entries: &[Value]) -> Result<usize, String> {
    serde_json::to_string(entries)
        .map(|encoded| encoded.chars().count())
        .map_err(|error| format!("failed to encode skill catalog: {error}"))
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    if limit <= 3 {
        return value.chars().take(limit).collect();
    }
    let mut truncated = value.chars().take(limit - 3).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::domain::content_digest;
    use crate::skill::model::{SkillMeta, SkillPackage};

    #[test]
    fn catalog_omits_explicitly_disabled_skills_and_honors_budget() {
        let snapshot = snapshot(vec![
            package("alpha", &"a".repeat(800), true),
            package("beta", &"b".repeat(800), true),
            package("private", "explicit only", false),
        ]);
        let view = SkillCatalogView::build(&snapshot, &BTreeSet::new(), 700).unwrap();
        assert!(view.truncated);
        assert!(view.entries.iter().all(|entry| entry["name"] != "private"));
        assert!(encoded_chars(&view.entries).unwrap() <= 700);
    }

    #[test]
    fn explicitly_requested_skill_is_included_even_when_implicit_is_disabled() {
        let snapshot = snapshot(vec![package("private", "explicit only", false)]);
        let explicit = BTreeSet::from(["private".to_string()]);
        let view = SkillCatalogView::build(&snapshot, &explicit, 8_000).unwrap();
        assert_eq!(view.entries[0]["name"], "private");
    }

    fn snapshot(packages: Vec<SkillPackage>) -> SkillSnapshot {
        SkillSnapshot::new(
            packages
                .into_iter()
                .map(|package| (package.meta.name.clone(), Arc::new(package)))
                .collect(),
        )
        .unwrap()
    }

    fn package(name: &str, description: &str, implicit: bool) -> SkillPackage {
        SkillPackage {
            meta: SkillMeta {
                name: name.to_string(),
                description: description.to_string(),
                ..SkillMeta::default()
            },
            source_path: PathBuf::from(format!("/{name}/SKILL.md")),
            instructions: Arc::from("instructions"),
            references: BTreeMap::new(),
            allow_implicit_invocation: implicit,
            digest: content_digest(&name).unwrap(),
        }
    }
}
