use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::domain::{content_digest, ChangeId, Digest};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Candidate {
    change_ids: Vec<ChangeId>,
    digest: Digest,
}

#[derive(Deserialize)]
struct UncheckedCandidate {
    change_ids: Vec<ChangeId>,
    digest: Digest,
}

impl<'de> Deserialize<'de> for Candidate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let unchecked = UncheckedCandidate::deserialize(deserializer)?;
        let candidate = Self::new(unchecked.change_ids).map_err(serde::de::Error::custom)?;
        if candidate.digest != unchecked.digest {
            return Err(serde::de::Error::custom(
                "candidate digest does not match change ids",
            ));
        }
        Ok(candidate)
    }
}

impl Candidate {
    pub fn new(change_ids: Vec<ChangeId>) -> Result<Self, String> {
        if change_ids.is_empty() {
            return Err("candidate must contain at least one change".to_string());
        }
        let unique = change_ids.iter().collect::<BTreeSet<_>>();
        if unique.len() != change_ids.len() {
            return Err("candidate contains duplicate change ids".to_string());
        }
        let digest = content_digest(&change_ids)?;
        Ok(Self { change_ids, digest })
    }

    pub fn change_ids(&self) -> &[ChangeId] {
        &self.change_ids
    }

    pub fn contains(&self, change_id: &ChangeId) -> bool {
        self.change_ids.contains(change_id)
    }

    pub fn digest(&self) -> &Digest {
        &self.digest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_rejects_empty_and_duplicate_change_sets() {
        assert!(Candidate::new(Vec::new()).is_err());
        let change = ChangeId::new("change-1").unwrap();
        assert!(Candidate::new(vec![change.clone(), change]).is_err());
    }

    #[test]
    fn deserialization_rejects_a_forged_digest() {
        let result = serde_json::from_value::<Candidate>(serde_json::json!({
            "change_ids": ["change-1"],
            "digest": "sha256:forged"
        }));
        assert!(result.is_err());
    }
}
