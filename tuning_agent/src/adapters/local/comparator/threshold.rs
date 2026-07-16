use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::capability::ComparisonPolicy;
use crate::domain::{
    CapabilityId, CapabilityKind, CapabilityMeta, ComparisonConclusion, ComparisonEvidence,
    ComparisonRequest, ConditionEvidence, Digest, EffectClass, EpisodePhase, ProviderClass,
    ProviderError, ProviderErrorKind, ProviderId, ProviderPin, ProviderVersion,
};
use crate::kernel::evaluation::{
    evaluate_metric_condition, ConditionOutcome, MetricCondition, MetricConditionEvidence,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThresholdComparisonSpec {
    pub conditions: Vec<MetricCondition>,
}

pub struct ThresholdComparisonPolicy {
    meta: CapabilityMeta,
}

impl ThresholdComparisonPolicy {
    pub fn new() -> Self {
        let provider = ProviderPin {
            provider_id: ProviderId::new("builtin.threshold-comparison")
                .expect("static provider id is valid"),
            provider_version: ProviderVersion::new("1").expect("static version is valid"),
            provider_class: ProviderClass::Builtin,
            manifest_digest: Digest::new("builtin-threshold-comparison-v1")
                .expect("static digest is valid"),
        };
        let mut meta = CapabilityMeta::new(
            CapabilityId::new("builtin/comparison.threshold.v1")
                .expect("static capability id is valid"),
            CapabilityKind::Comparison,
            EffectClass::PureComputation,
            provider,
            "Evaluate typed metric threshold conditions",
            json!({
                "type": "object",
                "required": ["conditions"],
                "additionalProperties": false,
                "properties": {
                    "conditions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 256
                    }
                }
            }),
            json!({
                "type": "object",
                "required": ["conclusion", "conditions"]
            }),
        )
        .with_allowed_phases([EpisodePhase::CommitPending]);
        meta.deterministic = true;
        meta.idempotent = true;
        Self { meta }
    }

    fn parse_specification(
        &self,
        specification: &serde_json::Value,
    ) -> Result<ThresholdComparisonSpec, ProviderError> {
        let spec: ThresholdComparisonSpec =
            serde_json::from_value(specification.clone()).map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("invalid threshold comparison specification: {error}"),
                )
            })?;
        if spec.conditions.is_empty() || spec.conditions.len() > 256 {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "threshold comparison requires between 1 and 256 conditions",
            ));
        }
        for condition in &spec.conditions {
            condition
                .validate()
                .map_err(|error| ProviderError::new(ProviderErrorKind::InvalidRequest, error))?;
        }
        Ok(spec)
    }
}

impl Default for ThresholdComparisonPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl ComparisonPolicy for ThresholdComparisonPolicy {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }

    fn validate_specification(
        &self,
        specification: &serde_json::Value,
    ) -> Result<(), ProviderError> {
        self.parse_specification(specification).map(|_| ())
    }

    fn compare(&self, request: &ComparisonRequest) -> Result<ComparisonEvidence, ProviderError> {
        let spec = self.parse_specification(&request.specification)?;
        let evaluated = spec
            .conditions
            .iter()
            .map(|condition| {
                evaluate_metric_condition(condition, &request.baseline, &request.candidate)
            })
            .collect::<Vec<_>>();

        // An explicit failure takes precedence over missing evidence. This preserves a
        // known regression even when another condition cannot be evaluated.
        let conclusion = if evaluated
            .iter()
            .any(|evidence| evidence.outcome == ConditionOutcome::Failed)
        {
            ComparisonConclusion::NotImproved
        } else if evaluated
            .iter()
            .any(|evidence| evidence.outcome == ConditionOutcome::Inconclusive)
        {
            ComparisonConclusion::Inconclusive
        } else {
            ComparisonConclusion::Improved
        };
        let conditions = evaluated.iter().map(domain_evidence).collect();

        Ok(ComparisonEvidence {
            conclusion,
            conditions,
            details: json!({
                "policy": self.meta.id.as_str(),
                "contract_id": request.contract_id.as_str(),
                "condition_count": evaluated.len(),
            }),
        })
    }
}

fn domain_evidence(evidence: &MetricConditionEvidence) -> ConditionEvidence {
    ConditionEvidence {
        name: evidence.condition.metric.clone(),
        passed: evidence.outcome == ConditionOutcome::Passed,
        details: json!({
            "operator": evidence.condition.op,
            "threshold": evidence.condition.value,
            "outcome": evidence.outcome,
            "baseline": evidence.baseline,
            "candidate": evidence.candidate,
            "reason": evidence.reason,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::domain::{
        ContractId, EpisodeId, InvocationContext, MetricBatch, MetricKind, MetricQuality,
        MetricValue, OperationId,
    };
    use crate::kernel::evaluation::MetricOperator;

    fn batch(metric: &str, value: f64) -> MetricBatch {
        MetricBatch {
            started_at_ns: 1,
            ended_at_ns: 2,
            quality: MetricQuality::Valid,
            workload_fingerprint: None,
            metrics: BTreeMap::from([(
                metric.to_string(),
                MetricValue {
                    value: json!(value),
                    unit: "ms".to_string(),
                    kind: MetricKind::Gauge,
                },
            )]),
            provenance: json!({}),
        }
    }

    fn request(specification: serde_json::Value) -> ComparisonRequest {
        ComparisonRequest {
            context: InvocationContext {
                episode_id: EpisodeId::new(1),
                operation_id: OperationId::new("compare").unwrap(),
            },
            contract_id: ContractId::new("contract-1").unwrap(),
            specification,
            baseline: batch("latency.p99", 100.0),
            candidate: batch("latency.p99", 80.0),
        }
    }

    #[test]
    fn all_thresholds_must_pass() {
        let policy = ThresholdComparisonPolicy::new();
        let evidence = policy
            .compare(&request(json!({
                "conditions": [{
                    "metric": "latency.p99",
                    "op": "decrease_percent_ge",
                    "value": 10.0
                }]
            })))
            .unwrap();

        assert_eq!(evidence.conclusion, ComparisonConclusion::Improved);
        assert!(evidence.conditions[0].passed);
    }

    #[test]
    fn missing_metric_is_inconclusive() {
        let policy = ThresholdComparisonPolicy::new();
        let evidence = policy
            .compare(&request(json!({
                "conditions": [{
                    "metric": "missing",
                    "op": "current_le",
                    "value": 1.0
                }]
            })))
            .unwrap();

        assert_eq!(evidence.conclusion, ComparisonConclusion::Inconclusive);
    }

    #[test]
    fn typed_spec_round_trips_operator_names() {
        let spec = ThresholdComparisonSpec {
            conditions: vec![MetricCondition::new(
                "latency.p99",
                MetricOperator::DecreasePercentGe,
                5.0,
            )],
        };

        assert_eq!(
            serde_json::to_value(spec).unwrap()["conditions"][0]["op"],
            "decrease_percent_ge"
        );
    }
}
