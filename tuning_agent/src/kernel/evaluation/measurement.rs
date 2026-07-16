use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::capability::MeasurementProvider;
use crate::domain::{
    CleanupReceipt, InvocationContext, MeasurementOpenRequest, MeasurementSampleRequest,
    MetricBatch, MetricKind, MetricQuality, MetricValue,
};
use crate::kernel::evaluation::{
    EvaluationDeadline, EvaluationError, EvaluationErrorKind, MeasurementBinding, SamplingPlan,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeasurementEvidence {
    pub batch: MetricBatch,
    pub cleanup: CleanupReceipt,
    pub samples_collected: u32,
}

pub(crate) fn collect_measurement(
    provider: Arc<dyn MeasurementProvider>,
    binding: &MeasurementBinding,
    context: &InvocationContext,
    plan: &SamplingPlan,
    deadline: &EvaluationDeadline,
) -> Result<MeasurementEvidence, EvaluationError> {
    deadline.ensure_provider_call(provider.meta(), "before measurement open")?;
    let opened = provider.open(&MeasurementOpenRequest {
        context: context.clone(),
        specification: binding.specification.clone(),
    });
    let open_budget = deadline.check("after measurement open");
    let session = match opened {
        Ok(session) => session,
        Err(error) => {
            open_budget?;
            return Err(EvaluationError::new(
                EvaluationErrorKind::Measurement,
                format!(
                    "measurement '{}' open failed: {error}",
                    binding.capability_id
                ),
            ));
        }
    };

    let samples = match open_budget {
        Ok(()) => sample_session(provider.as_ref(), context, &session, plan, deadline),
        Err(error) => Err(error),
    };

    // Cleanup takes precedence over the budget: once open succeeds, close is always
    // attempted exactly once, even when its declared timeout no longer fits.
    let close_admission =
        deadline.ensure_provider_call(provider.meta(), "before measurement close");
    let cleanup = provider.close(&session);
    let close_budget = deadline.check("after measurement close");

    let cleanup = match cleanup {
        Ok(receipt) if receipt.session_id != session.id => {
            return Err(EvaluationError::new(
                EvaluationErrorKind::Cleanup,
                format!(
                    "measurement '{}' returned cleanup receipt for session '{}', expected '{}'",
                    binding.capability_id, receipt.session_id, session.id
                ),
            ))
        }
        Ok(receipt) if receipt.cleaned_up => {
            ensure_output_limit(provider.meta(), &receipt, "cleanup receipt")?;
            receipt
        }
        Ok(receipt) => {
            return Err(EvaluationError::new(
                EvaluationErrorKind::Cleanup,
                format!(
                    "measurement '{}' did not confirm cleanup for session '{}'",
                    binding.capability_id, receipt.session_id
                ),
            ))
        }
        Err(error) => {
            let sample_context = samples
                .as_ref()
                .err()
                .map(|sample_error| format!("; sampling also failed: {sample_error}"))
                .unwrap_or_default();
            return Err(EvaluationError::new(
                EvaluationErrorKind::Cleanup,
                format!(
                    "measurement '{}' cleanup failed: {error}{sample_context}",
                    binding.capability_id
                ),
            ));
        }
    };

    close_admission?;
    close_budget?;
    let samples = samples?;
    let batch = aggregate_samples(samples)?;
    Ok(MeasurementEvidence {
        batch,
        cleanup,
        samples_collected: plan.sample_count,
    })
}

fn sample_session(
    provider: &dyn MeasurementProvider,
    context: &InvocationContext,
    session: &crate::domain::MeasurementSession,
    plan: &SamplingPlan,
    deadline: &EvaluationDeadline,
) -> Result<Vec<MetricBatch>, EvaluationError> {
    let mut samples = Vec::with_capacity(plan.sample_count as usize);
    for index in 0..plan.sample_count {
        let stage = format!("before measurement sample {}", index + 1);
        deadline.ensure_provider_call(provider.meta(), &stage)?;
        let sampled = provider.sample(&MeasurementSampleRequest {
            context: context.clone(),
            session: session.clone(),
        });
        let stage = format!("after measurement sample {}", index + 1);
        deadline.check(&stage)?;
        let batch = sampled.map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::Measurement,
                format!("measurement sample {} failed: {error}", index + 1),
            )
        })?;
        ensure_output_limit(provider.meta(), &batch, "measurement sample")?;
        batch.validate().map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::Measurement,
                format!("measurement sample {} is invalid: {error}", index + 1),
            )
        })?;
        samples.push(batch);
        if index + 1 < plan.sample_count && plan.sample_interval_ms > 0 {
            deadline.settle(
                Duration::from_millis(plan.sample_interval_ms),
                "while waiting between measurement samples",
            )?;
        }
    }
    Ok(samples)
}

