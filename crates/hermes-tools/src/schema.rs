//! Helper pembangun JSON Schema dan ekstraksi argumen tool.
use hermes_core::{HermesError, Result};
use serde_json::{json, Value};

pub fn string_prop(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

pub fn integer_prop(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

pub fn boolean_prop(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}

pub fn object_schema(properties: Vec<(&str, Value)>, required: Vec<&str>) -> Value {
    let props: serde_json::Map<String, Value> = properties
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn get_str<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| HermesError::tool("args", format!("argumen wajib `{name}` bertipe string")))
}

pub fn opt_str<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(|v| v.as_str())
}

pub fn opt_u64(args: &Value, name: &str) -> Option<u64> {
    args.get(name).and_then(|v| v.as_u64())
}

pub fn opt_i64(args: &Value, name: &str) -> Option<i64> {
    args.get(name).and_then(|v| v.as_i64())
}

pub fn opt_bool(args: &Value, name: &str) -> Option<bool> {
    args.get(name).and_then(|v| v.as_bool())
}

pub fn ok(tool_call_id: &str, name: &str, content: String) -> hermes_core::ToolResult {
    hermes_core::ToolResult {
        tool_call_id: tool_call_id.to_string(),
        name: name.to_string(),
        content,
        is_error: false,
    }
}

/// Batasi panjang output agar tidak membanjiri konteks.
pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let t: String = s.chars().take(max_chars).collect();
    format!("{t}\n…[dipotong, total {} karakter]", s.chars().count())
}
