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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActivationRequest {
    pub request_id: String,
    pub wait: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_skills: Vec<String>,
    pub event: ActivationEvent,
}

impl ActivationRequest {
    pub fn new(request_id: String, wait: bool, event: ActivationEvent) -> Self {
        Self {
            request_id,
            wait,
            requested_skills: Vec::new(),
            event,
        }
    }

    pub fn with_requested_skills(mut self, requested_skills: Vec<String>) -> Self {
        self.requested_skills = requested_skills;
        self
    }

    pub fn fire_and_forget(event: ActivationEvent) -> Self {
        Self {
            request_id: format!("legacy-{}", event.timestamp_ns),
            wait: false,
            requested_skills: Vec::new(),
            event,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationOutcomeStatus {
    Committed,
    NoCommit,
    RecoveryRequired,
    Rejected,
    Error,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActivationResponse {
    pub version: u32,
    pub request_id: String,
    pub status: ActivationOutcomeStatus,
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<crate::runtime::EpisodeOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ActivationResponse {
    pub fn rejected(request_id: String, error: String) -> Self {
        Self {
            version: 1,
            request_id,
            status: ActivationOutcomeStatus::Rejected,
            accepted: false,
            episode: None,
            error: Some(error),
        }
    }

    pub fn error(request_id: String, error: String) -> Self {
        Self {
            version: 1,
            request_id,
            status: ActivationOutcomeStatus::Error,
            accepted: false,
            episode: None,
            error: Some(error),
        }
    }

    pub fn from_episode(request_id: String, episode: crate::runtime::EpisodeOutcome) -> Self {
        let status = match episode.phase {
            crate::domain::EpisodePhase::Committed => ActivationOutcomeStatus::Committed,
            crate::domain::EpisodePhase::RecoveryRequired => {
                ActivationOutcomeStatus::RecoveryRequired
            }
            _ => ActivationOutcomeStatus::NoCommit,
        };
        Self {
            version: 1,
            request_id,
            status,
            accepted: true,
            episode: Some(episode),
            error: None,
        }
    }
}
