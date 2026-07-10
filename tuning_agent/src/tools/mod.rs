use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::types::escape_json;

#[derive(Clone, Debug, Serialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolInvocation {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolResult {
    pub call_id: String,
    pub name: String,
    pub ok: bool,
    pub content: String,
}

impl ToolResult {
    pub fn ok(call_id: String, name: String, content: String) -> Self {
        Self {
            call_id,
            name,
            ok: true,
            content,
        }
    }

    pub fn rejected(call_id: String, name: String, reason: String) -> Self {
        Self {
            call_id,
            name,
            ok: false,
            content: format!("rejected: {}", escape_json(&reason)),
        }
    }

    pub fn failed(call_id: String, name: String, content: String) -> Self {
        Self {
            call_id,
            name,
            ok: false,
            content,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolRegistry {
    tools: Vec<ToolSpec>,
}

impl ToolRegistry {
    pub fn builtin() -> Self {
        Self {
            tools: vec![
                ToolSpec {
                    name: "probe".to_string(),
                    description: "Request observation data by executing a read-only diagnostic shell command. Use experiment_write for kernel parameter changes.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["name", "command"],
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Short probe name used for audit."
                            },
                            "command": {
                                "type": "string",
                                "description": "Diagnostic shell command executed as: sh -c <command>."
                            },
                            "timeout_ms": {
                                "type": "integer",
                                "description": "Optional command timeout in milliseconds."
                            },
                            "working_dir": {
                                "type": "string",
                                "description": "Optional working directory."
                            }
                        }
                    }),
                },
                ToolSpec {
                    name: "experiment_write".to_string(),
                    description: "Run a structured kernel-parameter write as an experiment. The Act Kernel captures the old value before writing and can restore it without model-provided rollback commands.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["target", "value", "reason"],
                        "properties": {
                            "target": {
                                "type": "object",
                                "description": "Structured write target. Supported kinds: sysctl, proc_sys, sysfs, cgroup.",
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "description": "sysctl, proc_sys, sysfs, or cgroup."
                                    },
                                    "key": {
                                        "type": "string",
                                        "description": "Required for sysctl, for example vm.dirty_ratio."
                                    },
                                    "path": {
                                        "type": "string",
                                        "description": "Required for proc_sys, sysfs, or cgroup."
                                    }
                                },
                                "required": ["kind"],
                                "additionalProperties": false
                            },
                            "value": {
                                "type": "string",
                                "description": "Value to write."
                            },
                            "reason": {
                                "type": "string",
                                "description": "Why this experiment is being run."
                            },
                            "timeout_ms": {
                                "type": "integer",
                                "description": "Optional command timeout in milliseconds."
                            },
                            "working_dir": {
                                "type": "string",
                                "description": "Optional working directory."
                            }
                        }
                    }),
                },
                ToolSpec {
                    name: "commit".to_string(),
                    description: "Request final commit of explicitly listed experiment writes. Provide keep_writes plus a low-cost read-only measurement command that prints one JSON object. The Evaluation Kernel restores baseline A', samples it, applies only keep_writes as candidate B', samples again, then validates the model claim, workload invariants, regression guards, and fixed system guardrails.".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["reason", "keep_writes", "measurement", "primary_metrics"],
                        "properties": {
                            "reason": {
                                "type": "string",
                                "description": "Why the experiment result should be kept."
                            },
                            "keep_writes": {
                                "type": "array",
                                "description": "Exact target/value writes to keep if validation passes. Each entry must match a value already produced by experiment_write in this episode.",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["target", "value"],
                                    "properties": {
                                        "target": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "required": ["kind"],
                                            "properties": {
                                                "kind": { "type": "string" },
                                                "key": { "type": "string" },
                                                "path": { "type": "string" }
                                            }
                                        },
                                        "value": { "type": "string" }
                                    }
                                }
                            },
                            "measurement": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["command"],
                                "description": "Low-cost read-only shell command executed for both baseline A' and commit candidate B'. stdout must be a single JSON object.",
                                "properties": {
                                    "command": {
                                        "type": "string",
                                        "description": "Read-only measurement command. It must output exactly one JSON object on stdout."
                                    },
                                    "schema": {
                                        "type": "object",
                                        "description": "Optional field schema. Values may be number, counter, or bool.",
                                        "additionalProperties": {
                                            "type": "string"
                                        }
                                    },
                                    "timeout_ms": {
                                        "type": "integer",
                                        "description": "Optional timeout."
                                    },
                                    "working_dir": {
                                        "type": "string",
                                        "description": "Optional working directory."
                                    }
                                }
                            },
                            "primary_metrics": {
                                "type": "array",
                                "description": "At least one model-claimed metric that must improve when comparing A' against B'. Use names produced by measurement JSON or built-in guardrail metrics.",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["metric", "op", "value"],
                                    "properties": {
                                        "metric": { "type": "string" },
                                        "op": { "type": "string", "description": "Supported: decrease_percent_ge, decrease_abs_ge, increase_percent_ge, increase_abs_ge, increase_percent_le, increase_abs_le, decrease_percent_le, decrease_abs_le, change_percent_le, change_abs_le, current_le, current_ge." },
                                        "value": { "type": "number" }
                                    }
                                }
                            },
                            "regression_guards": {
                                "type": "array",
                                "description": "Model-declared metrics that may regress because of the experiment and must stay within bounds when comparing A' against B'. Values come from the same measurement JSON or built-in system metrics.",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["metric", "op", "value"],
                                    "properties": {
                                        "metric": { "type": "string" },
                                        "op": { "type": "string" },
                                        "value": { "type": "number" }
                                    }
                                }
                            },
                            "workload_invariants": {
                                "type": "array",
                                "description": "Conditions used to mark validation inconclusive when A' and B' workload shape is not comparable.",
                                "items": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "required": ["metric", "op", "value"],
                                    "properties": {
                                        "metric": { "type": "string" },
                                        "op": { "type": "string" },
                                        "value": { "type": "number" }
                                    }
                                }
                            },
                            "window_seconds": {
                                "type": "integer",
                                "description": "Optional evaluation sampling window per side. This is a model suggestion and is clamped by evaluation config."
                            },
                            "settle_seconds": {
                                "type": "integer",
                                "description": "Optional settle delay after baseline restore and candidate apply. This is a model suggestion and is clamped by evaluation config."
                            }
                        }
                    }),
                },
            ],
        }
    }

    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }
}
