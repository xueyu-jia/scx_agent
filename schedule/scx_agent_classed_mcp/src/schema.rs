use serde_json::{json, Value};

pub const PROVIDER_VERSION: &str = "1.0.0";
pub const MAX_TARGETS: usize = 128;

pub const TOOL_RULES_SNAPSHOT: &str = "scx_agent_classed.rules_snapshot";
pub const TOOL_MEASUREMENT_VALIDATE: &str = "scx_agent_classed.integrity_validate";
pub const TOOL_MEASUREMENT_OPEN: &str = "scx_agent_classed.integrity_open";
pub const TOOL_MEASUREMENT_SAMPLE: &str = "scx_agent_classed.integrity_sample";
pub const TOOL_MEASUREMENT_CLOSE: &str = "scx_agent_classed.integrity_close";
pub const TOOL_MUTATION_PREPARE: &str = "scx_agent_classed.rule_prepare";
pub const TOOL_MUTATION_APPLY: &str = "scx_agent_classed.rule_apply";
pub const TOOL_MUTATION_STATUS: &str = "scx_agent_classed.rule_status";
pub const TOOL_MUTATION_VERIFY: &str = "scx_agent_classed.rule_verify";
pub const TOOL_MUTATION_RESTORE: &str = "scx_agent_classed.rule_restore";
pub const TOOL_MUTATION_FINALIZE: &str = "scx_agent_classed.rule_finalize";

pub fn manifest() -> Value {
    json!({
        "schema_version": 1,
        "provider": {
            "id": "scx-agent-classed",
            "version": PROVIDER_VERSION
        },
        "capabilities": [
            capability(
                "rules.snapshot.v1",
                "probe",
                "read_only",
                "Read bounded active and persisted scx_agent_classed rule state",
                snapshot_arguments_schema(),
                snapshot_output_schema(),
                json!({"probe": TOOL_RULES_SNAPSHOT}),
                false,
                true,
            ),
            capability(
                "rule.upsert.v1",
                "mutation",
                "reversible_mutation",
                "Upsert one exact learned comm rule through the scheduler control socket",
                upsert_arguments_schema(),
                rule_state_schema(),
                json!({
                    "prepare": TOOL_MUTATION_PREPARE,
                    "apply": TOOL_MUTATION_APPLY,
                    "status": TOOL_MUTATION_STATUS,
                    "verify": TOOL_MUTATION_VERIFY,
                    "restore": TOOL_MUTATION_RESTORE,
                    "finalize": TOOL_MUTATION_FINALIZE
                }),
                false,
                true,
            ),
            capability(
                "classification.integrity.v1",
                "measurement",
                "read_only",
                "Measure learned-rule publication coverage and scheduler state integrity",
                integrity_specification_schema(),
                integrity_output_schema(),
                json!({
                    "validate": TOOL_MEASUREMENT_VALIDATE,
                    "open": TOOL_MEASUREMENT_OPEN,
                    "sample": TOOL_MEASUREMENT_SAMPLE,
                    "close": TOOL_MEASUREMENT_CLOSE
                }),
                false,
                false,
            )
        ]
    })
}

#[allow(clippy::too_many_arguments)]
fn capability(
    id: &str,
    kind: &str,
    effect: &str,
    description: &str,
    input_schema: Value,
    output_schema: Value,
    operations: Value,
    deterministic: bool,
    idempotent: bool,
) -> Value {
    let phases = if kind == "measurement" {
        json!(["commit_pending"])
    } else {
        json!(["clean", "experimenting"])
    };
    json!({
        "id": id,
        "kind": kind,
        "effect": effect,
        "description": description,
        "input_schema": input_schema,
        "output_schema": output_schema,
        "allowed_phases": phases,
        "limits": {"timeout_ms": 10000, "max_output_bytes": 131072},
        "deterministic": deterministic,
        "idempotent": idempotent,
        "operations": operations
    })
}

