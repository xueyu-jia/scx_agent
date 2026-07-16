use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::InvocationContext;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeRequest {
    pub context: InvocationContext,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProbeEvidence {
    pub observed_at_ns: u128,
    pub data: Value,
    pub warnings: Vec<String>,
}
