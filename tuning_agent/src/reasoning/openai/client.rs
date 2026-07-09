use std::time::Duration;

use serde_json::Value;

use crate::config::LlmConfig;
use crate::reasoning::openai::{ChatMessage, OpenAiAssistantOutput, OpenAiProtocol};
use crate::tools::ToolSpec;

pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
}

impl OpenAiConfig {
    pub fn from_config(config: &LlmConfig) -> Result<Self, String> {
        if config.base_url.trim().is_empty() {
            return Err("missing llm.base_url".to_string());
        }
        if config.api_key.trim().is_empty() {
            return Err("missing llm.api_key".to_string());
        }
        if config.model.trim().is_empty() {
            return Err("missing llm.model".to_string());
        }

        Ok(Self {
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            timeout: Duration::from_millis(config.timeout_ms),
        })
    }

    fn chat_completions_url(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }
}

pub struct OpenAiCompatibleClient {
    config: OpenAiConfig,
    agent: ureq::Agent,
}

impl OpenAiCompatibleClient {
    pub fn new(config: OpenAiConfig) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(config.timeout).build();
        Self { config, agent }
    }

    pub fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<OpenAiAssistantOutput, String> {
        let body = OpenAiProtocol::request_body(&self.config.model, messages, tools);
        let response = self
            .agent
            .post(&self.config.chat_completions_url())
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .set("Content-Type", "application/json")
            .send_json(body)
            .map_err(Self::format_error)?;

        let value: Value = response
            .into_json()
            .map_err(|err| format!("response JSON parse failed: {err}"))?;

        OpenAiProtocol::parse_response(value)
    }

    fn format_error(err: ureq::Error) -> String {
        match err {
            ureq::Error::Status(code, response) => {
                let body = response.into_string().unwrap_or_default();
                format!("HTTP {code}: {body}")
            }
            ureq::Error::Transport(err) => format!("transport error: {err}"),
        }
    }
}