pub fn tools() -> Vec<Value> {
    vec![
        tool(TOOL_RULES_SNAPSHOT, probe_request_schema()),
        tool(TOOL_MEASUREMENT_VALIDATE, validate_request_schema()),
        tool(TOOL_MEASUREMENT_OPEN, measurement_open_schema()),
        tool(TOOL_MEASUREMENT_SAMPLE, measurement_sample_schema()),
        tool(TOOL_MEASUREMENT_CLOSE, measurement_session_schema()),
        tool(TOOL_MUTATION_PREPARE, mutation_prepare_schema()),
        tool(TOOL_MUTATION_APPLY, mutation_operation_schema()),
        tool(TOOL_MUTATION_STATUS, mutation_status_schema()),
        tool(TOOL_MUTATION_VERIFY, mutation_verify_schema()),
        tool(TOOL_MUTATION_RESTORE, mutation_operation_schema()),
        tool(TOOL_MUTATION_FINALIZE, mutation_operation_schema()),
    ]
}

fn tool(name: &str, input_schema: Value) -> Value {
    json!({"name": name, "description": name, "inputSchema": input_schema})
}

fn string_schema(max_length: usize) -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": max_length})
}

fn comm_schema() -> Value {
    string_schema(15)
}

fn class_schema() -> Value {
    json!({"type": "string", "enum": ["latency", "batch"]})
}

fn rule_state_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["present"],
                "properties": {"present": {"const": false}}
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["present", "class"],
                "properties": {"present": {"const": true}, "class": class_schema()}
            }
        ]
    })
}

fn target_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["comm", "class"],
        "properties": {"comm": comm_schema(), "class": class_schema()}
    })
}

fn targets_schema() -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_TARGETS,
        "uniqueItems": true,
        "items": target_schema()
    })
}

fn comms_schema() -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_TARGETS,
        "uniqueItems": true,
        "items": comm_schema()
    })
}

fn context_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["episode_id", "operation_id"],
        "properties": {
            "episode_id": {"type": "integer", "minimum": 0},
            "operation_id": string_schema(256)
        }
    })
}

fn snapshot_arguments_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["comms"],
        "properties": {"comms": comms_schema()}
    })
}

fn upsert_arguments_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["comm", "class"],
        "properties": {"comm": comm_schema(), "class": class_schema()}
    })
}

fn integrity_specification_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["targets", "settle_timeout_ms"],
        "properties": {
            "targets": targets_schema(),
            "settle_timeout_ms": {"type": "integer", "minimum": 0, "maximum": 5000}
        }
    })
}

fn request_with(fields: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": fields
    })
}

fn probe_request_schema() -> Value {
    request_with(
        json!({"context": context_schema(), "arguments": snapshot_arguments_schema()}),
        &["context", "arguments"],
    )
}

fn validate_request_schema() -> Value {
    request_with(
        json!({"specification": integrity_specification_schema()}),
        &["specification"],
    )
}

fn measurement_open_schema() -> Value {
    request_with(
        json!({"context": context_schema(), "specification": integrity_specification_schema()}),
        &["context", "specification"],
    )
}

fn integrity_session_data_schema() -> Value {
    request_with(
        json!({
            "schema_version": {"const": 1},
            "targets": targets_schema(),
            "settle_timeout_ms": {"type": "integer", "minimum": 0, "maximum": 5000},
            "task_state_errors_at_open": {"type": "integer", "minimum": 0},
            "rule_refresh_deferred_at_open": {"type": "integer", "minimum": 0}
        }),
        &[
            "schema_version",
            "targets",
            "settle_timeout_ms",
            "task_state_errors_at_open",
            "rule_refresh_deferred_at_open",
        ],
    )
}

fn measurement_session_schema() -> Value {
    request_with(
        json!({"id": string_schema(256), "driver_data": integrity_session_data_schema()}),
        &["id", "driver_data"],
    )
}

fn measurement_sample_schema() -> Value {
    request_with(
        json!({"context": context_schema(), "session": measurement_session_schema()}),
        &["context", "session"],
    )
}

fn mutation_prepare_schema() -> Value {
    request_with(
        json!({"context": context_schema(), "arguments": upsert_arguments_schema()}),
        &["context", "arguments"],
    )
}

fn mutation_state_schema() -> Value {
    request_with(
        json!({"value": rule_state_schema(), "digest": string_schema(256)}),
        &["value", "digest"],
    )
}

