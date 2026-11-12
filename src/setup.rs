//! Interactive setup wizard: provider preset, base URL, API key, model and reasoning effort.

use crate::config::{
    atomic_json, config_path, credentials_path, load_config, mkdir_private, normalize_gateway_url, parse_config, resolve_api_key, save_config, save_credential, Config, DEFAULT_GATEWAY_URL,
    EFFORT_LEVELS,
};
use crate::provider::{discover_models, format_model_badge, resolve_model_capabilities, DiscoveredModel};
use anyhow::{bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;

const MAX_INVALID_ATTEMPTS: usize = 5;

pub struct ProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub provider: &'static str,
    pub base_url: &'static str,
    pub key_env: &'static str,
}

pub const PROVIDER_PRESETS: [ProviderPreset; 9] = [
    ProviderPreset { id: "anthropic", label: "Anthropic Official API (Claude Messages)", provider: "anthropic", base_url: "https://api.anthropic.com/v1", key_env: "ANTHROPIC_API_KEY" },
    ProviderPreset { id: "openai", label: "OpenAI Official API (Chat Completions)", provider: "openai-compatible", base_url: "https://api.openai.com/v1", key_env: "OPENAI_API_KEY" },
    ProviderPreset {
        id: "openai-responses",
        label: "OpenAI Official Responses API (Codex / GPT-5 / o-series)",
        provider: "openai-responses",
        base_url: "https://api.openai.com/v1",
        key_env: "OPENAI_API_KEY",
    },
    ProviderPreset { id: "openrouter", label: "OpenRouter Gateway", provider: "openai-compatible", base_url: "https://openrouter.ai/api/v1", key_env: "OPENROUTER_API_KEY" },
    ProviderPreset { id: "deepseek", label: "DeepSeek Official API", provider: "openai-compatible", base_url: "https://api.deepseek.com/v1", key_env: "DEEPSEEK_API_KEY" },
    ProviderPreset { id: "groq", label: "Groq Cloud API", provider: "openai-compatible", base_url: "https://api.groq.com/openai/v1", key_env: "GROQ_API_KEY" },
    ProviderPreset { id: "ollama", label: "Ollama Local (http://127.0.0.1:11434/v1)", provider: "openai-compatible", base_url: "http://127.0.0.1:11434/v1", key_env: "OLLAMA_API_KEY" },
    ProviderPreset {
        id: "yuristore",
        label: "YuriStore AI Gateway (https://api.yuristore.xyz/v1 · glm-5.3 / deepseek-v4.1 / space-bunny / beam)",
        provider: "openai-compatible",
        base_url: DEFAULT_GATEWAY_URL,
        key_env: "GOON_API_KEY",
    },
    ProviderPreset { id: "local-proxy", label: "Local AI Gateway (http://127.0.0.1:8000/v1)", provider: "openai-compatible", base_url: "http://127.0.0.1:8000/v1", key_env: "GOON_API_KEY" },
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

/// Parses Step 1 provider selection by `[1-N]` number, preset `id`, unambiguous alias, or unique prefix.
pub fn parse_provider_choice(raw: &str, default_index: usize) -> Option<usize> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(default_index.min(PROVIDER_PRESETS.len() - 1));
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        if n >= 1 && (n as usize) <= PROVIDER_PRESETS.len() {
            return Some((n as usize) - 1);
        }
        return None;
    }
    let slug: String =
        trimmed.to_ascii_lowercase().chars().map(|c| if c == '_' || c.is_whitespace() { '-' } else { c }).collect::<String>().split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
    if slug.is_empty() {
        return None;
    }
    if let Some(idx) = PROVIDER_PRESETS.iter().position(|p| p.id == slug) {
        return Some(idx);
    }
    let alias_id = match slug.as_str() {
        "claude" | "anthropic-official" => Some("anthropic"),
        "openai-compatible" | "openai-chat" | "openai-official" | "chatgpt" => Some("openai"),
        "responses" | "codex" | "openai-codex" => Some("openai-responses"),
        "open-router" => Some("openrouter"),
        "deep-seek" => Some("deepseek"),
        "ollama-local" => Some("ollama"),
        "yuri" | "yuri-store" | "yuristore-gateway" => Some("yuristore"),
        "local" | "proxy" | "local-gateway" => Some("local-proxy"),
        _ => None,
    };
    if let Some(id) = alias_id {
        return PROVIDER_PRESETS.iter().position(|p| p.id == id);
    }
    let matches: Vec<usize> = PROVIDER_PRESETS.iter().enumerate().filter_map(|(i, p)| p.id.starts_with(&slug).then_some(i)).collect();
    (matches.len() == 1).then(|| matches[0])
}

