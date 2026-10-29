//! Model factory, gateway model catalog and `/models` discovery.

use crate::config::{normalize_gateway_url, RuntimeConfig, EFFORT_LEVELS};
use crate::model::LanguageModel;
use crate::providers::{anthropic::AnthropicModel, openai_compat::OpenAICompatModel, openai_responses::OpenAIResponsesModel, stub::StubModel};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn create_model(config: &RuntimeConfig, session_id: Option<&str>) -> anyhow::Result<Arc<dyn LanguageModel>> {
    if config.stub {
        let fixture = crate::providers::stub::load_stub_fixture(config.stub_fixture.as_deref())?;
        let cursor = config.stub_cursor.clone().unwrap_or_else(|| Arc::new(Mutex::new(0)));
        return Ok(Arc::new(StubModel::new(fixture, cursor, Some(&config.model))));
    }
    let model_id = config.model.clone();
    let base_url = config.base_url.clone();
    let api_key = config.api_key.clone();
    let session_id = session_id.map(str::to_string);
    Ok(match config.provider.as_str() {
        "anthropic" => Arc::new(AnthropicModel { model_id, base_url, api_key, session_id }),
        "openai-responses" => Arc::new(OpenAIResponsesModel { model_id, base_url, api_key, session_id }),
        _ => Arc::new(OpenAICompatModel { model_id, base_url, api_key, session_id }),
    })
}

/// Effort for an auxiliary call (compaction, /recap, /btw): `low` whenever an effort is configured.
pub fn auxiliary_effort(effort: Option<&str>) -> Option<String> {
    effort.map(|_| "low".to_string())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscoveredModel {
    pub id: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_vision: Option<bool>,
    pub default_effort: Option<String>,
}

pub fn known_gateway_model(id: &str) -> Option<DiscoveredModel> {
    let (ctx, out, vision) = match id {
        "glm-5.3-flash" => (200_000, 32_000, true),
        "deepseek-v4.1-flash" => (200_000, 32_000, true),
        "space-bunny" => (1_048_576, 65_536, true),
        "beam" => (1_048_576, 65_536, false),
        _ => return None,
    };
    Some(DiscoveredModel { id: id.into(), context_window: Some(ctx), max_output_tokens: Some(out), supports_vision: Some(vision), default_effort: Some("max".into()) })
}

pub const KNOWN_GATEWAY_MODEL_IDS: [&str; 4] = ["glm-5.3-flash", "deepseek-v4.1-flash", "space-bunny", "beam"];

fn max_of(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) if x > 0 && y > 0 => Some(x.max(y)),
        (Some(x), _) if x > 0 => Some(x),
        (_, y) => y.or(a),
    }
}

pub fn resolve_model_capabilities(model_id: &str, discovered: Option<&DiscoveredModel>) -> DiscoveredModel {
    let clean = model_id.trim();
    let known = known_gateway_model(clean);
    DiscoveredModel {
        id: clean.into(),
        context_window: max_of(discovered.and_then(|d| d.context_window), known.as_ref().and_then(|k| k.context_window)),
        max_output_tokens: max_of(discovered.and_then(|d| d.max_output_tokens), known.as_ref().and_then(|k| k.max_output_tokens)),
        supports_vision: discovered.and_then(|d| d.supports_vision).or(known.as_ref().and_then(|k| k.supports_vision)),
        default_effort: discovered.and_then(|d| d.default_effort.clone()).or(known.and_then(|k| k.default_effort)),
    }
}

fn format_token_count(n: u64) -> String {
    if n % 1024 == 0 {
        return format!("{}k", n / 1024);
    }
    if n % 1000 == 0 {
        return format!("{}k", n / 1000);
    }
    format!("{}k", crate::jsutil::js_round(n as f64 / 1024.0) as i64)
}

