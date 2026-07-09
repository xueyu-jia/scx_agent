use crate::evaluate::EvaluationSample;
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum EvaluationVerdict {
    Improved,
    NoSignal,
    Inconclusive,
    Unsafe,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConditionResult {
    pub metric: String,
    pub op: String,
    pub value: f64,
    pub before: f64,
    pub after: f64,
    pub passed: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvaluationEvidence {
    pub baseline_prime: EvaluationSample,
    pub candidate_prime: EvaluationSample,
    pub primary: Vec<ConditionResult>,
    pub regression_guards: Vec<ConditionResult>,
    pub system_guardrails: Vec<ConditionResult>,
    pub workload_invariants: Vec<ConditionResult>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EvaluationDecision {
    pub verdict: EvaluationVerdict,
    pub accepted: bool,
    pub evidence: EvaluationEvidence,
}
