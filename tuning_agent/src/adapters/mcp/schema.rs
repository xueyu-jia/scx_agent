use std::collections::{BTreeSet, HashSet};

use serde_json::{Map, Value};

const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_SCHEMA_NODES: usize = 4096;

#[derive(Clone, Debug)]
pub(crate) struct JsonSchema {
    value: Value,
}

impl JsonSchema {
    pub(crate) fn compile(value: Value) -> Result<Self, String> {
        let mut nodes = 0;
        validate_schema(&value, "$", 0, &mut nodes)?;
        Ok(Self { value })
    }

    pub(crate) fn validate(&self, instance: &Value) -> Result<(), String> {
        validate_instance(&self.value, instance, "$", 0)
    }
}

fn validate_schema(
    schema: &Value,
    path: &str,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(format!("schema at {path} exceeds maximum nesting depth"));
    }
    *nodes = nodes.saturating_add(1);
    if *nodes > MAX_SCHEMA_NODES {
        return Err(format!("schema exceeds {MAX_SCHEMA_NODES} nodes"));
    }
    let object = schema.as_object().ok_or_else(|| {
        format!("schema at {path} must be an object; boolean schemas are unsupported")
    })?;

    for (keyword, value) in object {
        let keyword_path = property_path(path, keyword);
        match keyword.as_str() {
            "$schema" | "$id" | "$comment" | "title" | "description" => {
                require_string(value, &keyword_path)?;
            }
            "default" | "const" => {}
            "examples" => {
                require_array(value, &keyword_path)?;
            }
            "deprecated" | "readOnly" | "writeOnly" => {
                require_bool(value, &keyword_path)?;
            }
            "type" => validate_type_keyword(value, &keyword_path)?,
            "enum" => validate_enum(value, &keyword_path)?,
            "properties" => {
                let properties = require_object(value, &keyword_path)?;
                for (name, property_schema) in properties {
                    validate_schema(
                        property_schema,
                        &property_path(&keyword_path, name),
                        depth + 1,
                        nodes,
                    )?;
                }
            }
            "required" => validate_unique_string_array(value, &keyword_path)?,
            "additionalProperties" => {
                if !value.is_boolean() {
                    validate_schema(value, &keyword_path, depth + 1, nodes)?;
                }
            }
            "minProperties" | "maxProperties" | "minItems" | "maxItems" | "minLength"
            | "maxLength" => {
                require_nonnegative_usize(value, &keyword_path)?;
            }
            "items" => validate_schema(value, &keyword_path, depth + 1, nodes)?,
            "uniqueItems" => {
                require_bool(value, &keyword_path)?;
            }
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => {
                require_number(value, &keyword_path)?;
            }
            "allOf" | "anyOf" | "oneOf" => {
                let schemas = require_array(value, &keyword_path)?;
                for (index, nested) in schemas.iter().enumerate() {
                    validate_schema(nested, &format!("{keyword_path}/{index}"), depth + 1, nodes)?;
                }
            }
            "not" => validate_schema(value, &keyword_path, depth + 1, nodes)?,
            unsupported => {
                return Err(format!(
                    "unsupported JSON Schema keyword '{unsupported}' at {path}"
                ));
            }
        }
    }

    validate_range_pair(object, path, "minProperties", "maxProperties")?;
    validate_range_pair(object, path, "minItems", "maxItems")?;
    validate_range_pair(object, path, "minLength", "maxLength")?;
    validate_numeric_bounds(object, path)?;
    Ok(())
}

fn validate_instance(
    schema: &Value,
    instance: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(format!(
            "instance at {path} exceeds maximum validation depth"
        ));
    }
    let object = schema
        .as_object()
        .expect("JsonSchema construction validates every nested schema");

    if let Some(expected) = object.get("type") {
        let matched = match expected {
            Value::String(name) => matches_type(instance, name),
            Value::Array(names) => names
                .iter()
                .filter_map(Value::as_str)
                .any(|name| matches_type(instance, name)),
            _ => false,
        };
        if !matched {
            return Err(format!(
                "instance at {path} has type {}, expected {}",
                instance_type(instance),
                display_types(expected)
            ));
        }
    }

    if let Some(expected) = object.get("const") {
        if instance != expected {
            return Err(format!("instance at {path} does not match const"));
        }
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if !values.iter().any(|value| value == instance) {
            return Err(format!(
                "instance at {path} is not one of the allowed enum values"
            ));
        }
    }

    validate_compositions(object, instance, path, depth)?;

    if let Some(value) = instance.as_object() {
        validate_object(object, value, path, depth)?;
    }
    if let Some(value) = instance.as_array() {
        validate_array(object, value, path, depth)?;
    }
    if let Some(value) = instance.as_str() {
        validate_string(object, value, path)?;
    }
    if instance.is_number() {
        validate_number(object, instance, path)?;
    }
    Ok(())
}

