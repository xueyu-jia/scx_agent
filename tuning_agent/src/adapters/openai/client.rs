use std::io::Read;
use std::time::Duration;

use serde_json::Value;

use crate::adapters::openai::protocol::{ChatMessage, OpenAiProtocol};
use crate::agent::{AgentToolSpec, AgentTurn};
use crate::config::LlmConfig;

const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

pub(crate) struct OpenAiConfig {
    base_url: String,
    api_key: String,
    model: String,
    timeout: Duration,
    retry_count: u32,
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
        if config.timeout_ms == 0 {
            return Err("llm.timeout_ms must be greater than zero".to_string());
        }
        Ok(Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            timeout: Duration::from_millis(config.timeout_ms),
            retry_count: config.retry_count,
        })
    }

    fn url(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }
}

pub(crate) struct OpenAiClient {
    config: OpenAiConfig,
    agent: ureq::Agent,
}

impl OpenAiClient {
    pub fn new(config: OpenAiConfig) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(config.timeout).build();
        Self { config, agent }
    }

    pub fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[AgentToolSpec],
    ) -> Result<AgentTurn, String> {
        let body = OpenAiProtocol::request_body(&self.config.model, messages, tools);
        retry(self.config.retry_count, || self.complete_once(&body))
    }

    fn complete_once(&self, body: &Value) -> Result<AgentTurn, String> {
        let response = self
            .agent
            .post(&self.config.url())
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .set("Content-Type", "application/json")
            .send_json(body)
            .map_err(format_http_error)?;
        let body = read_limited_body(response, MAX_RESPONSE_BYTES)?;
        let value = serde_json::from_slice(&body)
            .map_err(|error| format!("response JSON parse failed: {error}"))?;
        OpenAiProtocol::parse_response(value)
    }
}

fn retry<T>(
    retry_count: u32,
    mut operation: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let attempts = u64::from(retry_count) + 1;
    for attempt in 1..=attempts {
        match operation() {
            Ok(value) => return Ok(value),
            Err(_) if attempt < attempts => std::thread::sleep(RETRY_INTERVAL),
            Err(error) => {
                return Err(format!(
                    "request failed after {attempts} attempt(s): {error}"
                ))
            }
        }
    }
    unreachable!("retry loop always has at least one attempt")
}

fn format_http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = read_limited_body(response, MAX_RESPONSE_BYTES)
                .map(|body| String::from_utf8_lossy(&body).into_owned())
                .unwrap_or_else(|error| format!("<unavailable: {error}>"));
            format!("HTTP {code}: {body}")
        }
        ureq::Error::Transport(error) => format!("transport error: {error}"),
    }
}

fn read_limited_body(response: ureq::Response, limit: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    response
        .into_reader()
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("response body read failed: {error}"))?;
    if body.len() > limit {
        return Err(format!("response body exceeds the {limit} byte limit"));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn retry_count_means_retries_after_the_initial_attempt() {
        let calls = Cell::new(0);
        let result = retry(2, || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err("temporary".to_string())
            } else {
                Ok(42)
            }
        });
        assert_eq!(result, Ok(42));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn response_body_limit_is_enforced_before_json_parsing() {
        let within_limit = ureq::Response::new(200, "OK", "1234").unwrap();
        assert_eq!(read_limited_body(within_limit, 4).unwrap(), b"1234");

        let oversized = ureq::Response::new(200, "OK", "12345").unwrap();
        assert!(read_limited_body(oversized, 4)
            .unwrap_err()
            .contains("exceeds"));
    }
}
