use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentToolInvocation {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentToolResult {
    pub call_id: String,
    pub name: String,
    pub ok: bool,
    pub content: Value,
}

impl AgentToolResult {
    pub fn success(invocation: &AgentToolInvocation, content: Value) -> Self {
        Self {
            call_id: invocation.id.clone(),
            name: invocation.name.clone(),
            ok: true,
            content,
        }
    }

    pub fn failure(invocation: &AgentToolInvocation, message: impl Into<String>) -> Self {
        Self {
            call_id: invocation.id.clone(),
            name: invocation.name.clone(),
            ok: false,
            content: serde_json::json!({ "error": message.into() }),
        }
    }
}
