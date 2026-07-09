mod openai;
mod openai_reasoner;
mod plan;

pub use openai_reasoner::OpenAiReasoner;
pub use plan::{ActionPlan, Plan, ReasoningOutput};

use crate::config::LlmConfig;
use crate::observation::CoreSnapshot;
use crate::reasoning::openai::OpenAiConfig;
use crate::tools::{ToolResult, ToolSpec};
use crate::types::Episode;

pub enum ReasoningInput<'a> {
    Initial {
        episode: &'a Episode,
        snapshot: &'a CoreSnapshot,
        tools: &'a [ToolSpec],
    },
    ToolResults(&'a [ToolResult]),
}

pub trait Reasoner {
    fn reason(&mut self, input: ReasoningInput<'_>) -> ReasoningOutput;
}

pub fn build_reasoner(config: &LlmConfig) -> Box<dyn Reasoner> {
    Box::new(OpenAiReasoner::new(OpenAiConfig::from_config(config)))
}
