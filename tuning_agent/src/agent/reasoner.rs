use serde_json::Value;

use crate::agent::{AgentToolInvocation, AgentToolResult, AgentToolSpec};

#[derive(Clone, Debug, PartialEq)]
pub enum AgentTurn {
    ToolCalls(Vec<AgentToolInvocation>),
    Final(String),
}

pub trait AgentReasoner {
    fn begin(&mut self, context: &Value, tools: &[AgentToolSpec]) -> Result<AgentTurn, String>;

    fn resume(&mut self, results: &[AgentToolResult]) -> Result<AgentTurn, String>;
}
