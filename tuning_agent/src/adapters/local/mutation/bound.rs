use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adapters::local::mutation::LinuxFileMutationDriver;
use crate::capability::MutationDriver;
use crate::domain::{
    content_digest, CapabilityId, CapabilityKind, CapabilityMeta, Digest, EffectClass,
    EpisodePhase, MutationApplyRequest, MutationFinalizeRequest, MutationPrepareRequest,
    MutationQuery, MutationReceipt, MutationRestoreRequest, MutationStatus, MutationVerification,
    MutationVerifyRequest, PreparedMutation, ProviderClass, ProviderError, ProviderId, ProviderPin,
    ProviderVersion,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LinuxMutationTarget {
    Sysctl { key: String },
    ProcSys { path: String },
    Sysfs { path: String },
    Cgroup { path: String },
}

impl LinuxMutationTarget {
    fn as_arguments(&self) -> Value {
        match self {
            Self::Sysctl { key } => json!({"kind": "sysctl", "key": key}),
            Self::ProcSys { path } => json!({"kind": "proc_sys", "path": path}),
            Self::Sysfs { path } => json!({"kind": "sysfs", "path": path}),
            Self::Cgroup { path } => json!({"kind": "cgroup", "path": path}),
        }
    }
}

pub struct BoundLinuxFileMutationDriver {
    inner: LinuxFileMutationDriver,
    target_arguments: Value,
    expected_resource: crate::domain::ResourceKey,
    expected_driver_data: Value,
}

impl BoundLinuxFileMutationDriver {
    pub fn new(
        capability_id: CapabilityId,
        description: impl Into<String>,
        target: LinuxMutationTarget,
    ) -> Result<Self, String> {
        let target_digest = content_digest(&target)?;
        let meta = bound_meta(capability_id, description, target_digest)?;
        let inner = LinuxFileMutationDriver::new(meta)?;
        let target_arguments = target.as_arguments();
        let (expected_resource, expected_driver_data) = inner
            .bound_identity(&target_arguments)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            inner,
            target_arguments,
            expected_resource,
            expected_driver_data,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test_file(
        capability_id: CapabilityId,
        description: impl Into<String>,
        path: impl Into<std::path::PathBuf>,
    ) -> Result<Self, String> {
        let path = path.into();
        let target_arguments = json!({"kind": "test_file"});
        let target_digest = content_digest(&json!({
            "kind": "test_file",
            "path": path.display().to_string(),
        }))?;
        let meta = bound_meta(capability_id, description, target_digest)?;
        let inner = LinuxFileMutationDriver::for_test_file(meta, &path)?;
        let (expected_resource, expected_driver_data) = inner
            .bound_identity(&target_arguments)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            inner,
            target_arguments,
            expected_resource,
            expected_driver_data,
        })
    }

    fn ensure_bound(&self, prepared: &PreparedMutation) -> Result<(), ProviderError> {
        if prepared.resource != self.expected_resource
            || prepared.driver_data != self.expected_driver_data
        {
            return Err(ProviderError::new(
                crate::domain::ProviderErrorKind::PermissionDenied,
                "prepared mutation does not match the capability's administrator-bound resource",
            ));
        }
        Ok(())
    }
}

fn bound_meta(
    capability_id: CapabilityId,
    description: impl Into<String>,
    target_digest: Digest,
) -> Result<CapabilityMeta, String> {
    let provider = ProviderPin {
        provider_id: ProviderId::new(format!("local.linux-file.{capability_id}"))?,
        provider_version: ProviderVersion::new("1")?,
        provider_class: ProviderClass::Local,
        manifest_digest: Digest::new(format!("bound-linux-file-v1:{target_digest}"))?,
    };
    let mut meta = CapabilityMeta::new(
        capability_id,
        CapabilityKind::Mutation,
        EffectClass::ReversibleMutation,
        provider,
        description,
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["value"],
            "properties": {
                "value": {"type": "string", "minLength": 1, "maxLength": 4096}
            }
        }),
        json!({
            "type": "object",
            "required": ["state", "observed"]
        }),
    )
    .with_allowed_phases([EpisodePhase::Clean, EpisodePhase::Experimenting]);
    meta.idempotent = true;
    Ok(meta)
}

