//! Interactive setup wizard: provider preset, base URL, API key, model and reasoning effort.

use crate::config::{config_path, credentials_path, load_config, normalize_gateway_url, resolve_api_key, save_config, save_credential, Config, DEFAULT_GATEWAY_URL, EFFORT_LEVELS};
use crate::provider::{discover_models, format_model_badge, resolve_model_capabilities, DiscoveredModel};
use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use std::sync::Arc;

pub struct ProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub provider: &'static str,
    pub base_url: &'static str,
}

pub const PROVIDER_PRESETS: [ProviderPreset; 7] = [
    ProviderPreset { id: "anthropic", label: "Anthropic Messages API (YuriStore / Claude / Anthropic-compatible)", provider: "anthropic", base_url: DEFAULT_GATEWAY_URL },
    ProviderPreset { id: "openai-responses", label: "OpenAI Responses API (YuriStore / Codex / Responses endpoint)", provider: "openai-responses", base_url: DEFAULT_GATEWAY_URL },
    ProviderPreset {
        id: "openai-compatible",
        label: "YuriStore AI Gateway (https://api.yuristore.xyz/v1 · glm-5.3 / deepseek-v4.1 / space-bunny / beam)",
        provider: "openai-compatible",
        base_url: DEFAULT_GATEWAY_URL,
    },
    ProviderPreset { id: "openrouter", label: "OpenRouter Gateway", provider: "openai-compatible", base_url: "https://openrouter.ai/api/v1" },
    ProviderPreset { id: "deepseek", label: "DeepSeek Official API", provider: "openai-compatible", base_url: "https://api.deepseek.com/v1" },
    ProviderPreset { id: "groq", label: "Groq Cloud API", provider: "openai-compatible", base_url: "https://api.groq.com/openai/v1" },
    ProviderPreset { id: "local-proxy", label: "Local AI Gateway (http://127.0.0.1:8000/v1)", provider: "openai-compatible", base_url: "http://127.0.0.1:8000/v1" },
];

pub fn effort_description(level: &str) -> &'static str {
    match level {
        "low" => "Fast responses with lighter reasoning",
        "medium" => "Balanced speed and reasoning depth for everyday tasks",
        "high" => "Deep reasoning for complex engineering and debugging",
        "xhigh" => "Extra-high reasoning depth for hard architectural problems",
        _ => "Maximum reasoning compute budget and exhaustive verification",
    }
}

/// Parses a typed effort (`2`..`6` index, name, or `default`/`off`/`none`/`0`/`1` to clear).
pub fn parse_effort_input(raw: &str, fallback: Option<&str>) -> Option<String> {
    let clean = raw.trim().to_lowercase();
    if clean.is_empty() {
        return fallback.map(str::to_string);
    }
    if ["default", "off", "none", "0", "1"].contains(&clean.as_str()) {
        return None;
    }
    if let Ok(n) = clean.parse::<i64>() {
        let idx = n - 2;
        if idx >= 0 && (idx as usize) < EFFORT_LEVELS.len() {
            return Some(EFFORT_LEVELS[idx as usize].to_string());
        }
    }
    if EFFORT_LEVELS.contains(&clean.as_str()) {
        return Some(clean);
    }
    fallback.map(str::to_string)
}

pub type AskFn = Arc<dyn Fn(String) -> BoxFuture<'static, Result<String>> + Send + Sync>;
pub type WriteFn = Arc<dyn Fn(&str) + Send + Sync>;
pub type DiscoverFn = Arc<dyn Fn(String, Option<String>, String) -> BoxFuture<'static, Vec<DiscoveredModel>> + Send + Sync>;

#[derive(Clone, Default)]
pub struct SetupIo {
    pub write: Option<WriteFn>,
    pub ask: Option<AskFn>,
    pub ask_secret: Option<AskFn>,
    pub discover: Option<DiscoverFn>,
}

fn default_ask() -> AskFn {
    Arc::new(|label: String| Box::pin(async move { Ok(crate::term::read_line(&label, None).await?.unwrap_or_default().trim().to_string()) }))
}

fn default_secret() -> AskFn {
    Arc::new(|label: String| Box::pin(async move { Ok(crate::term::read_hidden(&label).await?.unwrap_or_default().trim().to_string()) }))
}

