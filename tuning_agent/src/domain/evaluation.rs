use serde::{Deserialize, Serialize};

use crate::domain::{Digest, EpisodeId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationIntentPin {
    episode_id: EpisodeId,
    intent_digest: Digest,
    contract_digest: Digest,
}

impl EvaluationIntentPin {
    pub(crate) fn new(
        episode_id: EpisodeId,
        intent_digest: Digest,
        contract_digest: Digest,
    ) -> Self {
        Self {
            episode_id,
            intent_digest,
            contract_digest,
        }
    }

    pub fn episode_id(&self) -> EpisodeId {
        self.episode_id
    }

    pub fn intent_digest(&self) -> &Digest {
        &self.intent_digest
    }

    pub fn contract_digest(&self) -> &Digest {
        &self.contract_digest
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn pin_round_trips_and_exposes_only_validated_fields() {
        let pin = EvaluationIntentPin::new(
            EpisodeId::new(17),
            Digest::new("sha256:intent").unwrap(),
            Digest::new("sha256:contract").unwrap(),
        );

        let encoded = serde_json::to_value(&pin).unwrap();
        let decoded: EvaluationIntentPin = serde_json::from_value(encoded).unwrap();

        assert_eq!(decoded, pin);
        assert_eq!(decoded.episode_id(), EpisodeId::new(17));
        assert_eq!(
            decoded.intent_digest(),
            &Digest::new("sha256:intent").unwrap()
        );
        assert_eq!(
            decoded.contract_digest(),
            &Digest::new("sha256:contract").unwrap()
        );
    }

    #[test]
    fn pin_deserialization_rejects_unknown_or_invalid_fields() {
        let mut unknown = json!({
            "episode_id": 17,
            "intent_digest": "sha256:intent",
            "contract_digest": "sha256:contract"
        });
        unknown["unexpected"] = json!(true);
        assert!(serde_json::from_value::<EvaluationIntentPin>(unknown).is_err());

        let invalid = json!({
            "episode_id": 17,
            "intent_digest": "",
            "contract_digest": "sha256:contract"
        });
        assert!(serde_json::from_value::<EvaluationIntentPin>(invalid).is_err());
    }
}
