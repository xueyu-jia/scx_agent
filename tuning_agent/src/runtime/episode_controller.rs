use std::time::Duration;

use serde_json::Value;

use crate::act::{
    ActKernel, ActKernelConfig, ActResult, ActStatus, CommandRequest, ExperimentWriteRequest,
};
use crate::audit::AuditJournal;
use crate::config::Config;
use crate::evaluate::{
    EvaluationController, EvaluationKernel, EvaluationKernelConfig, EvaluationOutcome,
    EvaluationPlan,
};
use crate::observation::Observation;
use crate::reasoning::{build_reasoner, Plan, ReasoningInput};
use crate::runtime::episode_state::{EpisodePhase, EpisodeState};
use crate::tools::{ToolInvocation, ToolRegistry, ToolResult};
use crate::types::{AgentState, Episode};

pub struct EpisodeController<'a> {
    config: Config,
    observation: Observation,
    act_kernel: ActKernel,
    evaluation_controller: EvaluationController,
    tool_registry: ToolRegistry,
    audit: &'a mut AuditJournal,
}

pub struct EpisodeOutcome {
    pub episode: Episode,
    pub phase: EpisodePhase,
    pub act_result: ActResult,
}

impl<'a> EpisodeController<'a> {
    pub fn new(config: Config, audit: &'a mut AuditJournal) -> Self {
        Self {
            act_kernel: ActKernel::new(ActKernelConfig::from_config(&config.command)),
            evaluation_controller: EvaluationController::new(EvaluationKernel::new(
                EvaluationKernelConfig::from_config(&config.evaluation),
            )),
            config,
            observation: Observation,
            tool_registry: ToolRegistry::builtin(),
            audit,
        }
    }

    pub fn run(
        &mut self,
        episode: Episode,
        agent_state: AgentState,
    ) -> std::io::Result<EpisodeOutcome> {
        let mut state = EpisodeState::new(episode);
        self.audit
            .record_episode_started(&state.episode, agent_state)?;

        let snapshot = self.observation.core_snapshot()?;
        self.audit.record_snapshot(&state.episode, &snapshot)?;

        let mut reasoner = build_reasoner(&self.config.llm);
        let mut tool_results: Vec<ToolResult> = Vec::new();

        for round in 0..self.config.reasoning.max_rounds {
            let reasoning = if round == 0 {
                reasoner.reason(ReasoningInput::Initial {
                    episode: &state.episode,
                    snapshot: &snapshot,
                    tools: self.tool_registry.tools(),
                })
            } else {
                reasoner.reason(ReasoningInput::ToolResults(&tool_results))
            };
            self.audit.record_reasoning(&state.episode, &reasoning)?;

            match &reasoning.plan {
                Plan::ToolCalls(calls) => {
                    tool_results.clear();
                    for call in calls {
                        let result = self.dispatch_tool_call(&mut state, call);
                        self.audit.record_tool_result(&state.episode, &result)?;
                        tool_results.push(result);
                    }
                    if is_terminal_phase(state.phase) {
                        break;
                    }
                }
                Plan::DryRun(action) => {
                    let result = ActResult::without_rollback(
                        ActStatus::DryRun,
                        format!(
                            "dry-run accepted: {}; expected_effect={}",
                            action.summary, action.expected_effect
                        ),
                    );
                    self.audit.record_act_result(&state.episode, &result)?;
                    break;
                }
            }
        }

        let finish_result = self.finish_episode(&mut state);
        self.audit
            .record_act_result(&state.episode, &finish_result)?;

        println!(
            "episode {} finished; phase={:?}",
            state.episode.id, state.phase
        );
        Ok(EpisodeOutcome {
            episode: state.episode,
            phase: state.phase,
            act_result: finish_result,
        })
    }

