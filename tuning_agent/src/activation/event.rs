use crate::types::{escape_json, now_ns, Scope};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum EventSource {
    Cli,
    Ebpf,
    Program(String),
    Internal,
}

impl EventSource {
    pub fn as_json(&self) -> String {
        match self {
            Self::Cli => "\"cli\"".to_string(),
            Self::Ebpf => "\"ebpf\"".to_string(),
            Self::Program(name) => format!("{{\"program\":\"{}\"}}", escape_json(name)),
            Self::Internal => "\"internal\"".to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActivationEvent {
    pub source: EventSource,
    pub event_type: String,
    pub severity: Severity,
    pub scope: Scope,
    pub timestamp_ns: u128,
    pub evidence: serde_json::Value,
}

impl ActivationEvent {
    pub fn new(source: EventSource, event_type: String, severity: Severity, scope: Scope) -> Self {
        Self {
            source,
            event_type,
            severity,
            scope,
            timestamp_ns: now_ns(),
            evidence: serde_json::json!({}),
        }
    }
}
