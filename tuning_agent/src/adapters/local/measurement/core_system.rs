use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::capability::MeasurementProvider;
use crate::domain::{
    CapabilityId, CapabilityKind, CapabilityMeta, CapabilityRole, CleanupReceipt, Digest,
    EffectClass, EpisodePhase, MeasurementOpenRequest, MeasurementSampleRequest,
    MeasurementSession, MeasurementSessionId, MetricBatch, MetricKind, MetricQuality, MetricValue,
    ProviderClass, ProviderError, ProviderErrorKind, ProviderId, ProviderPin, ProviderVersion,
};

const SESSION_PROVIDER: &str = "builtin.core-system-measurement";

pub struct CoreSystemMeasurementProvider {
    meta: CapabilityMeta,
    proc_root: PathBuf,
}

impl CoreSystemMeasurementProvider {
    pub fn new() -> Self {
        Self::with_proc_root("/proc")
    }

    pub fn with_proc_root(proc_root: impl Into<PathBuf>) -> Self {
        let provider = ProviderPin {
            provider_id: ProviderId::new(SESSION_PROVIDER).expect("static provider id is valid"),
            provider_version: ProviderVersion::new("1").expect("static version is valid"),
            provider_class: ProviderClass::Builtin,
            manifest_digest: Digest::new("builtin-core-system-measurement-v1")
                .expect("static digest is valid"),
        };
        let mut meta = CapabilityMeta::new(
            CapabilityId::new("builtin/measurement.core-system.v1")
                .expect("static capability id is valid"),
            CapabilityKind::Measurement,
            EffectClass::ReadOnly,
            provider,
            "Read load average and Linux pressure stall information",
            json!({
                "type": "object",
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": [
                    "loadavg.1m",
                    "psi.cpu.some.avg10",
                    "psi.io.some.avg10",
                    "psi.memory.some.avg10"
                ],
                "properties": {
                    "loadavg.1m": {
                        "type": "number",
                        "description": "One-minute runnable and uninterruptible task load average."
                    },
                    "psi.cpu.some.avg10": {
                        "type": "number",
                        "description": "Ten-second average CPU some-stall pressure percentage."
                    },
                    "psi.cpu.full.avg10": {
                        "type": "number",
                        "description": "Ten-second average CPU full-stall pressure percentage when exported by the kernel."
                    },
                    "psi.io.some.avg10": {
                        "type": "number",
                        "description": "Ten-second average I/O some-stall pressure percentage."
                    },
                    "psi.io.full.avg10": {
                        "type": "number",
                        "description": "Ten-second average I/O full-stall pressure percentage when exported by the kernel."
                    },
                    "psi.memory.some.avg10": {
                        "type": "number",
                        "description": "Ten-second average memory some-stall pressure percentage."
                    },
                    "psi.memory.full.avg10": {
                        "type": "number",
                        "description": "Ten-second average memory full-stall pressure percentage when exported by the kernel."
                    }
                }
            }),
        )
        .with_allowed_phases([EpisodePhase::CommitPending]);
        meta.role = CapabilityRole::RuntimeSystemGuardrail;
        meta.idempotent = true;

        Self {
            meta,
            proc_root: proc_root.into(),
        }
    }

    fn validate_session(&self, session: &MeasurementSession) -> Result<(), ProviderError> {
        let provider = session.driver_data.get("provider").and_then(Value::as_str);
        if provider != Some(SESSION_PROVIDER) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "measurement session does not belong to core system provider",
            ));
        }
        Ok(())
    }

    fn collect(&self) -> Result<MetricBatch, ProviderError> {
        let started_at_ns = now_ns()?;
        let loadavg = read_file(&self.proc_root.join("loadavg"))?;
        let cpu = read_file(&self.proc_root.join("pressure/cpu"))?;
        let io = read_file(&self.proc_root.join("pressure/io"))?;
        let memory = read_file(&self.proc_root.join("pressure/memory"))?;

        let load = loadavg
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Protocol,
                    "failed to parse /proc/loadavg 1 minute value",
                )
            })?;
        let mut metrics = BTreeMap::from([("loadavg.1m".to_string(), gauge(load, "load"))]);
        let mut quality = MetricQuality::Valid;
        for (resource, content) in [("cpu", cpu), ("io", io), ("memory", memory)] {
            for row in ["some", "full"] {
                match parse_psi_avg10(&content, row) {
                    Some(value) => {
                        metrics.insert(
                            format!("psi.{resource}.{row}.avg10"),
                            gauge(value, "percent"),
                        );
                    }
                    None => quality = MetricQuality::Partial,
                }
            }
        }
        let ended_at_ns = now_ns()?;

        Ok(MetricBatch {
            started_at_ns,
            ended_at_ns,
            quality,
            workload_fingerprint: None,
            metrics,
            provenance: json!({
                "provider": SESSION_PROVIDER,
                "version": "1",
                "proc_root": self.proc_root,
            }),
        })
    }
}

impl Default for CoreSystemMeasurementProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MeasurementProvider for CoreSystemMeasurementProvider {
    fn meta(&self) -> &CapabilityMeta {
        &self.meta
    }