    fn dispatch_tool_call(
        &mut self,
        state: &mut EpisodeState,
        invocation: &ToolInvocation,
    ) -> ToolResult {
        if is_terminal_phase(state.phase) {
            return ToolResult::rejected(
                invocation.id.clone(),
                invocation.name.clone(),
                format!("tool call not allowed in terminal phase {:?}", state.phase),
            );
        }

        match invocation.name.as_str() {
            "probe" => self.execute_probe(invocation),
            "experiment_write" => self.execute_experiment_write(state, invocation),
            "commit" => self.execute_commit(state, invocation),
            _ => ToolResult::rejected(
                invocation.id.clone(),
                invocation.name.clone(),
                format!("unknown tool '{}'", invocation.name),
            ),
        }
    }

    fn execute_probe(&self, invocation: &ToolInvocation) -> ToolResult {
        let probe_name = match invocation.arguments.get("name").and_then(|v| v.as_str()) {
            Some(name) if !name.is_empty() => name,
            _ => {
                return ToolResult::rejected(
                    invocation.id.clone(),
                    invocation.name.clone(),
                    "probe.name is required".to_string(),
                )
            }
        };
        let request = match command_request_from_arguments(&invocation.arguments, "probe.command") {
            Ok(request) => request,
            Err(err) => {
                return ToolResult::rejected(invocation.id.clone(), invocation.name.clone(), err)
            }
        };

        match self.act_kernel.execute_command(&request) {
            Ok(report) => ToolResult::ok(
                invocation.id.clone(),
                invocation.name.clone(),
                serde_json::json!({
                    "probe": probe_name,
                    "kind": "command",
                    "result": report.to_json_value(self.act_kernel.output_limit()),
                })
                .to_string(),
            ),
            Err(err) => ToolResult::rejected(
                invocation.id.clone(),
                invocation.name.clone(),
                format!("probe command execution failed: {err}"),
            ),
        }
    }

    fn execute_experiment_write(
        &mut self,
        state: &mut EpisodeState,
        invocation: &ToolInvocation,
    ) -> ToolResult {
        if matches!(state.phase, EpisodePhase::Committed | EpisodePhase::Frozen) {
            return ToolResult::rejected(
                invocation.id.clone(),
                invocation.name.clone(),
                format!("experiment not allowed in phase {:?}", state.phase),
            );
        }

        let request = match ExperimentWriteRequest::from_json(&invocation.arguments) {
            Ok(request) => request,
            Err(err) => {
                return ToolResult::rejected(invocation.id.clone(), invocation.name.clone(), err)
            }
        };

        match self.act_kernel.experiment_write(&request) {
            Ok(report) => {
                state.set_phase(EpisodePhase::Experimenting);
                ToolResult::ok(
                    invocation.id.clone(),
                    invocation.name.clone(),
                    serde_json::to_string(&report)
                        .unwrap_or_else(|err| format!("serialization failed: {err}")),
                )
            }
            Err(err) if self.act_kernel.has_recovery_required() => {
                match self.rollback_to_clean(state) {
                    Ok(rollback) => ToolResult::failed(
                        invocation.id.clone(),
                        invocation.name.clone(),
                        format!("experiment write failed: {err}; rollback completed: {rollback}"),
                    ),
                    Err(rollback_error) => ToolResult::failed(
                        invocation.id.clone(),
                        invocation.name.clone(),
                        format!(
                            "experiment write failed: {err}; rollback failed: {rollback_error}"
                        ),
                    ),
                }
            }
            Err(err) => ToolResult::rejected(
                invocation.id.clone(),
                invocation.name.clone(),
                format!("experiment write failed: {err}"),
            ),
        }
    }

