pub mod control;
pub mod journal;
pub mod provider;
pub mod rpc;
pub mod schema;
pub mod workload;

pub use scx_agent_classed_control as control_wire;

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

pub fn validate_id(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("{label} is invalid");
    }
    Ok(())
}

pub fn validate_comm(comm: &str) -> Result<()> {
    control_wire::validate_comm(comm).map_err(anyhow::Error::msg)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
