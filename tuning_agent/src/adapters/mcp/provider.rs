use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapters::mcp::client::McpStdioClient;
use crate::adapters::mcp::schema::JsonSchema;
use crate::adapters::mcp::{McpAdapterError, McpAdapterErrorKind};
use crate::capability::{ComparisonPolicy, MeasurementProvider, MutationDriver, ProbeProvider};
use crate::domain::{
    content_digest, CapabilityMeta, CleanupReceipt, ComparisonConclusion, ComparisonEvidence,
    ComparisonRequest, ConditionEvidence, MeasurementOpenRequest, MeasurementSampleRequest,
    MeasurementSession, MeasurementSessionId, MetricBatch, MetricKind, MetricQuality, MetricValue,
    MutationApplyRequest, MutationFinalizeRequest, MutationOperationState, MutationPrepareRequest,
    MutationQuery, MutationReceipt, MutationRestoreRequest, MutationState, MutationStatus,
    MutationVerification, MutationVerifyRequest, OperationId, PreparedMutation, ProbeEvidence,
    ProbeRequest, ProviderError, ProviderErrorKind, ResourceKey,
};

pub(crate) trait ToolCaller: Send + Sync {
    fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<Value, McpAdapterError>;
}

impl ToolCaller for McpStdioClient {
    fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<Value, McpAdapterError> {
        McpStdioClient::call_tool(self, tool, arguments, timeout, max_output_bytes)
    }
}

struct ProviderCore {
    meta: CapabilityMeta,
    caller: Arc<dyn ToolCaller>,
    capability_input: JsonSchema,
}

impl ProviderCore {
    fn new(meta: CapabilityMeta, schemas: CapabilitySchemas, caller: Arc<dyn ToolCaller>) -> Self {
        Self {
            meta,
            caller,
            capability_input: schemas.input,
        }
    }

    fn invoke<Request: Serialize, Response: DeserializeOwned>(
        &self,
        tool: &OperationTool,
        request: &Request,
    ) -> Result<Response, ProviderError> {
        let output = self.invoke_value(tool, request)?;
        serde_json::from_value(output).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Protocol,
                format!(
                    "MCP tool '{}' returned an invalid structured response: {error}",
                    tool.name
                ),
            )
        })
    }

    fn invoke_value<Request: Serialize>(
        &self,
        tool: &OperationTool,
        request: &Request,
    ) -> Result<Value, ProviderError> {
        let arguments = serde_json::to_value(request).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("failed to encode MCP tool '{}' request: {error}", tool.name),
            )
        })?;
        tool.input.validate(&arguments).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!(
                    "MCP tool '{}' request violates its declared input schema: {error}",
                    tool.name
                ),
            )
        })?;
        let output = self
            .caller
            .call_tool(
                &tool.name,
                arguments,
                Duration::from_millis(self.meta.limits.timeout_ms),
                self.meta.limits.max_output_bytes,
            )
            .map_err(|error| error.provider_error())?;
        if let Some(schema) = &tool.output {
            schema.validate(&output).map_err(|error| {
                protocol(format!(
                    "MCP tool '{}' response violates its declared output schema: {error}",
                    tool.name
                ))
            })?;
        }
        Ok(output)
    }

    fn validate_capability_input(&self, value: &Value) -> Result<(), ProviderError> {
        self.capability_input.validate(value).map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("request violates the capability input schema: {error}"),
            )
        })
    }
}

#[derive(Clone)]
pub(crate) struct CapabilitySchemas {
    pub input: JsonSchema,
}

#[derive(Clone)]
pub(crate) struct OperationTool {
    pub name: String,
    pub input: JsonSchema,
    pub output: Option<JsonSchema>,
}

pub(crate) struct McpProbeProvider {
    core: ProviderCore,
    tool: OperationTool,
}

