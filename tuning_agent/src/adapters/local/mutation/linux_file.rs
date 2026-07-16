use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::capability::MutationDriver;
use crate::domain::{
    content_digest, CapabilityKind, CapabilityMeta, EffectClass, MutationApplyRequest,
    MutationFinalizeRequest, MutationOperationState, MutationPrepareRequest, MutationQuery,
    MutationReceipt, MutationRestoreRequest, MutationState, MutationStatus, MutationVerification,
    MutationVerifyRequest, OperationId, PreparedMutation, ProviderError, ProviderErrorKind,
    ResourceKey,
};

pub(crate) struct LinuxFileMutationDriver {
    meta: CapabilityMeta,
    mode: DriverMode,
    operations: Mutex<BTreeMap<OperationId, MutationStatus>>,
}

enum DriverMode {
    Production,
    #[cfg(test)]
    TestFile(PathBuf),
}

impl LinuxFileMutationDriver {
    pub(crate) fn new(meta: CapabilityMeta) -> Result<Self, String> {
        validate_meta(&meta)?;
        Ok(Self {
            meta,
            mode: DriverMode::Production,
            operations: Mutex::new(BTreeMap::new()),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test_file(
        meta: CapabilityMeta,
        path: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        validate_meta(&meta)?;
        let path = fs::canonicalize(path.into())
            .map_err(|error| format!("failed to canonicalize test file: {error}"))?;
        if !path.is_file() {
            return Err("test mutation target must be an existing regular file".into());
        }
        Ok(Self {
            meta,
            mode: DriverMode::TestFile(path),
            operations: Mutex::new(BTreeMap::new()),
        })
    }

    fn target_from_arguments(&self, arguments: &Value) -> Result<ResolvedTarget, ProviderError> {
        #[cfg(not(test))]
        debug_assert!(matches!(&self.mode, DriverMode::Production));

        let target = arguments
            .get("target")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("mutation target must be an object"))?;
        let kind = target
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("mutation target.kind is required"))?;

        #[cfg(test)]
        if let DriverMode::TestFile(path) = &self.mode {
            if kind != "test_file" || target.len() != 1 {
                return Err(invalid(
                    "test driver accepts only target={\"kind\":\"test_file\"}",
                ));
            }
            return Ok(ResolvedTarget {
                kind: "test_file".into(),
                path: path.clone(),
            });
        }

        let path = match kind {
            "sysctl" => {
                let key = target
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("sysctl target.key is required"))?;
                validate_sysctl_key(key)?;
                canonicalize_beneath(
                    &PathBuf::from("/proc/sys").join(key.replace('.', "/")),
                    Path::new("/proc/sys"),
                )?
            }
            "proc_sys" => canonicalize_declared_path(target, Path::new("/proc/sys"), kind)?,
            "sysfs" => canonicalize_declared_path(target, Path::new("/sys"), kind)?,
            "cgroup" => canonicalize_declared_path(target, Path::new("/sys/fs/cgroup"), kind)?,
            _ => {
                return Err(invalid(format!(
                    "unsupported Linux mutation target '{kind}'"
                )))
            }
        };
        Ok(ResolvedTarget {
            kind: kind.into(),
            path,
        })
    }

