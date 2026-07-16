use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::adapters::mcp::client::McpStdioClient;
use crate::adapters::mcp::provider::{
    caller, manifest_error, CapabilitySchemas, McpComparisonPolicy, McpMeasurementProvider,
    McpMutationDriver, McpProbeProvider, MeasurementTools, MutationTools, OperationTool,
    ToolCaller,
};
use crate::adapters::mcp::schema::JsonSchema;
use crate::capability::{ComparisonPolicy, MeasurementProvider, MutationDriver, ProbeProvider};
use crate::config::McpServerConfig;
use crate::domain::{
    content_digest, CapabilityId, CapabilityKind, CapabilityLimits, CapabilityMeta, EffectClass,
    EpisodePhase, ProviderClass, ProviderId, ProviderPin, ProviderVersion,
};

use crate::adapters::mcp::{McpAdapterError, McpAdapterErrorKind};

pub const TUNING_CAPABILITIES_URI: &str = "tuning://capabilities/v1";

const MANIFEST_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_CAPABILITIES: usize = 1024;
const MAX_TOOL_PAGES: usize = 64;
const MAX_TOOLS: usize = 4096;
const MAX_CAPABILITY_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub enum LoadedMcpCapability {
    Probe(Arc<dyn ProbeProvider>),
    Mutation(Arc<dyn MutationDriver>),
    Measurement(Arc<dyn MeasurementProvider>),
    Comparison(Arc<dyn ComparisonPolicy>),
}

