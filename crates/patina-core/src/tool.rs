//! The tool trait the agent loop calls, and argument validation against a tool's JSON Schema.
use crate::provider::ToolSpec;
use async_trait::async_trait;
use serde_json::Value;

/// What a tool does to the world. The policy engine decides on this, never on the tool's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Reads only; may run concurrently with other reads.
    Read,
    /// Changes files in the workspace.
    Edit,
    /// Runs a command.
    Command,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// True when files in the workspace changed, so the loop makes a checkpoint.
    pub changed_files: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> ToolOutput {
        ToolOutput { content: content.into(), ..Default::default() }
    }
    pub fn error(content: impl Into<String>) -> ToolOutput {
        ToolOutput { content: content.into(), is_error: true, changed_files: false }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn kind(&self) -> ToolKind;
    /// A unified diff of what the call would change, shown on the approval card. `None` for non-edits.
    async fn preview(&self, _args: &Value) -> Option<String> {
        None
    }
    async fn call(&self, args: Value) -> ToolOutput;
}

/// Validate `value` against the subset of JSON Schema that tool schemas use:
/// `type`, `properties`, `required`, `additionalProperties: false`, `enum`, `items`, `minimum`, `maximum`.
/// Keywords outside this subset are ignored, so a richer schema is never rejected wrongly.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "arguments")
}

fn type_ok(ty: &str, v: &Value) -> bool {
    match ty {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => true,
    }
}

fn check(schema: &Value, v: &Value, at: &str) -> Result<(), String> {
    let Some(s) = schema.as_object() else { return Ok(()) };
    match s.get("type") {
        Some(Value::String(t)) if !type_ok(t, v) => return Err(format!("{at} must be of type {t}")),
        Some(Value::Array(ts)) if !ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)) => {
            return Err(format!("{at} has the wrong type"));
        }
        _ => {}
    }
    if let Some(Value::Array(options)) = s.get("enum")
        && !options.contains(v)
    {
        return Err(format!("{at} must be one of {}", Value::Array(options.clone())));
    }
    if let Some(n) = v.as_f64() {
        if let Some(min) = s.get("minimum").and_then(Value::as_f64)
            && n < min
        {
            return Err(format!("{at} must be at least {min}"));
        }
        if let Some(max) = s.get("maximum").and_then(Value::as_f64)
            && n > max
        {
            return Err(format!("{at} must be at most {max}"));
        }
    }
    if let Some(obj) = v.as_object() {
        let props = s.get("properties").and_then(Value::as_object);
        for req in s.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !obj.contains_key(req) {
                return Err(format!("{at} is missing required field {req:?}"));
            }
        }
        for (k, child) in obj {
            match props.and_then(|p| p.get(k)) {
                Some(sub) => check(sub, child, &format!("{at}.{k}"))?,
                None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!("{at} has an unknown field {k:?}"));
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(arr)) = (s.get("items"), v.as_array()) {
        for (i, child) in arr.iter().enumerate() {
            check(items, child, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 2000},
                "mode": {"type": "string", "enum": ["a", "b"]},
                "tags": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn accepts_valid_arguments() {
        for ok in [json!({"path": "a"}), json!({"path": "a", "limit": 5, "mode": "b", "tags": ["x"]})] {
            assert_eq!(validate(&schema(), &ok), Ok(()), "{ok}");
        }
    }

    #[test]
    fn rejects_invalid_arguments_with_a_reason() {
        for (bad, why) in [
            (json!("text"), "type object"),
            (json!({}), "missing required"),
            (json!({"path": 5}), "arguments.path must be of type string"),
            (json!({"path": "a", "limit": 0}), "at least"),
            (json!({"path": "a", "limit": 1.5}), "type integer"),
            (json!({"path": "a", "mode": "z"}), "one of"),
            (json!({"path": "a", "extra": 1}), "unknown field"),
            (json!({"path": "a", "tags": [1]}), "tags[0]"),
        ] {
            let err = validate(&schema(), &bad).unwrap_err();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn unknown_keywords_are_ignored() {
        assert_eq!(validate(&json!({"type": "object", "patternProperties": {"x": {}}, "oneOf": []}), &json!({"a": 1})), Ok(()));
    }
}
