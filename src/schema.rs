//! JSON-schema validation for tool inputs.
//!
//! - `zod_parse` reproduces zod object parsing for the built-in tool schemas (exported from zod as
//!   draft-07): defaults are applied, unknown keys are stripped, output keys follow schema order,
//!   `.trim()` fields (listed by the caller) are trimmed before the length checks, and errors use
//!   zod's issue format.
//! - `ajv_validate` validates MCP/JSON tool schemas like Ajv (`strict: false`, formats ignored):
//!   no defaults or stripping, first error only, Ajv's message wording.

use crate::jsutil::{char_len, stringify, stringify_pretty};
use serde_json::{json, Map, Value};

fn received(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn path_value(path: &[Value]) -> Value {
    Value::Array(path.to_vec())
}

fn num_value(n: f64) -> Value {
    crate::jsutil::num(n)
}

fn fmt_num(n: f64) -> String {
    stringify(&num_value(n))
}

/// zod-compatible parse. `trims` lists dotted paths (`*` for array items) of `.trim()` strings.
pub fn zod_parse(schema: &Value, input: &Value, trims: &[&str]) -> Result<Value, String> {
    let mut issues = vec![];
    let out = zod_node(schema, Some(input), &mut vec![], "", trims, &mut issues);
    if issues.is_empty() {
        Ok(out.unwrap_or(Value::Null))
    } else {
        Err(stringify_pretty(&Value::Array(issues)))
    }
}

fn zod_node(schema: &Value, value: Option<&Value>, path: &mut Vec<Value>, dotted: &str, trims: &[&str], issues: &mut Vec<Value>) -> Option<Value> {
    let value = match value {
        None => match schema.get("default") {
            Some(d) => return Some(d.clone()),
            None => None,
        },
        Some(v) => Some(v),
    };
    let kind = schema.get("type").and_then(Value::as_str);
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if let Some(v) = value {
            if values.contains(v) {
                return Some(v.clone());
            }
        }
        let list = values.iter().map(stringify).collect::<Vec<_>>().join("|");
        issues.push(json!({ "code": "invalid_value", "values": values, "path": path_value(path), "message": format!("Invalid option: expected one of {list}") }));
        return None;
    }
    let type_issue = |expected: &str, issues: &mut Vec<Value>, path: &[Value]| {
        issues.push(json!({ "expected": expected, "code": "invalid_type", "path": path_value(path), "message": format!("Invalid input: expected {expected}, received {}", received(value)) }));
    };
    match kind {
        Some("string") => {
            let Some(Value::String(s)) = value else {
                type_issue("string", issues, path);
                return None;
            };
            let s = if trims.contains(&dotted) { s.trim().to_string() } else { s.clone() };
            let len = char_len(&s) as f64;
            if let Some(min) = schema.get("minLength").and_then(Value::as_f64) {
                if len < min {
                    issues.push(json!({ "origin": "string", "code": "too_small", "minimum": num_value(min), "inclusive": true, "path": path_value(path), "message": format!("Too small: expected string to have >={} characters", fmt_num(min)) }));
                    return None;
                }
            }
            if let Some(max) = schema.get("maxLength").and_then(Value::as_f64) {
                if len > max {
                    issues.push(json!({ "origin": "string", "code": "too_big", "maximum": num_value(max), "inclusive": true, "path": path_value(path), "message": format!("Too big: expected string to have <={} characters", fmt_num(max)) }));
                    return None;
                }
            }
            Some(Value::String(s))
        }
        Some("integer") | Some("number") => {
            let Some(Value::Number(n)) = value else {
                type_issue("number", issues, path);
                return None;
            };
            let f = n.as_f64().unwrap_or(f64::NAN);
            if kind == Some("integer") && (f.fract() != 0.0 || f.abs() > 9_007_199_254_740_991.0) {
                issues.push(json!({ "expected": "int", "format": "safeint", "code": "invalid_type", "path": path_value(path), "message": "Invalid input: expected int, received number" }));
                return None;
            }
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64) {
                if f < min {
                    issues.push(json!({ "origin": "number", "code": "too_small", "minimum": num_value(min), "inclusive": true, "path": path_value(path), "message": format!("Too small: expected number to be >={}", fmt_num(min)) }));
                    return None;
                }
            }
            if let Some(min) = schema.get("exclusiveMinimum").and_then(Value::as_f64) {
                if f <= min {
                    issues.push(json!({ "origin": "number", "code": "too_small", "minimum": num_value(min), "inclusive": false, "path": path_value(path), "message": format!("Too small: expected number to be >{}", fmt_num(min)) }));
                    return None;
                }
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64) {
                if f > max {
                    issues.push(json!({ "origin": "number", "code": "too_big", "maximum": num_value(max), "inclusive": true, "path": path_value(path), "message": format!("Too big: expected number to be <={}", fmt_num(max)) }));
                    return None;
                }
            }
            Some(Value::Number(n.clone()))
        }
        Some("boolean") => match value {
            Some(Value::Bool(b)) => Some(Value::Bool(*b)),
            _ => {
                type_issue("boolean", issues, path);
                None
            }
        },
        Some("array") => {
            let Some(Value::Array(items)) = value else {
                type_issue("array", issues, path);
                return None;
            };
            let item_schema = schema.get("items").cloned().unwrap_or(json!({}));
            let mut out = vec![];
            let before = issues.len();
            let child_dotted = if dotted.is_empty() { "*".to_string() } else { format!("{dotted}.*") };
            for (i, item) in items.iter().enumerate() {
                path.push(json!(i));
                if let Some(v) = zod_node(&item_schema, Some(item), path, &child_dotted, trims, issues) {
                    out.push(v);
                }
                path.pop();
            }
            if issues.len() > before {
                return None;
            }
            let len = items.len() as f64;
            if let Some(min) = schema.get("minItems").and_then(Value::as_f64) {
                if len < min {
                    issues.push(json!({ "origin": "array", "code": "too_small", "minimum": num_value(min), "inclusive": true, "path": path_value(path), "message": format!("Too small: expected array to have >={} items", fmt_num(min)) }));
                    return None;
                }
            }
            if let Some(max) = schema.get("maxItems").and_then(Value::as_f64) {
                if len > max {
                    issues.push(json!({ "origin": "array", "code": "too_big", "maximum": num_value(max), "inclusive": true, "path": path_value(path), "message": format!("Too big: expected array to have <={} items", fmt_num(max)) }));
                    return None;
                }
            }
            Some(Value::Array(out))
        }
        Some("object") | None if schema.get("properties").is_some() || kind == Some("object") => {
            let Some(Value::Object(obj)) = value else {
                type_issue("object", issues, path);
                return None;
            };
            let props = schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
            let required: Vec<&str> = schema.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let mut out = Map::new();
            let before = issues.len();
            for (key, prop) in &props {
                path.push(json!(key));
                let child_dotted = if dotted.is_empty() { key.clone() } else { format!("{dotted}.{key}") };
                let present = obj.get(key);
                if present.is_none() && !required.contains(&key.as_str()) && prop.get("default").is_none() {
                    path.pop();
                    continue;
                }
                if let Some(v) = zod_node(prop, present, path, &child_dotted, trims, issues) {
                    out.insert(key.clone(), v);
                }
                path.pop();
            }
            if issues.len() > before {
                return None;
            }
            Some(Value::Object(out))
        }
        _ => value.cloned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Ajv-style validation
// ---------------------------------------------------------------------------------------------

fn json_type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_f64().map(|f| f.fract() == 0.0).unwrap_or(false),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn resolve_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    root.pointer(&pointer.replace("~1", "/").replace("~0", "~"))
}

