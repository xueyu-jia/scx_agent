// SPDX-License-Identifier: GPL-2.0

use serde::{Deserialize, Serialize};

pub const CONTROL_VERSION: u32 = 1;
pub const MAX_COMM_BYTES: usize = 15;
pub const MAX_REQUEST_ID_BYTES: usize = 256;
pub const MAX_SNAPSHOT_COMMS: usize = 128;
pub const MAX_CONTROL_FRAME_BYTES: u64 = 64 * 1024;

pub fn validate_comm(comm: &str) -> Result<(), String> {
    if comm.is_empty() {
        return Err("comm must not be empty".to_string());
    }
    if comm.len() > MAX_COMM_BYTES {
        return Err(format!(
            "comm '{comm}' is {} bytes; Linux comm supports at most {MAX_COMM_BYTES}",
            comm.len()
        ));
    }
    if comm.as_bytes().contains(&0) {
        return Err("comm must not contain a NUL byte".to_string());
    }
    if comm.chars().any(char::is_control) {
        return Err("comm must not contain control characters".to_string());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleClass {
    Latency,
    Batch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleState {
    pub present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<RuleClass>,
}

impl RuleState {
    pub fn absent() -> Self {
        Self {
            present: false,
            class: None,
        }
    }

    pub fn present(class: RuleClass) -> Self {
        Self {
            present: true,
            class: Some(class),
        }
    }

    pub fn is_valid(&self) -> bool {
        self.present == self.class.is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlOp {
    GetRule,
    Snapshot,
    CompareAndSetRule,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    pub version: u32,
    pub request_id: String,
    pub op: ControlOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comms: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<RuleState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired: Option<RuleState>,
}

impl ControlRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != CONTROL_VERSION {
            return Err(format!(
                "unsupported control version {}; expected {CONTROL_VERSION}",
                self.version
            ));
        }
        if self.request_id.is_empty()
            || self.request_id.len() > MAX_REQUEST_ID_BYTES
            || self.request_id.chars().any(char::is_control)
        {
            return Err("invalid control request_id".to_string());
        }

        let valid = match self.op {
            ControlOp::GetRule => {
                self.comm.as_deref().is_some_and(|comm| validate_comm(comm).is_ok())
                    && self.comms.is_none()
                    && self.expected.is_none()
                    && self.desired.is_none()
            }
            ControlOp::Snapshot => {
                self.comm.is_none()
                    && self.comms.as_ref().is_some_and(|comms| {
                        !comms.is_empty()
                            && comms.len() <= MAX_SNAPSHOT_COMMS
                            && comms.iter().all(|comm| validate_comm(comm).is_ok())
                    })
                    && self.expected.is_none()
                    && self.desired.is_none()
            }
            ControlOp::CompareAndSetRule => {
                self.comm.as_deref().is_some_and(|comm| validate_comm(comm).is_ok())
                    && self.comms.is_none()
                    && self.expected.as_ref().is_some_and(RuleState::is_valid)
                    && self.desired.as_ref().is_some_and(RuleState::is_valid)
            }
        };
        if !valid {
            return Err("control request fields do not match operation".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlStatus {
    Ok,
    Applied,
    Noop,
    Conflict,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    Base,
    Learned,
    Default,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleObservation {
    pub comm: String,
    pub class: RuleClass,
    pub source: RuleSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_class: Option<RuleClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persisted_class: Option<RuleClass>,
    pub consistent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlStats {
    pub task_state_errors: u64,
    pub rule_refresh_deferred: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlResponse {
    pub version: u32,
    pub request_id: String,
    pub status: ControlStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<RuleState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleObservation>,
    pub revision: u64,
    pub rules_seq: u64,
    pub effective_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<ControlStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
