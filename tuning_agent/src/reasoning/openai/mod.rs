mod client;
mod protocol;

pub use client::{OpenAiCompatibleClient, OpenAiConfig};
pub use protocol::{ChatMessage, OpenAiAssistantOutput, OpenAiProtocol};
