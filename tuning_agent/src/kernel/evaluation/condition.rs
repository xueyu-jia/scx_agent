use serde::{Deserialize, Serialize};

use crate::domain::{MetricBatch, MetricKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricOperator {
    DecreasePercentGe,
    DecreaseAbsGe,
    IncreasePercentGe,
    IncreaseAbsGe,
    IncreasePercentLe,
    IncreaseAbsLe,
    DecreasePercentLe,
    DecreaseAbsLe,
    ChangePercentLe,
    ChangeAbsLe,
    CurrentLe,
    CurrentGe,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricCondition {
    pub metric: String,
    pub op: MetricOperator,
    pub value: f64,
}

impl MetricCondition {
    pub fn new(metric: impl Into<String>, op: MetricOperator, value: f64) -> Self {
        Self {
            metric: metric.into(),
            op,
            value,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.metric.trim().is_empty() {
            return Err("metric condition requires a metric name".to_string());
        }
        if !self.value.is_finite() {
            return Err(format!(
                "metric condition '{}' has a non-finite threshold",
                self.metric
            ));
        }
        if !matches!(
            self.op,
            MetricOperator::CurrentLe | MetricOperator::CurrentGe
        ) && self.value < 0.0
        {
            return Err(format!(
                "metric condition '{}' requires a non-negative delta threshold",
                self.metric
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOutcome {
    Passed,
    Failed,
    Inconclusive,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricConditionEvidence {
    pub condition: MetricCondition,
    pub outcome: ConditionOutcome,
    pub baseline: Option<f64>,
    pub candidate: Option<f64>,
    pub reason: String,
}

pub fn evaluate_metric_condition(
    condition: &MetricCondition,
    baseline: &MetricBatch,
    candidate: &MetricBatch,
) -> MetricConditionEvidence {
    if let Err(reason) = condition.validate() {
        return inconclusive(condition, None, None, reason);
    }

    let before_metric = baseline.metrics.get(&condition.metric);
    let after_metric = candidate.metrics.get(&condition.metric);
    let (before_metric, after_metric) = match (before_metric, after_metric) {
        (Some(before), Some(after)) => (before, after),
        _ => {
            return inconclusive(
                condition,
                None,
                None,
                format!(
                    "metric '{}' is missing on one or both sides",
                    condition.metric
                ),
            )
        }
    };
    if before_metric.kind != after_metric.kind || before_metric.unit != after_metric.unit {
        return inconclusive(
            condition,
            numeric_value(before_metric),
            numeric_value(after_metric),
            format!(
                "metric '{}' has different kinds or units across A/B measurements",
                condition.metric
            ),
        );
    }
    if !matches!(before_metric.kind, MetricKind::Gauge | MetricKind::Counter) {
        return inconclusive(
            condition,
            None,
            None,
            format!(
                "metric '{}' is not a numeric gauge or counter",
                condition.metric
            ),
        );
    }
    let before = numeric_value(before_metric);
    let after = numeric_value(after_metric);
    let (before, after) = match (before, after) {
        (Some(before), Some(after)) if before.is_finite() && after.is_finite() => (before, after),
        _ => {
            return inconclusive(
                condition,
                before,
                after,
                format!(
                    "metric '{}' is not a finite number on both sides",
                    condition.metric
                ),
            )
        }
    };

    let result = match condition.op {
        MetricOperator::DecreasePercentGe => {
            percent_change(before, before - after).map(|change| change >= condition.value)
        }
        MetricOperator::DecreaseAbsGe => Ok(before - after >= condition.value),
        MetricOperator::IncreasePercentGe => {
            percent_change(before, after - before).map(|change| change >= condition.value)
        }
        MetricOperator::IncreaseAbsGe => Ok(after - before >= condition.value),
        MetricOperator::IncreasePercentLe => percent_change(before, (after - before).max(0.0))
            .map(|change| change <= condition.value),
        MetricOperator::IncreaseAbsLe => Ok(after - before <= condition.value),
        MetricOperator::DecreasePercentLe => percent_change(before, (before - after).max(0.0))
            .map(|change| change <= condition.value),
        MetricOperator::DecreaseAbsLe => Ok(before - after <= condition.value),
        MetricOperator::ChangePercentLe => {
            percent_change(before, (after - before).abs()).map(|change| change <= condition.value)
        }
        MetricOperator::ChangeAbsLe => Ok((after - before).abs() <= condition.value),
        MetricOperator::CurrentLe => Ok(after <= condition.value),
        MetricOperator::CurrentGe => Ok(after >= condition.value),
    };

    match result {
        Ok(true) => MetricConditionEvidence {
            condition: condition.clone(),
            outcome: ConditionOutcome::Passed,
            baseline: Some(before),
            candidate: Some(after),
            reason: "condition passed".to_string(),
        },
        Ok(false) => MetricConditionEvidence {
            condition: condition.clone(),
            outcome: ConditionOutcome::Failed,
            baseline: Some(before),
            candidate: Some(after),
            reason: format!(
                "condition failed: baseline={before}, candidate={after}, threshold={}",
                condition.value
            ),
        },
        Err(reason) => inconclusive(condition, Some(before), Some(after), reason),
    }
}

fn numeric_value(metric: &crate::domain::MetricValue) -> Option<f64> {
    metric.value.as_f64()
}

fn percent_change(baseline: f64, delta: f64) -> Result<f64, String> {
    if baseline.abs() <= f64::EPSILON {
        return Err("percentage comparison is undefined for a zero baseline".to_string());
    }
    Ok(delta / baseline.abs() * 100.0)
}

fn inconclusive(
    condition: &MetricCondition,
    baseline: Option<f64>,
    candidate: Option<f64>,
    reason: String,
) -> MetricConditionEvidence {
    MetricConditionEvidence {
        condition: condition.clone(),
        outcome: ConditionOutcome::Inconclusive,
        baseline,
        candidate,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::domain::{MetricKind, MetricQuality, MetricValue};

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
                    unit: "count".to_string(),
                    kind: MetricKind::Gauge,
                },
            )]),
            provenance: json!({}),
        }
    }

    #[test]
    fn evaluates_decrease_percent_condition() {
        let condition =
            MetricCondition::new("latency.p99", MetricOperator::DecreasePercentGe, 10.0);

        let evidence = evaluate_metric_condition(
            &condition,
            &batch("latency.p99", 100.0),
            &batch("latency.p99", 85.0),
        );

        assert_eq!(evidence.outcome, ConditionOutcome::Passed);
    }

    #[test]
    fn zero_baseline_percent_is_inconclusive() {
        let condition = MetricCondition::new("errors", MetricOperator::IncreasePercentLe, 5.0);

        let evidence =
            evaluate_metric_condition(&condition, &batch("errors", 0.0), &batch("errors", 1.0));

        assert_eq!(evidence.outcome, ConditionOutcome::Inconclusive);
    }

    #[test]
    fn different_units_are_inconclusive() {
        let condition = MetricCondition::new("latency", MetricOperator::DecreaseAbsGe, 5.0);
        let baseline = batch("latency", 100.0);
        let mut candidate = batch("latency", 80.0);
        candidate.metrics.get_mut("latency").unwrap().unit = "seconds".to_string();

        let evidence = evaluate_metric_condition(&condition, &baseline, &candidate);

        assert_eq!(evidence.outcome, ConditionOutcome::Inconclusive);
        assert!(evidence.reason.contains("different kinds or units"));
    }
}