impl McpProbeProvider {
    pub(crate) fn new(
        meta: CapabilityMeta,
        schemas: CapabilitySchemas,
        tool: OperationTool,
        caller: Arc<dyn ToolCaller>,
    ) -> Self {
        Self {
            core: ProviderCore::new(meta, schemas, caller),
            tool,
        }
    }
}

impl ProbeProvider for McpProbeProvider {
    fn meta(&self) -> &CapabilityMeta {
        &self.core.meta
    }

    fn probe(&self, request: &ProbeRequest) -> Result<ProbeEvidence, ProviderError> {
        self.core.validate_capability_input(&request.arguments)?;
        let raw: WireProbeEvidence = self.core.invoke(&self.tool, request)?;
        raw.try_into()
    }
}

pub(crate) struct McpMeasurementProvider {
    core: ProviderCore,
    validate_tool: OperationTool,
    open_tool: OperationTool,
    sample_tool: OperationTool,
    close_tool: OperationTool,
}

impl McpMeasurementProvider {
    pub(crate) fn new(
        meta: CapabilityMeta,
        schemas: CapabilitySchemas,
        tools: MeasurementTools,
        caller: Arc<dyn ToolCaller>,
    ) -> Self {
        Self {
            core: ProviderCore::new(meta, schemas, caller),
            validate_tool: tools.validate,
            open_tool: tools.open,
            sample_tool: tools.sample,
            close_tool: tools.close,
        }
    }
}

impl MeasurementProvider for McpMeasurementProvider {
    fn meta(&self) -> &CapabilityMeta {
        &self.core.meta
    }

    fn validate_specification(&self, specification: &Value) -> Result<(), ProviderError> {
        validate_remote_specification(&self.core, &self.validate_tool, specification)
    }

    fn open(&self, request: &MeasurementOpenRequest) -> Result<MeasurementSession, ProviderError> {
        self.core
            .validate_capability_input(&request.specification)?;
        let raw: WireMeasurementSession = self.core.invoke(&self.open_tool, request)?;
        raw.try_into()
    }

    fn sample(&self, request: &MeasurementSampleRequest) -> Result<MetricBatch, ProviderError> {
        let raw: WireMetricBatch = self.core.invoke(&self.sample_tool, request)?;
        raw.try_into()
    }

    fn close(&self, session: &MeasurementSession) -> Result<CleanupReceipt, ProviderError> {
        let raw: WireCleanupReceipt = self.core.invoke(&self.close_tool, session)?;
        let receipt: CleanupReceipt = raw.try_into()?;
        if receipt.session_id != session.id {
            return Err(protocol(format!(
                "MCP measurement close returned session '{}', expected '{}'",
                receipt.session_id, session.id
            )));
        }
        Ok(receipt)
    }
}

pub(crate) struct McpComparisonPolicy {
    core: ProviderCore,
    validate_tool: OperationTool,
    compare_tool: OperationTool,
}

impl McpComparisonPolicy {
    pub(crate) fn new(
        meta: CapabilityMeta,
        schemas: CapabilitySchemas,
        validate_tool: OperationTool,
        compare_tool: OperationTool,
        caller: Arc<dyn ToolCaller>,
    ) -> Self {
        Self {
            core: ProviderCore::new(meta, schemas, caller),
            validate_tool,
            compare_tool,
        }
    }
}

impl ComparisonPolicy for McpComparisonPolicy {
    fn meta(&self) -> &CapabilityMeta {
        &self.core.meta
    }

    fn validate_specification(&self, specification: &Value) -> Result<(), ProviderError> {
        validate_remote_specification(&self.core, &self.validate_tool, specification)
    }

    fn compare(&self, request: &ComparisonRequest) -> Result<ComparisonEvidence, ProviderError> {
        self.core
            .validate_capability_input(&request.specification)?;
        let raw: WireComparisonEvidence = self.core.invoke(&self.compare_tool, request)?;
        raw.try_into()
    }
}