fn validate_compositions(
    schema: &Map<String, Value>,
    instance: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for (index, branch) in branches.iter().enumerate() {
            validate_instance(branch, instance, path, depth + 1)
                .map_err(|error| format!("allOf branch {index} failed: {error}"))?;
        }
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        if !branches
            .iter()
            .any(|branch| validate_instance(branch, instance, path, depth + 1).is_ok())
        {
            return Err(format!(
                "instance at {path} does not match any anyOf branch"
            ));
        }
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = branches
            .iter()
            .filter(|branch| validate_instance(branch, instance, path, depth + 1).is_ok())
            .count();
        if matches != 1 {
            return Err(format!(
                "instance at {path} matches {matches} oneOf branches, expected exactly one"
            ));
        }
    }
    if let Some(negated) = schema.get("not") {
        if validate_instance(negated, instance, path, depth + 1).is_ok() {
            return Err(format!("instance at {path} matches a forbidden not schema"));
        }
    }
    Ok(())
}

fn validate_object(
    schema: &Map<String, Value>,
    instance: &Map<String, Value>,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    validate_size(
        schema,
        "minProperties",
        "maxProperties",
        instance.len(),
        path,
    )?;

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !instance.contains_key(name) {
                return Err(format!(
                    "instance at {path} is missing required property '{name}'"
                ));
            }
        }
    }

    let properties = schema.get("properties").and_then(Value::as_object);
    for (name, value) in instance {
        if let Some(property_schema) = properties.and_then(|known| known.get(name)) {
            validate_instance(
                property_schema,
                value,
                &property_path(path, name),
                depth + 1,
            )?;
            continue;
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => {
                return Err(format!(
                    "instance at {path} contains unknown property '{name}'"
                ));
            }
            Some(additional) if !additional.is_boolean() => {
                validate_instance(additional, value, &property_path(path, name), depth + 1)?
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_array(
    schema: &Map<String, Value>,
    instance: &[Value],
    path: &str,
    depth: usize,
) -> Result<(), String> {
    validate_size(schema, "minItems", "maxItems", instance.len(), path)?;
    if schema
        .get("uniqueItems")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let mut seen = HashSet::with_capacity(instance.len());
        for item in instance {
            let encoded = serde_json::to_vec(item)
                .map_err(|error| format!("failed to compare array items at {path}: {error}"))?;
            if !seen.insert(encoded) {
                return Err(format!("array at {path} contains duplicate items"));
            }
        }
    }
    if let Some(items) = schema.get("items") {
        for (index, item) in instance.iter().enumerate() {
            validate_instance(items, item, &format!("{path}/{index}"), depth + 1)?;
        }
    }
    Ok(())
}

fn validate_string(schema: &Map<String, Value>, instance: &str, path: &str) -> Result<(), String> {
    validate_size(
        schema,
        "minLength",
        "maxLength",
        instance.chars().count(),
        path,
    )
}

fn validate_number(
    schema: &Map<String, Value>,
    instance: &Value,
    path: &str,
) -> Result<(), String> {
    let number = instance
        .as_f64()
        .ok_or_else(|| format!("number at {path} cannot be represented for bound validation"))?;
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
        if number < minimum {
            return Err(format!("number at {path} is less than minimum {minimum}"));
        }
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
        if number > maximum {
            return Err(format!(
                "number at {path} is greater than maximum {maximum}"
            ));
        }
    }
    if let Some(minimum) = schema.get("exclusiveMinimum").and_then(Value::as_f64) {
        if number <= minimum {
            return Err(format!(
                "number at {path} is not greater than exclusiveMinimum {minimum}"
            ));
        }
    }
    if let Some(maximum) = schema.get("exclusiveMaximum").and_then(Value::as_f64) {
        if number >= maximum {
            return Err(format!(
                "number at {path} is not less than exclusiveMaximum {maximum}"
            ));
        }
    }
    Ok(())
}

fn validate_type_keyword(value: &Value, path: &str) -> Result<(), String> {
    match value {
        Value::String(name) => validate_type_name(name, path),
        Value::Array(names) if !names.is_empty() => {
            let mut seen = BTreeSet::new();
            for name in names {
                let name = require_string(name, path)?;
                validate_type_name(name, path)?;
                if !seen.insert(name) {
                    return Err(format!("type array at {path} contains duplicate '{name}'"));
                }
            }
            Ok(())
        }
        _ => Err(format!(
            "type at {path} must be a supported type name or a non-empty array"
        )),
    }
}

fn validate_type_name(name: &str, path: &str) -> Result<(), String> {
    if matches!(
        name,
        "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
    ) {
        Ok(())
    } else {
        Err(format!("unsupported JSON type '{name}' at {path}"))
    }
}

fn validate_enum(value: &Value, path: &str) -> Result<(), String> {
    let values = require_array(value, path)?;
    for (index, left) in values.iter().enumerate() {
        if values[..index].iter().any(|right| right == left) {
            return Err(format!("enum at {path} contains a duplicate value"));
        }
    }
    Ok(())
}

fn validate_unique_string_array(value: &Value, path: &str) -> Result<(), String> {
    let values = require_array(value, path)?;
    let mut seen = BTreeSet::new();
    for value in values {
        let value = require_string(value, path)?;
        if !seen.insert(value) {
            return Err(format!("array at {path} contains duplicate '{value}'"));
        }
    }
    Ok(())
}

fn validate_range_pair(
    schema: &Map<String, Value>,
    path: &str,
    minimum: &str,
    maximum: &str,
) -> Result<(), String> {
    let Some(minimum_value) = schema.get(minimum).and_then(Value::as_u64) else {
        return Ok(());
    };
    let Some(maximum_value) = schema.get(maximum).and_then(Value::as_u64) else {
        return Ok(());
    };
    if minimum_value > maximum_value {
        Err(format!(
            "schema at {path} has {minimum} greater than {maximum}"
        ))
    } else {
        Ok(())
    }
}

fn validate_numeric_bounds(schema: &Map<String, Value>, path: &str) -> Result<(), String> {
    if let (Some(minimum), Some(maximum)) = (
        schema.get("minimum").and_then(Value::as_f64),
        schema.get("maximum").and_then(Value::as_f64),
    ) {
        if minimum > maximum {
            return Err(format!("schema at {path} has minimum greater than maximum"));
        }
    }
    Ok(())
}

fn validate_size(
    schema: &Map<String, Value>,
    minimum: &str,
    maximum: &str,
    actual: usize,
    path: &str,
) -> Result<(), String> {
    if let Some(limit) = schema.get(minimum).and_then(Value::as_u64) {
        if actual < limit as usize {
            return Err(format!(
                "value at {path} has size {actual}, below {minimum} {limit}"
            ));
        }
    }
    if let Some(limit) = schema.get(maximum).and_then(Value::as_u64) {
        if actual > limit as usize {
            return Err(format!(
                "value at {path} has size {actual}, above {maximum} {limit}"
            ));
        }
    }
    Ok(())
}

fn matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value.as_f64().is_some_and(|number| number.fract() == 0.0)
        }
        "string" => value.is_string(),
        _ => false,
    }
}

