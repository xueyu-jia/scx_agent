use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{ContractId, InvocationContext, MetricBatch};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComparisonRequest {
    pub context: InvocationContext,
    pub contract_id: ContractId,
    pub specification: Value,
    pub baseline: MetricBatch,
    pub candidate: MetricBatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonConclusion {
    Improved,
    NotImproved,
    Inconclusive,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConditionEvidence {
    pub name: String,
    pub passed: bool,
    pub details: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComparisonEvidence {
    pub conclusion: ComparisonConclusion,
    pub conditions: Vec<ConditionEvidence>,
    pub details: Value,
}
