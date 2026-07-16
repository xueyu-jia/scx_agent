use serde::{Deserialize, Serialize};

use crate::domain::{content_digest, Digest, EvaluationIntentPin};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommitAuthorization {
    intent_pin: EvaluationIntentPin,
    candidate_digest: Digest,
    decision_digest: Digest,
    evaluation_evidence_digest: Digest,
    authorization_digest: Digest,
}

#[derive(Deserialize)]
struct UncheckedCommitAuthorization {
    intent_pin: EvaluationIntentPin,
    candidate_digest: Digest,
    decision_digest: Digest,
    evaluation_evidence_digest: Digest,
    authorization_digest: Digest,
}

#[derive(Serialize)]
struct AuthorizationPayload<'a> {
    intent_pin: &'a EvaluationIntentPin,
    candidate_digest: &'a Digest,
    decision_digest: &'a Digest,
    evaluation_evidence_digest: &'a Digest,
}

impl<'de> Deserialize<'de> for CommitAuthorization {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let unchecked = UncheckedCommitAuthorization::deserialize(deserializer)?;
        let authorization = Self::issue(
            unchecked.intent_pin,
            unchecked.candidate_digest,
            unchecked.decision_digest,
            unchecked.evaluation_evidence_digest,
        )
        .map_err(serde::de::Error::custom)?;
        if authorization.authorization_digest != unchecked.authorization_digest {
            return Err(serde::de::Error::custom(
                "commit authorization digest does not match its evidence digests",
            ));
        }
        Ok(authorization)
    }
}

impl CommitAuthorization {
    pub(crate) fn issue(
        intent_pin: EvaluationIntentPin,
        candidate_digest: Digest,
        decision_digest: Digest,
        evaluation_evidence_digest: Digest,
    ) -> Result<Self, String> {
        let authorization_digest = content_digest(&AuthorizationPayload {
            intent_pin: &intent_pin,
            candidate_digest: &candidate_digest,
            decision_digest: &decision_digest,
            evaluation_evidence_digest: &evaluation_evidence_digest,
        })?;
        Ok(Self {
            intent_pin,
            candidate_digest,
            decision_digest,
            evaluation_evidence_digest,
            authorization_digest,
        })
    }

    pub fn intent_pin(&self) -> &EvaluationIntentPin {
        &self.intent_pin
    }

    pub fn contract_digest(&self) -> &Digest {
        self.intent_pin.contract_digest()
    }

    pub fn candidate_digest(&self) -> &Digest {
        &self.candidate_digest
    }

    pub fn decision_digest(&self) -> &Digest {
        &self.decision_digest
    }

    pub fn evaluation_evidence_digest(&self) -> &Digest {
        &self.evaluation_evidence_digest
    }

    pub fn authorization_digest(&self) -> &Digest {
        &self.authorization_digest
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::domain::EpisodeId;

    fn digest(value: &str) -> Digest {
        content_digest(&value).unwrap()
    }

    fn intent_pin() -> EvaluationIntentPin {
        EvaluationIntentPin::new(EpisodeId::new(17), digest("intent"), digest("contract"))
    }

    #[test]
    fn authorization_round_trips_and_rejects_tampered_evidence() {
        let authorization = CommitAuthorization::issue(
            intent_pin(),
            digest("candidate"),
            digest("decision"),
            digest("evidence"),
        )
        .unwrap();
        let encoded = serde_json::to_value(&authorization).unwrap();
        assert_eq!(
            serde_json::from_value::<CommitAuthorization>(encoded.clone()).unwrap(),
            authorization
        );

        let mut tampered = encoded;
        tampered["evaluation_evidence_digest"] = json!(digest("other evidence"));
        assert!(serde_json::from_value::<CommitAuthorization>(tampered).is_err());
    }

    #[test]
    fn authorization_digest_binds_the_evaluation_intent_pin() {
        let authorization = CommitAuthorization::issue(
            intent_pin(),
            digest("candidate"),
            digest("decision"),
            digest("evidence"),
        )
        .unwrap();
        let mut tampered = serde_json::to_value(authorization).unwrap();
        tampered["intent_pin"]["intent_digest"] = json!(digest("other intent"));

        assert!(serde_json::from_value::<CommitAuthorization>(tampered).is_err());
    }
}