struct AjvError {
    path: String,
    message: String,
}

fn ajv_node(root: &Value, schema: &Value, value: &Value, path: &str, depth: usize) -> Result<(), AjvError> {
    let fail = |message: String| Err(AjvError { path: path.to_string(), message });
    if depth > 64 {
        return Ok(());
    }
    match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return fail("boolean schema is false".into()),
        Value::Object(_) => {}
        _ => return Ok(()),
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        if let Some(target) = resolve_ref(root, reference) {
            ajv_node(root, target, value, path, depth + 1)?;
        }
    }
    if let Some(kind) = schema.get("type") {
        let kinds: Vec<&str> = match kind {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !kinds.is_empty() && !kinds.iter().any(|k| json_type_matches(k, value)) {
            return fail(format!("must be {}", kinds.join(",")));
        }
    }
    if let Some(c) = schema.get("const") {
        if c != value {
            return fail("must be equal to constant".into());
        }
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.contains(value) {
            return fail("must be equal to one of the allowed values".into());
        }
    }
    if let Some(n) = value.as_f64() {
        if let Some(m) = schema.get("maximum").and_then(Value::as_f64) {
            if n > m {
                return fail(format!("must be <= {}", fmt_num(m)));
            }
        }
        if let Some(m) = schema.get("minimum").and_then(Value::as_f64) {
            if n < m {
                return fail(format!("must be >= {}", fmt_num(m)));
            }
        }
        if let Some(m) = schema.get("exclusiveMaximum").and_then(Value::as_f64) {
            if n >= m {
                return fail(format!("must be < {}", fmt_num(m)));
            }
        }
        if let Some(m) = schema.get("exclusiveMinimum").and_then(Value::as_f64) {
            if n <= m {
                return fail(format!("must be > {}", fmt_num(m)));
            }
        }
        if let Some(m) = schema.get("multipleOf").and_then(Value::as_f64) {
            if m > 0.0 && ((n / m) - (n / m).round()).abs() > 1e-9 {
                return fail(format!("must be multiple of {}", fmt_num(m)));
            }
        }
    }
    if let Some(s) = value.as_str() {
        let len = char_len(s) as f64;
        if let Some(m) = schema.get("maxLength").and_then(Value::as_f64) {
            if len > m {
                return fail(format!("must NOT have more than {} characters", fmt_num(m)));
            }
        }
        if let Some(m) = schema.get("minLength").and_then(Value::as_f64) {
            if len < m {
                return fail(format!("must NOT have fewer than {} characters", fmt_num(m)));
            }
        }
        if let Some(p) = schema.get("pattern").and_then(Value::as_str) {
            if let Ok(re) = fancy_regex::Regex::new(p) {
                if !re.is_match(s).unwrap_or(true) {
                    return fail(format!("must match pattern \"{p}\""));
                }
            }
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(m) = schema.get("maxItems").and_then(Value::as_f64) {
            if items.len() as f64 > m {
                return fail(format!("must NOT have more than {} items", fmt_num(m)));
            }
        }
        if let Some(m) = schema.get("minItems").and_then(Value::as_f64) {
            if (items.len() as f64) < m {
                return fail(format!("must NOT have fewer than {} items", fmt_num(m)));
            }
        }
        match schema.get("items") {
            Some(Value::Array(tuple)) => {
                for (i, (item, s)) in items.iter().zip(tuple.iter()).enumerate() {
                    ajv_node(root, s, item, &format!("{path}/{i}"), depth + 1)?;
                }
            }
            Some(s) => {
                for (i, item) in items.iter().enumerate() {
                    ajv_node(root, s, item, &format!("{path}/{i}"), depth + 1)?;
                }
            }
            None => {}
        }
        if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
            for i in 0..items.len() {
                for j in 0..i {
                    if items[i] == items[j] {
                        return fail(format!("must NOT have duplicate items (items ## {j} and {i} are identical)"));
                    }
                }
            }
        }
    }
    if let Some(obj) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !obj.contains_key(key) {
                    return fail(format!("must have required property '{key}'"));
                }
            }
        }
        if let Some(m) = schema.get("maxProperties").and_then(Value::as_f64) {
            if obj.len() as f64 > m {
                return fail(format!("must NOT have more than {} properties", fmt_num(m)));
            }
        }
        if let Some(m) = schema.get("minProperties").and_then(Value::as_f64) {
            if (obj.len() as f64) < m {
                return fail(format!("must NOT have fewer than {} properties", fmt_num(m)));
            }
        }
        let props = schema.get("properties").and_then(Value::as_object);
        let pattern_props = schema.get("patternProperties").and_then(Value::as_object);
        for (key, v) in obj {
            let child = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
            let mut matched = false;
            if let Some(s) = props.and_then(|p| p.get(key)) {
                matched = true;
                ajv_node(root, s, v, &child, depth + 1)?;
            }
            if let Some(pp) = pattern_props {
                for (pattern, s) in pp {
                    if fancy_regex::Regex::new(pattern).ok().and_then(|re| re.is_match(key).ok()).unwrap_or(false) {
                        matched = true;
                        ajv_node(root, s, v, &child, depth + 1)?;
                    }
                }
            }
            if !matched {
                match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => return fail("must NOT have additional properties".into()),
                    Some(s @ Value::Object(_)) => ajv_node(root, s, v, &child, depth + 1)?,
                    _ => {}
                }
            }
        }
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        for s in all {
            ajv_node(root, s, value, path, depth + 1)?;
        }
    }
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
        if !any.iter().any(|s| ajv_node(root, s, value, path, depth + 1).is_ok()) {
            return fail("must match a schema in anyOf".into());
        }
    }
    if let Some(one) = schema.get("oneOf").and_then(Value::as_array) {
        if one.iter().filter(|s| ajv_node(root, s, value, path, depth + 1).is_ok()).count() != 1 {
            return fail("must match exactly one schema in oneOf".into());
        }
    }
    if let Some(not) = schema.get("not") {
        if ajv_node(root, not, value, path, depth + 1).is_ok() {
            return fail("must NOT be valid".into());
        }
    }
    Ok(())
}