    fn target_from_prepared(
        &self,
        prepared: &PreparedMutation,
    ) -> Result<ResolvedTarget, ProviderError> {
        self.validate_prepared_identity(prepared)?;
        let kind = prepared
            .driver_data
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("prepared mutation is missing driver kind"))?;
        let path = prepared
            .driver_data
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| invalid("prepared mutation is missing driver path"))?;

        #[cfg(test)]
        if let DriverMode::TestFile(allowed) = &self.mode {
            if kind == "test_file" && &path == allowed {
                return Ok(ResolvedTarget {
                    kind: kind.into(),
                    path,
                });
            }
            return Err(permission(
                "prepared test target is not the bound test file",
            ));
        }

        let root = root_for_kind(kind)?;
        let canonical = canonicalize_beneath(&path, root)?;
        if canonical != path {
            return Err(permission("prepared target path changed after preparation"));
        }
        Ok(ResolvedTarget {
            kind: kind.into(),
            path,
        })
    }

    pub(super) fn bound_identity(
        &self,
        target_arguments: &Value,
    ) -> Result<(ResourceKey, Value), ProviderError> {
        let target = self.target_from_arguments(&json!({
            "target": target_arguments,
        }))?;
        prepared_identity(&target)
    }

    fn validate_prepared_identity(&self, prepared: &PreparedMutation) -> Result<(), ProviderError> {
        if prepared.capability_id != self.meta.id || prepared.provider != self.meta.provider {
            return Err(permission(
                "prepared mutation does not belong to this provider pin",
            ));
        }
        Ok(())
    }

    fn store_status(&self, status: MutationStatus) -> Result<(), ProviderError> {
        self.operations
            .lock()
            .map_err(|_| internal("operation status lock is poisoned"))?
            .insert(status.operation_id.clone(), status);
        Ok(())
    }

    fn receipt(
        &self,
        operation_id: OperationId,
        state: MutationOperationState,
        observed: Option<MutationState>,
        driver_data: Value,
    ) -> Result<MutationReceipt, ProviderError> {
        self.store_status(MutationStatus {
            operation_id: operation_id.clone(),
            state,
            observed: observed.clone(),
            driver_data: driver_data.clone(),
        })?;
        Ok(MutationReceipt {
            operation_id,
            state,
            observed,
            driver_data,
        })
    }
}

impl MutationDriver for LinuxFileMutationDriver {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }

    fn prepare(&self, request: &MutationPrepareRequest) -> Result<PreparedMutation, ProviderError> {
        let target = self.target_from_arguments(&request.arguments)?;
        let desired = request
            .arguments
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("mutation value must be a string"))?;
        if desired.contains('\n') || desired.contains('\r') {
            return Err(invalid("mutation value must contain exactly one line"));
        }
        let baseline = mutation_state(read_value(&target.path)?)?;
        let desired = mutation_state(desired.to_string())?;
        let (resource, driver_data) = prepared_identity(&target)?;

        Ok(PreparedMutation {
            capability_id: self.meta.id.clone(),
            provider: self.meta.provider.clone(),
            resource,
            baseline,
            desired,
            driver_data,
        })
    }

    fn apply(&self, request: &MutationApplyRequest) -> Result<MutationReceipt, ProviderError> {
        let target = self.target_from_prepared(&request.prepared)?;
        let baseline = state_string(&request.prepared.baseline)?;
        let desired = state_string(&request.prepared.desired)?;
        let current = read_value(&target.path)?;
        if current != baseline && current != desired {
            return Err(conflict(format!(
                "target '{}' drifted before apply",
                target.path.display()
            )));
        }
        if current != desired {
            if let Err(write_error) = write_value(&target.path, &desired) {
                let state = read_value(&target.path)
                    .ok()
                    .and_then(|value| mutation_state(value).ok());
                let operation_state = if state.as_ref() == Some(&request.prepared.desired) {
                    MutationOperationState::Applied
                } else if state.as_ref() == Some(&request.prepared.baseline) {
                    MutationOperationState::NotApplied
                } else {
                    MutationOperationState::Unknown
                };
                self.store_status(MutationStatus {
                    operation_id: request.operation_id.clone(),
                    state: operation_state,
                    observed: state,
                    driver_data: request.prepared.driver_data.clone(),
                })?;
                return Err(write_error);
            }
        }
        let observed = mutation_state(read_value(&target.path)?)?;
        let state = if observed == request.prepared.desired {
            MutationOperationState::Applied
        } else {
            MutationOperationState::Unknown
        };
        self.receipt(
            request.operation_id.clone(),
            state,
            Some(observed),
            request.prepared.driver_data.clone(),
        )
    }

    fn status(&self, query: &MutationQuery) -> Result<MutationStatus, ProviderError> {
        Ok(self
            .operations
            .lock()
            .map_err(|_| internal("operation status lock is poisoned"))?
            .get(&query.operation_id)
            .cloned()
            .unwrap_or(MutationStatus {
                operation_id: query.operation_id.clone(),
                state: MutationOperationState::Unknown,
                observed: None,
                driver_data: Value::Null,
            }))
    }

    fn verify(
        &self,
        request: &MutationVerifyRequest,
    ) -> Result<MutationVerification, ProviderError> {
        let target = self.target_from_prepared(&request.prepared)?;
        let observed = mutation_state(read_value(&target.path)?)?;
        Ok(MutationVerification {
            matched: observed == request.expected,
            observed: Some(observed),
            details: json!({"path": target.path}),
        })
    }

    fn restore(&self, request: &MutationRestoreRequest) -> Result<MutationReceipt, ProviderError> {
        let target = self.target_from_prepared(&request.prepared)?;
        let baseline = state_string(&request.prepared.baseline)?;
        let desired = state_string(&request.prepared.desired)?;
        let current = read_value(&target.path)?;
        if current != baseline && current != desired {
            return Err(conflict(format!(
                "target '{}' drifted before restore",
                target.path.display()
            )));
        }
        if current != baseline {
            write_value(&target.path, &baseline)?;
        }
        let observed = mutation_state(read_value(&target.path)?)?;
        let state = if observed == request.prepared.baseline {
            MutationOperationState::Restored
        } else {
            MutationOperationState::Unknown
        };
        self.receipt(
            request.operation_id.clone(),
            state,
            Some(observed),
            request.prepared.driver_data.clone(),
        )
    }

    fn finalize(
        &self,
        request: &MutationFinalizeRequest,
    ) -> Result<MutationReceipt, ProviderError> {
        let target = self.target_from_prepared(&request.prepared)?;
        let observed = mutation_state(read_value(&target.path)?)?;
        if observed != request.prepared.desired {
            return Err(conflict(format!(
                "target '{}' drifted before finalize",
                target.path.display()
            )));
        }
        self.receipt(
            request.operation_id.clone(),
            MutationOperationState::Finalized,
            Some(observed),
            request.prepared.driver_data.clone(),
        )
    }
}

