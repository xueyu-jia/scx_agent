use std::time::Duration;

use serde_json::Value;

use crate::act::{CommandRequest, CommitWrite};
use crate::evaluate::{ConditionResult, EvaluationSample};

#[derive(Clone, Debug)]
pub struct EvaluationPlan {
    pub reason: String,
    pub measurement: MeasurementProgram,
    pub primary: Vec<MetricCondition>,
    pub regression_guards: Vec<MetricCondition>,
    pub workload_invariants: Vec<MetricCondition>,
    pub keep_writes: Vec<CommitWrite>,
    pub window: Option<Duration>,
    pub settle: Option<Duration>,
}

impl EvaluationPlan {
    pub fn from_commit_arguments(arguments: &Value) -> Result<Self, String> {
        let reason = arguments
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("unspecified")
            .to_string();
        let primary = parse_conditions(arguments, "primary_metrics")?;
        if primary.is_empty() {
            return Err("commit requires at least one primary criterion".to_string());
        }

        Ok(Self {
            reason,
            measurement: parse_measurement(arguments)?,
            primary,
            regression_guards: parse_conditions(arguments, "regression_guards")?,
            workload_invariants: parse_conditions(arguments, "workload_invariants")?,
            keep_writes: parse_keep_writes(arguments)?,
            window: parse_duration_seconds(arguments, "window_seconds"),
            settle: parse_duration_seconds(arguments, "settle_seconds"),
        })
    }
}

#[derive(Clone, Debug)]
pub struct MeasurementProgram {
    pub request: CommandRequest,
    pub schema: Vec<MeasurementField>,
}

#[derive(Clone, Debug)]
pub struct MeasurementField {
    pub name: String,
    pub value_type: MeasurementValueType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MeasurementValueType {
    Number,
    Counter,
    Bool,
}

#[derive(Clone, Debug)]
pub struct MetricCondition {
    pub metric: String,
    pub op: String,
    pub value: f64,
}

impl MetricCondition {
    pub fn new(metric: &str, op: &str, value: f64) -> Self {
        Self {
            metric: metric.to_string(),
            op: op.to_string(),
            value,
        }
    }

    pub fn evaluate(
        &self,
        baseline: &EvaluationSample,
        candidate: &EvaluationSample,
    ) -> ConditionResult {
        let before = baseline.get(&self.metric);
        let after = candidate.get(&self.metric);

        let (passed, reason) = match (before, after) {
            (Some(before), Some(after)) => {
                let passed = match self.op.as_str() {
                    "decrease_percent_ge" => {
                        before > 0.0 && ((before - after) / before * 100.0) >= self.value
                    }
                    "decrease_abs_ge" => before - after >= self.value,
                    "increase_percent_ge" => {
                        before > 0.0 && ((after - before) / before * 100.0) >= self.value
                    }
                    "increase_abs_ge" => after - before >= self.value,
                    "increase_percent_le" => {
                        before == 0.0 || ((after - before).max(0.0) / before * 100.0) <= self.value
                    }
                    "increase_abs_le" => after - before <= self.value,
                    "decrease_percent_le" => {
                        before == 0.0 || ((before - after).max(0.0) / before * 100.0) <= self.value
                    }
                    "decrease_abs_le" => before - after <= self.value,
                    "change_percent_le" => {
                        before == 0.0 || ((after - before).abs() / before * 100.0) <= self.value
                    }
                    "change_abs_le" => (after - before).abs() <= self.value,
                    "current_le" => after <= self.value,
                    "current_ge" => after >= self.value,
                    _ => false,
                };
                let reason = if passed {
                    "passed".to_string()
                } else if is_supported_op(&self.op) {
                    format!(
                        "condition failed: {} {} {}; baseline={} candidate={}",
                        self.metric, self.op, self.value, before, after
                    )
                } else {
                    format!("unsupported metric op '{}'", self.op)
                };
                (passed, reason)
            }
            _ => (false, unsupported_metric(&self.metric)),
        };

        ConditionResult {
            metric: self.metric.clone(),
            op: self.op.clone(),
            value: self.value,
            before: before.unwrap_or(0.0),
            after: after.unwrap_or(0.0),
            passed,
            reason,
        }
    }
}

fn parse_measurement(arguments: &Value) -> Result<MeasurementProgram, String> {
    let value = arguments
        .get("measurement")
        .ok_or_else(|| "commit.measurement is required".to_string())?;
    let command = value
        .get("command")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| "commit.measurement.command is required".to_string())?;