impl MutationDriver for BoundLinuxFileMutationDriver {
    fn meta(&self) -> &CapabilityMeta {
        self.inner.meta()
    }

    fn prepare(&self, request: &MutationPrepareRequest) -> Result<PreparedMutation, ProviderError> {
        let object = request.arguments.as_object().ok_or_else(|| {
            ProviderError::new(
                crate::domain::ProviderErrorKind::InvalidRequest,
                "bound Linux mutation arguments must be an object",
            )
        })?;
        if object.len() != 1 || !object.contains_key("value") {
            return Err(ProviderError::new(
                crate::domain::ProviderErrorKind::InvalidRequest,
                "bound Linux mutation accepts only the value field",
            ));
        }
        let value = object.get("value").and_then(Value::as_str).ok_or_else(|| {
            ProviderError::new(
                crate::domain::ProviderErrorKind::InvalidRequest,
                "bound Linux mutation value must be a string",
            )
        })?;
        if value.is_empty() || value.len() > 4096 {
            return Err(ProviderError::new(
                crate::domain::ProviderErrorKind::InvalidRequest,
                "bound Linux mutation value must contain between 1 and 4096 bytes",
            ));
        }
        let mut request = request.clone();
        request.arguments = json!({
            "target": self.target_arguments.clone(),
            "value": value,
        });
        let prepared = self.inner.prepare(&request)?;
        self.ensure_bound(&prepared)?;
        Ok(prepared)
    }

    fn apply(&self, request: &MutationApplyRequest) -> Result<MutationReceipt, ProviderError> {
        self.ensure_bound(&request.prepared)?;
        self.inner.apply(request)
    }

    fn status(&self, query: &MutationQuery) -> Result<MutationStatus, ProviderError> {
        self.inner.status(query)
    }

    fn verify(
        &self,
        request: &MutationVerifyRequest,
    ) -> Result<MutationVerification, ProviderError> {
        self.ensure_bound(&request.prepared)?;
        self.inner.verify(request)
    }

    fn restore(&self, request: &MutationRestoreRequest) -> Result<MutationReceipt, ProviderError> {
        self.ensure_bound(&request.prepared)?;
        self.inner.restore(request)
    }

    fn finalize(
        &self,
        request: &MutationFinalizeRequest,
    ) -> Result<MutationReceipt, ProviderError> {
        self.ensure_bound(&request.prepared)?;
        self.inner.finalize(request)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::domain::{EpisodeId, InvocationContext, OperationId};

    #[test]
    fn bound_driver_rejects_agent_selected_targets() {
        let path = std::env::temp_dir().join(format!(
            "tuning-agent-bound-driver-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "old\n").unwrap();
        let driver = BoundLinuxFileMutationDriver::for_test_file(
            CapabilityId::new("local/test-bound").unwrap(),
            "test",
            &path,
        )
        .unwrap();
        let request = MutationPrepareRequest {
            context: InvocationContext {
                episode_id: EpisodeId::new(1),
                operation_id: OperationId::new("prepare").unwrap(),
            },
            arguments: json!({
                "value": "new",
                "target": {"kind": "sysfs", "path": "/sys/other"}
            }),
        };
        assert!(driver.prepare(&request).is_err());

        let oversized = MutationPrepareRequest {
            context: request.context,
            arguments: json!({"value": "x".repeat(4097)}),
        };
        assert!(driver.prepare(&oversized).is_err());

        let mut tampered = driver
            .prepare(&MutationPrepareRequest {
                context: InvocationContext {
                    episode_id: EpisodeId::new(1),
                    operation_id: OperationId::new("prepare-valid").unwrap(),
                },
                arguments: json!({"value": "new"}),
            })
            .unwrap();
        tampered.driver_data["path"] = json!("/tmp/a-different-target");
        assert!(driver
            .apply(&MutationApplyRequest {
                operation_id: OperationId::new("apply-tampered").unwrap(),
                prepared: tampered,
            })
            .is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
        let _ = fs::remove_file(path);
    }
}