fn ensure_output_limit(
    meta: &crate::domain::CapabilityMeta,
    value: &impl Serialize,
    label: &str,
) -> Result<(), EvaluationError> {
    let size = serde_json::to_vec(value)
        .map_err(|error| {
            EvaluationError::new(
                EvaluationErrorKind::Measurement,
                format!("failed to encode {label}: {error}"),
            )
        })?
        .len();
    if size > meta.limits.max_output_bytes {
        return Err(EvaluationError::new(
            EvaluationErrorKind::Measurement,
            format!(
                "measurement '{}' {label} exceeded its {} byte output limit",
                meta.id, meta.limits.max_output_bytes
            ),
        ));
    }
    Ok(())
}

fn aggregate_samples(samples: Vec<MetricBatch>) -> Result<MetricBatch, EvaluationError> {
    if samples.is_empty() {
        return Err(EvaluationError::new(
            EvaluationErrorKind::Measurement,
            "measurement returned no samples",
        ));
    }
    if samples.len() == 1 {
        return Ok(samples.into_iter().next().expect("one sample exists"));
    }

    let started_at_ns = samples
        .iter()
        .map(|sample| sample.started_at_ns)
        .min()
        .unwrap_or_default();
    let ended_at_ns = samples
        .iter()
        .map(|sample| sample.ended_at_ns)
        .max()
        .unwrap_or_default();
    let mut quality = samples
        .iter()
        .map(|sample| sample.quality)
        .fold(MetricQuality::Valid, worst_quality);

    let metric_names = samples
        .iter()
        .flat_map(|sample| sample.metrics.keys().cloned())
        .collect::<BTreeSet<_>>();
    let mut metrics = BTreeMap::new();
    for metric in metric_names {
        match aggregate_metric(&samples, &metric) {
            Some(value) => {
                metrics.insert(metric, value);
            }
            None => quality = worst_quality(quality, MetricQuality::Partial),
        }
    }

    let first_fingerprint = samples[0].workload_fingerprint.clone();
    let fingerprints_match = samples
        .iter()
        .all(|sample| sample.workload_fingerprint == first_fingerprint);
    let workload_fingerprint = if fingerprints_match {
        first_fingerprint
    } else {
        quality = worst_quality(quality, MetricQuality::Partial);
        None
    };
    let provenance = json!({
        "aggregation": "median",
        "sample_count": samples.len(),
        "samples": samples.iter().map(|sample| sample.provenance.clone()).collect::<Vec<_>>(),
    });

    Ok(MetricBatch {
        started_at_ns,
        ended_at_ns,
        quality,
        workload_fingerprint,
        metrics,
        provenance,
    })
}

fn aggregate_metric(samples: &[MetricBatch], metric: &str) -> Option<MetricValue> {
    let first = samples.first()?.metrics.get(metric)?;
    let mut numbers = Vec::with_capacity(samples.len());
    let mut booleans = Vec::with_capacity(samples.len());

    for sample in samples {
        let value = sample.metrics.get(metric)?;
        if value.kind != first.kind || value.unit != first.unit {
            return None;
        }
        match value.kind {
            MetricKind::Gauge | MetricKind::Counter => numbers.push(value.value.as_f64()?),
            MetricKind::Boolean => booleans.push(value.value.as_bool()?),
            MetricKind::Histogram => return None,
        }
    }

    let value = match first.kind {
        MetricKind::Gauge | MetricKind::Counter => {
            numbers.sort_by(f64::total_cmp);
            let middle = numbers.len() / 2;
            let median = if numbers.len() % 2 == 0 {
                (numbers[middle - 1] + numbers[middle]) / 2.0
            } else {
                numbers[middle]
            };
            json!(median)
        }
        MetricKind::Boolean => {
            let first = *booleans.first()?;
            if !booleans.iter().all(|value| *value == first) {
                return None;
            }
            json!(first)
        }
        MetricKind::Histogram => return None,
    };

    Some(MetricValue {
        value,
        unit: first.unit.clone(),
        kind: first.kind,
    })
}