    fn validate_specification(&self, specification: &Value) -> Result<(), ProviderError> {
        if !specification.is_object()
            || specification
                .as_object()
                .is_some_and(|value| !value.is_empty())
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "core system measurement specification must be an empty object",
            ));
        }
        Ok(())
    }

    fn open(&self, request: &MeasurementOpenRequest) -> Result<MeasurementSession, ProviderError> {
        self.validate_specification(&request.specification)?;
        let suffix = request.context.operation_id.as_str();
        let mut session_name = format!("core-{suffix}");
        while session_name.len() > 256 {
            let mut end = session_name.len() - 1;
            while !session_name.is_char_boundary(end) {
                end -= 1;
            }
            session_name.truncate(end);
        }
        Ok(MeasurementSession {
            id: MeasurementSessionId::new(session_name)
                .map_err(|error| ProviderError::new(ProviderErrorKind::InvalidRequest, error))?,
            driver_data: json!({
                "provider": SESSION_PROVIDER,
                "episode_id": request.context.episode_id.get(),
            }),
        })
    }

    fn sample(&self, request: &MeasurementSampleRequest) -> Result<MetricBatch, ProviderError> {
        self.validate_session(&request.session)?;
        self.collect()
    }

    fn close(&self, session: &MeasurementSession) -> Result<CleanupReceipt, ProviderError> {
        self.validate_session(session)?;
        Ok(CleanupReceipt {
            session_id: session.id.clone(),
            cleaned_up: true,
            details: json!({
                "provider": SESSION_PROVIDER,
                "managed_resources": 0,
            }),
        })
    }
}

fn read_file(path: &Path) -> Result<String, ProviderError> {
    fs::read_to_string(path).map_err(|error| {
        ProviderError::new(
            ProviderErrorKind::Unavailable,
            format!("failed to read '{}': {error}", path.display()),
        )
    })
}

fn parse_psi_avg10(content: &str, row: &str) -> Option<f64> {
    content
        .lines()
        .find(|line| line.split_whitespace().next() == Some(row))?
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn gauge(value: f64, unit: &str) -> MetricValue {
    MetricValue {
        value: json!(value),
        unit: unit.to_string(),
        kind: MetricKind::Gauge,
    }
}

fn now_ns() -> Result<u128, ProviderError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                format!("system clock precedes Unix epoch: {error}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::domain::{EpisodeId, InvocationContext, OperationId};

    fn fixture() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tuning-agent-core-measurement-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("pressure")).unwrap();
        fs::write(root.join("loadavg"), "1.25 1.00 0.75 1/10 42\n").unwrap();
        let psi = "some avg10=2.50 avg60=1.00 avg300=0.50 total=10\nfull avg10=0.25 avg60=0.10 avg300=0.05 total=2\n";
        fs::write(root.join("pressure/cpu"), psi).unwrap();
        fs::write(root.join("pressure/io"), psi).unwrap();
        fs::write(root.join("pressure/memory"), psi).unwrap();
        root
    }

    #[test]
    fn returns_typed_load_and_psi_metrics() {
        let root = fixture();
        let provider = CoreSystemMeasurementProvider::with_proc_root(&root);
        let context = InvocationContext {
            episode_id: EpisodeId::new(1),
            operation_id: OperationId::new("baseline").unwrap(),
        };
        let session = provider
            .open(&MeasurementOpenRequest {
                context: context.clone(),
                specification: json!({}),
            })
            .unwrap();

        let batch = provider
            .sample(&MeasurementSampleRequest { context, session })
            .unwrap();

        assert_eq!(batch.quality, MetricQuality::Valid);
        assert_eq!(batch.metrics["loadavg.1m"].value, json!(1.25));
        assert_eq!(batch.metrics["loadavg.1m"].unit, "load");
        assert_eq!(batch.metrics["psi.io.full.avg10"].value, json!(0.25));
        assert_eq!(batch.metrics["psi.io.full.avg10"].kind, MetricKind::Gauge);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn metadata_declares_exact_output_metric_names() {
        let provider = CoreSystemMeasurementProvider::new();
        assert_eq!(provider.meta().role, CapabilityRole::RuntimeSystemGuardrail);
        assert!(!provider.meta().is_agent_selectable());
        let properties = provider.meta().output_schema["properties"]
            .as_object()
            .unwrap();

        for metric in [
            "loadavg.1m",
            "psi.cpu.some.avg10",
            "psi.cpu.full.avg10",
            "psi.io.some.avg10",
            "psi.io.full.avg10",
            "psi.memory.some.avg10",
            "psi.memory.full.avg10",
        ] {
            assert!(properties.contains_key(metric), "missing metric {metric}");
        }
        assert!(!properties.contains_key("cpu.some.avg10"));
        assert!(!properties.contains_key("loadavg.one_minute"));
    }

    #[test]
    fn missing_psi_row_marks_batch_partial_instead_of_inventing_zero() {
        let root = fixture();
        fs::write(
            root.join("pressure/cpu"),
            "some avg10=2.50 avg60=1.00 avg300=0.50 total=10\n",
        )
        .unwrap();
        let provider = CoreSystemMeasurementProvider::with_proc_root(&root);
        let context = InvocationContext {
            episode_id: EpisodeId::new(1),
            operation_id: OperationId::new("baseline").unwrap(),
        };
        let session = provider
            .open(&MeasurementOpenRequest {
                context: context.clone(),
                specification: json!({}),
            })
            .unwrap();

        let batch = provider
            .sample(&MeasurementSampleRequest { context, session })
            .unwrap();

        assert_eq!(batch.quality, MetricQuality::Partial);
        assert!(!batch.metrics.contains_key("psi.cpu.full.avg10"));
        let _ = fs::remove_dir_all(root);
    }
}