    fn execute_commit(
        &mut self,
        state: &mut EpisodeState,
        invocation: &ToolInvocation,
    ) -> ToolResult {
        if matches!(state.phase, EpisodePhase::Committed | EpisodePhase::Frozen) {
            return ToolResult::rejected(
                invocation.id.clone(),
                invocation.name.clone(),
                format!("commit not allowed in phase {:?}", state.phase),
            );
        }

        state.set_phase(EpisodePhase::CommitPending);

        if !self.act_kernel.has_experiment_writes() {
            let rollback = self.rollback_to_clean(state);
            return ToolResult::failed(
                invocation.id.clone(),
                invocation.name.clone(),
                serde_json::json!({
                    "committed": false,
                    "result": "NoSignal",
                    "reason": "commit requires at least one experiment write",
                    "rollback": rollback.unwrap_or_else(|err| format!("rollback failed: {err}")),
                    "phase": format!("{:?}", state.phase),
                })
                .to_string(),
            );
        }

        let plan = match EvaluationPlan::from_commit_arguments(&invocation.arguments) {
            Ok(plan) => plan,
            Err(err) => {
                let rollback = self.rollback_to_clean(state);
                return ToolResult::failed(
                    invocation.id.clone(),
                    invocation.name.clone(),
                    serde_json::json!({
                        "committed": false,
                        "result": "NoSignal",
                        "reason": err,
                        "rollback": rollback.unwrap_or_else(|err| format!("rollback failed: {err}")),
                        "phase": format!("{:?}", state.phase),
                    })
                    .to_string(),
                );
            }
        };
        state.commit_request = Some(plan.clone());

        match self.evaluation_controller.validate_commit(
            &plan,
            &mut self.act_kernel,
            &self.observation,
        ) {
            EvaluationOutcome::Accepted(report) => {
                if let Some(decision) = &report.decision {
                    state.evaluation_decision = Some(decision.clone());
                }
                match self.act_kernel.finalize_commit(&plan.keep_writes) {
                    Ok(finalize) => {
                        state.set_phase(EpisodePhase::Committed);
                        ToolResult::ok(
                            invocation.id.clone(),
                            invocation.name.clone(),
                            serde_json::json!({
                                "committed": true,
                                "validation": "baseline_candidate",
                                "report": report,
                                "finalize": finalize,
                                "phase": format!("{:?}", state.phase),
                            })
                            .to_string(),
                        )
                    }
                    Err(err) => {
                        let rollback = self.rollback_to_clean(state);
                        ToolResult::failed(
                            invocation.id.clone(),
                            invocation.name.clone(),
                            match rollback {
                                Ok(final_rollback) => serde_json::json!({
                                    "committed": false,
                                    "validation": "baseline_candidate",
                                    "report": report,
                                    "finalize_error": err,
                                    "final_rollback": final_rollback,
                                    "phase": format!("{:?}", state.phase),
                                }),
                                Err(rollback_error) => serde_json::json!({
                                    "committed": false,
                                    "validation": "baseline_candidate",
                                    "report": report,
                                    "finalize_error": err,
                                    "final_rollback_error": rollback_error,
                                    "phase": format!("{:?}", state.phase),
                                }),
                            }
                            .to_string(),
                        )
                    }
                }
            }
            EvaluationOutcome::Rejected(report) => {
                if let Some(decision) = &report.decision {
                    state.evaluation_decision = Some(decision.clone());
                }
                match self.rollback_to_clean(state) {
                    Ok(final_rollback) => ToolResult::failed(
                        invocation.id.clone(),
                        invocation.name.clone(),
                        serde_json::json!({
                            "committed": false,
                            "validation": "baseline_candidate",
                            "report": report,
                            "final_rollback": final_rollback,
                            "phase": format!("{:?}", state.phase),
                        })
                        .to_string(),
                    ),
                    Err(err) => ToolResult::failed(
                        invocation.id.clone(),
                        invocation.name.clone(),
                        serde_json::json!({
                            "committed": false,
                            "validation": "baseline_candidate",
                            "report": report,
                            "final_rollback_error": err,
                            "phase": format!("{:?}", state.phase),
                        })
                        .to_string(),
                    ),
                }
            }
            EvaluationOutcome::Inconclusive(report) => {
                if let Some(decision) = &report.decision {
                    state.evaluation_decision = Some(decision.clone());
                }
                match self.rollback_to_clean(state) {
                    Ok(final_rollback) => ToolResult::failed(
                        invocation.id.clone(),
                        invocation.name.clone(),
                        serde_json::json!({
                            "committed": false,
                            "validation": "baseline_candidate",
                            "result": "Inconclusive",
                            "report": report,
                            "final_rollback": final_rollback,
                            "phase": format!("{:?}", state.phase),
                        })
                        .to_string(),
                    ),
                    Err(err) => ToolResult::failed(
                        invocation.id.clone(),
                        invocation.name.clone(),
                        serde_json::json!({
                            "committed": false,
                            "validation": "baseline_candidate",
                            "result": "Inconclusive",
                            "report": report,
                            "final_rollback_error": err,
                            "phase": format!("{:?}", state.phase),
                        })
                        .to_string(),
                    ),
                }
            }
            EvaluationOutcome::Frozen(report) => {
                let rollback = self.rollback_to_clean(state);
                ToolResult::failed(
                    invocation.id.clone(),
                    invocation.name.clone(),
                    match rollback {
                        Ok(final_rollback) => serde_json::json!({
                            "committed": false,
                            "validation": "baseline_candidate",
                            "report": report,
                            "final_rollback": final_rollback,
                            "phase": format!("{:?}", state.phase),
                        }),
                        Err(rollback_error) => serde_json::json!({
                            "committed": false,
                            "validation": "baseline_candidate",
                            "report": report,
                            "final_rollback_error": rollback_error,
                            "phase": format!("{:?}", state.phase),
                        }),
                    }
                    .to_string(),
                )
            }
        }
    }