fn instance_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn display_types(value: &Value) -> String {
    match value {
        Value::String(name) => name.clone(),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "a supported type".to_string(),
    }
}

fn require_object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("value at {path} must be an object"))
}

fn require_array<'a>(value: &'a Value, path: &str) -> Result<&'a [Value], String> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("value at {path} must be an array"))
}

fn require_string<'a>(value: &'a Value, path: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("value at {path} must be a string"))
}

fn require_bool(value: &Value, path: &str) -> Result<(), String> {
    value
        .as_bool()
        .map(|_| ())
        .ok_or_else(|| format!("value at {path} must be a boolean"))
}

fn require_number(value: &Value, path: &str) -> Result<(), String> {
    value
        .as_f64()
        .map(|_| ())
        .ok_or_else(|| format!("value at {path} must be a number"))
}

fn require_nonnegative_usize(value: &Value, path: &str) -> Result<(), String> {
    value
        .as_u64()
        .and_then(|number| usize::try_from(number).ok())
        .map(|_| ())
        .ok_or_else(|| format!("value at {path} must be a non-negative platform-sized integer"))
}

fn property_path(base: &str, property: &str) -> String {
    format!("{base}/{}", property.replace('~', "~0").replace('/', "~1"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn validates_supported_object_array_and_composition_keywords() {
        let schema = JsonSchema::compile(json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["mode", "values"],
            "properties": {
                "mode": {"enum": ["latency", "throughput"]},
                "values": {
                    "type": "array",
                    "minItems": 1,
                    "uniqueItems": true,
                    "items": {"type": "integer", "minimum": 0}
                },
                "optional": {"anyOf": [{"type": "string"}, {"type": "null"}]}
            }
        }))
        .unwrap();

        assert!(schema
            .validate(&json!({"mode": "latency", "values": [1, 2]}))
            .is_ok());
        assert!(schema
            .validate(&json!({"mode": "latency", "values": [1, 1]}))
            .unwrap_err()
            .contains("duplicate"));
        assert!(schema
            .validate(&json!({"mode": "invalid", "values": [1]}))
            .is_err());
        assert!(schema
            .validate(&json!({"mode": "latency", "values": [1], "extra": true}))
            .is_err());
    }

    #[test]
    fn rejects_unknown_keywords_and_boolean_schemas_at_discovery() {
        assert!(
            JsonSchema::compile(json!({"type": "string", "pattern": "x"}))
                .unwrap_err()
                .contains("unsupported")
        );
        assert!(JsonSchema::compile(Value::Bool(true)).is_err());
    }

    #[test]
    fn one_of_requires_exactly_one_matching_branch() {
        let schema = JsonSchema::compile(json!({
            "oneOf": [{"type": "number"}, {"type": "integer"}]
        }))
        .unwrap();
        assert!(schema.validate(&json!(1)).is_err());
        assert!(schema.validate(&json!(1.5)).is_ok());
    }
}
