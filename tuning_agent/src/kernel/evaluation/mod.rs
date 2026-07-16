mod condition;
mod contract;
mod error;
mod freezer;
mod intent;
mod measurement;
mod protocol;
mod verdict;

pub use condition::{
    evaluate_metric_condition, ConditionOutcome, MetricCondition, MetricConditionEvidence,
    MetricOperator,
};
pub use contract::{
    CapabilityBindingPin, ComparisonBinding, EvaluationContractSpec, FrozenEvaluationContract,
    MeasurementBinding, SamplingPlan, MAX_PRIMARY_COMPARISONS, MAX_REGRESSION_GUARDS,
    MAX_WORKLOAD_INVARIANTS,
};
pub use error::{EvaluationError, EvaluationErrorKind};
pub use freezer::ContractFreezer;
#[cfg(test)]
pub use intent::ObjectiveStatement;
pub use intent::{EvaluationIntentSpec, FrozenEvaluationIntent, MAX_OBJECTIVE_STATEMENT_BYTES};
pub use measurement::MeasurementEvidence;
pub use protocol::{AbEvaluationEvidence, AbEvaluationProtocol};
pub use verdict::{ComparisonEvidenceGroups, EvaluationDecision, EvaluationVerdict, VerdictKernel};

pub(crate) use measurement::collect_measurement;
pub(crate) use protocol::EvaluationDeadline;
#[cfg(test)]
pub(crate) use protocol::TRUSTED_GUARDRAIL_MEASUREMENT_ID;