    let mut request = CommandRequest::new(command.to_string());
    request.timeout = value
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis);
    request.working_dir = value
        .get("working_dir")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(ToString::to_string);

    Ok(MeasurementProgram {
        request,
        schema: parse_measurement_schema(value)?,
    })
}

fn parse_keep_writes(arguments: &Value) -> Result<Vec<CommitWrite>, String> {
    let value = arguments
        .get("keep_writes")
        .ok_or_else(|| "commit.keep_writes is required".to_string())?;
    let array = value
        .as_array()
        .ok_or_else(|| "commit.keep_writes must be an array".to_string())?;
    if array.is_empty() {
        return Err("commit.keep_writes must not be empty".to_string());
    }
    array.iter().map(CommitWrite::from_json).collect()
}

fn parse_measurement_schema(value: &Value) -> Result<Vec<MeasurementField>, String> {
    let Some(schema) = value.get("schema") else {
        return Ok(Vec::new());
    };
    let object = schema
        .as_object()
        .ok_or_else(|| "commit.measurement.schema must be an object".to_string())?;
    let mut fields = Vec::with_capacity(object.len());
    for (name, value) in object {
        let value_type = value
            .as_str()
            .ok_or_else(|| format!("commit.measurement.schema.{name} must be a string"))?;
        fields.push(MeasurementField {
            name: name.clone(),
            value_type: parse_measurement_value_type(value_type)?,
        });
    }
    Ok(fields)
}

fn parse_measurement_value_type(value: &str) -> Result<MeasurementValueType, String> {
    match value {
        "number" => Ok(MeasurementValueType::Number),
        "counter" => Ok(MeasurementValueType::Counter),
        "bool" => Ok(MeasurementValueType::Bool),
        _ => Err(format!(
            "unsupported measurement value type '{value}'; supported: number, counter, bool"
        )),
    }
}

fn parse_conditions(arguments: &Value, field: &str) -> Result<Vec<MetricCondition>, String> {
    let Some(value) = arguments.get(field) else {
        return Ok(Vec::new());
    };
    let array = value
        .as_array()
        .ok_or_else(|| format!("{field} must be an array"))?;
    let mut conditions = Vec::with_capacity(array.len());
    for item in array {
        let metric = item
            .get("metric")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{field}.metric is required"))?;
        let op = item
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{field}.op is required"))?;
        let value = item
            .get("value")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| format!("{field}.value is required"))?;
        conditions.push(MetricCondition {
            metric: metric.to_string(),
            op: op.to_string(),
            value,
        });
    }
    Ok(conditions)
}

fn parse_duration_seconds(arguments: &Value, field: &str) -> Option<Duration> {
    arguments
        .get(field)
        .and_then(|v| v.as_u64())
        .map(Duration::from_secs)
}

fn is_supported_op(op: &str) -> bool {
    matches!(
        op,
        "decrease_percent_ge"
            | "decrease_abs_ge"
            | "increase_percent_ge"
            | "increase_abs_ge"
            | "increase_percent_le"
            | "increase_abs_le"
            | "decrease_percent_le"
            | "decrease_abs_le"
            | "change_percent_le"
            | "change_abs_le"
            | "current_le"
            | "current_ge"
    )
}

fn unsupported_metric(metric: &str) -> String {
    format!("metric '{metric}' was not present in measurement output or system guardrails")
}