    fn finish_episode(&mut self, state: &mut EpisodeState) -> ActResult {
        match state.phase {
            EpisodePhase::Committed => Self::act_result_from_state(
                state,
                ActStatus::Completed,
                "episode committed; uncommitted experiment writes restored".to_string(),
            ),
            EpisodePhase::Frozen => Self::act_result_from_state(
                state,
                ActStatus::Rejected,
                "episode frozen after execution failure".to_string(),
            ),
            _ if !self.act_kernel.has_experiment_writes() => Self::act_result_from_state(
                state,
                ActStatus::Completed,
                format!(
                    "episode ended in {:?}; no experiments to rollback",
                    state.phase
                ),
            ),
            _ => match self.rollback_to_clean(state) {
                Ok(summary) => Self::act_result_from_state(
                    state,
                    ActStatus::Completed,
                    format!("episode ended without commit; rollback executed: {summary}"),
                ),
                Err(err) => Self::act_result_from_state(
                    state,
                    ActStatus::Rejected,
                    format!("rollback failed; episode frozen: {err}"),
                ),
            },
        }
    }

    fn rollback_to_clean(&mut self, state: &mut EpisodeState) -> Result<String, String> {
        let rollback_required = self.act_kernel.has_experiment_writes();
        if rollback_required {
            state.rollback_required = true;
            state.rollback_attempted = true;
        }
        match self.act_kernel.discard_episode_writes() {
            Ok(report) => {
                if rollback_required {
                    state.rollback_succeeded = Some(true);
                    state.rollback_error = None;
                }
                let summary = serde_json::to_string(&report)
                    .unwrap_or_else(|err| format!("serialization failed: {err}"));
                state.commit_request = None;
                state.set_phase(EpisodePhase::Clean);
                Ok(summary)
            }
            Err(err) => {
                if rollback_required {
                    state.rollback_succeeded = Some(false);
                    state.rollback_error = Some(err.clone());
                }
                state.set_phase(EpisodePhase::Frozen);
                Err(err)
            }
        }
    }

    fn act_result_from_state(
        state: &EpisodeState,
        status: ActStatus,
        message: String,
    ) -> ActResult {
        ActResult {
            status,
            message,
            rollback_required: state.rollback_required,
            rollback_attempted: state.rollback_attempted,
            rollback_succeeded: state.rollback_succeeded,
            rollback_error: state.rollback_error.clone(),
        }
    }
}