impl LoadedMcpCapability {
    pub fn meta(&self) -> &CapabilityMeta {
        match self {
            Self::Probe(provider) => provider.meta(),
            Self::Mutation(provider) => provider.meta(),
            Self::Measurement(provider) => provider.meta(),
            Self::Comparison(provider) => provider.meta(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkippedMcpCapability {
    pub capability_id: String,
    pub reason: String,
}

pub struct LoadedMcpServer {
    server_id: String,
    provider: ProviderPin,
    capabilities: Vec<LoadedMcpCapability>,
    skipped: Vec<SkippedMcpCapability>,
}

impl LoadedMcpServer {
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn provider(&self) -> &ProviderPin {
        &self.provider
    }

    pub fn capabilities(&self) -> &[LoadedMcpCapability] {
        &self.capabilities
    }

    pub fn skipped(&self) -> &[SkippedMcpCapability] {
        &self.skipped
    }

    pub fn into_capabilities(self) -> Vec<LoadedMcpCapability> {
        self.capabilities
    }
}

pub fn load_server(config: &McpServerConfig) -> Result<LoadedMcpServer, McpAdapterError> {
    let client = McpStdioClient::connect(config)?;
    let manifest = read_manifest(&client)?;
    let tools = discover_tools(&client)?;
    build_server(config, manifest, tools, caller(client))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema_version: u32,
    provider: RawProvider,
    capabilities: Vec<RawCapability>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    id: String,
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapability {
    id: String,
    kind: CapabilityKind,
    effect: EffectClass,
    description: String,
    input_schema: Value,
    output_schema: Value,
    allowed_phases: Vec<EpisodePhase>,
    limits: RawLimits,
    deterministic: bool,
    idempotent: bool,
    operations: RawOperations,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    timeout_ms: u64,
    max_output_bytes: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawOperations {
    probe: Option<String>,
    validate: Option<String>,
    open: Option<String>,
    sample: Option<String>,
    close: Option<String>,
    compare: Option<String>,
    prepare: Option<String>,
    apply: Option<String>,
    status: Option<String>,
    verify: Option<String>,
    restore: Option<String>,
    finalize: Option<String>,
}

#[derive(Clone)]
struct DiscoveredTool {
    input_schema: Value,
    output_schema: Option<Value>,
}

fn read_manifest(client: &McpStdioClient) -> Result<RawManifest, McpAdapterError> {
    let result = client.request(
        "resources/read",
        json!({"uri": TUNING_CAPABILITIES_URI}),
        client.request_timeout(),
    )?;
    let object = result
        .as_object()
        .ok_or_else(|| protocol_error("MCP resources/read result must be an object"))?;
    let contents = object
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error("MCP resources/read result is missing contents"))?;
    if contents.len() != 1 {
        return Err(protocol_error(format!(
            "MCP resources/read must return exactly one manifest resource, got {}",
            contents.len()
        )));
    }
    let content = contents[0]
        .as_object()
        .ok_or_else(|| protocol_error("MCP manifest resource content must be an object"))?;
    if content.get("uri").and_then(Value::as_str) != Some(TUNING_CAPABILITIES_URI) {
        return Err(protocol_error(format!(
            "MCP manifest resource must echo URI '{TUNING_CAPABILITIES_URI}'"
        )));
    }
    let text = content.get("text").and_then(Value::as_str).ok_or_else(|| {
        protocol_error("MCP manifest resource must contain text JSON, not blob content")
    })?;
    if content.contains_key("blob") {
        return Err(protocol_error(
            "MCP manifest resource must not mix text and blob content",
        ));
    }
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(manifest_error(format!(
            "MCP capability manifest exceeds {MAX_MANIFEST_BYTES} bytes"
        )));
    }
    serde_json::from_str(text).map_err(|error| {
        manifest_error(format!(
            "failed to decode strict MCP capability manifest: {error}"
        ))
    })
}

fn discover_tools(
    client: &McpStdioClient,
) -> Result<BTreeMap<String, DiscoveredTool>, McpAdapterError> {
    let mut tools = BTreeMap::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = BTreeSet::new();

    for _ in 0..MAX_TOOL_PAGES {
        let params = cursor
            .as_ref()
            .map(|cursor| json!({"cursor": cursor}))
            .unwrap_or_else(|| json!({}));
        let result = client.request("tools/list", params, client.request_timeout())?;
        let object = result
            .as_object()
            .ok_or_else(|| protocol_error("MCP tools/list result must be an object"))?;
        let page = object
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| protocol_error("MCP tools/list result is missing tools"))?;

        for raw_tool in page {
            if tools.len() >= MAX_TOOLS {
                return Err(protocol_error(format!(
                    "MCP tools/list exceeds the {MAX_TOOLS}-tool limit"
                )));
            }
            let tool = raw_tool
                .as_object()
                .ok_or_else(|| protocol_error("MCP tools/list tool must be an object"))?;
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol_error("MCP tools/list tool is missing name"))?;
            validate_tool_name(name)?;
            let input_schema = tool.get("inputSchema").cloned().ok_or_else(|| {
                protocol_error(format!("MCP tool '{name}' is missing inputSchema"))
            })?;
            if !input_schema.is_object() {
                return Err(protocol_error(format!(
                    "MCP tool '{name}' inputSchema must be an object"
                )));
            }
            let output_schema = tool.get("outputSchema").cloned();
            if output_schema
                .as_ref()
                .is_some_and(|schema| !schema.is_object())
            {
                return Err(protocol_error(format!(
                    "MCP tool '{name}' outputSchema must be an object when declared"
                )));
            }
            if tools
                .insert(
                    name.to_string(),
                    DiscoveredTool {
                        input_schema,
                        output_schema,
                    },
                )
                .is_some()
            {
                return Err(protocol_error(format!(
                    "MCP tools/list contains duplicate tool '{name}'"
                )));
            }
        }

        cursor = match object.get("nextCursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) if !cursor.is_empty() => Some(cursor.clone()),
            Some(_) => {
                return Err(protocol_error(
                    "MCP tools/list nextCursor must be a non-empty string or null",
                ));
            }
        };
        let Some(next) = cursor.as_ref() else {
            return Ok(tools);
        };
        if !seen_cursors.insert(next.clone()) {
            return Err(protocol_error(
                "MCP tools/list repeated a pagination cursor",
            ));
        }
    }
    Err(protocol_error(format!(
        "MCP tools/list exceeds the {MAX_TOOL_PAGES}-page limit"
    )))
}

fn build_server(
    config: &McpServerConfig,
    manifest: RawManifest,
    tools: BTreeMap<String, DiscoveredTool>,
    caller: Arc<dyn ToolCaller>,
) -> Result<LoadedMcpServer, McpAdapterError> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(manifest_error(format!(
            "unsupported MCP capability manifest schema_version {}, expected {MANIFEST_SCHEMA_VERSION}",
            manifest.schema_version
        )));
    }
    if manifest.capabilities.len() > MAX_CAPABILITIES {
        return Err(manifest_error(format!(
            "MCP capability manifest exceeds the {MAX_CAPABILITIES}-capability limit"
        )));
    }

