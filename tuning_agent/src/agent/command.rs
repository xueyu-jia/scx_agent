use serde_json::Value;

use crate::domain::{CapabilityId, ChangeId};
use crate::kernel::evaluation::EvaluationIntentSpec;
use crate::skill::SkillCommand;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DecodedToolCall {
    Context(SkillCommand),
    Action(AgentCommand),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentCommand {
    Probe {
        call_id: String,
        capability_id: CapabilityId,
        arguments: Value,
    },
    BeginExperiment {
        call_id: String,
        intent: EvaluationIntentSpec,
    },
    Mutation {
        call_id: String,
        capability_id: CapabilityId,
        arguments: Value,
        reason: String,
    },
    RequestCommit {
        call_id: String,
        change_ids: Vec<ChangeId>,
        reason: String,
    },
    Abort {
        call_id: String,
        reason: String,
    },
}