fn command_request_from_arguments(
    arguments: &Value,
    field: &str,
) -> Result<CommandRequest, String> {
    let command = arguments
        .get("command")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{field} is required"))?;
    let mut request = CommandRequest::new(command.to_string());
    request.timeout = arguments
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis);
    request.working_dir = arguments
        .get("working_dir")
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    Ok(request)
}

fn is_terminal_phase(phase: EpisodePhase) -> bool {
    matches!(phase, EpisodePhase::Committed | EpisodePhase::Frozen)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::activation::{ActivationEvent, EventSource, Severity};
    use crate::types::Scope;

    #[test]
    fn commit_applies_only_commit_candidate_before_accepting() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-controller-commit-{}",
            std::process::id()
        ));
        std::fs::write(&path, "old\n").unwrap();
        let mut audit = AuditJournal::new(temp_audit_path("accept"));
        let mut controller = EpisodeController::new(test_config(), &mut audit);
        let mut state = EpisodeState::new(test_episode());

        let experiment = experiment_invocation(&path, "experiment", "test commit candidate");
        let result = controller.execute_experiment_write(&mut state, &experiment);
        assert!(result.ok);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "experiment\n");

        let commit = commit_invocation(json!({
            "reason": "test deterministic candidate validation",
            "keep_writes": [
                {
                    "target": {
                        "kind": "file",
                        "path": path.display().to_string()
                    },
                    "value": "experiment"
                }
            ],
            "measurement": {
                "command": "printf '{\"test_metric\":1}\n'",
                "schema": { "test_metric": "number" },
                "timeout_ms": 1000
            },
            "primary_metrics": [
                {
                    "metric": "test_metric",
                    "op": "current_ge",
                    "value": 0
                }
            ],
            "window_seconds": 0,
            "settle_seconds": 0
        }));

        let result = controller.execute_commit(&mut state, &commit);
        assert!(result.ok, "{}", result.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "experiment\n");
        assert_eq!(state.phase, EpisodePhase::Committed);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn failed_commit_rolls_back_replayed_experiment() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-controller-failed-commit-{}",
            std::process::id()
        ));
        std::fs::write(&path, "old\n").unwrap();
        let mut audit = AuditJournal::new(temp_audit_path("reject"));
        let mut controller = EpisodeController::new(test_config(), &mut audit);
        let mut state = EpisodeState::new(test_episode());

        let experiment = experiment_invocation(&path, "experiment", "test failed commit rollback");
        let result = controller.execute_experiment_write(&mut state, &experiment);
        assert!(result.ok);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "experiment\n");

        let commit = commit_invocation(json!({
            "reason": "test deterministic reject rollback",
            "keep_writes": [
                {
                    "target": {
                        "kind": "file",
                        "path": path.display().to_string()
                    },
                    "value": "experiment"
                }
            ],
            "measurement": {
                "command": "printf '{\"test_metric\":1}\n'",
                "schema": { "test_metric": "number" },
                "timeout_ms": 1000
            },
            "primary_metrics": [
                {
                    "metric": "test_metric",
                    "op": "decrease_abs_ge",
                    "value": 999999
                }
            ],
            "window_seconds": 0,
            "settle_seconds": 0
        }));

        let result = controller.execute_commit(&mut state, &commit);
        assert!(!result.ok);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
        assert_eq!(state.phase, EpisodePhase::Clean);
        assert!(!controller.act_kernel.has_experiment_writes());
        let final_result = controller.finish_episode(&mut state);
        assert!(final_result.rollback_required);
        assert!(final_result.rollback_attempted);
        assert_eq!(final_result.rollback_succeeded, Some(true));
        assert_eq!(final_result.rollback_error, None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn inconclusive_commit_rolls_back_and_returns_to_clean() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-controller-inconclusive-{}",
            std::process::id()
        ));
        std::fs::write(&path, "old\n").unwrap();
        let mut audit = AuditJournal::new(temp_audit_path("inconclusive"));
        let mut controller = EpisodeController::new(test_config(), &mut audit);
        let mut state = EpisodeState::new(test_episode());

        let experiment =
            experiment_invocation(&path, "experiment", "test inconclusive commit rollback");
        let result = controller.execute_experiment_write(&mut state, &experiment);
        assert!(result.ok);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "experiment\n");

        let commit = commit_invocation(json!({
            "reason": "test deterministic inconclusive rollback",
            "keep_writes": [
                {
                    "target": {
                        "kind": "file",
                        "path": path.display().to_string()
                    },
                    "value": "experiment"
                }
            ],
            "measurement": {
                "command": "printf '{\"test_metric\":1}\n'",
                "schema": { "test_metric": "number" },
                "timeout_ms": 1000
            },
            "primary_metrics": [
                {
                    "metric": "test_metric",
                    "op": "current_ge",
                    "value": 0
                }
            ],
            "workload_invariants": [
                {
                    "metric": "test_metric",
                    "op": "current_le",
                    "value": 0
                }
            ],
            "window_seconds": 0,
            "settle_seconds": 0
        }));

        let result = controller.execute_commit(&mut state, &commit);
        assert!(!result.ok);
        assert!(
            result.content.contains("Inconclusive"),
            "{}",
            result.content
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
        assert_eq!(state.phase, EpisodePhase::Clean);
        assert!(!controller.act_kernel.has_experiment_writes());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rollback_failure_freezes_episode_and_records_failure() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-controller-rollback-failure-{}",
            std::process::id()
        ));
        std::fs::write(&path, "old\n").unwrap();
        let mut audit = AuditJournal::new(temp_audit_path("rollback-failure"));
        let mut controller = EpisodeController::new(test_config(), &mut audit);
        let mut state = EpisodeState::new(test_episode());

        let experiment = experiment_invocation(&path, "experiment", "test rollback failure");
        let result = controller.execute_experiment_write(&mut state, &experiment);
        assert!(result.ok);
        std::fs::remove_file(&path).unwrap();

        let final_result = controller.finish_episode(&mut state);

        assert_eq!(state.phase, EpisodePhase::Frozen);
        assert!(final_result.rollback_required);
        assert!(final_result.rollback_attempted);
        assert_eq!(final_result.rollback_succeeded, Some(false));
        assert!(final_result
            .rollback_error
            .as_deref()
            .is_some_and(|error| error.contains("failed to read")));
    }

    fn experiment_invocation(path: &std::path::Path, value: &str, reason: &str) -> ToolInvocation {
        ToolInvocation {
            id: "call_exp".to_string(),
            name: "experiment_write".to_string(),
            arguments: json!({
                "target": {
                    "kind": "file",
                    "path": path.display().to_string()
                },
                "value": value,
                "reason": reason,
            }),
        }
    }

    fn commit_invocation(arguments: serde_json::Value) -> ToolInvocation {
        ToolInvocation {
            id: "call_commit".to_string(),
            name: "commit".to_string(),
            arguments,
        }
    }

    fn test_episode() -> Episode {
        Episode::new(ActivationEvent::new(
            EventSource::Cli,
            "test".to_string(),
            Severity::Info,
            Scope::Host,
        ))
    }

    fn test_config() -> Config {
        let mut config = Config::default();
        config.evaluation.default_window_seconds = 1;
        config.evaluation.min_window_seconds = 1;
        config.evaluation.max_window_seconds = 1;
        config.evaluation.default_settle_seconds = 0;
        config.evaluation.min_settle_seconds = 0;
        config.evaluation.max_settle_seconds = 0;
        config
    }

    fn temp_audit_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tuning-agent-controller-{name}-{}.jsonl",
            std::process::id()
        ))
    }
}