pub(crate) struct McpMutationDriver {
    core: ProviderCore,
    tools: MutationTools,
}

impl McpMutationDriver {
    pub(crate) fn new(
        meta: CapabilityMeta,
        schemas: CapabilitySchemas,
        tools: MutationTools,
        caller: Arc<dyn ToolCaller>,
    ) -> Self {
        Self {
            core: ProviderCore::new(meta, schemas, caller),
            tools,
        }
    }
}

impl MutationDriver for McpMutationDriver {
    fn meta(&self) -> &CapabilityMeta {
        &self.core.meta
    }

    fn prepare(&self, request: &MutationPrepareRequest) -> Result<PreparedMutation, ProviderError> {
        self.core.validate_capability_input(&request.arguments)?;
        let raw: WirePreparedMutation = self.core.invoke(&self.tools.prepare, request)?;
        let remote_resource = ResourceKey::new(raw.resource).map_err(invalid_response)?;
        let resource = ResourceKey::new(format!(
            "{}/{}",
            self.core.meta.provider.provider_id,
            remote_resource.as_str()
        ))
        .map_err(invalid_response)?;
        Ok(PreparedMutation {
            capability_id: self.core.meta.id.clone(),
            provider: self.core.meta.provider.clone(),
            resource,
            baseline: raw.baseline.try_into()?,
            desired: raw.desired.try_into()?,
            driver_data: raw.driver_data,
        })
    }

    fn apply(&self, request: &MutationApplyRequest) -> Result<MutationReceipt, ProviderError> {
        let raw: WireMutationReceipt = self.core.invoke(&self.tools.apply, request)?;
        raw.into_receipt(&request.operation_id)
    }

    fn status(&self, query: &MutationQuery) -> Result<MutationStatus, ProviderError> {
        let raw: WireMutationStatus = self.core.invoke(&self.tools.status, query)?;
        raw.into_status(&query.operation_id)
    }

    fn verify(
        &self,
        request: &MutationVerifyRequest,
    ) -> Result<MutationVerification, ProviderError> {
        let raw: WireMutationVerification = self.core.invoke(&self.tools.verify, request)?;
        Ok(MutationVerification {
            matched: raw.matched,
            observed: raw.observed.map(TryInto::try_into).transpose()?,
            details: raw.details,
        })
    }

    fn restore(&self, request: &MutationRestoreRequest) -> Result<MutationReceipt, ProviderError> {
        let raw: WireMutationReceipt = self.core.invoke(&self.tools.restore, request)?;
        raw.into_receipt(&request.operation_id)
    }

    fn finalize(
        &self,
        request: &MutationFinalizeRequest,
    ) -> Result<MutationReceipt, ProviderError> {
        let raw: WireMutationReceipt = self.core.invoke(&self.tools.finalize, request)?;
        raw.into_receipt(&request.operation_id)
    }
}

#[derive(Clone)]
pub(crate) struct MeasurementTools {
    pub validate: OperationTool,
    pub open: OperationTool,
    pub sample: OperationTool,
    pub close: OperationTool,
}

#[derive(Clone)]
pub(crate) struct MutationTools {
    pub prepare: OperationTool,
    pub apply: OperationTool,
    pub status: OperationTool,
    pub verify: OperationTool,
    pub restore: OperationTool,
    pub finalize: OperationTool,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct ValidateSpecificationRequest<'a> {
    specification: &'a Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValidateSpecificationResponse {
    valid: bool,
    #[serde(default)]
    message: Option<String>,
}

