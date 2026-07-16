use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::{EpisodeId, EpisodePhase};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub timestamp_ns: u128,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<EpisodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<EpisodePhase>,
    pub data: Value,
}

impl AuditRecord {
    pub fn runtime(event: impl Into<String>, data: Value) -> Self {
        Self {
            timestamp_ns: now_ns(),
            event: event.into(),
            episode_id: None,
            phase: None,
            data,
        }
    }

    pub fn episode(
        event: impl Into<String>,
        episode_id: EpisodeId,
        phase: EpisodePhase,
        data: Value,
    ) -> Self {
        Self {
            timestamp_ns: now_ns(),
            event: event.into(),
            episode_id: Some(episode_id),
            phase: Some(phase),
            data,
        }
    }
}

pub trait AuditSink {
    fn record(&mut self, record: &AuditRecord) -> io::Result<()>;
}

pub struct JsonlAuditSink {
    path: PathBuf,
}

impl JsonlAuditSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl AuditSink for JsonlAuditSink {
    fn record(&mut self, record: &AuditRecord) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, record).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()
    }
}

fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn jsonl_sink_writes_structured_episode_records() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-audit-sink-{}-{}.jsonl",
            std::process::id(),
            now_ns()
        ));
        let mut sink = JsonlAuditSink::new(&path);
        sink.record(&AuditRecord::episode(
            "candidate_frozen",
            EpisodeId::new(9),
            EpisodePhase::CommitPending,
            serde_json::json!({"digest": "sha256:test"}),
        ))
        .unwrap();

        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["event"], "candidate_frozen");
        assert_eq!(value["episode_id"], 9);
        assert_eq!(value["phase"], "commit_pending");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn jsonl_sink_refuses_to_follow_audit_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-audit-symlink-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir_all(&root).unwrap();
        let target = root.join("target.jsonl");
        let link = root.join("audit.jsonl");
        fs::write(&target, "preserve\n").unwrap();
        symlink(&target, &link).unwrap();
        let mut sink = JsonlAuditSink::new(&link);

        assert!(sink
            .record(&AuditRecord::runtime("must_not_write", Value::Null))
            .is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "preserve\n");
        let _ = fs::remove_dir_all(root);
    }
}
