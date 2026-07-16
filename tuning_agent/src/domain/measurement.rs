use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{InvocationContext, MeasurementSessionId};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Gauge,
    Counter,
    Boolean,
    Histogram,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricQuality {
    Valid,
    Partial,
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricValue {
    pub value: Value,
    pub unit: String,
    pub kind: MetricKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricBatch {
    pub started_at_ns: u128,
    pub ended_at_ns: u128,
    pub quality: MetricQuality,
    pub workload_fingerprint: Option<String>,
    pub metrics: BTreeMap<String, MetricValue>,
    pub provenance: Value,
}

impl MetricBatch {
    pub fn validate(&self) -> Result<(), String> {
        if self.ended_at_ns < self.started_at_ns {
            return Err("metric batch ends before it starts".to_string());
        }
        if self.metrics.is_empty() {
            return Err("metric batch must contain at least one metric".to_string());
        }
        if self.metrics.len() > 4096 {
            return Err("metric batch contains more than 4096 metrics".to_string());
        }
        if let Some(fingerprint) = &self.workload_fingerprint {
            validate_text("workload fingerprint", fingerprint, 4096)?;
        }
        for (name, metric) in &self.metrics {
            validate_text("metric name", name, 256)?;
            validate_text(&format!("metric '{name}' unit"), &metric.unit, 128)?;
            let kind_matches = match metric.kind {
                MetricKind::Gauge | MetricKind::Counter => {
                    metric.value.as_f64().is_some_and(f64::is_finite)
                }
                MetricKind::Boolean => metric.value.is_boolean(),
                MetricKind::Histogram => metric.value.is_array() || metric.value.is_object(),
            };
            if !kind_matches {
                return Err(format!(
                    "metric '{name}' value does not match its declared kind"
                ));
            }
        }
        Ok(())
    }
}

fn validate_text(label: &str, value: &str, max_len: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > max_len
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(format!("{label} is invalid"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasurementOpenRequest {
    pub context: InvocationContext,
    pub specification: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasurementSession {
    pub id: MeasurementSessionId,
    pub driver_data: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasurementSampleRequest {
    pub context: InvocationContext,
    pub session: MeasurementSession,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CleanupReceipt {
    pub session_id: MeasurementSessionId,
    pub cleaned_up: bool,
    pub details: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn batch() -> MetricBatch {
        MetricBatch {
            started_at_ns: 1,
            ended_at_ns: 2,
            quality: MetricQuality::Valid,
            workload_fingerprint: Some("workload-1".to_string()),
            metrics: BTreeMap::from([(
                "throughput".to_string(),
                MetricValue {
                    value: json!(10.0),
                    unit: "ops/s".to_string(),
                    kind: MetricKind::Gauge,
                },
            )]),
            provenance: json!({}),
        }
    }

    #[test]
    fn metric_batch_validates_time_and_declared_value_kind() {
        let mut invalid_time = batch();
        invalid_time.ended_at_ns = 0;
        assert!(invalid_time.validate().is_err());

        let mut invalid_kind = batch();
        invalid_kind.metrics.get_mut("throughput").unwrap().value = json!(true);
        assert!(invalid_kind.validate().is_err());

        assert!(batch().validate().is_ok());
    }
}