    let remote_provider_id =
        ProviderId::new(manifest.provider.id.clone()).map_err(manifest_error)?;
    let provider_version =
        ProviderVersion::new(manifest.provider.version.clone()).map_err(manifest_error)?;
    let provider_id = ProviderId::new(format!("mcp/{}/{}", config.id, remote_provider_id.as_str()))
        .map_err(manifest_error)?;
    let manifest_digest = content_digest(&manifest).map_err(manifest_error)?;
    let provider = ProviderPin {
        provider_id,
        provider_version,
        provider_class: ProviderClass::Mcp,
        manifest_digest,
    };

    let mut declared_ids = BTreeSet::new();
    for capability in &manifest.capabilities {
        CapabilityId::new(capability.id.clone()).map_err(manifest_error)?;
        if !declared_ids.insert(capability.id.clone()) {
            return Err(manifest_error(format!(
                "MCP manifest contains duplicate capability id '{}'",
                capability.id
            )));
        }
    }
    for allowed in &config.allowed_capabilities {
        if !declared_ids.contains(allowed) {
            return Err(McpAdapterError::new(
                McpAdapterErrorKind::InvalidConfig,
                format!(
                    "MCP server '{}' allowlist references undeclared capability '{allowed}'",
                    config.id
                ),
            ));
        }
    }
    let allowlist = config
        .allowed_capabilities
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();

    let mut loaded = Vec::new();
    let mut skipped = Vec::new();
    for raw in manifest.capabilities {
        let validated = validate_capability(config, raw, &provider)?;
        let source_id = validated.source_id.clone();
        let normalized_id = validated.meta.id.to_string();
        let kind = validated.meta.kind;
        // Resolve every declared operation before filtering. An allowlist is an authority
        // boundary, not permission for a server to publish dangling or invalid contracts.
        let capability = build_capability(validated, &tools, caller.clone())?;
        if !allowlist.is_empty() && !allowlist.contains(source_id.as_str()) {
            skipped.push(SkippedMcpCapability {
                capability_id: normalized_id,
                reason: "not selected by the server capability allowlist".to_string(),
            });
            continue;
        }
        if kind == CapabilityKind::Mutation && !config.allow_mutations {
            skipped.push(SkippedMcpCapability {
                capability_id: normalized_id,
                reason: "MCP mutations are disabled for this server".to_string(),
            });
            continue;
        }

        loaded.push(capability);
    }

    Ok(LoadedMcpServer {
        server_id: config.id.clone(),
        provider,
        capabilities: loaded,
        skipped,
    })
}

struct ValidatedCapability {
    source_id: String,
    meta: CapabilityMeta,
    input_schema: JsonSchema,
    operations: RawOperations,
}

