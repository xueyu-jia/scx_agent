use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::act::ActKernel;
use crate::config::EvaluationConfig;
use crate::evaluate::{
    ConditionResult, EvaluationDecision, EvaluationEvidence, EvaluationPlan, EvaluationSample,
    EvaluationVerdict, MeasurementValueType, MetricCondition,
};
use crate::observation::Observation;

pub struct EvaluationKernelConfig {
    default_window: Duration,
    min_window: Duration,
    max_window: Duration,
    default_settle: Duration,
    min_settle: Duration,
    max_settle: Duration,
}

impl EvaluationKernelConfig {
    pub fn from_config(config: &EvaluationConfig) -> Self {
        Self {
            default_window: Duration::from_secs(config.default_window_seconds),
            min_window: Duration::from_secs(config.min_window_seconds),
            max_window: Duration::from_secs(config.max_window_seconds),
            default_settle: Duration::from_secs(config.default_settle_seconds),
            min_settle: Duration::from_secs(config.min_settle_seconds),
            max_settle: Duration::from_secs(config.max_settle_seconds),
        }
    }
}

pub struct EvaluationKernel {
    config: EvaluationKernelConfig,
}

impl EvaluationKernel {
    pub fn new(config: EvaluationKernelConfig) -> Self {
        Self { config }
    }

    pub fn settle(&self, plan: &EvaluationPlan) {
        let settle = self.effective_settle(plan);
        if !settle.is_zero() {
            thread::sleep(settle);
        }
    }

    pub fn sample(
        &self,
        observation: &Observation,
        act: &ActKernel,
        plan: &EvaluationPlan,
    ) -> Result<EvaluationSample, String> {
        let window = self.effective_window(plan);
        let sample_count = window.as_secs().max(1) as usize;
        let mut samples = Vec::with_capacity(sample_count);

        for index in 0..sample_count {
            let snapshot = observation
                .core_snapshot()
                .map_err(|err| format!("core snapshot failed: {err}"))?;
            let builtin_sample = EvaluationSample::from_core_snapshot(&snapshot);
            let measurement_sample = run_measurement(act, plan)?;
            samples.push(builtin_sample.merge(measurement_sample));
            if index + 1 < sample_count {
                thread::sleep(Duration::from_secs(1));
            }
        }

        Ok(EvaluationSample::median(samples))
    }

    fn effective_window(&self, plan: &EvaluationPlan) -> Duration {
        clamp_duration(
            plan.window.unwrap_or(self.config.default_window),
            self.config.min_window,
            self.config.max_window,
        )
    }

    fn effective_settle(&self, plan: &EvaluationPlan) -> Duration {
        clamp_duration(
            plan.settle.unwrap_or(self.config.default_settle),
            self.config.min_settle,
            self.config.max_settle,
        )
    }

    pub fn evaluate(
        &self,
        plan: &EvaluationPlan,
        baseline_prime: EvaluationSample,
        candidate_prime: EvaluationSample,
    ) -> EvaluationDecision {
        let primary = evaluate_conditions(&plan.primary, &baseline_prime, &candidate_prime);
        let regression_guards =
            evaluate_conditions(&plan.regression_guards, &baseline_prime, &candidate_prime);
        let system_guardrails = evaluate_conditions(
            &system_guardrail_conditions(),
            &baseline_prime,
            &candidate_prime,
        );
        let workload_invariants =
            evaluate_conditions(&plan.workload_invariants, &baseline_prime, &candidate_prime);

        let primary_passed = primary.iter().all(|result| result.passed);
        let regression_guards_passed = regression_guards.iter().all(|result| result.passed);
        let system_guardrails_passed = system_guardrails.iter().all(|result| result.passed);
        let workload_invariants_passed = workload_invariants.iter().all(|result| result.passed);

        let verdict = if !regression_guards_passed || !system_guardrails_passed {
            EvaluationVerdict::Unsafe
        } else if !workload_invariants_passed {
            EvaluationVerdict::Inconclusive
        } else if primary_passed {
            EvaluationVerdict::Improved
        } else {
            EvaluationVerdict::NoSignal
        };
        let accepted = verdict == EvaluationVerdict::Improved;

        EvaluationDecision {
            verdict,
            accepted,
            evidence: EvaluationEvidence {
                baseline_prime,
                candidate_prime,
                primary,
                regression_guards,
                system_guardrails,
                workload_invariants,
            },
        }
    }
}

fn run_measurement(act: &ActKernel, plan: &EvaluationPlan) -> Result<EvaluationSample, String> {
    let report = act.execute_read(&plan.measurement.request)?;
    if !report.succeeded() {
        return Err(format!(
            "measurement command failed: status={:?} timed_out={} stderr={}",
            report.status, report.timed_out, report.stderr
        ));
    }
    let output_limit = act.evaluation_output_limit();
    if report.stdout.len() > output_limit {
        return Err(format!(
            "measurement output exceeded limit: {} > {} bytes",
            report.stdout.len(),
            output_limit
        ));
    }

    let raw: Value = serde_json::from_str(report.stdout.trim())
        .map_err(|err| format!("measurement output is not valid JSON: {err}"))?;
    validate_measurement_schema(plan, &raw)?;
    EvaluationSample::from_measurement_json(raw)
}

fn validate_measurement_schema(plan: &EvaluationPlan, raw: &Value) -> Result<(), String> {
    let object = raw
        .as_object()
        .ok_or_else(|| "measurement output must be a JSON object".to_string())?;
    for field in &plan.measurement.schema {
        let value = object
            .get(&field.name)
            .ok_or_else(|| format!("measurement output missing field '{}'", field.name))?;
        match field.value_type {
            MeasurementValueType::Number | MeasurementValueType::Counter => {
                if !value.is_number() {
                    return Err(format!(
                        "measurement field '{}' must be numeric",
                        field.name
                    ));
                }
            }
            MeasurementValueType::Bool => {
                if !value.is_boolean() {
                    return Err(format!("measurement field '{}' must be bool", field.name));
                }
            }
        }
    }
    Ok(())
}

fn system_guardrail_conditions() -> Vec<MetricCondition> {
    Vec::from([
        MetricCondition::new("psi.cpu.full.avg10", "increase_abs_le", 1.0),
        MetricCondition::new("psi.io.full.avg10", "increase_abs_le", 1.0),
        MetricCondition::new("psi.memory.full.avg10", "increase_abs_le", 1.0),
        MetricCondition::new("loadavg.1m", "increase_percent_le", 50.0),
    ])
}

fn evaluate_conditions(
    conditions: &[crate::evaluate::MetricCondition],
    baseline: &EvaluationSample,
    candidate: &EvaluationSample,
) -> Vec<ConditionResult> {
    conditions
        .iter()
        .map(|condition| condition.evaluate(baseline, candidate))
        .collect()
}

fn clamp_duration(value: Duration, min: Duration, max: Duration) -> Duration {
    value.max(min).min(max.max(min))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_duration_respects_bounds() {
        let min = Duration::from_secs(3);
        let max = Duration::from_secs(60);

        assert_eq!(
            clamp_duration(Duration::from_secs(1), min, max),
            Duration::from_secs(3)
        );
        assert_eq!(
            clamp_duration(Duration::from_secs(10), min, max),
            Duration::from_secs(10)
        );
        assert_eq!(
            clamp_duration(Duration::from_secs(300), min, max),
            Duration::from_secs(60)
        );
    }
}