fn worst_quality(current: MetricQuality, next: MetricQuality) -> MetricQuality {
    match (current, next) {
        (MetricQuality::Invalid, _) | (_, MetricQuality::Invalid) => MetricQuality::Invalid,
        (MetricQuality::Partial, _) | (_, MetricQuality::Partial) => MetricQuality::Partial,
        _ => MetricQuality::Valid,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::capability::MeasurementProvider;
    use crate::domain::{
        CapabilityId, CapabilityKind, CapabilityMeta, Digest, EffectClass, EpisodeId,
        MeasurementSession, MeasurementSessionId, OperationId, ProviderClass, ProviderError,
        ProviderErrorKind, ProviderId, ProviderPin, ProviderVersion,
    };

    struct FailingMeasurement {
        meta: CapabilityMeta,
        closes: AtomicUsize,
        wrong_cleanup_session: bool,
        open_delay: Duration,
    }

    impl MeasurementProvider for FailingMeasurement {
        fn meta(&self) -> &CapabilityMeta {
            &self.meta
        }

        fn validate_specification(
            &self,
            _specification: &serde_json::Value,
        ) -> Result<(), ProviderError> {
            Ok(())
        }

        fn open(
            &self,
            _request: &MeasurementOpenRequest,
        ) -> Result<MeasurementSession, ProviderError> {
            if !self.open_delay.is_zero() {
                std::thread::sleep(self.open_delay);
            }
            Ok(MeasurementSession {
                id: MeasurementSessionId::new("session-1").unwrap(),
                driver_data: json!({}),
            })
        }

        fn sample(
            &self,
            _request: &MeasurementSampleRequest,
        ) -> Result<MetricBatch, ProviderError> {
            Err(ProviderError::new(
                ProviderErrorKind::Unavailable,
                "sample unavailable",
            ))
        }

        fn close(&self, session: &MeasurementSession) -> Result<CleanupReceipt, ProviderError> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(CleanupReceipt {
                session_id: if self.wrong_cleanup_session {
                    MeasurementSessionId::new("another-session").unwrap()
                } else {
                    session.id.clone()
                },
                cleaned_up: true,
                details: json!({}),
            })
        }
    }

    fn meta() -> CapabilityMeta {
        CapabilityMeta::new(
            CapabilityId::new("test/measurement").unwrap(),
            CapabilityKind::Measurement,
            EffectClass::ReadOnly,
            ProviderPin {
                provider_id: ProviderId::new("test").unwrap(),
                provider_version: ProviderVersion::new("1").unwrap(),
                provider_class: ProviderClass::Builtin,
                manifest_digest: Digest::new("test-digest").unwrap(),
            },
            "test measurement",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
    }

    #[test]
    fn close_is_attempted_when_sampling_fails() {
        let provider = Arc::new(FailingMeasurement {
            meta: meta(),
            closes: AtomicUsize::new(0),
            wrong_cleanup_session: false,
            open_delay: Duration::ZERO,
        });
        let binding = MeasurementBinding {
            capability_id: provider.meta.id.clone(),
            specification: json!({}),
        };
        let context = InvocationContext {
            episode_id: EpisodeId::new(1),
            operation_id: OperationId::new("evaluate").unwrap(),
        };
        let deadline = EvaluationDeadline::start(Duration::from_secs(60)).unwrap();

        let result = collect_measurement(
            provider.clone(),
            &binding,
            &context,
            &SamplingPlan {
                settle_ms: 0,
                sample_count: 1,
                sample_interval_ms: 0,
            },
            &deadline,
        );

        assert_eq!(result.unwrap_err().kind, EvaluationErrorKind::Measurement);
        assert_eq!(provider.closes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cleanup_receipt_must_match_the_opened_session() {
        let provider = Arc::new(FailingMeasurement {
            meta: meta(),
            closes: AtomicUsize::new(0),
            wrong_cleanup_session: true,
            open_delay: Duration::ZERO,
        });
        let binding = MeasurementBinding {
            capability_id: provider.meta.id.clone(),
            specification: json!({}),
        };
        let context = InvocationContext {
            episode_id: EpisodeId::new(1),
            operation_id: OperationId::new("evaluate").unwrap(),
        };
        let deadline = EvaluationDeadline::start(Duration::from_secs(60)).unwrap();

        let error = collect_measurement(
            provider,
            &binding,
            &context,
            &SamplingPlan {
                settle_ms: 0,
                sample_count: 1,
                sample_interval_ms: 0,
            },
            &deadline,
        )
        .unwrap_err();

        assert_eq!(error.kind, EvaluationErrorKind::Cleanup);
        assert!(error.message.contains("expected 'session-1'"));
    }

    #[test]
    fn close_is_attempted_when_open_returns_after_the_deadline() {
        let mut provider_meta = meta();
        provider_meta.limits.timeout_ms = 1;
        let provider = Arc::new(FailingMeasurement {
            meta: provider_meta,
            closes: AtomicUsize::new(0),
            wrong_cleanup_session: false,
            open_delay: Duration::from_millis(20),
        });
        let binding = MeasurementBinding {
            capability_id: provider.meta.id.clone(),
            specification: json!({}),
        };
        let context = InvocationContext {
            episode_id: EpisodeId::new(1),
            operation_id: OperationId::new("evaluate-budget").unwrap(),
        };
        let deadline = EvaluationDeadline::start(Duration::from_millis(10)).unwrap();

        let error = collect_measurement(
            provider.clone(),
            &binding,
            &context,
            &SamplingPlan {
                settle_ms: 0,
                sample_count: 1,
                sample_interval_ms: 0,
            },
            &deadline,
        )
        .unwrap_err();

        assert_eq!(error.kind, EvaluationErrorKind::BudgetExceeded);
        assert_eq!(provider.closes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn numeric_samples_are_aggregated_by_median() {
        let make = |value| MetricBatch {
            started_at_ns: 1,
            ended_at_ns: 2,
            quality: MetricQuality::Valid,
            workload_fingerprint: Some("workload".to_string()),
            metrics: BTreeMap::from([(
                "latency".to_string(),
                MetricValue {
                    value: json!(value),
                    unit: "ms".to_string(),
                    kind: MetricKind::Gauge,
                },
            )]),
            provenance: json!({}),
        };

        let batch = aggregate_samples(vec![make(10.0), make(30.0), make(20.0)]).unwrap();

        assert_eq!(batch.metrics["latency"].value, json!(20.0));
        assert_eq!(batch.quality, MetricQuality::Valid);
    }
}
