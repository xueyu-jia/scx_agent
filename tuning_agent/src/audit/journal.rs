use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::act::ActResult;
use crate::activation::ActivationEvent;
use crate::observation::CoreSnapshot;
use crate::reasoning::ReasoningOutput;
use crate::runtime::episode_state::EpisodePhase;
use crate::tools::ToolResult;
use crate::types::{escape_json, now_ns, AgentState, Episode};

pub struct AuditJournal {
    path: PathBuf,
}

impl AuditJournal {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn record_activation_rejected(
        &mut self,
        event: &ActivationEvent,
        state: AgentState,
    ) -> std::io::Result<()> {
        self.append(&format!(
            "{{\"ts\":{},\"kind\":\"activation_rejected\",\"event_type\":\"{}\",\"state\":\"{:?}\"}}",
            now_ns(),
            escape_json(&event.event_type),
            state
        ))
    }

    pub fn record_episode_started(
        &mut self,
        episode: &Episode,
        state: AgentState,
    ) -> std::io::Result<()> {
        self.append(&format!(
            "{{\"ts\":{},\"kind\":\"episode_started\",\"episode_id\":{},\"started_ns\":{},\"activation_ts\":{},\"state\":\"{:?}\",\"event_type\":\"{}\",\"source\":{},\"severity\":\"{}\",\"scope\":{}}}",
            now_ns(),
            episode.id,
            episode.started_ns,
            episode.activation.timestamp_ns,
            state,
            escape_json(&episode.activation.event_type),
            episode.activation.source.as_json(),
            episode.activation.severity.as_str(),
            episode.activation.scope.as_json()
        ))
    }

    pub fn record_snapshot(
        &mut self,
        episode: &Episode,
        snapshot: &CoreSnapshot,
    ) -> std::io::Result<()> {
        self.append(&format!(
            "{{\"ts\":{},\"kind\":\"core_snapshot\",\"episode_id\":{},\"snapshot_ts\":{},\"loadavg\":\"{}\",\"stat_bytes\":{},\"meminfo_bytes\":{},\"psi_cpu_bytes\":{},\"psi_memory_bytes\":{},\"psi_io_bytes\":{},\"net_snmp_bytes\":{},\"softnet_bytes\":{}}}",
            now_ns(),
            episode.id,
            snapshot.timestamp_ns,
            escape_json(&snapshot.loadavg),
            snapshot.stat.len(),
            snapshot.meminfo.len(),
            snapshot.pressure_cpu.len(),
            snapshot.pressure_memory.len(),
            snapshot.pressure_io.len(),
            snapshot.net_snmp.len(),
            snapshot.net_softnet_stat.len()
        ))
    }

    pub fn record_reasoning(
        &mut self,
        episode: &Episode,
        reasoning: &ReasoningOutput,
    ) -> std::io::Result<()> {
        let plan = format!("{:?}", reasoning.plan);
        self.append(&format!(
            "{{\"ts\":{},\"kind\":\"reasoning\",\"episode_id\":{},\"raw_json\":\"{}\",\"plan\":\"{}\"}}",
            now_ns(),
            episode.id,
            escape_json(&reasoning.raw_json),
            escape_json(&plan)
        ))
    }

    pub fn record_act_result(
        &mut self,
        episode: &Episode,
        result: &ActResult,
    ) -> std::io::Result<()> {
        self.append(
            &serde_json::json!({
                "ts": now_ns(),
                "kind": "act_result",
                "episode_id": episode.id,
                "status": format!("{:?}", result.status),
                "message": result.message,
                "rollback_required": result.rollback_required,
                "rollback_attempted": result.rollback_attempted,
                "rollback_succeeded": result.rollback_succeeded,
                "rollback_error": result.rollback_error,
            })
            .to_string(),
        )
    }

    pub fn record_tool_result(
        &mut self,
        episode: &Episode,
        result: &ToolResult,
    ) -> std::io::Result<()> {
        self.append(&format!(
            "{{\"ts\":{},\"kind\":\"tool_result\",\"episode_id\":{},\"call_id\":\"{}\",\"tool\":\"{}\",\"ok\":{},\"content_bytes\":{},\"content\":\"{}\"}}",
            now_ns(),
            episode.id,
            escape_json(&result.call_id),
            escape_json(&result.name),
            result.ok,
            result.content.len(),
            escape_json(&result.content)
        ))
    }

    pub fn record_episode_finished(
        &mut self,
        episode: &Episode,
        state: AgentState,
        phase: EpisodePhase,
        result: &ActResult,
    ) -> std::io::Result<()> {
        self.append(
            &serde_json::json!({
                "ts": now_ns(),
                "kind": "episode_finished",
                "episode_id": episode.id,
                "state": format!("{state:?}"),
                "phase": format!("{phase:?}"),
                "status": format!("{:?}", result.status),
            })
            .to_string(),
        )
    }

    fn append(&mut self, line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::act::ActStatus;
    use crate::activation::{EventSource, Severity};
    use crate::types::Scope;

    #[test]
    fn rollback_and_final_state_fields_are_structured() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-audit-rollback-{}-{}.jsonl",
            std::process::id(),
            now_ns()
        ));
        let episode = Episode::new(ActivationEvent::new(
            EventSource::Cli,
            "test".to_string(),
            Severity::Info,
            Scope::Host,
        ));
        let result = ActResult {
            status: ActStatus::Rejected,
            message: "rollback failed".to_string(),
            rollback_required: true,
            rollback_attempted: true,
            rollback_succeeded: Some(false),
            rollback_error: Some("permission denied".to_string()),
        };
        let mut journal = AuditJournal::new(path.clone());

        journal.record_act_result(&episode, &result).unwrap();
        journal
            .record_episode_finished(&episode, AgentState::Frozen, EpisodePhase::Frozen, &result)
            .unwrap();

        let records = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records[0]["rollback_required"], true);
        assert_eq!(records[0]["rollback_attempted"], true);
        assert_eq!(records[0]["rollback_succeeded"], false);
        assert_eq!(records[0]["rollback_error"], "permission denied");
        assert_eq!(records[1]["state"], "Frozen");
        assert_eq!(records[1]["phase"], "Frozen");
        assert_eq!(records[1]["status"], "Rejected");
        let _ = fs::remove_file(path);
    }
}
