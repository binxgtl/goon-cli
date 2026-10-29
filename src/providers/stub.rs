//! Deterministic offline provider driven by a JSON fixture (`--stub`, `--stub-fixture FILE`).

use super::part_channel;
use crate::model::*;
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StubUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StubToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StubError {
    pub message: String,
    pub status_code: Option<u16>,
    pub retryable: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StubTurn {
    pub matcher: Option<String>,
    pub reasoning: Vec<String>,
    pub text: Vec<String>,
    pub tool_calls: Vec<StubToolCall>,
    pub usage: Option<StubUsage>,
    pub error: Option<StubError>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StubFixture {
    pub model: Option<String>,
    pub input: Option<String>,
    pub turns: Vec<StubTurn>,
    pub auxiliary: Option<StubTurn>,
}

fn text_turn(text: &str) -> StubTurn {
    StubTurn { text: vec![text.into()], ..Default::default() }
}

pub fn default_fixture() -> StubFixture {
    StubFixture {
        model: Some("stub-model".into()),
        input: None,
        turns: vec![StubTurn {
            text: vec!["[stub] deterministic reply".into()],
            usage: Some(StubUsage { input_tokens: Some(10), output_tokens: Some(5), total_tokens: Some(15), ..Default::default() }),
            ..Default::default()
        }],
        auxiliary: None,
    }
}

fn strict_keys(value: &Value, allowed: &[&str], path: &str) -> Result<()> {
    if let Some(map) = value.as_object() {
        for key in map.keys() {
            if !allowed.contains(&key.as_str()) {
                bail!("Invalid stub fixture: unrecognized key \"{key}\" in {path}");
            }
        }
        Ok(())
    } else {
        bail!("Invalid stub fixture: {path} must be an object")
    }
}

fn opt_count(value: &Value, key: &str, path: &str) -> Result<Option<u64>> {
    match value.get(key) {
        None => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if n >= 0.0 && n.fract() == 0.0 => Ok(Some(n as u64)),
            _ => bail!("Invalid stub fixture: {path}.{key} must be a non-negative integer"),
        },
    }
}

fn string_list(value: &Value, key: &str, path: &str) -> Result<Vec<String>> {
    match value.get(key) {
        None => Ok(vec![]),
        Some(Value::Array(items)) => items.iter().map(|i| i.as_str().map(str::to_string).ok_or_else(|| anyhow!("Invalid stub fixture: {path}.{key} must contain strings"))).collect(),
        Some(_) => bail!("Invalid stub fixture: {path}.{key} must be an array"),
    }
}

fn parse_turn(value: &Value, path: &str) -> Result<StubTurn> {
    strict_keys(value, &["match", "reasoning", "text", "toolCalls", "usage", "error"], path)?;
    let matcher = match value.get("match") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => bail!("Invalid stub fixture: {path}.match must be a string"),
    };
    let mut tool_calls = vec![];
    if let Some(calls) = value.get("toolCalls") {
        let Some(items) = calls.as_array() else { bail!("Invalid stub fixture: {path}.toolCalls must be an array") };
        for (i, call) in items.iter().enumerate() {
            let p = format!("{path}.toolCalls[{i}]");
            strict_keys(call, &["id", "name", "input"], &p)?;
            let id = call.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("Invalid stub fixture: {p}.id is required"))?;
            let name = call.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("Invalid stub fixture: {p}.name is required"))?;
            tool_calls.push(StubToolCall { id: id.into(), name: name.into(), input: call.get("input").cloned().unwrap_or(json!({})) });
        }
    }
    let usage = match value.get("usage") {
        None => None,
        Some(u) => {
            let p = format!("{path}.usage");
            strict_keys(u, &["inputTokens", "outputTokens", "totalTokens", "cacheReadTokens", "cacheWriteTokens"], &p)?;
            Some(StubUsage {
                input_tokens: opt_count(u, "inputTokens", &p)?,
                output_tokens: opt_count(u, "outputTokens", &p)?,
                total_tokens: opt_count(u, "totalTokens", &p)?,
                cache_read_tokens: opt_count(u, "cacheReadTokens", &p)?,
                cache_write_tokens: opt_count(u, "cacheWriteTokens", &p)?,
            })
        }
    };
    let error = match value.get("error") {
        None => None,
        Some(e) => {
            let message = e.get("message").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("Invalid stub fixture: {path}.error.message is required"))?;
            let status_code = match e.get("statusCode") {
                None => None,
                Some(v) => match v.as_u64() {
                    Some(n) if (100..=599).contains(&n) => Some(n as u16),
                    _ => bail!("Invalid stub fixture: {path}.error.statusCode must be 100-599"),
                },
            };
            Some(StubError { message: message.into(), status_code, retryable: e.get("retryable").and_then(Value::as_bool) })
        }
    };
    Ok(StubTurn { matcher, reasoning: string_list(value, "reasoning", path)?, text: string_list(value, "text", path)?, tool_calls, usage, error })
}

pub fn parse_fixture(value: &Value) -> Result<StubFixture> {
    strict_keys(value, &["v", "model", "input", "turns", "auxiliary"], "fixture")?;
    if value.get("v") != Some(&json!(1)) {
        bail!("Invalid stub fixture: v must be 1");
    }
    let turns = value.get("turns").and_then(Value::as_array).ok_or_else(|| anyhow!("Invalid stub fixture: turns must be an array"))?;
    Ok(StubFixture {
        model: value.get("model").and_then(Value::as_str).map(str::to_string),
        input: value.get("input").and_then(Value::as_str).map(str::to_string),
        turns: turns.iter().enumerate().map(|(i, t)| parse_turn(t, &format!("turns[{i}]"))).collect::<Result<_>>()?,
        auxiliary: value.get("auxiliary").map(|a| parse_turn(a, "auxiliary")).transpose()?,
    })
}