/// Validates Step 5 reasoning effort selection instead of silently falling back on typos.
pub fn parse_effort_choice(raw: &str, fallback: Option<&str>) -> std::result::Result<Option<String>, String> {
    let clean = raw.trim().to_ascii_lowercase();
    if clean.is_empty() {
        return Ok(fallback.map(str::to_string));
    }
    if ["default", "off", "none", "0", "1"].contains(&clean.as_str()) {
        return Ok(None);
    }
    if let Some(level) = parse_effort_input(&clean, None) {
        return Ok(Some(level));
    }
    Err(format!("  ! Invalid effort \"{}\". Enter 1-{} or one of: default, {}.\n", raw.trim(), EFFORT_LEVELS.len() + 1, EFFORT_LEVELS.join(", ")))
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
    Arc::new(|label: String| {
        Box::pin(async move {
            let Some(line) = crate::term::read_line_cancelable(&label, None).await? else {
                bail!(crate::term::InputCancelled);
            };
            Ok(line.trim().to_string())
        })
    })
}

fn default_secret() -> AskFn {
    Arc::new(|label: String| {
        Box::pin(async move {
            let Some(line) = crate::term::read_hidden(&label).await? else {
                bail!(crate::term::InputCancelled);
            };
            Ok(line.trim().to_string())
        })
    })
}

pub async fn run_setup_wizard(initial: Option<Config>, io: SetupIo) -> Result<Config> {
    run_setup_wizard_in(initial, io, None).await
}

