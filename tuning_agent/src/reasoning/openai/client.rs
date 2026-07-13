use std::time::Duration;

use serde_json::Value;

use crate::config::LlmConfig;
use crate::reasoning::openai::{ChatMessage, OpenAiAssistantOutput, OpenAiProtocol};
use crate::tools::ToolSpec;

const RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    pub retry_count: u32,
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
            retry_count: config.retry_count,
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
        retry_with_delay(self.config.retry_count, RETRY_INTERVAL, || {
            self.complete_once(&body)
        })
    }

    fn complete_once(&self, body: &Value) -> Result<OpenAiAssistantOutput, String> {
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

fn retry_with_delay<T>(
    retry_count: u32,
    retry_delay: Duration,
    mut operation: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let total_attempts = u64::from(retry_count) + 1;

    for attempt in 1..=total_attempts {
        match operation() {
            Ok(value) => return Ok(value),
            Err(_) if attempt < total_attempts => std::thread::sleep(retry_delay),
            Err(err) => {
                return Err(format!(
                    "request failed after {total_attempts} attempt(s): {err}"
                ));
            }
        }
    }

    unreachable!("retry loop always performs at least one attempt")
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn retries_every_error_up_to_configured_count() {
        let attempts = Cell::new(0);

        let result = retry_with_delay(3, Duration::ZERO, || {
            let attempt = attempts.get() + 1;
            attempts.set(attempt);
            if attempt <= 3 {
                Err(format!("failure {attempt}"))
            } else {
                Ok("success")
            }
        });

        assert_eq!(result.as_deref(), Ok("success"));
        assert_eq!(attempts.get(), 4);
    }

    #[test]
    fn reports_last_error_after_all_attempts_fail() {
        let attempts = Cell::new(0);

        let error = retry_with_delay::<()>(2, Duration::ZERO, || {
            let attempt = attempts.get() + 1;
            attempts.set(attempt);
            Err(format!("failure {attempt}"))
        })
        .expect_err("all attempts should fail");

        assert_eq!(attempts.get(), 3);
        assert_eq!(error, "request failed after 3 attempt(s): failure 3");
    }
}