fn validate_remote_specification(
    core: &ProviderCore,
    tool: &OperationTool,
    specification: &Value,
) -> Result<(), ProviderError> {
    core.validate_capability_input(specification)?;
    let response: ValidateSpecificationResponse =
        core.invoke(tool, &ValidateSpecificationRequest { specification })?;
    if response.valid {
        Ok(())
    } else {
        Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            response
                .message
                .unwrap_or_else(|| "MCP provider rejected the specification".to_string()),
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProbeEvidence {
    observed_at_ns: u128,
    data: Value,
    warnings: Vec<String>,
}

impl TryFrom<WireProbeEvidence> for ProbeEvidence {
    type Error = ProviderError;

    fn try_from(raw: WireProbeEvidence) -> Result<Self, Self::Error> {
        if raw.warnings.len() > 256
            || raw
                .warnings
                .iter()
                .any(|warning| warning.len() > 4096 || warning.chars().any(char::is_control))
        {
            return Err(protocol("MCP probe returned invalid warnings"));
        }
        Ok(Self {
            observed_at_ns: raw.observed_at_ns,
            data: raw.data,
            warnings: raw.warnings,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMeasurementSession {
    id: String,
    #[serde(default)]
    driver_data: Value,
}

impl TryFrom<WireMeasurementSession> for MeasurementSession {
    type Error = ProviderError;

    fn try_from(raw: WireMeasurementSession) -> Result<Self, Self::Error> {
        Ok(Self {
            id: MeasurementSessionId::new(raw.id).map_err(invalid_response)?,
            driver_data: raw.driver_data,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCleanupReceipt {
    session_id: String,
    cleaned_up: bool,
    #[serde(default)]
    details: Value,
}

impl TryFrom<WireCleanupReceipt> for CleanupReceipt {
    type Error = ProviderError;

    fn try_from(raw: WireCleanupReceipt) -> Result<Self, Self::Error> {
        Ok(Self {
            session_id: MeasurementSessionId::new(raw.session_id).map_err(invalid_response)?,
            cleaned_up: raw.cleaned_up,
            details: raw.details,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMetricBatch {
    started_at_ns: u128,
    ended_at_ns: u128,
    quality: MetricQuality,
    #[serde(default)]
    workload_fingerprint: Option<String>,
    metrics: BTreeMap<String, WireMetricValue>,
    #[serde(default)]
    provenance: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMetricValue {
    value: Value,
    unit: String,
    kind: MetricKind,
}

impl TryFrom<WireMetricBatch> for MetricBatch {
    type Error = ProviderError;

    fn try_from(raw: WireMetricBatch) -> Result<Self, Self::Error> {
        if raw.ended_at_ns < raw.started_at_ns {
            return Err(protocol("MCP metric batch ends before it starts"));
        }
        if raw.metrics.len() > 4096 {
            return Err(protocol("MCP metric batch contains too many metrics"));
        }
        if raw
            .workload_fingerprint
            .as_ref()
            .is_some_and(|fingerprint| fingerprint.len() > 4096)
        {
            return Err(protocol("MCP workload fingerprint exceeds 4096 bytes"));
        }
        let mut metrics = BTreeMap::new();
        for (name, metric) in raw.metrics {
            if name.is_empty()
                || name.len() > 256
                || name.trim() != name
                || name.chars().any(char::is_control)
            {
                return Err(protocol("MCP metric name is invalid"));
            }
            if metric.unit.is_empty()
                || metric.unit.len() > 128
                || metric.unit.chars().any(char::is_control)
            {
                return Err(protocol(format!("MCP metric '{name}' has an invalid unit")));
            }
            let valid_value = match metric.kind {
                MetricKind::Gauge | MetricKind::Counter => {
                    metric.value.as_f64().is_some_and(f64::is_finite)
                }
                MetricKind::Boolean => metric.value.is_boolean(),
                MetricKind::Histogram => metric.value.is_array() || metric.value.is_object(),
            };
            if !valid_value {
                return Err(protocol(format!(
                    "MCP metric '{name}' value does not match its declared kind"
                )));
            }
            metrics.insert(
                name,
                MetricValue {
                    value: metric.value,
                    unit: metric.unit,
                    kind: metric.kind,
                },
            );
        }
        Ok(Self {
            started_at_ns: raw.started_at_ns,
            ended_at_ns: raw.ended_at_ns,
            quality: raw.quality,
            workload_fingerprint: raw.workload_fingerprint,
            metrics,
            provenance: raw.provenance,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireComparisonEvidence {
    conclusion: ComparisonConclusion,
    conditions: Vec<WireConditionEvidence>,
    #[serde(default)]
    details: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireConditionEvidence {
    name: String,
    passed: bool,
    #[serde(default)]
    details: Value,
}

impl TryFrom<WireComparisonEvidence> for ComparisonEvidence {
    type Error = ProviderError;

    fn try_from(raw: WireComparisonEvidence) -> Result<Self, Self::Error> {
        if raw.conditions.len() > 4096 {
            return Err(protocol("MCP comparison returned too many conditions"));
        }
        let mut conditions = Vec::with_capacity(raw.conditions.len());
        for condition in raw.conditions {
            if condition.name.is_empty()
                || condition.name.len() > 256
                || condition.name.chars().any(char::is_control)
            {
                return Err(protocol("MCP comparison condition name is invalid"));
            }
            conditions.push(ConditionEvidence {
                name: condition.name,
                passed: condition.passed,
                details: condition.details,
            });
        }
        Ok(Self {
            conclusion: raw.conclusion,
            conditions,
            details: raw.details,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMutationState {
    value: Value,
}

impl TryFrom<WireMutationState> for MutationState {
    type Error = ProviderError;

    fn try_from(raw: WireMutationState) -> Result<Self, Self::Error> {
        let digest = content_digest(&raw.value).map_err(invalid_response)?;
        Ok(Self {
            value: raw.value,
            digest,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePreparedMutation {
    resource: String,
    baseline: WireMutationState,
    desired: WireMutationState,
    #[serde(default)]
    driver_data: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMutationReceipt {
    operation_id: String,
    state: MutationOperationState,
    #[serde(default)]
    observed: Option<WireMutationState>,
    #[serde(default)]
    driver_data: Value,
}

impl WireMutationReceipt {
    fn into_receipt(self, expected: &OperationId) -> Result<MutationReceipt, ProviderError> {
        let operation_id = OperationId::new(self.operation_id).map_err(invalid_response)?;
        if &operation_id != expected {
            return Err(protocol(format!(
                "MCP mutation receipt operation '{operation_id}' does not match '{expected}'"
            )));
        }
        Ok(MutationReceipt {
            operation_id,
            state: self.state,
            observed: self.observed.map(TryInto::try_into).transpose()?,
            driver_data: self.driver_data,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMutationStatus {
    operation_id: String,
    state: MutationOperationState,
    #[serde(default)]
    observed: Option<WireMutationState>,
    #[serde(default)]
    driver_data: Value,
}

impl WireMutationStatus {
    fn into_status(self, expected: &OperationId) -> Result<MutationStatus, ProviderError> {
        let operation_id = OperationId::new(self.operation_id).map_err(invalid_response)?;
        if &operation_id != expected {
            return Err(protocol(format!(
                "MCP mutation status operation '{operation_id}' does not match '{expected}'"
            )));
        }
        Ok(MutationStatus {
            operation_id,
            state: self.state,
            observed: self.observed.map(TryInto::try_into).transpose()?,
            driver_data: self.driver_data,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMutationVerification {
    matched: bool,
    #[serde(default)]
    observed: Option<WireMutationState>,
    #[serde(default)]
    details: Value,
}

fn invalid_response(message: impl ToString) -> ProviderError {
    protocol(message.to_string())
}

fn protocol(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Protocol, message)
}

pub(crate) fn caller(client: McpStdioClient) -> Arc<dyn ToolCaller> {
    Arc::new(client)
}

pub(crate) fn manifest_error(message: impl Into<String>) -> McpAdapterError {
    McpAdapterError::new(McpAdapterErrorKind::Manifest, message)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::domain::{
        CapabilityId, CapabilityKind, CapabilityLimits, Digest, EffectClass, EpisodeId,
        EpisodePhase, InvocationContext, ProviderClass, ProviderId, ProviderPin, ProviderVersion,
    };

    struct MockCaller {
        outputs: Mutex<VecDeque<Value>>,
    }

    impl ToolCaller for MockCaller {
        fn call_tool(
            &self,
            _tool: &str,
            _arguments: Value,
            _timeout: Duration,
            _max_output_bytes: usize,
        ) -> Result<Value, McpAdapterError> {
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| manifest_error("mock output exhausted"))
        }
    }

    #[test]
    fn probe_adapter_decodes_strict_structured_evidence() {
        let caller = Arc::new(MockCaller {
            outputs: Mutex::new(VecDeque::from([json!({
                "observed_at_ns": 7,
                "data": {"psi": 1.5},
                "warnings": []
            })])),
        });
        let provider = McpProbeProvider::new(
            meta(CapabilityKind::Probe),
            schemas(),
            operation("observe"),
            caller,
        );
        let evidence = provider
            .probe(&ProbeRequest {
                context: context("probe"),
                arguments: json!({}),
            })
            .unwrap();
        assert_eq!(evidence.data["psi"], 1.5);
    }

    #[test]
    fn mutation_prepare_forces_local_capability_pin_and_computes_state_digests() {
        let caller = Arc::new(MockCaller {
            outputs: Mutex::new(VecDeque::from([json!({
                "resource": "mcp:test/resource",
                "baseline": {"value": "old"},
                "desired": {"value": "new"},
                "driver_data": {"token": "opaque"}
            })])),
        });
        let meta = meta(CapabilityKind::Mutation);
        let provider = McpMutationDriver::new(
            meta.clone(),
            schemas(),
            MutationTools {
                prepare: operation("prepare"),
                apply: operation("apply"),
                status: operation("status"),
                verify: operation("verify"),
                restore: operation("restore"),
                finalize: operation("finalize"),
            },
            caller,
        );
        let prepared = provider
            .prepare(&MutationPrepareRequest {
                context: context("prepare"),
                arguments: json!({"value": "new"}),
            })
            .unwrap();
        assert_eq!(prepared.capability_id, meta.id);
        assert_eq!(prepared.provider, meta.provider);
        assert_eq!(prepared.resource.as_str(), "mcp/test/mcp:test/resource");
        assert_eq!(prepared.baseline.value, "old");
        assert!(prepared.baseline.digest.as_str().starts_with("sha256:"));
    }

    #[test]
    fn measurement_adapter_rejects_metric_kind_mismatch() {
        let caller = Arc::new(MockCaller {
            outputs: Mutex::new(VecDeque::from([json!({
                "started_at_ns": 1,
                "ended_at_ns": 2,
                "quality": "valid",
                "metrics": {
                    "latency": {"value": "not-a-number", "unit": "ms", "kind": "gauge"}
                }
            })])),
        });
        let provider = McpMeasurementProvider::new(
            meta(CapabilityKind::Measurement),
            schemas(),
            MeasurementTools {
                validate: operation("validate"),
                open: operation("open"),
                sample: operation("sample"),
                close: operation("close"),
            },
            caller,
        );
        let result = provider.sample(&MeasurementSampleRequest {
            context: context("sample"),
            session: MeasurementSession {
                id: MeasurementSessionId::new("session").unwrap(),
                driver_data: Value::Null,
            },
        });
        assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Protocol);
    }

    #[test]
    fn probe_validates_capability_arguments_before_calling_mcp() {
        let caller = Arc::new(MockCaller {
            outputs: Mutex::new(VecDeque::from([json!({
                "observed_at_ns": 7,
                "data": {},
                "warnings": []
            })])),
        });
        let provider = McpProbeProvider::new(
            meta(CapabilityKind::Probe),
            CapabilitySchemas {
                input: JsonSchema::compile(json!({
                    "type": "object",
                    "required": ["section"],
                    "properties": {"section": {"type": "string"}}
                }))
                .unwrap(),
            },
            operation("observe"),
            caller.clone(),
        );
        let error = provider
            .probe(&ProbeRequest {
                context: context("probe-schema"),
                arguments: json!({}),
            })
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(caller.outputs.lock().unwrap().len(), 1);
    }

    #[test]
    fn probe_validates_wire_request_and_structured_response_schemas() {
        let caller = Arc::new(MockCaller {
            outputs: Mutex::new(VecDeque::from([json!({
                "observed_at_ns": 7,
                "data": {},
                "warnings": []
            })])),
        });
        let mut tool = operation("observe");
        tool.input = JsonSchema::compile(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["context"] ,
            "properties": {"context": {"type": "object"}}
        }))
        .unwrap();
        let provider =
            McpProbeProvider::new(meta(CapabilityKind::Probe), schemas(), tool, caller.clone());
        let error = provider
            .probe(&ProbeRequest {
                context: context("wire-schema"),
                arguments: json!({}),
            })
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(caller.outputs.lock().unwrap().len(), 1);

        let mut tool = operation("observe");
        tool.output = Some(
            JsonSchema::compile(json!({
                "type": "object",
                "required": ["server_attestation"]
            }))
            .unwrap(),
        );
        let provider = McpProbeProvider::new(meta(CapabilityKind::Probe), schemas(), tool, caller);
        let error = provider
            .probe(&ProbeRequest {
                context: context("output-schema"),
                arguments: json!({}),
            })
            .unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Protocol);
    }

    fn context(operation: &str) -> crate::domain::InvocationContext {
        InvocationContext {
            episode_id: EpisodeId::new(1),
            operation_id: OperationId::new(operation).unwrap(),
        }
    }

    fn schemas() -> CapabilitySchemas {
        CapabilitySchemas {
            input: JsonSchema::compile(json!({"type": "object"})).unwrap(),
        }
    }

    fn operation(name: &str) -> OperationTool {
        OperationTool {
            name: name.to_string(),
            input: JsonSchema::compile(json!({"type": "object"})).unwrap(),
            output: None,
        }
    }

    fn meta(kind: CapabilityKind) -> CapabilityMeta {
        let (effect, phases) = match kind {
            CapabilityKind::Probe => (
                EffectClass::ReadOnly,
                vec![EpisodePhase::Clean, EpisodePhase::Experimenting],
            ),
            CapabilityKind::Mutation => (
                EffectClass::ReversibleMutation,
                vec![EpisodePhase::Clean, EpisodePhase::Experimenting],
            ),
            CapabilityKind::Measurement => {
                (EffectClass::ReadOnly, vec![EpisodePhase::CommitPending])
            }
            CapabilityKind::Comparison => (
                EffectClass::PureComputation,
                vec![EpisodePhase::CommitPending],
            ),
        };
        let mut meta = CapabilityMeta::new(
            CapabilityId::new(format!("mcp/test/{kind:?}")).unwrap(),
            kind,
            effect,
            ProviderPin {
                provider_id: ProviderId::new("mcp/test").unwrap(),
                provider_version: ProviderVersion::new("1").unwrap(),
                provider_class: ProviderClass::Mcp,
                manifest_digest: Digest::new("sha256:test").unwrap(),
            },
            "test",
            json!({"type": "object"}),
            json!({"type": "object"}),
        )
        .with_allowed_phases(phases);
        meta.limits = CapabilityLimits {
            timeout_ms: 100,
            max_output_bytes: 4096,
        };
        meta.idempotent = kind == CapabilityKind::Mutation;
        meta.deterministic = kind == CapabilityKind::Comparison;
        meta
    }
}