fn validate_capability(
    config: &McpServerConfig,
    raw: RawCapability,
    provider: &ProviderPin,
) -> Result<ValidatedCapability, McpAdapterError> {
    let source_id = CapabilityId::new(raw.id.clone()).map_err(manifest_error)?;
    let id = CapabilityId::new(format!("mcp/{}/{}", config.id, source_id.as_str()))
        .map_err(manifest_error)?;
    if raw.description.trim().is_empty()
        || raw.description.len() > 4096
        || raw.description.chars().any(char::is_control)
    {
        return Err(manifest_error(format!(
            "MCP capability '{id}' has an invalid description"
        )));
    }
    validate_security_contract(&id, &raw)?;
    validate_phases(&id, raw.kind, &raw.allowed_phases)?;
    if raw.limits.timeout_ms == 0
        || raw.limits.max_output_bytes == 0
        || raw.limits.max_output_bytes > MAX_CAPABILITY_OUTPUT_BYTES
    {
        return Err(manifest_error(format!(
            "MCP capability '{id}' has invalid execution limits"
        )));
    }
    validate_operation_shape(&id, raw.kind, &raw.operations)?;

    let input_schema = JsonSchema::compile(raw.input_schema.clone()).map_err(|error| {
        manifest_error(format!(
            "MCP capability '{id}' has an invalid input_schema: {error}"
        ))
    })?;
    JsonSchema::compile(raw.output_schema.clone()).map_err(|error| {
        manifest_error(format!(
            "MCP capability '{id}' has an invalid output_schema: {error}"
        ))
    })?;

    let mut meta = CapabilityMeta::new(
        id,
        raw.kind,
        raw.effect,
        provider.clone(),
        raw.description,
        raw.input_schema,
        raw.output_schema,
    )
    .with_allowed_phases(raw.allowed_phases);
    meta.limits = CapabilityLimits {
        timeout_ms: raw.limits.timeout_ms.min(config.request_timeout_ms),
        max_output_bytes: raw.limits.max_output_bytes,
    };
    meta.deterministic = raw.deterministic;
    meta.idempotent = raw.idempotent;
    Ok(ValidatedCapability {
        source_id: source_id.to_string(),
        meta,
        input_schema,
        operations: raw.operations,
    })
}

fn validate_security_contract(
    id: &CapabilityId,
    raw: &RawCapability,
) -> Result<(), McpAdapterError> {
    let valid = match raw.kind {
        CapabilityKind::Probe | CapabilityKind::Measurement => raw.effect == EffectClass::ReadOnly,
        CapabilityKind::Comparison => {
            raw.effect == EffectClass::PureComputation && raw.deterministic
        }
        CapabilityKind::Mutation => raw.effect == EffectClass::ReversibleMutation && raw.idempotent,
    };
    if valid {
        Ok(())
    } else {
        Err(manifest_error(format!(
            "MCP capability '{id}' violates the required effect/determinism/idempotency contract"
        )))
    }
}

fn validate_phases(
    id: &CapabilityId,
    kind: CapabilityKind,
    phases: &[EpisodePhase],
) -> Result<(), McpAdapterError> {
    let mut actual = phases.to_vec();
    actual.sort_by_key(|phase| *phase as u8);
    if actual.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(manifest_error(format!(
            "MCP capability '{id}' contains duplicate allowed phases"
        )));
    }
    let expected = match kind {
        CapabilityKind::Probe | CapabilityKind::Mutation => {
            vec![EpisodePhase::Clean, EpisodePhase::Experimenting]
        }
        CapabilityKind::Measurement | CapabilityKind::Comparison => {
            vec![EpisodePhase::CommitPending]
        }
    };
    if actual != expected {
        return Err(manifest_error(format!(
            "MCP capability '{id}' must declare exactly the phases permitted for {kind:?}"
        )));
    }
    Ok(())
}

fn validate_operation_shape(
    id: &CapabilityId,
    kind: CapabilityKind,
    operations: &RawOperations,
) -> Result<(), McpAdapterError> {
    let fields = [
        ("probe", &operations.probe),
        ("validate", &operations.validate),
        ("open", &operations.open),
        ("sample", &operations.sample),
        ("close", &operations.close),
        ("compare", &operations.compare),
        ("prepare", &operations.prepare),
        ("apply", &operations.apply),
        ("status", &operations.status),
        ("verify", &operations.verify),
        ("restore", &operations.restore),
        ("finalize", &operations.finalize),
    ];
    let expected: &[&str] = match kind {
        CapabilityKind::Probe => &["probe"],
        CapabilityKind::Measurement => &["validate", "open", "sample", "close"],
        CapabilityKind::Comparison => &["validate", "compare"],
        CapabilityKind::Mutation => &[
            "prepare", "apply", "status", "verify", "restore", "finalize",
        ],
    };
    for (name, value) in fields {
        if expected.contains(&name) != value.is_some() {
            return Err(manifest_error(format!(
                "MCP capability '{id}' operations must declare exactly {}",
                expected.join(", ")
            )));
        }
        if let Some(value) = value {
            validate_tool_name(value).map_err(|error| {
                manifest_error(format!(
                    "MCP capability '{id}' operation '{name}' is invalid: {error}"
                ))
            })?;
        }
    }
    Ok(())
}