pub fn format_model_badge(m: &DiscoveredModel) -> String {
    let mut parts = vec![];
    if let Some(ctx) = m.context_window.filter(|v| *v > 0) {
        parts.push(format!("ctx: {}", format_token_count(ctx)));
    }
    if let Some(out) = m.max_output_tokens.filter(|v| *v > 0) {
        parts.push(format!("out: {}", format_token_count(out)));
    }
    match m.supports_vision {
        Some(true) => parts.push("vision".into()),
        Some(false) => parts.push("text-only".into()),
        None => {}
    }
    if let Some(effort) = &m.default_effort {
        parts.push(format!("effort: {effort}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(" · "))
    }
}

fn positive_int(candidates: &[Option<&Value>]) -> Option<u64> {
    for value in candidates.iter().flatten() {
        if let Some(n) = value.as_f64() {
            if n.fract() == 0.0 && n > 0.0 && n <= 9_007_199_254_740_991.0 {
                return Some(n as u64);
            }
        }
    }
    None
}

pub fn parse_models_listing(parsed: &Value) -> Vec<DiscoveredModel> {
    let items: Vec<Value> = if let Some(a) = parsed.as_array() {
        a.clone()
    } else if let Some(a) = parsed.get("data").and_then(Value::as_array) {
        a.clone()
    } else if let Some(a) = parsed.get("models").and_then(Value::as_array) {
        a.clone()
    } else {
        vec![]
    };
    let mut seen = HashSet::new();
    let mut results = vec![];
    for rec in items {
        if !rec.is_object() {
            continue;
        }
        let id = rec.get("id").and_then(Value::as_str).map(|s| s.trim().to_string()).or_else(|| rec.get("name").and_then(Value::as_str).map(|s| s.trim().to_string())).unwrap_or_default();
        if id.is_empty() || !seen.insert(id.clone()) {
            continue;
        }
        let known = known_gateway_model(&id);
        let one_m = Value::from(1_048_576u64);
        let known_ctx = known.as_ref().and_then(|k| k.context_window).map(Value::from);
        let raw_ctx = positive_int(&[
            if rec.get("supports_1m_context") == Some(&Value::Bool(true)) { Some(&one_m) } else { None },
            rec.get("context_window"),
            rec.get("context_length"),
            rec.get("max_context_length"),
            rec.get("max_input_tokens"),
            rec.get("inputTokenLimit"),
            known_ctx.as_ref(),
        ]);
        let context_window = match (raw_ctx, known.as_ref().and_then(|k| k.context_window)) {
            (Some(r), Some(k)) => Some(r.max(k)),
            (r, _) => r,
        };
        let known_out = known.as_ref().and_then(|k| k.max_output_tokens).map(Value::from);
        let raw_out = positive_int(&[rec.get("max_output_tokens"), rec.get("max_completion_tokens"), rec.get("max_tokens"), rec.get("outputTokenLimit"), known_out.as_ref()]);
        let max_output_tokens = match (raw_out, known.as_ref().and_then(|k| k.max_output_tokens)) {
            (Some(r), Some(k)) => Some(r.max(k)),
            (r, _) => r,
        };
        let supports_vision = rec.get("supports_vision").and_then(Value::as_bool).or(known.as_ref().and_then(|k| k.supports_vision));
        let raw_effort = rec.get("reasoning_effort").and_then(Value::as_str).map(|s| s.trim().to_lowercase());
        let default_effort = match raw_effort {
            Some(e) if EFFORT_LEVELS.contains(&e.as_str()) => Some(e),
            _ => known.and_then(|k| k.default_effort),
        };
        results.push(DiscoveredModel { id, context_window, max_output_tokens, supports_vision, default_effort });
    }
    results
}

pub async fn discover_models(base_url: &str, api_key: Option<&str>, provider: &str, cancel: Option<CancellationToken>) -> Vec<DiscoveredModel> {
    let clean = normalize_gateway_url(base_url);
    let mut request = crate::providers::http_client().get(format!("{clean}/models")).header("accept", "application/json").timeout(Duration::from_secs(4));
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        request = request.header("authorization", format!("Bearer {key}"));
        if provider == "anthropic" {
            request = request.header("x-api-key", key).header("anthropic-version", "2023-06-01");
        }
    }
    let cancel = cancel.unwrap_or_default();
    let fetch = async {
        let response = request.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let bytes = response.bytes().await.ok()?;
        if bytes.len() > 4 * 1024 * 1024 {
            return None;
        }
        let parsed: Value = serde_json::from_slice(&bytes).ok()?;
        Some(parse_models_listing(&parsed))
    };
    tokio::select! {
        result = fetch => result.unwrap_or_default(),
        _ = cancel.cancelled() => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn badge_and_listing() {
        let models = parse_models_listing(&json!({"data": [{"id": "beam", "context_length": 10}, {"id": "x", "max_tokens": 4096, "supports_vision": false, "reasoning_effort": "HIGH"}, {"id": "x"}]}));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].context_window, Some(1_048_576));
        assert_eq!(format_model_badge(&models[0]), " (ctx: 1024k · out: 64k · text-only · effort: max)");
        assert_eq!(format_model_badge(&models[1]), " (out: 4k · text-only · effort: high)");
    }
}
