use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::act::{
    CommandRequest, CommitWrite, ExecutionReport, ExperimentWriteRequest, WriteTarget,
};
use crate::config::CommandConfig;

pub struct ActKernelConfig {
    command_timeout: Duration,
    command_output_limit: usize,
    evaluation_output_limit: usize,
}

impl ActKernelConfig {
    pub fn from_config(config: &CommandConfig) -> Self {
        Self {
            command_timeout: Duration::from_millis(config.timeout_ms),
            command_output_limit: config.output_limit_bytes,
            evaluation_output_limit: config.evaluation_output_limit_bytes,
        }
    }

    pub fn command_output_limit(&self) -> usize {
        self.command_output_limit
    }

    pub fn evaluation_output_limit(&self) -> usize {
        self.evaluation_output_limit
    }
}

pub struct ActKernel {
    config: ActKernelConfig,
    targets: BTreeMap<WriteTarget, TargetState>,
}

#[derive(Clone, Debug)]
struct TargetState {
    original_value: String,
    current_value: String,
    experiment_values: BTreeSet<String>,
    write_state: TargetWriteState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetWriteState {
    Prepared,
    Applied,
    RecoveryRequired,
}

#[derive(Clone, Debug, Serialize)]
pub struct WriteReport {
    pub target: WriteTarget,
    pub old_value: String,
    pub requested_value: String,
    pub current_value: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RestoreReport {
    pub restored: Vec<WriteReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyReport {
    pub applied: Vec<WriteReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CommitReport {
    pub kept: Vec<WriteReport>,
    pub restored: Vec<WriteReport>,
}

impl ActKernel {
    pub fn new(config: ActKernelConfig) -> Self {
        Self {
            config,
            targets: BTreeMap::new(),
        }
    }

    pub fn execute_command(&self, request: &CommandRequest) -> Result<ExecutionReport, String> {
        if request.command.trim().is_empty() {
            return Err("command is empty".to_string());
        }
        run_shell_command(
            &request.command,
            request.working_dir.as_deref(),
            request.timeout.unwrap_or(self.config.command_timeout),
        )
    }

    pub fn experiment_write(
        &mut self,
        request: &ExperimentWriteRequest,
    ) -> Result<WriteReport, String> {
        self.experiment_write_with(request, Self::write_target)
    }

    fn experiment_write_with(
        &mut self,
        request: &ExperimentWriteRequest,
        write: impl FnOnce(&WriteTarget, &str) -> Result<(), String>,
    ) -> Result<WriteReport, String> {
        request.target.validate()?;
        let old_value = self.read_target(&request.target)?;
        let was_tracked = self.targets.contains_key(&request.target);
        self.ensure_target_state(&request.target, &old_value);
        if let Err(write_error) = write(&request.target, &request.value) {
            return Err(self.reconcile_failed_write(
                &request.target,
                &old_value,
                was_tracked,
                write_error,
            ));
        }

        let current_value = match self.read_target(&request.target) {
            Ok(value) => value,
            Err(verify_error) => {
                self.mark_recovery_required(&request.target, &old_value);
                return Err(format!(
                    "write may have succeeded but verification failed: {verify_error}"
                ));
            }
        };

        if current_value == old_value && current_value != request.value {
            self.clear_prepared_target(&request.target, was_tracked);
            return Err(format!(
                "write completed but target value remained '{}'",
                current_value
            ));
        }
        if current_value != request.value {
            self.mark_recovery_required(&request.target, &current_value);
            return Err(format!(
                "write produced unexpected value '{}'; requested '{}'",
                current_value, request.value
            ));
        }

        let state = self
            .targets
            .get_mut(&request.target)
            .expect("target state must exist after ensure_target_state");
        state.current_value = current_value.clone();
        state.experiment_values.insert(request.value.clone());
        state.write_state = TargetWriteState::Applied;

        Ok(WriteReport {
            target: request.target.clone(),
            old_value,
            requested_value: request.value.clone(),
            current_value,
            reason: request.reason.clone(),
        })
    }

    pub fn restore_to_baseline(&mut self) -> Result<RestoreReport, String> {
        let mut restored = Vec::new();
        let targets = self
            .targets
            .iter()
            .filter(|(_, state)| state.write_state != TargetWriteState::Prepared)
            .map(|(target, _)| target.clone())
            .collect::<Vec<_>>();
        for target in targets {
            let original_value = self
                .targets
                .get(&target)
                .map(|state| state.original_value.clone())
                .ok_or_else(|| "missing target state".to_string())?;
            let old_value = self.read_target(&target)?;
            Self::write_target(&target, &original_value)?;
            let current_value = self.read_target(&target)?;
            if let Some(state) = self.targets.get_mut(&target) {
                state.current_value = current_value.clone();
            }
            restored.push(WriteReport {
                target,
                old_value,
                requested_value: original_value,
                current_value,
                reason: "restore_to_baseline".to_string(),
            });
        }
        Ok(RestoreReport { restored })
    }

    pub fn discard_episode_writes(&mut self) -> Result<RestoreReport, String> {
        let report = self.restore_to_baseline()?;
        self.targets.clear();
        Ok(report)
    }

    pub fn apply_commit_candidate(
        &mut self,
        keep_writes: &[CommitWrite],
    ) -> Result<ApplyReport, String> {
        self.validate_commit_writes(keep_writes)?;
        let mut applied = Vec::new();
        for keep in keep_writes {
            let old_value = self.read_target(&keep.target)?;
            Self::write_target(&keep.target, &keep.value)?;
            let current_value = self.read_target(&keep.target)?;
            if let Some(state) = self.targets.get_mut(&keep.target) {
                state.current_value = current_value.clone();
            }
            applied.push(WriteReport {
                target: keep.target.clone(),
                old_value,
                requested_value: keep.value.clone(),
                current_value,
                reason: "apply_commit_candidate".to_string(),
            });
        }
        Ok(ApplyReport { applied })
    }

    pub fn finalize_commit(&mut self, keep_writes: &[CommitWrite]) -> Result<CommitReport, String> {
        self.validate_commit_writes(keep_writes)?;
        let keep_map = keep_writes
            .iter()
            .map(|write| (write.target.clone(), write.value.clone()))
            .collect::<BTreeMap<_, _>>();

        let mut kept = Vec::new();
        let mut restored = Vec::new();
        for target in self.targets.keys().cloned().collect::<Vec<_>>() {
            let target_state = self
                .targets
                .get(&target)
                .cloned()
                .ok_or_else(|| "missing target state".to_string())?;
            let requested_value = keep_map
                .get(&target)
                .cloned()
                .unwrap_or(target_state.original_value);
            let reason = if keep_map.contains_key(&target) {
                "finalize_commit_keep"
            } else {
                "finalize_commit_restore"
            };
            let old_value = self.read_target(&target)?;
            Self::write_target(&target, &requested_value)?;
            let current_value = self.read_target(&target)?;
            let report = WriteReport {
                target,
                old_value,
                requested_value,
                current_value,
                reason: reason.to_string(),
            };
            if reason == "finalize_commit_keep" {
                kept.push(report);
            } else {
                restored.push(report);
            }
        }
        self.targets.clear();
        Ok(CommitReport { kept, restored })
    }

    pub fn has_experiment_writes(&self) -> bool {
        self.targets
            .values()
            .any(|state| state.write_state != TargetWriteState::Prepared)
    }

    pub fn has_recovery_required(&self) -> bool {
        self.targets
            .values()
            .any(|state| state.write_state == TargetWriteState::RecoveryRequired)
    }

    pub fn output_limit(&self) -> usize {
        self.config.command_output_limit()
    }

    pub fn evaluation_output_limit(&self) -> usize {
        self.config.evaluation_output_limit()
    }

    fn ensure_target_state(&mut self, target: &WriteTarget, old_value: &str) {
        self.targets
            .entry(target.clone())
            .or_insert_with(|| TargetState {
                original_value: old_value.to_string(),
                current_value: old_value.to_string(),
                experiment_values: BTreeSet::new(),
                write_state: TargetWriteState::Prepared,
            });
    }

    fn reconcile_failed_write(
        &mut self,
        target: &WriteTarget,
        old_value: &str,
        was_tracked: bool,
        write_error: String,
    ) -> String {
        match self.read_target(target) {
            Ok(current_value) if current_value == old_value => {
                self.clear_prepared_target(target, was_tracked);
                write_error
            }
            Ok(current_value) => {
                self.mark_recovery_required(target, &current_value);
                format!("{write_error}; target changed from '{old_value}' to '{current_value}'")
            }
            Err(verify_error) => {
                self.mark_recovery_required(target, old_value);
                format!("{write_error}; target state could not be verified: {verify_error}")
            }
        }
    }

    fn clear_prepared_target(&mut self, target: &WriteTarget, was_tracked: bool) {
        if !was_tracked {
            self.targets.remove(target);
        }
    }

    fn mark_recovery_required(&mut self, target: &WriteTarget, current_value: &str) {
        if let Some(state) = self.targets.get_mut(target) {
            state.current_value = current_value.to_string();
            state.write_state = TargetWriteState::RecoveryRequired;
        }
    }

    fn validate_commit_writes(&self, keep_writes: &[CommitWrite]) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for keep in keep_writes {
            keep.target.validate()?;
            if !seen.insert(keep.target.clone()) {
                return Err("keep_writes contains duplicate target".to_string());
            }
            let Some(state) = self.targets.get(&keep.target) else {
                return Err(format!(
                    "commit target {:?} was not written during this episode",
                    keep.target
                ));
            };
            if !state.experiment_values.contains(&keep.value) {
                return Err(format!(
                    "commit value '{}' for target {:?} was not produced by an experiment write",
                    keep.value, keep.target
                ));
            }
        }
        Ok(())
    }

    fn read_target(&self, target: &WriteTarget) -> Result<String, String> {
        let path = target.path();
        fs::read_to_string(&path)
            .map(|value| value.trim_end_matches('\n').to_string())
            .map_err(|err| format!("failed to read '{}': {err}", path.display()))
    }

    fn write_target(target: &WriteTarget, value: &str) -> Result<(), String> {
        let path = target.path();
        fs::write(&path, format!("{value}\n"))
            .map_err(|err| format!("failed to write '{}': {err}", path.display()))
    }
}

fn run_shell_command(
    command: &str,
    working_dir: Option<&str>,
    timeout: Duration,
) -> Result<ExecutionReport, String> {
    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(working_dir) = working_dir {
        builder.current_dir(working_dir);
    }

    let mut child = builder
        .spawn()
        .map_err(|err| format!("spawn failed: {err}"))?;
    let started = Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|err| format!("wait failed: {err}"))?;
                return Ok(ExecutionReport {
                    command: command.to_string(),
                    status: output.status.code(),
                    timed_out: false,
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .map_err(|err| format!("wait after kill failed: {err}"))?;
                return Ok(ExecutionReport {
                    command: command.to_string(),
                    status: output.status.code(),
                    timed_out: true,
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            Err(err) => return Err(format!("try_wait failed: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn execute_command_runs_shell_script() {
        let kernel = ActKernel::new(ActKernelConfig::from_config(&CommandConfig::default()));
        let request = CommandRequest {
            command: "printf tuning-agent".to_string(),
            working_dir: None,
            timeout: Some(Duration::from_millis(1000)),
        };

        let report = kernel.execute_command(&request).unwrap();

        assert!(report.succeeded());
        assert_eq!(report.stdout, "tuning-agent");
    }

    #[test]
    fn execute_command_allows_shell_redirection() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-unrestricted-command-{}",
            std::process::id()
        ));
        let kernel = ActKernel::new(ActKernelConfig::from_config(&CommandConfig::default()));
        let request = CommandRequest {
            command: format!(
                "printf tuning-agent > '{}'; cat '{}'",
                path.display(),
                path.display()
            ),
            working_dir: None,
            timeout: Some(Duration::from_millis(1000)),
        };

        let report = kernel.execute_command(&request).unwrap();

        assert!(report.succeeded());
        assert_eq!(report.stdout, "tuning-agent");
        assert_eq!(fs::read_to_string(&path).unwrap(), "tuning-agent");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn write_restore_and_finalize_keep_only_commit_candidates() {
        let path_a =
            std::env::temp_dir().join(format!("tuning-agent-write-a-{}", std::process::id()));
        let path_b =
            std::env::temp_dir().join(format!("tuning-agent-write-b-{}", std::process::id()));
        fs::write(&path_a, "old_a\n").unwrap();
        fs::write(&path_b, "old_b\n").unwrap();

        let mut kernel = ActKernel::new(ActKernelConfig::from_config(&CommandConfig::default()));
        let target_a = WriteTarget::File {
            path: path_a.clone(),
        };
        let target_b = WriteTarget::File {
            path: path_b.clone(),
        };

        kernel
            .experiment_write(&ExperimentWriteRequest {
                target: target_a.clone(),
                value: "new_a".to_string(),
                reason: "test".to_string(),
            })
            .unwrap();
        kernel
            .experiment_write(&ExperimentWriteRequest {
                target: target_b.clone(),
                value: "new_b".to_string(),
                reason: "test".to_string(),
            })
            .unwrap();

        kernel
            .finalize_commit(&[CommitWrite {
                target: target_a.clone(),
                value: "new_a".to_string(),
            }])
            .unwrap();

        assert_eq!(fs::read_to_string(path_a).unwrap(), "new_a\n");
        assert_eq!(fs::read_to_string(path_b).unwrap(), "old_b\n");
    }

    #[test]
    fn failed_write_without_a_value_change_is_not_tracked() {
        let path =
            std::env::temp_dir().join(format!("tuning-agent-write-denied-{}", std::process::id()));
        fs::write(&path, "old\n").unwrap();
        let mut kernel = ActKernel::new(ActKernelConfig::from_config(&CommandConfig::default()));
        let request = ExperimentWriteRequest {
            target: WriteTarget::File { path: path.clone() },
            value: "new".to_string(),
            reason: "test unchanged failure".to_string(),
        };

        let error = kernel
            .experiment_write_with(&request, |_, _| Err("permission denied".to_string()))
            .expect_err("write should fail");

        assert_eq!(error, "permission denied");
        assert!(!kernel.has_experiment_writes());
        assert!(!kernel.has_recovery_required());
        assert!(kernel.discard_episode_writes().unwrap().restored.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn partial_write_failure_is_tracked_and_restored() {
        let path =
            std::env::temp_dir().join(format!("tuning-agent-write-partial-{}", std::process::id()));
        fs::write(&path, "old\n").unwrap();
        let mut kernel = ActKernel::new(ActKernelConfig::from_config(&CommandConfig::default()));
        let request = ExperimentWriteRequest {
            target: WriteTarget::File { path: path.clone() },
            value: "partial".to_string(),
            reason: "test partial failure".to_string(),
        };

        let error = kernel
            .experiment_write_with(&request, |target, value| {
                fs::write(target.path(), format!("{value}\n")).unwrap();
                Err("simulated write failure".to_string())
            })
            .expect_err("write should report its simulated failure");

        assert!(error.contains("target changed"), "{error}");
        assert!(kernel.has_experiment_writes());
        assert!(kernel.has_recovery_required());
        let report = kernel.discard_episode_writes().unwrap();
        assert_eq!(report.restored.len(), 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
        assert!(!kernel.has_experiment_writes());
        let _ = fs::remove_file(path);
    }
}