struct ResolvedTarget {
    kind: String,
    path: PathBuf,
}

fn prepared_identity(target: &ResolvedTarget) -> Result<(ResourceKey, Value), ProviderError> {
    let resource = ResourceKey::new(format!(
        "linux-file:{}:{}",
        target.kind,
        target.path.display()
    ))
    .map_err(invalid)?;
    Ok((
        resource,
        json!({
            "kind": target.kind.clone(),
            "path": target.path.clone(),
        }),
    ))
}

fn validate_meta(meta: &CapabilityMeta) -> Result<(), String> {
    if meta.kind != CapabilityKind::Mutation || meta.effect != EffectClass::ReversibleMutation {
        return Err(
            "LinuxFileMutationDriver requires reversible mutation capability metadata".into(),
        );
    }
    Ok(())
}

fn validate_sysctl_key(key: &str) -> Result<(), ProviderError> {
    if key.is_empty()
        || key.contains('/')
        || key.split('.').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err(invalid("invalid sysctl key"));
    }
    Ok(())
}

fn canonicalize_declared_path(
    target: &serde_json::Map<String, Value>,
    root: &Path,
    kind: &str,
) -> Result<PathBuf, ProviderError> {
    let path = target
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| invalid(format!("{kind} target.path is required")))?;
    canonicalize_beneath(&path, root)
}

fn canonicalize_beneath(path: &Path, root: &Path) -> Result<PathBuf, ProviderError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(permission(
            "mutation target must be a normalized absolute path",
        ));
    }
    if !path.starts_with(root) {
        return Err(permission(format!(
            "mutation target '{}' is outside '{}'",
            path.display(),
            root.display()
        )));
    }
    let canonical_root = fs::canonicalize(root).map_err(|error| {
        internal(format!(
            "failed to canonicalize root '{}': {error}",
            root.display()
        ))
    })?;
    let canonical_path = fs::canonicalize(path).map_err(|error| {
        invalid(format!(
            "failed to canonicalize mutation target '{}': {error}",
            path.display()
        ))
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(permission("mutation target escapes its allowed root"));
    }
    Ok(canonical_path)
}

fn root_for_kind(kind: &str) -> Result<&'static Path, ProviderError> {
    match kind {
        "sysctl" | "proc_sys" => Ok(Path::new("/proc/sys")),
        "sysfs" => Ok(Path::new("/sys")),
        "cgroup" => Ok(Path::new("/sys/fs/cgroup")),
        _ => Err(permission(
            "prepared target kind is not allowed in production",
        )),
    }
}

fn mutation_state(value: String) -> Result<MutationState, ProviderError> {
    let digest = content_digest(&value).map_err(invalid)?;
    Ok(MutationState {
        value: Value::String(value),
        digest,
    })
}