fn build_capability(
    capability: ValidatedCapability,
    discovered: &BTreeMap<String, DiscoveredTool>,
    caller: Arc<dyn ToolCaller>,
) -> Result<LoadedMcpCapability, McpAdapterError> {
    let schemas = CapabilitySchemas {
        input: capability.input_schema,
    };
    let id = capability.meta.id.clone();
    let operations = capability.operations;
    Ok(match capability.meta.kind {
        CapabilityKind::Probe => LoadedMcpCapability::Probe(Arc::new(McpProbeProvider::new(
            capability.meta,
            schemas,
            resolve_tool(&id, required(operations.probe, "probe")?, discovered)?,
            caller,
        ))),
        CapabilityKind::Measurement => {
            let tools = MeasurementTools {
                validate: resolve_tool(
                    &id,
                    required(operations.validate, "validate")?,
                    discovered,
                )?,
                open: resolve_tool(&id, required(operations.open, "open")?, discovered)?,
                sample: resolve_tool(&id, required(operations.sample, "sample")?, discovered)?,
                close: resolve_tool(&id, required(operations.close, "close")?, discovered)?,
            };
            LoadedMcpCapability::Measurement(Arc::new(McpMeasurementProvider::new(
                capability.meta,
                schemas,
                tools,
                caller,
            )))
        }
        CapabilityKind::Comparison => {
            LoadedMcpCapability::Comparison(Arc::new(McpComparisonPolicy::new(
                capability.meta,
                schemas,
                resolve_tool(&id, required(operations.validate, "validate")?, discovered)?,
                resolve_tool(&id, required(operations.compare, "compare")?, discovered)?,
                caller,
            )))
        }
        CapabilityKind::Mutation => {
            let tools = MutationTools {
                prepare: resolve_tool(&id, required(operations.prepare, "prepare")?, discovered)?,
                apply: resolve_tool(&id, required(operations.apply, "apply")?, discovered)?,
                status: resolve_tool(&id, required(operations.status, "status")?, discovered)?,
                verify: resolve_tool(&id, required(operations.verify, "verify")?, discovered)?,
                restore: resolve_tool(&id, required(operations.restore, "restore")?, discovered)?,
                finalize: resolve_tool(
                    &id,
                    required(operations.finalize, "finalize")?,
                    discovered,
                )?,
            };
            LoadedMcpCapability::Mutation(Arc::new(McpMutationDriver::new(
                capability.meta,
                schemas,
                tools,
                caller,
            )))
        }
    })
}

fn required(value: Option<String>, operation: &str) -> Result<String, McpAdapterError> {
    value.ok_or_else(|| manifest_error(format!("missing required MCP operation '{operation}'")))
}

fn resolve_tool(
    capability_id: &CapabilityId,
    name: String,
    discovered: &BTreeMap<String, DiscoveredTool>,
) -> Result<OperationTool, McpAdapterError> {
    let tool = discovered.get(&name).ok_or_else(|| {
        manifest_error(format!(
            "MCP capability '{capability_id}' references absent tool '{name}'"
        ))
    })?;
    let input = JsonSchema::compile(tool.input_schema.clone()).map_err(|error| {
        manifest_error(format!(
            "MCP tool '{name}' has an unsupported or invalid inputSchema: {error}"
        ))
    })?;
    let output = tool
        .output_schema
        .clone()
        .map(JsonSchema::compile)
        .transpose()
        .map_err(|error| {
            manifest_error(format!(
                "MCP tool '{name}' has an unsupported or invalid outputSchema: {error}"
            ))
        })?;
    Ok(OperationTool {
        name,
        input,
        output,
    })
}

fn validate_tool_name(name: &str) -> Result<(), McpAdapterError> {
    if name.is_empty()
        || name.len() > 256
        || name.trim() != name
        || name.chars().any(char::is_control)
    {
        Err(protocol_error("MCP tool name is invalid"))
    } else {
        Ok(())
    }
}

