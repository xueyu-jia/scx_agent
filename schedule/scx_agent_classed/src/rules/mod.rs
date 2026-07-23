// SPDX-License-Identifier: GPL-2.0

mod store;

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{bail, Result};
use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use store::RuleStore;
pub const RULES_SCHEMA_VERSION: u32 = 1;
pub const COMM_KEY_LEN: usize = crate::bpf_intf::agent_consts_AGENT_COMM_LEN as usize;
pub const MAX_COMM_LEN: usize = crate::control_wire::MAX_COMM_BYTES;
pub const MAX_RULES: usize = crate::bpf_intf::agent_consts_AGENT_MAX_RULES as usize;
const _: () = assert!(MAX_COMM_LEN == COMM_KEY_LEN - 1);

pub type RuleTable = BTreeMap<Comm, RuleClass>;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Comm(String);

impl Comm {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        crate::control_wire::validate_comm(&value).map_err(anyhow::Error::msg)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_bpf_key(&self) -> [u8; COMM_KEY_LEN] {
        let mut key = [0; COMM_KEY_LEN];
        key[..self.0.len()].copy_from_slice(self.0.as_bytes());
        key
    }
}

impl fmt::Display for Comm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl TryFrom<&str> for Comm {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for Comm {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for Comm {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleClass {
    Latency,
    Batch,
}

impl RuleClass {
    pub fn as_bpf_id(self) -> u32 {
        match self {
            Self::Latency => crate::bpf_intf::workload_class_CLASS_LATENCY,
            Self::Batch => crate::bpf_intf::workload_class_CLASS_BATCH,
        }
    }
}

impl fmt::Display for RuleClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latency => formatter.write_str("latency"),
            Self::Batch => formatter.write_str("batch"),
        }
    }
}

impl TryFrom<&str> for RuleClass {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "latency" => Ok(Self::Latency),
            "batch" => Ok(Self::Batch),
            other => bail!("unknown workload class '{other}', expected latency or batch"),
        }
    }
}

impl From<RuleClass> for crate::control_wire::RuleClass {
    fn from(class: RuleClass) -> Self {
        match class {
            RuleClass::Latency => Self::Latency,
            RuleClass::Batch => Self::Batch,
        }
    }
}

impl From<crate::control_wire::RuleClass> for RuleClass {
    fn from(class: crate::control_wire::RuleClass) -> Self {
        match class {
            crate::control_wire::RuleClass::Latency => Self::Latency,
            crate::control_wire::RuleClass::Batch => Self::Batch,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuleState {
    Absent,
    Present(RuleClass),
}

impl RuleState {
    pub fn from_option(class: Option<RuleClass>) -> Self {
        match class {
            Some(class) => Self::Present(class),
            None => Self::Absent,
        }
    }

    pub fn class(self) -> Option<RuleClass> {
        match self {
            Self::Absent => None,
            Self::Present(class) => Some(class),
        }
    }
}

impl From<RuleState> for crate::control_wire::RuleState {
    fn from(state: RuleState) -> Self {
        match state {
            RuleState::Absent => Self::absent(),
            RuleState::Present(class) => Self::present(class.into()),
        }
    }
}

impl TryFrom<crate::control_wire::RuleState> for RuleState {
    type Error = anyhow::Error;

    fn try_from(state: crate::control_wire::RuleState) -> Result<Self> {
        match (state.present, state.class) {
            (false, None) => Ok(Self::Absent),
            (true, Some(class)) => Ok(Self::Present(class.into())),
            (true, None) => bail!("class is required when present is true"),
            (false, Some(_)) => bail!("class must be omitted when present is false"),
        }
    }
}

impl Serialize for RuleState {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        crate::control_wire::RuleState::from(*self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RuleState {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let state = crate::control_wire::RuleState::deserialize(deserializer)?;
        Self::try_from(state).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    Base,
    Learned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct EffectiveRule {
    pub class: RuleClass,
    pub source: RuleSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuleSnapshot {
    revision: u64,
    base: RuleTable,
    learned: RuleTable,
    effective: RuleTable,
}

impl RuleSnapshot {
    pub fn new(base: RuleTable, learned: RuleTable, revision: u64) -> Result<Self> {
        for comm in learned.keys() {
            if base.contains_key(comm) {
                bail!("learned rule for comm '{comm}' conflicts with a read-only base rule");
            }
        }

        let mut effective = learned.clone();
        effective.extend(base.iter().map(|(comm, class)| (comm.clone(), *class)));
        if effective.len() > MAX_RULES {
            bail!(
                "effective rule table contains {} entries, maximum is {MAX_RULES}",
                effective.len()
            );
        }

        Ok(Self {
            revision,
            base,
            learned,
            effective,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn base(&self) -> &RuleTable {
        &self.base
    }

    pub fn learned(&self) -> &RuleTable {
        &self.learned
    }

    pub fn effective(&self) -> &RuleTable {
        &self.effective
    }

    pub fn learned_state(&self, comm: &Comm) -> RuleState {
        RuleState::from_option(self.learned.get(comm).copied())
    }

    pub fn effective_rule(&self, comm: &Comm) -> Option<EffectiveRule> {
        store::effective_rule(&self.base, &self.learned, comm)
    }

    pub fn canonical_learned_json(&self) -> Result<Vec<u8>> {
        store::canonical_document(self.revision, &self.learned)
    }
}

pub type RuleSet = RuleSnapshot;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CasStatus {
    Applied,
    Noop,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CasResult {
    pub status: CasStatus,
    pub comm: Comm,
    /// Learned state observed before the CAS.
    pub previous: RuleState,
    /// Learned state after the CAS. On conflict this equals `previous`.
    pub current: RuleState,
    /// Effective state after applying base-rule precedence.
    pub effective: RuleState,
    pub revision: u64,
}
