use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::act::ActResult;
use crate::activation::ActivationEvent;
use crate::observation::CoreSnapshot;
use crate::reasoning::ReasoningOutput;
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
        self.append(&format!(
            "{{\"ts\":{},\"kind\":\"act_result\",\"episode_id\":{},\"status\":\"{:?}\",\"message\":\"{}\",\"rollback_performed\":{}}}",
            now_ns(),
            episode.id,
            result.status,
            escape_json(&result.message),
            result.rollback_performed
        ))
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
    ) -> std::io::Result<()> {
        self.append(&format!(
            "{{\"ts\":{},\"kind\":\"episode_finished\",\"episode_id\":{},\"state\":\"{:?}\"}}",
            now_ns(),
            episode.id,
            state
        ))
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