fn protocol_error(message: impl Into<String>) -> McpAdapterError {
    McpAdapterError::new(McpAdapterErrorKind::Protocol, message)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::json;

    use super::*;

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
    fn manifest_is_strict_and_forces_mcp_provider_pin() {
        let raw = parse_manifest(json!({
            "schema_version": 1,
            "provider": {"id": "remote", "version": "1.2.3"},
            "capabilities": [probe_manifest()]
        }));
        let loaded = build_server(
            &config(),
            raw,
            tools(&["observe"]),
            Arc::new(MockCaller {
                outputs: Mutex::new(VecDeque::new()),
            }),
        )
        .unwrap();
        assert_eq!(loaded.capabilities().len(), 1);
        assert_eq!(loaded.provider().provider_class, ProviderClass::Mcp);
        assert_eq!(loaded.provider().provider_id.as_str(), "mcp/server/remote");
        assert_eq!(
            loaded.capabilities()[0].meta().id.as_str(),
            "mcp/server/probe"
        );
        assert!(loaded
            .provider()
            .manifest_digest
            .as_str()
            .starts_with("sha256:"));
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_missing_tools() {
        let invalid = json!({
            "schema_version": 1,
            "provider": {"id": "remote", "version": "1", "class": "local"},
            "capabilities": []
        });
        assert!(serde_json::from_value::<RawManifest>(invalid).is_err());

        let error = build_server(
            &config(),
            parse_manifest(json!({
                "schema_version": 1,
                "provider": {"id": "remote", "version": "1"},
                "capabilities": [probe_manifest()]
            })),
            BTreeMap::new(),
            Arc::new(MockCaller {
                outputs: Mutex::new(VecDeque::new()),
            }),
        )
        .err()
        .expect("missing referenced tool must fail manifest loading");
        assert_eq!(error.kind, McpAdapterErrorKind::Manifest);
        assert!(error.message.contains("absent tool"));
    }

    #[test]
    fn mutation_requires_opt_in_and_is_reported_as_skipped() {
        let mut mutation = probe_manifest();
        mutation["id"] = json!("mutation");
        mutation["kind"] = json!("mutation");
        mutation["effect"] = json!("reversible_mutation");
        mutation["idempotent"] = json!(true);
        mutation["operations"] = json!({
            "prepare": "prepare", "apply": "apply", "status": "status",
            "verify": "verify", "restore": "restore", "finalize": "finalize"
        });
        let raw = parse_manifest(json!({
            "schema_version": 1,
            "provider": {"id": "remote", "version": "1"},
            "capabilities": [mutation]
        }));
        let loaded = build_server(
            &config(),
            raw,
            tools(&[
                "prepare", "apply", "status", "verify", "restore", "finalize",
            ]),
            Arc::new(MockCaller {
                outputs: Mutex::new(VecDeque::new()),
            }),
        )
        .unwrap();
        assert!(loaded.capabilities().is_empty());
        assert_eq!(loaded.skipped().len(), 1);
        assert_eq!(loaded.skipped()[0].capability_id, "mcp/server/mutation");
    }

    #[test]
    fn server_allowlist_matches_raw_ids_before_namespace_normalization() {
        let raw = parse_manifest(json!({
            "schema_version": 1,
            "provider": {"id": "remote", "version": "1"},
            "capabilities": [probe_manifest()]
        }));
        let mut config = config();
        config.allowed_capabilities = vec!["probe".to_string()];
        let loaded = build_server(
            &config,
            raw,
            tools(&["observe"]),
            Arc::new(MockCaller {
                outputs: Mutex::new(VecDeque::new()),
            }),
        )
        .unwrap();
        assert_eq!(
            loaded.capabilities()[0].meta().id.as_str(),
            "mcp/server/probe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_server_discovers_and_invokes_a_persistent_stdio_provider() {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "fixture", "version": "1"}
            }
        })
        .to_string();
        let manifest_text = json!({
            "schema_version": 1,
            "provider": {"id": "remote", "version": "1"},
            "capabilities": [probe_manifest()]
        })
        .to_string();
        let resource = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "contents": [{
                    "uri": TUNING_CAPABILITIES_URI,
                    "mimeType": "application/json",
                    "text": manifest_text
                }]
            }
        })
        .to_string();
        let tool_list = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {
                "tools": [{
                    "name": "observe",
                    "description": "fixture probe",
                    "inputSchema": {"type": "object"},
                    "outputSchema": {"type": "object"},
                    "annotations": {"readOnlyHint": false}
                }]
            }
        })
        .to_string();
        let tool_result = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "result": {
                "content": [],
                "structuredContent": {
                    "observed_at_ns": 9,
                    "data": {"source": "stdio"},
                    "warnings": []
                },
                "isError": false
            }
        })
        .to_string();
        let script = r#"
            IFS= read -r initialize || exit 10
            case "$initialize" in *\"method\":\"initialize\"*) ;; *) exit 11 ;; esac
            printf '%s\n' "$1"
            IFS= read -r initialized || exit 12
            case "$initialized" in *notifications/initialized*) ;; *) exit 13 ;; esac
            IFS= read -r resources || exit 14
            case "$resources" in *resources/read*) ;; *) exit 15 ;; esac
            printf '%s\n' "$2"
            IFS= read -r tools || exit 16
            case "$tools" in *tools/list*) ;; *) exit 17 ;; esac
            printf '%s\n' "$3"
            IFS= read -r call || exit 18
            case "$call" in *tools/call*) ;; *) exit 19 ;; esac
            printf '%s\n' "$4"
            IFS= read -r _keep_alive
        "#;
        let config = McpServerConfig {
            id: "stdio".to_string(),
            enabled: true,
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                script.to_string(),
                "mcp-fixture".to_string(),
                initialize,
                resource,
                tool_list,
                tool_result,
            ],
            request_timeout_ms: 1_000,
            ..McpServerConfig::default()
        };

        let loaded = load_server(&config).unwrap();
        let probe = match &loaded.capabilities()[0] {
            LoadedMcpCapability::Probe(provider) => provider.clone(),
            _ => panic!("fixture must load a probe"),
        };
        let evidence = probe
            .probe(&crate::domain::ProbeRequest {
                context: crate::domain::InvocationContext {
                    episode_id: crate::domain::EpisodeId::new(1),
                    operation_id: crate::domain::OperationId::new("stdio-probe").unwrap(),
                },
                arguments: json!({}),
            })
            .unwrap();
        assert_eq!(evidence.data["source"], "stdio");
        assert_eq!(probe.meta().provider.provider_class, ProviderClass::Mcp);
    }

    #[test]
    fn referenced_tool_schema_rejects_unsupported_keywords_during_discovery() {
        let raw = parse_manifest(json!({
            "schema_version": 1,
            "provider": {"id": "remote", "version": "1"},
            "capabilities": [probe_manifest()]
        }));
        let mut discovered = tools(&["observe"]);
        discovered.get_mut("observe").unwrap().input_schema =
            json!({"type": "string", "pattern": "unsafe-unsupported"});
        let error = build_server(
            &config(),
            raw,
            discovered,
            Arc::new(MockCaller {
                outputs: Mutex::new(VecDeque::new()),
            }),
        )
        .err()
        .expect("unsupported operation schema must fail manifest loading");
        assert!(error.message.contains("unsupported"));
    }

    fn parse_manifest(value: Value) -> RawManifest {
        serde_json::from_value(value).unwrap()
    }

    fn probe_manifest() -> Value {
        json!({
            "id": "probe",
            "kind": "probe",
            "effect": "read_only",
            "description": "Observe scheduler state",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"},
            "allowed_phases": ["clean", "experimenting"],
            "limits": {"timeout_ms": 1000, "max_output_bytes": 65536},
            "deterministic": false,
            "idempotent": false,
            "operations": {"probe": "observe"}
        })
    }

    fn tools(names: &[&str]) -> BTreeMap<String, DiscoveredTool> {
        names
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    DiscoveredTool {
                        input_schema: json!({"type": "object"}),
                        output_schema: Some(json!({"type": "object"})),
                    },
                )
            })
            .collect()
    }

    fn config() -> McpServerConfig {
        McpServerConfig {
            id: "server".to_string(),
            enabled: true,
            command: "unused".to_string(),
            request_timeout_ms: 500,
            ..McpServerConfig::default()
        }
    }
}