pub fn load_stub_fixture(path: Option<&str>) -> Result<StubFixture> {
    let Some(path) = path else { return Ok(default_fixture()) };
    let absolute = crate::files::resolve_lexical(&std::env::current_dir()?, path);
    let info = std::fs::metadata(&absolute).map_err(|e| anyhow!("Stub fixture not found: {} ({}).", absolute.display(), e))?;
    if !info.is_file() {
        bail!("Stub fixture is not a file: {}.", absolute.display());
    }
    if info.len() > 1024 * 1024 {
        bail!("Stub fixture exceeds 1 MiB: {} ({} bytes).", absolute.display(), info.len());
    }
    let text = std::fs::read_to_string(&absolute)?;
    let parsed: Value = crate::jsutil::parse_json(&text).map_err(|e| anyhow!("Stub fixture is not valid JSON: {} ({}).", absolute.display(), e))?;
    parse_fixture(&parsed)
}

pub struct StubModel {
    pub fixture: StubFixture,
    pub cursor: Arc<Mutex<usize>>,
    pub model_id: String,
}

impl StubModel {
    pub fn new(fixture: StubFixture, cursor: Arc<Mutex<usize>>, model_id: Option<&str>) -> Self {
        let model_id = model_id.map(str::to_string).or_else(|| fixture.model.clone()).unwrap_or_else(|| "stub-model".into());
        StubModel { fixture, cursor, model_id }
    }

    fn last_user_text(messages: &[Value]) -> String {
        for message in messages.iter().rev() {
            if role_of(message) != "user" {
                continue;
            }
            return content_parts(message).iter().filter(|p| part_type(p) == "text").filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n");
        }
        String::new()
    }

    pub fn select_turn(&self, messages: &[Value]) -> StubTurn {
        let text = Self::last_user_text(messages);
        let mut cursor = self.cursor.lock().unwrap();
        let unused = &self.fixture.turns[(*cursor).min(self.fixture.turns.len())..];
        if unused.is_empty() {
            return text_turn("[stub] script exhausted");
        }
        let matches = |needle: &Option<String>| needle.as_ref().map(|n| !n.is_empty() && text.contains(n.as_str())).unwrap_or(false);
        let mut chosen = unused.iter().position(|t| matches(&t.matcher));
        if chosen.is_none() {
            if let Some(input) = &self.fixture.input {
                if text.contains(input.as_str()) {
                    chosen = Some(0);
                }
            }
        }
        if chosen.is_none() && unused.len() == 1 {
            chosen = Some(0);
        }
        if let Some(c) = chosen {
            let turn = unused[c].clone();
            *cursor += c + 1;
            return turn;
        }
        self.fixture.auxiliary.clone().unwrap_or_else(|| text_turn("[stub] auxiliary response"))
    }
}

fn to_usage(turn: &StubTurn) -> Usage {
    let usage = turn.usage.clone().unwrap_or_default();
    let input = usage.input_tokens.unwrap_or(0);
    // A total-only fixture must still move token/cost budgets; derive the missing side.
    let output = usage.output_tokens.unwrap_or_else(|| usage.total_tokens.map(|t| t.saturating_sub(input)).unwrap_or(0));
    Usage { input_tokens: Some(input), output_tokens: Some(output), cache_read_tokens: Some(usage.cache_read_tokens.unwrap_or(0)), cache_write_tokens: Some(usage.cache_write_tokens.unwrap_or(0)), reasoning_tokens: Some(0) }
}

impl LanguageModel for StubModel {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn provider(&self) -> &str {
        "goon-stub"
    }

    fn stream(&self, request: ModelRequest) -> BoxFuture<'static, Result<PartStream, ApiError>> {
        let turn = self.select_turn(&request.messages);
        Box::pin(async move {
            if let Some(error) = &turn.error {
                let mut api = ApiError::http(error.status_code.unwrap_or(400), error.message.clone(), Default::default());
                api.retryable = error.retryable.unwrap_or(false);
                return Err(api);
            }
            let mut parts = vec![];
            for (i, text) in turn.reasoning.iter().enumerate() {
                let id = format!("reasoning-{i}");
                parts.push(StreamPart::ReasoningDelta { id: id.clone(), delta: text.clone(), provider_metadata: None });
                parts.push(StreamPart::ReasoningEnd { id, provider_metadata: None });
            }
            for call in &turn.tool_calls {
                let json = crate::jsutil::stringify(&call.input);
                parts.push(StreamPart::ToolInputStart { id: call.id.clone(), tool_name: call.name.clone() });
                parts.push(StreamPart::ToolInputDelta { id: call.id.clone(), delta: json.clone() });
                parts.push(StreamPart::ToolCall { id: call.id.clone(), tool_name: call.name.clone(), input: json, provider_metadata: None });
            }
            for (i, text) in turn.text.iter().enumerate() {
                if text.is_empty() {
                    continue;
                }
                let id = format!("text-{i}");
                parts.push(StreamPart::TextDelta { id: id.clone(), delta: text.clone(), provider_metadata: None });
                parts.push(StreamPart::TextEnd { id, provider_metadata: None });
            }
            let finish = if turn.tool_calls.is_empty() { FinishReason::Stop } else { FinishReason::ToolCalls };
            parts.push(StreamPart::Finish { usage: to_usage(&turn), finish_reason: finish });
            let (tx, stream) = part_channel();
            tokio::spawn(async move {
                for part in parts {
                    if tx.send(part).await.is_err() {
                        return;
                    }
                }
            });
            Ok(stream)
        })
    }
}

/// Reads a stub fixture from `path` (relative to the process cwd).
pub fn fixture_path_exists(path: &str) -> bool {
    Path::new(path).exists()
}
