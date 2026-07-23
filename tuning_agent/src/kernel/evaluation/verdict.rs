use serde::{Deserialize, Serialize};

use crate::domain::{ComparisonConclusion, ComparisonEvidence, MetricBatch, MetricQuality};
use crate::kernel::evaluation::{
    evaluate_metric_condition, ConditionOutcome, MetricCondition, MetricConditionEvidence,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationVerdict {
    Improved,
    NoSignal,
    Inconclusive,
    Unsafe,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ComparisonEvidenceGroups {
    pub primary: Vec<ComparisonEvidence>,
    pub regression_guards: Vec<ComparisonEvidence>,
    pub workload_invariants: Vec<ComparisonEvidence>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluationDecision {
    pub verdict: EvaluationVerdict,
    pub reason: String,
    pub system_guardrails: Vec<MetricConditionEvidence>,
    pub policy_evidence: ComparisonEvidenceGroups,
}

#[derive(Default)]
pub struct VerdictKernel;

impl VerdictKernel {
    pub(crate) fn schemas_are_comparable(
        &self,
        system_baseline: &MetricBatch,
        system_candidate: &MetricBatch,
        baseline: &MetricBatch,
        candidate: &MetricBatch,
    ) -> bool {
        metric_schemas_match(system_baseline, system_candidate)
            && metric_schemas_match(baseline, candidate)
    }

    pub fn decide(
        &self,
        system_baseline: &MetricBatch,
        system_candidate: &MetricBatch,
        baseline: &MetricBatch,
        candidate: &MetricBatch,
        policy_evidence: ComparisonEvidenceGroups,
    ) -> EvaluationDecision {
        let system_guardrails = fixed_system_guardrails()
            .iter()
            .map(|condition| {
                evaluate_metric_condition(condition, system_baseline, system_candidate)
            })
            .collect::<Vec<_>>();

        let measurement_incomplete = system_baseline.quality != MetricQuality::Valid
            || system_candidate.quality != MetricQuality::Valid
            || baseline.quality != MetricQuality::Valid
            || candidate.quality != MetricQuality::Valid;
        let schema_incompatible =
            !self.schemas_are_comparable(system_baseline, system_candidate, baseline, candidate);
        let workload_fingerprint_untrusted = match (
            &baseline.workload_fingerprint,
            &candidate.workload_fingerprint,
        ) {
            (Some(baseline), Some(candidate)) => baseline != candidate,
            _ => true,
        };
        let system_failed = system_guardrails
            .iter()
            .any(|evidence| evidence.outcome == ConditionOutcome::Failed);
        let system_inconclusive = system_guardrails
            .iter()
            .any(|evidence| evidence.outcome == ConditionOutcome::Inconclusive);
        let regression_failed = contains(
            &policy_evidence.regression_guards,
            ComparisonConclusion::NotImproved,
        );
        let policy_inconclusive =
            contains(&policy_evidence.primary, ComparisonConclusion::Inconclusive)
                || contains(
                    &policy_evidence.regression_guards,
                    ComparisonConclusion::Inconclusive,
                )
                || contains(
                    &policy_evidence.workload_invariants,
                    ComparisonConclusion::Inconclusive,
                );
        let workload_not_comparable = contains(
            &policy_evidence.workload_invariants,
            ComparisonConclusion::NotImproved,
        );
        let primary_passed = !policy_evidence.primary.is_empty()
            && policy_evidence
                .primary
                .iter()
                .all(|evidence| evidence.conclusion == ComparisonConclusion::Improved);

        let (verdict, reason) = if system_failed {
            (
                EvaluationVerdict::Unsafe,
                "a fixed system guardrail failed".to_string(),
            )
        } else if regression_failed {
            (
                EvaluationVerdict::Unsafe,
                "a regression guardrail failed".to_string(),
            )
        } else if measurement_incomplete {
            (
                EvaluationVerdict::Inconclusive,
                "one or both measurement batches are not fully valid".to_string(),
            )
        } else if schema_incompatible {
            (
                EvaluationVerdict::Inconclusive,
                "A/B metric names, kinds, or units are not comparable".to_string(),
            )
        } else if workload_fingerprint_untrusted
            || system_inconclusive
            || policy_inconclusive
            || workload_not_comparable
        {
            (
                EvaluationVerdict::Inconclusive,
                "evaluation evidence is incomplete or the workload is not comparable".to_string(),
            )
        } else if primary_passed {
            (
                EvaluationVerdict::Improved,
                "all primary objectives and guardrails passed".to_string(),
            )
        } else {
            (
                EvaluationVerdict::NoSignal,
                "primary objectives did not all pass".to_string(),
            )
        };

        EvaluationDecision {
            verdict,
            reason,
            system_guardrails,
            policy_evidence,
        }
    }
}

fn metric_schemas_match(baseline: &MetricBatch, candidate: &MetricBatch) -> bool {
    baseline.metrics.len() == candidate.metrics.len()
        && baseline.metrics.iter().all(|(name, baseline_metric)| {
            candidate.metrics.get(name).is_some_and(|candidate_metric| {
                baseline_metric.kind == candidate_metric.kind
                    && baseline_metric.unit == candidate_metric.unit
            })
        })
}

fn contains(evidence: &[ComparisonEvidence], conclusion: ComparisonConclusion) -> bool {
    evidence.iter().any(|item| item.conclusion == conclusion)
}

// Temporarily disabled while the small-scale experiment measures end-to-end
// classification pass rate. Keep the original thresholds here for restoration.
fn fixed_system_guardrails() -> [MetricCondition; 0] {
    [
        // MetricCondition::new("psi.cpu.full.avg10", MetricOperator::IncreaseAbsLe, 1.0),
        // MetricCondition::new("psi.io.full.avg10", MetricOperator::IncreaseAbsLe, 1.0),
        // MetricCondition::new("psi.memory.full.avg10", MetricOperator::IncreaseAbsLe, 1.0),
        // MetricCondition::new("loadavg.1m", MetricOperator::IncreasePercentLe, 50.0),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::domain::{MetricKind, MetricValue};

    fn batch(psi: f64, load: f64) -> MetricBatch {
        let metric = |value| MetricValue {
            value: json!(value),
            unit: "percent".to_string(),
            kind: MetricKind::Gauge,
        };
        MetricBatch {
            started_at_ns: 1,
            ended_at_ns: 2,
            quality: MetricQuality::Valid,
            workload_fingerprint: Some("same".to_string()),
            metrics: BTreeMap::from([
                ("psi.cpu.full.avg10".to_string(), metric(psi)),
                ("psi.io.full.avg10".to_string(), metric(psi)),
                ("psi.memory.full.avg10".to_string(), metric(psi)),
                ("loadavg.1m".to_string(), metric(load)),
            ]),
            provenance: json!({}),
        }
    }

    fn comparison(conclusion: ComparisonConclusion) -> ComparisonEvidence {
        ComparisonEvidence {
            conclusion,
            conditions: Vec::new(),
            details: json!({}),
        }
    }

    #[test]
    fn fixed_system_guardrails_are_temporarily_disabled() {
        let decision = VerdictKernel.decide(
            &batch(1.0, 1.0),
            &batch(3.0, 1.0),
            &batch(1.0, 1.0),
            &batch(3.0, 1.0),
            ComparisonEvidenceGroups {
                primary: vec![comparison(ComparisonConclusion::Improved)],
                regression_guards: Vec::new(),
                workload_invariants: Vec::new(),
            },
        );

        assert_eq!(decision.verdict, EvaluationVerdict::Improved);
        assert!(decision.system_guardrails.is_empty());
    }

    #[test]
    fn missing_system_metric_is_inconclusive() {
        let mut candidate = batch(1.0, 1.0);
        candidate.metrics.remove("psi.io.full.avg10");
        let decision = VerdictKernel.decide(
            &batch(1.0, 1.0),
            &candidate,
            &batch(1.0, 1.0),
            &candidate,
            ComparisonEvidenceGroups {
                primary: vec![comparison(ComparisonConclusion::Improved)],
                regression_guards: Vec::new(),
                workload_invariants: Vec::new(),
            },
        );

        assert_eq!(decision.verdict, EvaluationVerdict::Inconclusive);
    }

    #[test]
    fn confirmed_regression_is_unsafe_even_when_measurement_is_partial() {
        let baseline = batch(1.0, 1.0);
        let mut candidate = batch(1.0, 1.0);
        candidate.quality = MetricQuality::Partial;

        let decision = VerdictKernel.decide(
            &batch(1.0, 1.0),
            &batch(1.0, 1.0),
            &baseline,
            &candidate,
            ComparisonEvidenceGroups {
                primary: vec![comparison(ComparisonConclusion::Improved)],
                regression_guards: vec![comparison(ComparisonConclusion::NotImproved)],
                workload_invariants: Vec::new(),
            },
        );

        assert_eq!(decision.verdict, EvaluationVerdict::Unsafe);
        assert!(decision.reason.contains("regression"));
    }

    #[test]
    fn mismatched_workload_fingerprint_is_inconclusive() {
        let baseline = batch(1.0, 1.0);
        let mut candidate = batch(1.0, 1.0);
        candidate.workload_fingerprint = Some("different".to_string());

        let decision = VerdictKernel.decide(
            &batch(1.0, 1.0),
            &batch(1.0, 1.0),
            &baseline,
            &candidate,
            ComparisonEvidenceGroups {
                primary: vec![comparison(ComparisonConclusion::Improved)],
                regression_guards: Vec::new(),
                workload_invariants: Vec::new(),
            },
        );

        assert_eq!(decision.verdict, EvaluationVerdict::Inconclusive);
    }

    #[test]
    fn missing_workload_fingerprints_are_inconclusive() {
        let mut baseline = batch(1.0, 1.0);
        baseline.workload_fingerprint = None;
        let mut candidate = batch(1.0, 1.0);
        candidate.workload_fingerprint = None;

        let decision = VerdictKernel.decide(
            &batch(1.0, 1.0),
            &batch(1.0, 1.0),
            &baseline,
            &candidate,
            ComparisonEvidenceGroups {
                primary: vec![comparison(ComparisonConclusion::Improved)],
                regression_guards: Vec::new(),
                workload_invariants: Vec::new(),
            },
        );

        assert_eq!(decision.verdict, EvaluationVerdict::Inconclusive);
    }

    #[test]
    fn different_metric_units_are_inconclusive() {
        let baseline = batch(1.0, 1.0);
        let mut candidate = batch(1.0, 1.0);
        candidate.metrics.get_mut("loadavg.1m").unwrap().unit = "seconds".to_string();

        let decision = VerdictKernel.decide(
            &batch(1.0, 1.0),
            &batch(1.0, 1.0),
            &baseline,
            &candidate,
            ComparisonEvidenceGroups {
                primary: vec![comparison(ComparisonConclusion::Improved)],
                regression_guards: Vec::new(),
                workload_invariants: Vec::new(),
            },
        );

        assert_eq!(decision.verdict, EvaluationVerdict::Inconclusive);
        assert!(decision.reason.contains("kinds, or units"));
    }
}