fn state_string(state: &MutationState) -> Result<String, ProviderError> {
    state
        .value
        .as_str()
        .map(ToString::to_string)
        .ok_or_else(|| invalid("Linux file mutation state must be a string"))
}

fn read_value(path: &Path) -> Result<String, ProviderError> {
    fs::read_to_string(path)
        .map(|value| value.trim_end_matches(['\r', '\n']).to_string())
        .map_err(|error| {
            internal(format!(
                "failed to read mutation target '{}': {error}",
                path.display()
            ))
        })
}

fn write_value(path: &Path, value: &str) -> Result<(), ProviderError> {
    fs::write(path, format!("{value}\n")).map_err(|error| {
        internal(format!(
            "failed to write mutation target '{}': {error}",
            path.display()
        ))
    })
}

fn invalid(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

fn permission(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::PermissionDenied, message)
}

fn conflict(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Conflict, message)
}

fn internal(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Internal, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CapabilityId, CapabilityKind, Digest, EffectClass, EpisodeId, InvocationContext,
        ProviderClass, ProviderId, ProviderPin, ProviderVersion,
    };

    #[test]
    fn bound_test_file_supports_apply_verify_and_restore() {
        let path = temp_file("lifecycle");
        fs::write(&path, "old\n").unwrap();
        let driver = LinuxFileMutationDriver::for_test_file(test_meta(), &path).unwrap();
        let prepared = driver
            .prepare(&MutationPrepareRequest {
                context: context("prepare"),
                arguments: json!({
                    "target": {"kind": "test_file"},
                    "value": "new"
                }),
            })
            .unwrap();
        let apply = driver
            .apply(&MutationApplyRequest {
                operation_id: operation("apply"),
                prepared: prepared.clone(),
            })
            .unwrap();
        assert_eq!(apply.state, MutationOperationState::Applied);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");

        let verified = driver
            .verify(&MutationVerifyRequest {
                operation_id: operation("verify"),
                prepared: prepared.clone(),
                expected: prepared.desired.clone(),
            })
            .unwrap();
        assert!(verified.matched);

        let restored = driver
            .restore(&MutationRestoreRequest {
                operation_id: operation("restore"),
                prepared,
            })
            .unwrap();
        assert_eq!(restored.state, MutationOperationState::Restored);
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn mutation_state_uses_a_fixed_length_digest_for_large_values() {
        let state = mutation_state("x".repeat(4096)).unwrap();

        assert!(state.digest.as_str().starts_with("sha256:"));
        assert_eq!(state.digest.as_str().len(), 71);
    }

    #[test]
    fn test_constructor_does_not_accept_a_caller_selected_path() {
        let path = temp_file("bound");
        fs::write(&path, "old\n").unwrap();
        let driver = LinuxFileMutationDriver::for_test_file(test_meta(), &path).unwrap();
        let error = driver
            .prepare(&MutationPrepareRequest {
                context: context("prepare"),
                arguments: json!({
                    "target": {"kind": "test_file", "path": "/tmp/other"},
                    "value": "new"
                }),
            })
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn production_driver_rejects_arbitrary_files() {
        let driver = LinuxFileMutationDriver::new(test_meta()).unwrap();
        let error = driver
            .prepare(&MutationPrepareRequest {
                context: context("prepare"),
                arguments: json!({
                    "target": {"kind": "sysfs", "path": "/tmp/not-sysfs"},
                    "value": "new"
                }),
            })
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::PermissionDenied);
    }

    fn test_meta() -> CapabilityMeta {
        CapabilityMeta::new(
            CapabilityId::new("local/linux-file").unwrap(),
            CapabilityKind::Mutation,
            EffectClass::ReversibleMutation,
            ProviderPin {
                provider_id: ProviderId::new("linux-file-test").unwrap(),
                provider_version: ProviderVersion::new("1").unwrap(),
                provider_class: ProviderClass::Local,
                manifest_digest: Digest::new("test-manifest").unwrap(),
            },
            "test Linux file mutation",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
    }

    fn context(id: &str) -> InvocationContext {
        InvocationContext {
            episode_id: EpisodeId::new(1),
            operation_id: operation(id),
        }
    }

    fn operation(id: &str) -> OperationId {
        OperationId::new(format!("test/{id}")).unwrap()
    }

    fn temp_file(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tuning-agent-linux-file-{label}-{}",
            std::process::id()
        ))
    }
}
