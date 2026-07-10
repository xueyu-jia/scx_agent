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

    pub fn execute_read(&self, request: &CommandRequest) -> Result<ExecutionReport, String> {
        if request.command.trim().is_empty() {
            return Err("command is empty".to_string());
        }
        validate_read_command(&request.command)?;
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
        request.target.validate()?;
        let old_value = self.read_target(&request.target)?;
        self.ensure_target_state(&request.target, &old_value);
        self.write_target(&request.target, &request.value)?;
        let current_value = self.read_target(&request.target)?;

        let state = self
            .targets
            .get_mut(&request.target)
            .expect("target state must exist after ensure_target_state");
        state.current_value = current_value.clone();
        state.experiment_values.insert(request.value.clone());

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
        for target in self.targets.keys().cloned().collect::<Vec<_>>() {
            let original_value = self
                .targets
                .get(&target)
                .map(|state| state.original_value.clone())
                .ok_or_else(|| "missing target state".to_string())?;
            let old_value = self.read_target(&target)?;
            self.write_target(&target, &original_value)?;
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
            self.write_target(&keep.target, &keep.value)?;
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
            self.write_target(&target, &requested_value)?;
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
        !self.targets.is_empty()
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
            });
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

    fn write_target(&self, target: &WriteTarget, value: &str) -> Result<(), String> {
        let path = target.path();
        fs::write(&path, format!("{value}\n"))
            .map_err(|err| format!("failed to write '{}': {err}", path.display()))
    }
}

fn validate_read_command(command: &str) -> Result<(), String> {
    let trimmed = command.trim();
    let lower = trimmed.to_ascii_lowercase();

    if contains_unquoted_redirection(trimmed) {
        return Err("read command may not use shell redirection".to_string());
    }

    let blocked_fragments = [
        "tee ",
        "sysctl -w",
        "/proc/sys",
        " kill ",
        " pkill ",
        " killall ",
        " mount ",
        " umount ",
        " ip link set",
        " tc qdisc",
        " curl ",
        " wget ",
        " nc ",
        " netcat ",
        " ssh ",
        " sudo ",
        " nohup ",
        " disown",
    ];
    for fragment in blocked_fragments {
        if lower.contains(fragment) || lower.starts_with(fragment.trim_start()) {
            return Err(format!(
                "read command contains blocked fragment '{fragment}'"
            ));
        }
    }

    if lower.contains(" &") || lower.ends_with('&') {
        return Err("read command may not start background jobs".to_string());
    }
    if trimmed.contains('`') {
        return Err("read command may not use backtick substitution".to_string());
    }

    Ok(())
}

fn contains_unquoted_redirection(command: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for byte in command.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if !in_single => escaped = true,
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'>' | b'<' if !in_single && !in_double => return true,
            _ => {}
        }
    }

    false
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
    fn execute_read_runs_shell_command() {
        let kernel = ActKernel::new(ActKernelConfig::from_config(&CommandConfig::default()));
        let request = CommandRequest {
            command: "printf tuning-agent".to_string(),
            working_dir: None,
            timeout: Some(Duration::from_millis(1000)),
        };

        let report = kernel.execute_read(&request).unwrap();

        assert!(report.succeeded());
        assert_eq!(report.stdout, "tuning-agent");
    }

    #[test]
    fn read_rejects_mutating_command() {
        let kernel = ActKernel::new(ActKernelConfig::from_config(&CommandConfig::default()));
        let request = CommandRequest {
            command: "printf 1 > /tmp/tuning-agent-invalid".to_string(),
            working_dir: None,
            timeout: Some(Duration::from_millis(1000)),
        };

        let err = kernel.execute_read(&request).unwrap_err();

        assert!(err.contains("redirection"), "{err}");
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
}
