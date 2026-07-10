use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::activation::ActivationEvent;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Scope {
    Host,
    Cgroup(String),
}

impl Scope {
    pub fn as_json(&self) -> String {
        match self {
            Self::Host => "\"host\"".to_string(),
            Self::Cgroup(path) => format!("{{\"cgroup\":\"{}\"}}", escape_json(path)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    Sleeping,
    Active,
    Cooldown,
    Frozen,
}

#[derive(Clone, Debug)]
pub struct Episode {
    pub id: u128,
    pub started_ns: u128,
    pub activation: ActivationEvent,
}

impl Episode {
    pub fn new(activation: ActivationEvent) -> Self {
        let started_ns = now_ns();
        Self {
            id: started_ns,
            started_ns,
            activation,
        }
    }
}

pub fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

pub fn escape_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                use fmt::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}
