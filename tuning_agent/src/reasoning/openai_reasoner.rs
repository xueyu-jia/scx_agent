use crate::observation::CoreSnapshot;
use crate::reasoning::openai::{
    ChatMessage, OpenAiAssistantOutput, OpenAiCompatibleClient, OpenAiConfig,
};
use crate::reasoning::{ActionPlan, Plan, Reasoner, ReasoningInput, ReasoningOutput};
use crate::tools::ToolSpec;
use crate::types::{escape_json, Episode};

pub struct OpenAiReasoner {
    client: Result<OpenAiCompatibleClient, String>,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolSpec>,
}

impl OpenAiReasoner {
    pub fn new(config: Result<OpenAiConfig, String>) -> Self {
        Self {
            client: config.map(OpenAiCompatibleClient::new),
            messages: Vec::with_capacity(8),
            tools: Vec::new(),
        }
    }

    fn initialize(&mut self, episode: &Episode, snapshot: &CoreSnapshot, tools: &[ToolSpec]) {
        self.tools.clear();
        self.tools.extend_from_slice(tools);
        self.messages.clear();
        self.messages.push(ChatMessage::System(system_prompt()));
        self.messages
            .push(ChatMessage::User(context_json(episode, snapshot)));
    }

    fn call(&mut self) -> ReasoningOutput {
        let client = match &self.client {
            Ok(client) => client,
            Err(err) => {
                return ReasoningOutput {
                    raw_json: format!(
                        "{{\"error\":\"llm_not_configured\",\"message\":\"{}\"}}",
                        escape_json(err)
                    ),
                    plan: Plan::DryRun(ActionPlan {
                        summary: "OpenAI-compatible reasoner is not configured".to_string(),
                        expected_effect: "no system state change".to_string(),
                    }),
                };
            }
        };

        match client.complete(&self.messages, &self.tools) {
            Ok(OpenAiAssistantOutput::ToolCalls(calls)) => {
                self.messages
                    .push(ChatMessage::AssistantToolCalls(calls.clone()));
                ReasoningOutput {
                    raw_json: serde_json::to_string(&calls).unwrap_or_else(|_| "[]".to_string()),
                    plan: Plan::ToolCalls(calls),
                }
            }
            Ok(OpenAiAssistantOutput::Content(content)) => {
                self.messages
                    .push(ChatMessage::AssistantContent(content.clone()));
                ReasoningOutput {
                    raw_json: content,
                    plan: Plan::DryRun(ActionPlan {
                        summary: "model returned final plan JSON; execution remains dry-run until schema mapping is enabled".to_string(),
                        expected_effect: "no system state change".to_string(),
                    }),
                }
            }
            Err(err) => ReasoningOutput {
                raw_json: format!(
                    "{{\"error\":\"openai_compatible_call_failed\",\"message\":\"{}\"}}",
                    escape_json(&err)
                ),
                plan: Plan::DryRun(ActionPlan {
                    summary: "LLM call failed".to_string(),
                    expected_effect: "no system state change".to_string(),
                }),
            },
        }
    }
}

impl Reasoner for OpenAiReasoner {
    fn reason(&mut self, input: ReasoningInput<'_>) -> ReasoningOutput {
        match input {
            ReasoningInput::Initial {
                episode,
                snapshot,
                tools,
            } => {
                self.initialize(episode, snapshot, tools);
                self.call()
            }
            ReasoningInput::ToolResults(results) => {
                for result in results {
                    self.messages.push(ChatMessage::ToolResult(result.clone()));
                }
                self.call()
            }
        }
    }
}

fn system_prompt() -> String {
    include_str!("../system_prompt.md").trim().to_string()
}

fn context_json(episode: &Episode, snapshot: &CoreSnapshot) -> String {
    format!(
        "{{\"episode_id\":{},\"activation\":{{\"event_type\":\"{}\",\"source\":{},\"severity\":\"{}\",\"scope\":{}}},\"snapshot\":{{\"timestamp_ns\":{},\"loadavg\":\"{}\",\"stat_bytes\":{},\"meminfo_bytes\":{},\"psi_cpu\":\"{}\",\"psi_memory\":\"{}\",\"psi_io\":\"{}\",\"net_snmp_bytes\":{},\"softnet_bytes\":{}}}}}",
        episode.id,
        escape_json(&episode.activation.event_type),
        episode.activation.source.as_json(),
        episode.activation.severity.as_str(),
        episode.activation.scope.as_json(),
        snapshot.timestamp_ns,
        escape_json(&snapshot.loadavg),
        snapshot.stat.len(),
        snapshot.meminfo.len(),
        escape_json(&snapshot.pressure_cpu),
        escape_json(&snapshot.pressure_memory),
        escape_json(&snapshot.pressure_io),
        snapshot.net_snmp.len(),
        snapshot.net_softnet_stat.len()
    )
}
