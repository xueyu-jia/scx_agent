use serde_json::{json, Value};

use crate::tools::{ToolInvocation, ToolResult, ToolSpec};

#[derive(Clone, Debug)]
pub enum ChatMessage {
    System(String),
    User(String),
    AssistantContent(String),
    AssistantToolCalls(Vec<ToolInvocation>),
    ToolResult(ToolResult),
}

#[derive(Clone, Debug)]
pub enum OpenAiAssistantOutput {
    Content(String),
    ToolCalls(Vec<ToolInvocation>),
}

pub struct OpenAiProtocol;

impl OpenAiProtocol {
    pub fn request_body(model: &str, messages: &[ChatMessage], tools: &[ToolSpec]) -> Value {
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
        } else {
            body["response_format"] = json!({ "type": "json_object" });
        }

        body
    }

    pub fn parse_response(value: Value) -> Result<OpenAiAssistantOutput, String> {
        let message = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .ok_or_else(|| "missing choices[0].message".to_string())?;

        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .map(Self::parse_tool_call)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        if !tool_calls.is_empty() {
            return Ok(OpenAiAssistantOutput::ToolCalls(tool_calls));
        }

        let content = message
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| "missing message.content".to_string())?;

        Ok(OpenAiAssistantOutput::Content(content.to_string()))
    }

    fn message_json(message: &ChatMessage) -> Value {
        match message {
            ChatMessage::System(content) => json!({
                "role": "system",
                "content": content,
            }),
            ChatMessage::User(content) => json!({
                "role": "user",
                "content": content,
            }),
            ChatMessage::AssistantContent(content) => json!({
                "role": "assistant",
                "content": content,
            }),
            ChatMessage::AssistantToolCalls(calls) => json!({
                "role": "assistant",
                "content": null,
                "tool_calls": calls.iter().map(Self::tool_call_json).collect::<Vec<_>>(),
            }),
            ChatMessage::ToolResult(result) => json!({
                "role": "tool",
                "tool_call_id": result.call_id,
                "content": result.content,
            }),
        }
    }

    fn tool_call_json(call: &ToolInvocation) -> Value {
        json!({
            "id": call.id,
            "type": "function",
            "function": {
                "name": call.name,
                "arguments": call.arguments.to_string(),
            }
        })
    }

    fn parse_tool_call(value: &Value) -> Result<ToolInvocation, String> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool_call missing id".to_string())?;
        let function = value
            .get("function")
            .ok_or_else(|| "tool_call missing function".to_string())?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool_call.function missing name".to_string())?;
        let args = function
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool_call.function missing arguments".to_string())?;
        let arguments = serde_json::from_str(args)
            .map_err(|err| format!("tool_call arguments were not JSON: {err}"))?;

        Ok(ToolInvocation {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn request_body_encodes_tools_without_policy() {
        let tools = vec![ToolSpec {
            name: "probe".to_string(),
            description: "Request evidence".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                }
            }),
        }];
        let body = OpenAiProtocol::request_body(
            "test-model",
            &[ChatMessage::User("ctx".to_string())],
            &tools,
        );

        assert_eq!(body["model"], "test-model");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "probe");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["command"]["type"],
            "string"
        );
    }

    #[test]
    fn parse_response_decodes_tool_calls() {
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "probe",
                            "arguments": "{\"name\":\"net_snmp\",\"command\":\"cat /proc/net/snmp\"}"
                        }
                    }]
                }
            }]
        });

        let output = OpenAiProtocol::parse_response(response).expect("response should parse");
        match output {
            OpenAiAssistantOutput::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "probe");
                assert_eq!(calls[0].arguments["command"], "cat /proc/net/snmp");
            }
            OpenAiAssistantOutput::Content(_) => panic!("expected tool calls"),
        }
    }

    #[test]
    fn parse_response_decodes_content() {
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "{\"next_step\":\"dry_run\"}"
                }
            }]
        });

        let output = OpenAiProtocol::parse_response(response).expect("response should parse");
        match output {
            OpenAiAssistantOutput::Content(content) => {
                assert_eq!(content, "{\"next_step\":\"dry_run\"}");
            }
            OpenAiAssistantOutput::ToolCalls(_) => panic!("expected content"),
        }
    }
}