fn provider_pin_schema() -> Value {
    request_with(
        json!({
            "provider_id": string_schema(256),
            "provider_version": string_schema(256),
            "provider_class": {"const": "mcp"},
            "manifest_digest": string_schema(256)
        }),
        &[
            "provider_id",
            "provider_version",
            "provider_class",
            "manifest_digest",
        ],
    )
}

fn mutation_driver_data_schema() -> Value {
    request_with(
        json!({
            "schema_version": {"const": 1},
            "comm": comm_schema(),
            "baseline": rule_state_schema(),
            "desired": rule_state_schema()
        }),
        &["schema_version", "comm", "baseline", "desired"],
    )
}

fn prepared_mutation_schema() -> Value {
    request_with(
        json!({
            "capability_id": string_schema(256),
            "provider": provider_pin_schema(),
            "resource": string_schema(4096),
            "baseline": mutation_state_schema(),
            "desired": mutation_state_schema(),
            "driver_data": mutation_driver_data_schema()
        }),
        &[
            "capability_id",
            "provider",
            "resource",
            "baseline",
            "desired",
            "driver_data",
        ],
    )
}

fn mutation_operation_schema() -> Value {
    request_with(
        json!({"operation_id": string_schema(256), "prepared": prepared_mutation_schema()}),
        &["operation_id", "prepared"],
    )
}

fn mutation_status_schema() -> Value {
    request_with(
        json!({"operation_id": string_schema(256)}),
        &["operation_id"],
    )
}

fn mutation_verify_schema() -> Value {
    request_with(
        json!({
            "operation_id": string_schema(256),
            "prepared": prepared_mutation_schema(),
            "expected": mutation_state_schema()
        }),
        &["operation_id", "prepared", "expected"],
    )
}

fn rule_observation_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["comm", "class", "source", "consistent"],
        "properties": {
            "comm": comm_schema(),
            "class": class_schema(),
            "source": {"type": "string", "enum": ["base", "learned", "default"]},
            "active_class": class_schema(),
            "persisted_class": class_schema(),
            "consistent": {"type": "boolean"}
        }
    })
}

fn snapshot_output_schema() -> Value {
    request_with(
        json!({
            "rules": {"type": "array", "maxItems": MAX_TARGETS, "items": rule_observation_schema()},
            "revision": {"type": "integer", "minimum": 0},
            "rules_seq": {"type": "integer", "minimum": 0},
            "effective_digest": string_schema(256),
            "stats": request_with(
                json!({
                    "task_state_errors": {"type": "integer", "minimum": 0},
                    "rule_refresh_deferred": {"type": "integer", "minimum": 0}
                }),
                &["task_state_errors", "rule_refresh_deferred"],
            )
        }),
        &[
            "rules",
            "revision",
            "rules_seq",
            "effective_digest",
            "stats",
        ],
    )
}

fn integrity_output_schema() -> Value {
    request_with(
        json!({
            "active_rule_coverage": {
                "type": "number",
                "minimum": 0,
                "maximum": 1,
                "description": "Fraction of requested targets whose expected class is present in the active BPF rule map."
            },
            "persisted_rule_coverage": {
                "type": "number",
                "minimum": 0,
                "maximum": 1,
                "description": "Fraction of requested targets whose expected class is present in the durable learned-rule file."
            },
            "active_persisted_consistency": {
                "type": "number",
                "minimum": 0,
                "maximum": 1,
                "description": "Fraction of requested targets whose active and persisted classes agree; absence from both also agrees."
            },
            "task_state_errors_delta": {
                "type": "number",
                "minimum": 0,
                "description": "Scheduler task-state errors observed since this measurement session opened."
            },
            "rule_refresh_deferred_delta": {
                "type": "number",
                "minimum": 0,
                "description": "Rule refresh attempts safely deferred since this measurement session opened."
            }
        }),
        &[
            "active_rule_coverage",
            "persisted_rule_coverage",
            "active_persisted_consistency",
            "task_state_errors_delta",
            "rule_refresh_deferred_delta",
        ],
    )
}