pub(crate) async fn run_setup_wizard_in(initial: Option<Config>, io: SetupIo, config_dir_override: Option<PathBuf>) -> Result<Config> {
    let current = match initial {
        Some(c) => c,
        None => load_config(&Map::new())?,
    };
    let write: WriteFn = io.write.clone().unwrap_or_else(|| Arc::new(|t: &str| eprint!("{t}")));
    let ask = io.ask.clone().unwrap_or_else(default_ask);
    let ask_secret = io.ask_secret.clone().or_else(|| io.ask.clone()).unwrap_or_else(default_secret);
    let discover: DiscoverFn = io.discover.clone().unwrap_or_else(|| Arc::new(|url, key, prov| Box::pin(async move { discover_models(&url, key.as_deref(), &prov, None).await })));

    write(&format!("\n◆ goon v{} setup\n  Provider, endpoint, API key, model and reasoning effort. Esc cancels; nothing is saved until the end.\n\nStep 1/5 · Provider\n", crate::version::VERSION));
    let normalized_current_url = current.base_url.as_deref().map(normalize_gateway_url);
    let matched_preset_idx = PROVIDER_PRESETS.iter().position(|p| p.provider == current.provider && normalized_current_url.as_deref().is_some_and(|u| u == p.base_url));
    let default_index = matched_preset_idx
        .or_else(|| PROVIDER_PRESETS.iter().position(|p| p.provider == current.provider && p.key_env == current.key_env && current.base_url.is_none()))
        .or_else(|| PROVIDER_PRESETS.iter().position(|p| p.provider == current.provider && current.base_url.is_none()))
        .unwrap_or(0);
    for (idx, preset) in PROVIDER_PRESETS.iter().enumerate() {
        let marker = if idx == default_index { "●" } else { " " };
        write(&format!("  {marker} [{}] {} [{}]\n        ({} · {} · {})\n", idx + 1, preset.label, preset.id, preset.provider, preset.base_url, preset.key_env));
    }
    let preset_ids = PROVIDER_PRESETS.iter().map(|p| p.id).collect::<Vec<_>>().join(", ");
    let mut chosen_idx = None;
    for _ in 0..MAX_INVALID_ATTEMPTS {
        let choice = ask(format!("\nSelect provider [1-{}] or name (default {}): ", PROVIDER_PRESETS.len(), default_index + 1)).await?.trim().to_string();
        if let Some(idx) = parse_provider_choice(&choice, default_index) {
            chosen_idx = Some(idx);
            break;
        }
        write(&format!("  ! Invalid provider \"{choice}\". Enter 1-{} or preset name ({preset_ids}).\n", PROVIDER_PRESETS.len()));
    }
    let Some(chosen_idx) = chosen_idx else {
        bail!("Too many invalid provider selections.");
    };
    let preset = &PROVIDER_PRESETS[chosen_idx];
    let default_url = match (&current.base_url, matched_preset_idx == Some(chosen_idx)) {
        (Some(u), true) => u.clone(),
        _ => preset.base_url.to_string(),
    };
    write(&format!("\nStep 2/5 · API Base URL ({}):\n", preset.provider));
    let url_input = ask(format!("Base URL [{default_url}]: ")).await?.trim().to_string();
    let base_url = normalize_gateway_url(if url_input.is_empty() { &default_url } else { &url_input });

    let cfg_file = config_dir_override.as_ref().map(|d| d.join("config.json")).unwrap_or_else(config_path);
    let creds_file = config_dir_override.as_ref().map(|d| d.join("credentials.json")).unwrap_or_else(credentials_path);
    let load_creds_map = |file: &std::path::Path| -> Map<String, Value> {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| match v {
                Value::Object(m) if m.values().all(Value::is_string) => Some(m),
                _ => None,
            })
            .unwrap_or_default()
    };

    let is_standard_key_env = current.key_env == "GOON_API_KEY" || PROVIDER_PRESETS.iter().any(|p| p.key_env == current.key_env);
    let target_key_env = if !is_standard_key_env && matched_preset_idx == Some(chosen_idx) { current.key_env.clone() } else { preset.key_env.to_string() };
    let existing_key = if config_dir_override.is_some() {
        load_creds_map(&creds_file).get(&target_key_env).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
    } else {
        let mut probe_cfg = current.clone();
        probe_cfg.key_env = target_key_env.clone();
        resolve_api_key(&probe_cfg)
    };
    let key_optional = preset.id == "ollama";
    let existing_real_key = existing_key.clone().filter(|k| !(key_optional && k == "ollama"));

    let key_hint = match (&existing_real_key, key_optional) {
        (Some(k), _) => format!("saved ({}...{}) — press Enter to keep", crate::jsutil::take_chars(k, 4), crate::jsutil::tail_chars(k, 2)),
        (None, true) => "optional for local Ollama — press Enter to skip".to_string(),
        (None, false) => format!("stored in {}", creds_file.display()),
    };
    write(&format!("\nStep 3/5 · API Key ({target_key_env}, {key_hint}):\n"));
    let secret_prompt = if key_optional && existing_real_key.is_none() { "API Key (optional, press Enter to skip): " } else { "API Key (input hidden): " };
    let key_input = ask_secret(secret_prompt.into()).await?.trim().to_string();
    let active_key = if key_input.is_empty() { existing_real_key.clone() } else { Some(key_input.clone()) };

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
    let mut model_attempts = 0usize;
    while model.is_empty() {
        if model_attempts >= MAX_INVALID_ATTEMPTS {
            bail!("Model ID is required.");
        }
        model_attempts += 1;
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
    let mut effort_resolved = None;
    for _ in 0..MAX_INVALID_ATTEMPTS {
        let effort_raw = ask(format!("Select effort [1-{}] or name (default: {}): ", EFFORT_LEVELS.len() + 1, current_effort.as_deref().unwrap_or("default"))).await?.trim().to_string();
        match parse_effort_choice(&effort_raw, current_effort.as_deref()) {
            Ok(val) => {
                effort_resolved = Some(val);
                break;
            }
            Err(msg) => write(&msg),
        }
    }
    let Some(effort) = effort_resolved else {
        bail!("Too many invalid reasoning effort selections.");
    };

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
    overrides.insert("keyEnv".into(), json!(target_key_env));
    let save_key_val: Option<&str> = if !key_input.is_empty() {
        Some(key_input.as_str())
    } else if key_optional && existing_key.is_none() {
        Some("ollama")
    } else {
        None
    };
    let (updated, final_key) = if let Some(dir) = &config_dir_override {
        mkdir_private(dir)?;
        let mut base_map = std::fs::read_to_string(&cfg_file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        for (k, v) in overrides {
            if v.is_null() {
                base_map.remove(&k);
            } else {
                base_map.insert(k, v);
            }
        }
        let updated = parse_config(&Value::Object(base_map))?;
        if let Some(val) = save_key_val {
            let mut creds = load_creds_map(&creds_file);
            creds.insert(target_key_env.clone(), Value::String(val.to_string()));
            atomic_json(&creds_file, &Value::Object(creds), false)?;
        }
        atomic_json(&cfg_file, &updated.to_value(), false)?;
        let resolved = load_creds_map(&creds_file).get(&updated.key_env).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        (updated, resolved)
    } else {
        let updated = load_config(&overrides)?;
        if let Some(val) = save_key_val {
            save_credential(&target_key_env, val)?;
        }
        save_config(&updated)?;
        let resolved = resolve_api_key(&updated);
        (updated, resolved)
    };
    let has_real_key = final_key.as_deref().is_some_and(|k| !(key_optional && k == "ollama"));
    let key_status = if has_real_key {
        "configured".into()
    } else if key_optional {
        "optional (local Ollama)".into()
    } else {
        format!("missing (set {})", updated.key_env)
    };
    let rows: Vec<(&str, String)> = vec![
        ("provider", updated.provider.clone()),
        ("base URL", updated.base_url.clone().unwrap_or_else(|| "default".into())),
        ("model", updated.model.clone().unwrap_or_default()),
        ("effort", updated.effort.clone().unwrap_or_else(|| "default".into())),
        ("context", format!("{} tokens (output {})", updated.context_window, updated.max_output_tokens)),
        ("API key", key_status),
        ("config", cfg_file.display().to_string()),
    ];
    write(&format!("\n● Setup saved\n{}\n\n", rows.iter().map(|(l, v)| format!("  {l:<9} {v}")).collect::<Vec<_>>().join("\n")));
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[test]
    fn effort_input() {
        assert_eq!(parse_effort_input("3", None).as_deref(), Some("medium"));
        assert_eq!(parse_effort_input("MAX", None).as_deref(), Some("max"));
        assert_eq!(parse_effort_input("default", Some("low")), None);
        assert_eq!(parse_effort_input("", Some("low")).as_deref(), Some("low"));
        assert_eq!(parse_effort_input("bogus", Some("high")).as_deref(), Some("high"));
    }

    #[test]
    fn presets_have_official_urls_and_preserve_openrouter_slot_4() {
        assert_eq!(PROVIDER_PRESETS.len(), 9);
        assert_eq!(PROVIDER_PRESETS[0].id, "anthropic");
        assert_eq!(PROVIDER_PRESETS[0].base_url, "https://api.anthropic.com/v1");
        assert_eq!(PROVIDER_PRESETS[0].key_env, "ANTHROPIC_API_KEY");
        assert_eq!(PROVIDER_PRESETS[1].id, "openai");
        assert_eq!(PROVIDER_PRESETS[1].base_url, "https://api.openai.com/v1");
        assert_eq!(PROVIDER_PRESETS[1].key_env, "OPENAI_API_KEY");
        assert_eq!(PROVIDER_PRESETS[2].id, "openai-responses");
        assert_eq!(PROVIDER_PRESETS[2].base_url, "https://api.openai.com/v1");
        assert_eq!(PROVIDER_PRESETS[2].key_env, "OPENAI_API_KEY");
        assert_eq!(PROVIDER_PRESETS[3].id, "openrouter");
        assert_eq!(PROVIDER_PRESETS[3].provider, "openai-compatible");
        assert_eq!(PROVIDER_PRESETS[3].base_url, "https://openrouter.ai/api/v1");
        assert_eq!(PROVIDER_PRESETS[3].key_env, "OPENROUTER_API_KEY");
        assert_eq!(PROVIDER_PRESETS[4].id, "deepseek");
        assert_eq!(PROVIDER_PRESETS[4].key_env, "DEEPSEEK_API_KEY");
        assert_eq!(PROVIDER_PRESETS[5].id, "groq");
        assert_eq!(PROVIDER_PRESETS[5].key_env, "GROQ_API_KEY");
        assert_eq!(PROVIDER_PRESETS[6].id, "ollama");
        assert_eq!(PROVIDER_PRESETS[6].base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(PROVIDER_PRESETS[6].key_env, "OLLAMA_API_KEY");
        assert_eq!(PROVIDER_PRESETS[7].id, "yuristore");
        assert_eq!(PROVIDER_PRESETS[7].base_url, DEFAULT_GATEWAY_URL);
        assert_eq!(PROVIDER_PRESETS[8].id, "local-proxy");
        assert_eq!(PROVIDER_PRESETS[8].base_url, "http://127.0.0.1:8000/v1");
    }

    #[test]
    fn provider_choice_accepts_numbers_ids_aliases_and_rejects_invalid_or_ambiguous() {
        assert_eq!(parse_provider_choice("", 3), Some(3));
        for n in 1..=9 {
            assert_eq!(parse_provider_choice(&n.to_string(), 0), Some(n - 1));
        }
        assert_eq!(parse_provider_choice("Anthropic", 5), Some(0));
        assert_eq!(parse_provider_choice("claude", 5), Some(0));
        assert_eq!(parse_provider_choice("OPENAI", 0), Some(1));
        assert_eq!(parse_provider_choice("codex", 0), Some(2));
        assert_eq!(parse_provider_choice("openrouter", 0), Some(3));
        assert_eq!(parse_provider_choice("ollama", 0), Some(6));
        assert_eq!(parse_provider_choice("yuristore", 0), Some(7));
        assert_eq!(parse_provider_choice("local_proxy", 0), Some(8));
        assert_eq!(parse_provider_choice("anth", 5), Some(0));
        assert_eq!(parse_provider_choice("0", 0), None);
        assert_eq!(parse_provider_choice("10", 0), None);
        assert_eq!(parse_provider_choice("-1", 0), None);
        assert_eq!(parse_provider_choice("open", 0), None);
        assert_eq!(parse_provider_choice("bogus", 0), None);
    }

    #[test]
    fn effort_choice_validates_and_rejects_typos() {
        assert_eq!(parse_effort_choice("", Some("high")).unwrap().as_deref(), Some("high"));
        assert_eq!(parse_effort_choice("1", Some("high")).unwrap(), None);
        assert_eq!(parse_effort_choice("default", Some("high")).unwrap(), None);
        assert_eq!(parse_effort_choice("4", None).unwrap().as_deref(), Some("high"));
        assert_eq!(parse_effort_choice("xhigh", None).unwrap().as_deref(), Some("xhigh"));
        assert!(parse_effort_choice("meduim", Some("low")).is_err());
        assert!(parse_effort_choice("99", None).is_err());
    }

    fn scripted_io(answers: Vec<&str>, secret: &str, out: Arc<Mutex<String>>, asked: Arc<Mutex<Vec<String>>>, probed_keys: Arc<Mutex<Vec<Option<String>>>>) -> SetupIo {
        let queue = Arc::new(Mutex::new(answers.into_iter().map(str::to_string).collect::<VecDeque<_>>()));
        let secret_val = secret.to_string();
        SetupIo {
            write: Some(Arc::new(move |text: &str| {
                out.lock().unwrap().push_str(text);
            })),
            ask: Some(Arc::new(move |label: String| {
                asked.lock().unwrap().push(label);
                let next = queue.lock().unwrap().pop_front().unwrap_or_default();
                Box::pin(async move { Ok(next) })
            })),
            ask_secret: Some(Arc::new(move |_label: String| {
                let s = secret_val.clone();
                Box::pin(async move { Ok(s) })
            })),
            discover: Some(Arc::new(move |_url, key, _prov| {
                probed_keys.lock().unwrap().push(key);
                Box::pin(async move { vec![] })
            })),
        }
    }

    #[tokio::test]
    async fn wizard_isolates_per_provider_credentials_reprompts_invalid_and_handles_ollama_optional_key() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("goon");

        let out1 = Arc::new(Mutex::new(String::new()));
        let asked1 = Arc::new(Mutex::new(vec![]));
        let probed1 = Arc::new(Mutex::new(vec![]));
        let cfg1 = run_setup_wizard_in(
            Some(Config::default()),
            scripted_io(vec!["anthropic", "", "claude-sonnet-4-5", "high"], "sk-ant-secret-111", out1.clone(), asked1.clone(), probed1.clone()),
            Some(dir.clone()),
        )
        .await
        .unwrap();
        assert_eq!(cfg1.provider, "anthropic");
        assert_eq!(cfg1.base_url.as_deref(), Some("https://api.anthropic.com/v1"));
        assert_eq!(cfg1.key_env, "ANTHROPIC_API_KEY");
        assert!(!asked1.lock().unwrap().iter().any(|l| l.to_ascii_lowercase().contains("api key")));

        let out2 = Arc::new(Mutex::new(String::new()));
        let asked2 = Arc::new(Mutex::new(vec![]));
        let probed2 = Arc::new(Mutex::new(vec![]));
        let cfg2 = run_setup_wizard_in(
            Some(cfg1.clone()),
            scripted_io(vec!["not-a-preset", "openrouter", "", "openai/gpt-5", "meduim", "medium"], "sk-or-secret-222", out2.clone(), asked2, probed2.clone()),
            Some(dir.clone()),
        )
        .await
        .unwrap();
        assert_eq!(cfg2.provider, "openai-compatible");
        assert_eq!(cfg2.base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
        assert_eq!(cfg2.key_env, "OPENROUTER_API_KEY");
        assert_eq!(cfg2.effort.as_deref(), Some("medium"));
        assert!(out2.lock().unwrap().contains("Invalid provider"));
        assert!(out2.lock().unwrap().contains("Invalid effort"));
        assert_eq!(probed2.lock().unwrap()[0].as_deref(), Some("sk-or-secret-222"));

        let creds_raw = std::fs::read_to_string(dir.join("credentials.json")).unwrap();
        let creds: serde_json::Value = serde_json::from_str(&creds_raw).unwrap();
        assert_eq!(creds["ANTHROPIC_API_KEY"], "sk-ant-secret-111");
        assert_eq!(creds["OPENROUTER_API_KEY"], "sk-or-secret-222");

        // Fresh Config::default() highlights [8] YuriStore (not Ollama)
        assert!(out1.lock().unwrap().contains("● [8] YuriStore AI Gateway"), "fresh install should default to YuriStore:\n{}", out1.lock().unwrap());

        // Save a GOON_API_KEY under YuriStore first to verify Ollama never exfiltrates GOON_API_KEY
        let out_yuri = Arc::new(Mutex::new(String::new()));
        let cfg_yuri = run_setup_wizard_in(
            Some(cfg2),
            scripted_io(vec!["yuristore", "", "glm-5.3", "default"], "sk-yuri-paid-secret-9999", out_yuri, Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(vec![]))),
            Some(dir.clone()),
        )
        .await
        .unwrap();
        assert_eq!(cfg_yuri.key_env, "GOON_API_KEY");

        // Keyless Ollama setup isolates OLLAMA_API_KEY, sends None to /models, and persists "ollama" sentinel
        let out3 = Arc::new(Mutex::new(String::new()));
        let probed3 = Arc::new(Mutex::new(vec![]));
        let cfg3 =
            run_setup_wizard_in(Some(cfg_yuri), scripted_io(vec!["ollama", "", "qwen2.5-coder:7b", "default"], "", out3.clone(), Arc::new(Mutex::new(vec![])), probed3.clone()), Some(dir.clone()))
                .await
                .unwrap();
        assert_eq!(cfg3.base_url.as_deref(), Some("http://127.0.0.1:11434/v1"));
        assert_eq!(cfg3.key_env, "OLLAMA_API_KEY");
        assert_eq!(probed3.lock().unwrap().as_slice(), &[None], "Ollama /models probe must never leak GOON_API_KEY");
        assert!(out3.lock().unwrap().contains("optional (local Ollama)"));
        let creds_after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("credentials.json")).unwrap()).unwrap();
        assert_eq!(creds_after["OLLAMA_API_KEY"], "ollama");
        assert_eq!(creds_after["GOON_API_KEY"], "sk-yuri-paid-secret-9999");

        // Re-running Ollama setup with stored "ollama" sentinel still treats key as optional and probes with None
        let out4 = Arc::new(Mutex::new(String::new()));
        let probed4 = Arc::new(Mutex::new(vec![]));
        let cfg4 = run_setup_wizard_in(Some(cfg3), scripted_io(vec!["", "", "qwen2.5-coder:14b", "default"], "", out4.clone(), Arc::new(Mutex::new(vec![])), probed4.clone()), Some(dir.clone()))
            .await
            .unwrap();
        assert_eq!(cfg4.model.as_deref(), Some("qwen2.5-coder:14b"));
        assert!(out4.lock().unwrap().contains("optional for local Ollama — press Enter to skip"));
        assert!(out4.lock().unwrap().contains("optional (local Ollama)"));
        assert_eq!(probed4.lock().unwrap().as_slice(), &[None]);

        let err = run_setup_wizard_in(
            Some(Config::default()),
            scripted_io(vec!["bad", "bad", "bad", "bad", "bad"], "", Arc::new(Mutex::new(String::new())), Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(vec![]))),
            Some(dir),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Too many invalid provider selections"));
    }
}
