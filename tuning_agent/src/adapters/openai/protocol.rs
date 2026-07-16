use serde_json::{json, Value};

use crate::agent::{AgentToolInvocation, AgentToolResult, AgentToolSpec, AgentTurn};

const MAX_TOOL_CALLS_PER_TURN: usize = 64;
const MAX_ASSISTANT_CONTENT_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug)]
pub(crate) enum ChatMessage {
    System(String),
    User(String),
    AssistantContent(String),
    AssistantToolCalls(Vec<AgentToolInvocation>),
    ToolResult(AgentToolResult),
}

pub(crate) struct OpenAiProtocol;

impl OpenAiProtocol {
    pub fn request_body(model: &str, messages: &[ChatMessage], tools: &[AgentToolSpec]) -> Value {
        let mut body = json!({
            "model": model,
            "temperature": 0,
            "messages": messages.iter().map(Self::message_json).collect::<Vec<_>>(),
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema,
                            }
                        })
                    })
                    .collect(),
            );
            body["tool_choice"] = json!("auto");
        }
        body
    }

    pub fn parse_response(value: Value) -> Result<AgentTurn, String> {
        let message = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .ok_or_else(|| "missing choices[0].message".to_string())?;
        let calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                if calls.len() > MAX_TOOL_CALLS_PER_TURN {
                    return Err(format!(
                        "assistant returned {} tool calls; the limit is {MAX_TOOL_CALLS_PER_TURN}",
                        calls.len()
                    ));
                }
                calls
                    .iter()
                    .map(Self::parse_tool_call)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        if !calls.is_empty() {
            return Ok(AgentTurn::ToolCalls(calls));
        }
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                "assistant response has neither tool calls nor text content".to_string()
            })?;
        if content.len() > MAX_ASSISTANT_CONTENT_BYTES {
            return Err(format!(
                "assistant content exceeds the {MAX_ASSISTANT_CONTENT_BYTES} byte limit"
            ));
        }
        Ok(AgentTurn::Final(content.to_string()))
    }

    fn message_json(message: &ChatMessage) -> Value {
        match message {
            ChatMessage::System(content) => json!({"role": "system", "content": content}),
            ChatMessage::User(content) => json!({"role": "user", "content": content}),
            ChatMessage::AssistantContent(content) => {
                json!({"role": "assistant", "content": content})
            }
            ChatMessage::AssistantToolCalls(calls) => json!({
                "role": "assistant",
                "content": null,
                "tool_calls": calls.iter().map(Self::tool_call_json).collect::<Vec<_>>(),
            }),
            ChatMessage::ToolResult(result) => json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "content": serde_json::json!({
                    "ok": result.ok,
                    "result": result.content,
                }).to_string(),
            }),
        }
    }

    fn tool_call_json(call: &AgentToolInvocation) -> Value {
        json!({
            "id": call.id,
            "type": "function",
            "function": {
                "name": call.name,
                "arguments": call.arguments.to_string(),
            }
        })
    }

    fn parse_tool_call(value: &Value) -> Result<AgentToolInvocation, String> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| valid_token(id))
            .ok_or_else(|| "tool call is missing a non-empty id".to_string())?;
        let function = value
            .get("function")
            .ok_or_else(|| "tool call is missing function".to_string())?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_token(name))
            .ok_or_else(|| "tool call function is missing a non-empty name".to_string())?;
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool call function is missing arguments".to_string())?;
        let arguments: Value = serde_json::from_str(arguments)
            .map_err(|error| format!("tool call arguments are not valid JSON: {error}"))?;
        if !arguments.is_object() {
            return Err("tool call arguments must be a JSON object".to_string());
        }
        Ok(AgentToolInvocation {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        })
    }
}

fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_capability_derived_tool_schema() {
        let body = OpenAiProtocol::request_body(
            "model",
            &[ChatMessage::User("context".into())],
            &[AgentToolSpec {
                name: "probe_1234".into(),
                description: "Read PSI".into(),
                input_schema: json!({"type": "object"}),
            }],
        );
        assert_eq!(body["tools"][0]["function"]["name"], "probe_1234");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn response_decodes_new_agent_invocations() {
        let turn = OpenAiProtocol::parse_response(json!({
            "choices": [{"message": {"tool_calls": [{
                "id": "call-1",
                "function": {"name": "begin_experiment", "arguments": "{\"objective\":\"latency\"}"}
            }]}}]
        }))
        .unwrap();
        let AgentTurn::ToolCalls(calls) = turn else {
            panic!("expected tool calls");
        };
        assert_eq!(calls[0].name, "begin_experiment");
        assert_eq!(calls[0].arguments["objective"], "latency");
    }

    #[test]
    fn response_rejects_non_object_tool_arguments() {
        let error = OpenAiProtocol::parse_response(json!({
            "choices": [{"message": {"tool_calls": [{
                "id": "call-1",
                "function": {"name": "probe", "arguments": "[]"}
            }]}}]
        }))
        .unwrap_err();

        assert!(error.contains("JSON object"));
    }
}