/// Ajv-style validation (first error), formatted like `ajv.errorsText`.
pub fn ajv_validate(schema: &Value, value: &Value) -> Result<(), String> {
    ajv_node(schema, schema, value, "", 0).map_err(|e| format!("data{} {}", e.path, e.message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zod_defaults_strip_and_trim() {
        let schema = json!({"type": "object", "properties": {"path": {"type": "string"}, "offset": {"default": 1, "type": "integer", "minimum": 1}, "tags": {"type": "array", "items": {"type": "string", "minLength": 1}}}, "required": ["path"], "additionalProperties": false});
        let out = zod_parse(&schema, &json!({"extra": 1, "path": "a", "tags": ["  x "]}), &["tags.*"]).unwrap();
        assert_eq!(stringify(&out), r#"{"path":"a","offset":1,"tags":["x"]}"#);
        let err = zod_parse(&schema, &json!({"tags": ["   "]}), &["tags.*"]).unwrap_err();
        assert!(err.contains("Invalid input: expected string, received undefined"), "{err}");
        assert!(err.contains("Too small: expected string to have >=1 characters"));
        let err = zod_parse(&schema, &json!({"path": "a", "offset": 0.5}), &[]).unwrap_err();
        assert!(err.contains("expected int"));
    }

    #[test]
    fn ajv_messages() {
        let schema = json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 2}}, "required": ["q"]});
        assert_eq!(ajv_validate(&schema, &json!({})).unwrap_err(), "data must have required property 'q'");
        assert_eq!(ajv_validate(&schema, &json!({"q": 1, "n": 1})).unwrap_err(), "data/n must be >= 2");
        assert!(ajv_validate(&schema, &json!({"q": 1, "n": 3})).is_ok());
    }
}