pub async fn run_setup_wizard(initial: Option<Config>, io: SetupIo) -> Result<Config> {
    let current = match initial {
        Some(c) => c,
        None => load_config(&Map::new())?,
    };
    let existing_key = resolve_api_key(&current);
    let write: WriteFn = io.write.clone().unwrap_or_else(|| Arc::new(|t: &str| eprint!("{t}")));
    let ask = io.ask.clone().unwrap_or_else(default_ask);
    let ask_secret = io.ask_secret.clone().or_else(|| io.ask.clone()).unwrap_or_else(default_secret);
    let discover: DiscoverFn = io.discover.clone().unwrap_or_else(|| Arc::new(|url, key, prov| Box::pin(async move { discover_models(&url, key.as_deref(), &prov, None).await })));

    write(&format!("\n◆ goon v{} setup\n  Provider, endpoint, API key, model and reasoning effort. Esc cancels; nothing is saved until the end.\n\nStep 1/5 · Provider\n", crate::version::VERSION));
    let default_index = PROVIDER_PRESETS.iter().position(|p| p.provider == current.provider && current.base_url.as_deref().map(|u| u == p.base_url).unwrap_or(true)).unwrap_or(0);
    for (idx, preset) in PROVIDER_PRESETS.iter().enumerate() {
        let marker = if idx == default_index { "●" } else { " " };
        write(&format!("  {marker} [{}] {}\n        ({} · {})\n", idx + 1, preset.label, preset.provider, preset.base_url));
    }
    let choice = ask(format!("\nSelect provider [1-{}] (default {}): ", PROVIDER_PRESETS.len(), default_index + 1)).await?.trim().to_string();
    let chosen = if choice.is_empty() { Some(default_index as i64) } else { crate::config::js_number(&choice).is_finite().then(|| crate::config::js_number(&choice) as i64 - 1) };
    let chosen_idx = chosen.filter(|i| *i >= 0 && (*i as usize) < PROVIDER_PRESETS.len()).map(|i| i as usize);
    let preset = &PROVIDER_PRESETS[chosen_idx.unwrap_or(default_index)];
    let default_url = match (&current.base_url, chosen_idx == Some(default_index)) {
        (Some(u), true) => u.clone(),
        _ => preset.base_url.to_string(),
    };
    write(&format!("\nStep 2/5 · API Base URL ({}):\n", preset.provider));
    let url_input = ask(format!("Base URL [{default_url}]: ")).await?.trim().to_string();
    let base_url = normalize_gateway_url(if url_input.is_empty() { &default_url } else { &url_input });

    let key_hint = match &existing_key {
        Some(k) => format!("saved ({}...{}) — press Enter to keep", crate::jsutil::take_chars(k, 4), crate::jsutil::tail_chars(k, 2)),
        None => format!("stored in {}", credentials_path().display()),
    };
    write(&format!("\nStep 3/5 · API Key ({}, {key_hint}):\n", current.key_env));
    let key_input = ask_secret("API Key (input hidden): ".into()).await?.trim().to_string();
    let active_key = if key_input.is_empty() { existing_key.clone() } else { Some(key_input.clone()) };

    write(&format!("\nStep 4/5 · Model ID (probing {base_url}/models ...):\n"));
    let discovered = discover(base_url.clone(), active_key.clone(), preset.provider.to_string()).await;
    if !discovered.is_empty() {
        let shown = &discovered[..discovered.len().min(30)];
        for (idx, m) in shown.iter().enumerate() {
            write(&format!("    [{}] {}{}\n", idx + 1, m.id, format_model_badge(m)));
        }
        if discovered.len() > shown.len() {
            write(&format!("    ... and {} more models available on endpoint\n", discovered.len() - shown.len()));
        }
    } else {
        write("  (Endpoint did not return a /models list — enter your exact model ID below)\n");
    }
    let fallback_model = current.model.clone().unwrap_or_default();
    if !fallback_model.is_empty() && !discovered.is_empty() && !discovered.iter().any(|d| d.id == fallback_model) {
        write(&format!("  ! Current model \"{fallback_model}\" is not listed by this endpoint.\n"));
    }
    let mut model = String::new();
    let limit = discovered.len().min(30);
    while model.is_empty() {
        let keep = if fallback_model.is_empty() { String::new() } else { format!(" (Enter keeps {fallback_model})") };
        let hint = if !discovered.is_empty() { format!("Enter model ID or number [1-{limit}]{keep}: ") } else { format!("Enter exact model ID{keep}: ") };
        let input = ask(hint).await?.trim().to_string();
        let idx = input.parse::<i64>().ok().map(|n| n - 1);
        model = match idx {
            Some(i) if !input.is_empty() && i >= 0 && (i as usize) < limit => discovered[i as usize].id.clone(),
            _ => {
                if input.is_empty() {
                    fallback_model.clone()
                } else {
                    input
                }
            }
        };
        if model.is_empty() {
            write("  ! Model ID is required (no models are hardcoded).\n");
        }
    }
    let meta = resolve_model_capabilities(&model, discovered.iter().find(|d| d.id == model));
    let next_context = meta.context_window.unwrap_or(current.context_window);
    let raw_max_out = meta.max_output_tokens.unwrap_or(current.max_output_tokens);
    let next_max_out = 131_072u64.min(raw_max_out.min((next_context / 2).saturating_sub(1)));

    write("\nStep 5/5 · Reasoning Effort:\n");
    let current_effort = current.effort.clone().or(meta.default_effort.clone());
    write(&format!("  {} [1] default  — Use provider/model default reasoning behavior\n", if current_effort.is_none() { "●" } else { " " }));
    for (idx, level) in EFFORT_LEVELS.iter().enumerate() {
        let marker = if current_effort.as_deref() == Some(*level) { "●" } else { " " };
        write(&format!("  {marker} [{}] {:<8} — {}\n", idx + 2, level, effort_description(level)));
    }
    let effort_raw = ask(format!("Select effort [1-{}] or name (default: {}): ", EFFORT_LEVELS.len() + 1, current_effort.as_deref().unwrap_or("default"))).await?.trim().to_string();
    let effort = parse_effort_input(&effort_raw, current_effort.as_deref());

    let mut overrides = Map::new();
    overrides.insert("provider".into(), json!(preset.provider));
    overrides.insert("baseUrl".into(), json!(base_url));
    overrides.insert("model".into(), json!(model));
    if let Some(e) = &effort {
        overrides.insert("effort".into(), json!(e));
    } else {
        overrides.insert("effort".into(), Value::Null);
    }
    overrides.insert("contextWindow".into(), json!(next_context));
    overrides.insert("maxOutputTokens".into(), json!(next_max_out));
    overrides.insert("keyEnv".into(), json!(current.key_env));
    let updated = load_config(&overrides)?;
    if !key_input.is_empty() {
        save_credential(&current.key_env, &key_input)?;
    }
    save_config(&updated)?;
    let final_key = resolve_api_key(&updated);
    let rows: Vec<(&str, String)> = vec![
        ("provider", updated.provider.clone()),
        ("base URL", updated.base_url.clone().unwrap_or_else(|| "default".into())),
        ("model", updated.model.clone().unwrap_or_default()),
        ("effort", updated.effort.clone().unwrap_or_else(|| "default".into())),
        ("context", format!("{} tokens (output {})", updated.context_window, updated.max_output_tokens)),
        ("API key", if final_key.is_some() { "configured".into() } else { format!("missing (set {})", updated.key_env) }),
        ("config", config_path().display().to_string()),
    ];
    write(&format!("\n● Setup saved\n{}\n\n", rows.iter().map(|(l, v)| format!("  {l:<9} {v}")).collect::<Vec<_>>().join("\n")));
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_input() {
        assert_eq!(parse_effort_input("3", None).as_deref(), Some("medium"));
        assert_eq!(parse_effort_input("MAX", None).as_deref(), Some("max"));
        assert_eq!(parse_effort_input("default", Some("low")), None);
        assert_eq!(parse_effort_input("", Some("low")).as_deref(), Some("low"));
        assert_eq!(parse_effort_input("bogus", Some("high")).as_deref(), Some("high"));
    }
}
