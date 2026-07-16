use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Scope {
    Host,
    Cgroup(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum EventSource {
    Cli,
    Program(String),
    Internal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
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
