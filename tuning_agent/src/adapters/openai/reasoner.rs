use serde_json::Value;

use crate::adapters::openai::client::{OpenAiClient, OpenAiConfig};
use crate::adapters::openai::protocol::ChatMessage;
use crate::agent::{AgentReasoner, AgentToolResult, AgentToolSpec, AgentTurn};
use crate::config::LlmConfig;

pub struct OpenAiReasoner {
    client: OpenAiClient,
    messages: Vec<ChatMessage>,
    tools: Vec<AgentToolSpec>,
    started: bool,
}

impl OpenAiReasoner {
    pub fn new(config: &LlmConfig) -> Result<Self, String> {
        Ok(Self {
            client: OpenAiClient::new(OpenAiConfig::from_config(config)?),
            messages: Vec::new(),
            tools: Vec::new(),
            started: false,
        })
    }

    fn complete(&mut self) -> Result<AgentTurn, String> {
        let turn = self.client.complete(&self.messages, &self.tools)?;
        match &turn {
            AgentTurn::ToolCalls(calls) => self
                .messages
                .push(ChatMessage::AssistantToolCalls(calls.clone())),
            AgentTurn::Final(content) => self
                .messages
                .push(ChatMessage::AssistantContent(content.clone())),
        }
        Ok(turn)
    }
}

impl AgentReasoner for OpenAiReasoner {
    fn begin(&mut self, context: &Value, tools: &[AgentToolSpec]) -> Result<AgentTurn, String> {
        if self.started {
            return Err("reasoner session has already started".to_string());
        }
        self.started = true;
        self.tools = tools.to_vec();
        self.messages.push(ChatMessage::System(
            include_str!("../../system_prompt.md").trim().to_string(),
        ));
        self.messages.push(ChatMessage::User(context.to_string()));
        self.complete()
    }

    fn resume(&mut self, results: &[AgentToolResult]) -> Result<AgentTurn, String> {
        if !self.started {
            return Err("reasoner session has not started".to_string());
        }
        if results.is_empty() {
            return Err("at least one tool result is required to resume".to_string());
        }
        self.messages
            .extend(results.iter().cloned().map(ChatMessage::ToolResult));
        self.complete()
    }
}
