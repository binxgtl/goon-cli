//! Configuration: `~/.config/goon/config.json`, environment overrides and CLI flags.
//!
//! The on-disk format is the one the TypeScript build wrote (zod schema with defaults, strict
//! keys): unknown keys are rejected, missing keys get defaults, and saving writes every field in
//! schema order with JS number formatting.

use crate::jsutil::{self, num};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
pub const DEFAULT_GATEWAY_URL: &str = "https://api.yuristore.xyz/v1";

#[derive(Clone, Debug, PartialEq)]
pub struct ModelPrice {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    pub cached_input_per_1m: Option<f64>,
    pub cache_write_per_1m: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyPolicy {
    pub require_tests: bool,
    pub require_lint: bool,
    pub require_typecheck: bool,
    pub allow_warnings: bool,
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        Self { require_tests: false, require_lint: false, require_typecheck: false, allow_warnings: true }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyConfig {
    pub enabled: bool,
    pub commands: Vec<String>,
    pub timeout_ms: u64,
    pub max_repair_rounds: u64,
    pub policy: VerifyPolicy,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self { enabled: true, commands: vec![], timeout_ms: 120_000, max_repair_rounds: 2, policy: VerifyPolicy::default() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub provider: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub key_env: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub timeout_ms: u64,
    pub run_token_budget: u64,
    pub max_cost_usd: f64,
    pub max_time_ms: u64,
    pub pricing: Option<Value>,
    pub inline_reasoning: String,
    pub show_reasoning: bool,
    pub approval_mode: Option<String>,
    pub allow_net: bool,
    pub allow_net_hosts: Vec<String>,
    pub view_mode: Option<String>,
    pub verify: VerifyConfig,
    pub event_log: bool,
    pub stub: bool,
    pub stub_fixture: Option<String>,
    pub memory_enabled: bool,
    pub rag_enabled: bool,
    pub rag_top_k: u64,
    pub rag_capacity: u64,
    pub rag_threshold: f64,
    pub rag_index_max_files: u64,
    pub rag_index_max_file_bytes: u64,
    pub context_reserve_ratio: f64,
    pub context_conversation_ratio: f64,
    pub context_files_ratio: f64,
    pub multi_agent_mode: String,
    pub multi_agent_rework_cap: u64,
    pub multi_agent_max_concurrency: u64,
    pub multi_agent_mcp_tools: Vec<String>,
    pub embedder_provider: String,
    pub embedder_model: Option<String>,
    pub embedder_base_url: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        parse_config(&json!({})).expect("defaults are valid")
    }
}

/// Configuration resolved for a run (workspace, credential and data directory attached).
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub config: Config,
    pub cwd: PathBuf,
    pub api_key: String,
    pub data_dir: PathBuf,
    pub base_url: String,
    pub model: String,
    pub stub_cursor: Option<Arc<Mutex<usize>>>,
}

impl std::ops::Deref for RuntimeConfig {
    type Target = Config;
    fn deref(&self) -> &Config {
        &self.config
    }
}

impl std::ops::DerefMut for RuntimeConfig {
    fn deref_mut(&mut self) -> &mut Config {
        &mut self.config
    }
}

// ---------------------------------------------------------------------------------------------
// Schema validation (mirrors the zod schema: types, ranges, enums, defaults, strict keys)
// ---------------------------------------------------------------------------------------------

struct Issues(Vec<String>);

impl Issues {
    fn push(&mut self, path: &str, message: impl Into<String>) {
        self.0.push(format!("{path}: {}", message.into()));
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.as_f64().map(f64::is_nan).unwrap_or(false) => "nan",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn get_enum(map: &Map<String, Value>, key: &str, allowed: &[&str], default: Option<&str>, issues: &mut Issues) -> Option<String> {
    match map.get(key) {
        None => default.map(str::to_string),
        Some(Value::String(s)) if allowed.contains(&s.as_str()) => Some(s.clone()),
        Some(other) => {
            issues.push(key, format!("Invalid option: expected one of {} (received {})", allowed.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join("|"), jsutil::stringify(other)));
            default.map(str::to_string)
        }
    }
}

fn get_bool(map: &Map<String, Value>, key: &str, default: bool, issues: &mut Issues) -> bool {
    match map.get(key) {
        None => default,
        Some(Value::Bool(b)) => *b,
        Some(other) => {
            issues.push(key, format!("Invalid input: expected boolean, received {}", type_name(other)));
            default
        }
    }
}

fn get_number(map: &Map<String, Value>, key: &str, int: bool, min: Option<f64>, max: Option<f64>, issues: &mut Issues) -> Option<f64> {
    let value = map.get(key)?;
    let Some(n) = value.as_f64() else {
        issues.push(key, format!("Invalid input: expected number, received {}", type_name(value)));
        return None;
    };
    if !n.is_finite() {
        issues.push(key, "Invalid input: expected number, received nan");
        return None;
    }
    if int && n.fract() != 0.0 {
        issues.push(key, "Invalid input: expected int, received number");
        return None;
    }
    if let Some(min) = min {
        if n < min {
            issues.push(key, format!("Too small: expected number to be >={}", jsutil::stringify(&num(min))));
            return None;
        }
    }
    if let Some(max) = max {
        if n > max {
            issues.push(key, format!("Too big: expected number to be <={}", jsutil::stringify(&num(max))));
            return None;
        }
    }
    Some(n)
}

fn get_string(map: &Map<String, Value>, key: &str, issues: &mut Issues) -> Option<String> {
    match map.get(key) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            issues.push(key, format!("Invalid input: expected string, received {}", type_name(other)));
            None
        }
    }
}

fn get_string_array(map: &Map<String, Value>, key: &str, issues: &mut Issues) -> Vec<String> {
    match map.get(key) {
        None => vec![],
        Some(Value::Array(items)) => {
            let mut out = vec![];
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    other => issues.push(&format!("{key}.{i}"), format!("Invalid input: expected string, received {}", type_name(other))),
                }
            }
            out
        }
        Some(other) => {
            issues.push(key, format!("Invalid input: expected array, received {}", type_name(other)));
            vec![]
        }
    }
}

fn is_valid_url(text: &str) -> bool {
    url::Url::parse(text).is_ok()
}

fn validate_price(path: &str, value: &Value, issues: &mut Issues) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    let allowed = ["inputPer1M", "outputPer1M", "cachedInputPer1M", "cacheWritePer1M"];
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            issues.push(path, format!("Unrecognized key: \"{key}\""));
            return false;
        }
    }
    for key in ["inputPer1M", "outputPer1M"] {
        match map.get(key).and_then(Value::as_f64) {
            Some(n) if n >= 0.0 => {}
            _ => return false,
        }
    }
    for key in ["cachedInputPer1M", "cacheWritePer1M"] {
        if let Some(v) = map.get(key) {
            match v.as_f64() {
                Some(n) if n >= 0.0 => {}
                _ => return false,
            }
        }
    }
    true
}

fn validate_pricing(value: &Value, issues: &mut Issues) -> bool {
    let Value::Object(map) = value else {
        issues.push("pricing", format!("Invalid input: expected record, received {}", type_name(value)));
        return false;
    };
    for (key, entry) in map {
        let mut scratch = Issues(vec![]);
        if validate_price(&format!("pricing.{key}"), entry, &mut scratch) {
            continue;
        }
        // Nested `{ "models": { "model": price } }` shape.
        let nested_ok = match entry {
            Value::Object(inner) => inner.iter().all(|(k, v)| validate_price(&format!("pricing.{key}.{k}"), v, &mut Issues(vec![]))),
            _ => false,
        };
        if !nested_ok {
            issues.push(&format!("pricing.{key}"), "Invalid input");
            return false;
        }
    }
    true
}

fn parse_verify(value: Option<&Value>, issues: &mut Issues) -> VerifyConfig {
    let Some(value) = value else {
        return VerifyConfig::default();
    };
    let Value::Object(map) = value else {
        issues.push("verify", format!("Invalid input: expected object, received {}", type_name(value)));
        return VerifyConfig::default();
    };
    for key in map.keys() {
        if !["enabled", "commands", "timeoutMs", "maxRepairRounds", "policy"].contains(&key.as_str()) {
            issues.push("verify", format!("Unrecognized key: \"{key}\""));
        }
    }
    let mut sub = Issues(vec![]);
    let enabled = get_bool(map, "enabled", true, &mut sub);
    let commands = get_string_array(map, "commands", &mut sub);
    let timeout_ms = get_number(map, "timeoutMs", true, Some(1000.0), Some(600_000.0), &mut sub).unwrap_or(120_000.0) as u64;
    let max_repair_rounds = get_number(map, "maxRepairRounds", true, Some(0.0), Some(10.0), &mut sub).unwrap_or(2.0) as u64;
    let policy = match map.get("policy") {
        None => VerifyPolicy::default(),
        Some(Value::Object(p)) => {
            for key in p.keys() {
                if !["requireTests", "requireLint", "requireTypecheck", "allowWarnings"].contains(&key.as_str()) {
                    sub.push("policy", format!("Unrecognized key: \"{key}\""));
                }
            }
            VerifyPolicy {
                require_tests: get_bool(p, "requireTests", false, &mut sub),
                require_lint: get_bool(p, "requireLint", false, &mut sub),
                require_typecheck: get_bool(p, "requireTypecheck", false, &mut sub),
                allow_warnings: get_bool(p, "allowWarnings", true, &mut sub),
            }
        }
        Some(other) => {
            sub.push("policy", format!("Invalid input: expected object, received {}", type_name(other)));
            VerifyPolicy::default()
        }
    };
    for issue in sub.0 {
        issues.0.push(format!("verify.{issue}"));
    }
    VerifyConfig { enabled, commands, timeout_ms, max_repair_rounds, policy }
}

const CONFIG_KEYS: &[&str] = &[
    "provider", "baseUrl", "model", "effort", "keyEnv", "contextWindow", "maxOutputTokens", "timeoutMs",
    "runTokenBudget", "maxCostUsd", "maxTimeMs", "pricing", "inlineReasoning", "showReasoning", "approvalMode",
    "allowNet", "allowNetHosts", "viewMode", "verify", "eventLog", "stub", "stubFixture", "memoryEnabled",
    "ragEnabled", "ragTopK", "ragCapacity", "ragThreshold", "ragIndexMaxFiles", "ragIndexMaxFileBytes",
    "contextReserveRatio", "contextConversationRatio", "contextFilesRatio", "multiAgentMode",
    "multiAgentReworkCap", "multiAgentMaxConcurrency", "multiAgentMcpTools", "embedderProvider",
    "embedderModel", "embedderBaseUrl",
];

/// `configSchema.parse(value)`.
pub fn parse_config(value: &Value) -> Result<Config> {
    let empty = Map::new();
    let map = match value {
        Value::Object(map) => map,
        Value::Null => &empty,
        other => bail!("Invalid config: expected object, received {}", type_name(other)),
    };
    let mut issues = Issues(vec![]);
    let unknown: Vec<&String> = map.keys().filter(|k| !CONFIG_KEYS.contains(&k.as_str())).collect();
    if !unknown.is_empty() {
        issues.push("config", format!("Unrecognized key(s) in object: {}", unknown.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ")));
    }
    let provider = get_enum(map, "provider", &["openai-compatible", "openai-responses", "anthropic"], Some("openai-compatible"), &mut issues).unwrap();
    let base_url = get_string(map, "baseUrl", &mut issues);
    if let Some(url) = &base_url {
        if !is_valid_url(url) {
            issues.push("baseUrl", "Invalid URL");
        }
    }
    let model = match get_string(map, "model", &mut issues) {
        Some(m) => {
            let t = m.trim().to_string();
            if t.is_empty() {
                issues.push("model", "Too small: expected string to have >=1 characters");
                None
            } else {
                Some(t)
            }
        }
        None => None,
    };
    let effort = get_enum(map, "effort", &EFFORT_LEVELS, None, &mut issues);
    let key_env = get_string(map, "keyEnv", &mut issues).unwrap_or_else(|| "GOON_API_KEY".into());
    if !regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap().is_match(&key_env) {
        issues.push("keyEnv", "Invalid string: must match pattern /^[A-Za-z_][A-Za-z0-9_]*$/");
    }
    let context_window = get_number(map, "contextWindow", true, Some(8192.0), Some(2_000_000.0), &mut issues).unwrap_or(128_000.0) as u64;
    let max_output_tokens = get_number(map, "maxOutputTokens", true, Some(128.0), Some(131_072.0), &mut issues).unwrap_or(16_384.0) as u64;
    let timeout_ms = get_number(map, "timeoutMs", true, Some(1000.0), Some(1_800_000.0), &mut issues).unwrap_or(300_000.0) as u64;
    let run_token_budget = get_number(map, "runTokenBudget", true, Some(0.0), None, &mut issues).unwrap_or(0.0) as u64;
    let max_cost_usd = get_number(map, "maxCostUsd", false, Some(0.0), None, &mut issues).unwrap_or(0.0);
    let max_time_ms = get_number(map, "maxTimeMs", true, Some(0.0), None, &mut issues).unwrap_or(0.0) as u64;
    let pricing = match map.get("pricing") {
        None => None,
        Some(p) => {
            if validate_pricing(p, &mut issues) {
                Some(p.clone())
            } else {
                None
            }
        }
    };
    let inline_reasoning = get_enum(map, "inlineReasoning", &["leading-think", "off"], Some("leading-think"), &mut issues).unwrap();
    let show_reasoning = get_bool(map, "showReasoning", false, &mut issues);
    let approval_mode = get_enum(map, "approvalMode", &["ask", "auto", "plan", "readonly"], None, &mut issues);
    let allow_net = get_bool(map, "allowNet", false, &mut issues);
    let allow_net_hosts = get_string_array(map, "allowNetHosts", &mut issues);
    let view_mode = get_enum(map, "viewMode", &["normal", "verbose", "transcript"], None, &mut issues);
    let verify = parse_verify(map.get("verify"), &mut issues);
    let event_log = get_bool(map, "eventLog", false, &mut issues);
    let stub = get_bool(map, "stub", false, &mut issues);
    let stub_fixture = get_string(map, "stubFixture", &mut issues);
    let memory_enabled = get_bool(map, "memoryEnabled", false, &mut issues);
    let rag_enabled = get_bool(map, "ragEnabled", false, &mut issues);
    let rag_top_k = get_number(map, "ragTopK", true, Some(1.0), Some(100.0), &mut issues).unwrap_or(10.0) as u64;
    let rag_capacity = get_number(map, "ragCapacity", true, Some(10.0), Some(100_000.0), &mut issues).unwrap_or(5000.0) as u64;
    let rag_threshold = get_number(map, "ragThreshold", false, Some(0.0), Some(1.0), &mut issues).unwrap_or(0.0);
    let rag_index_max_files = get_number(map, "ragIndexMaxFiles", true, Some(1.0), Some(50_000.0), &mut issues).unwrap_or(2000.0) as u64;
    let rag_index_max_file_bytes = get_number(map, "ragIndexMaxFileBytes", true, Some(1024.0), Some(10_485_760.0), &mut issues).unwrap_or(1_048_576.0) as u64;
    let context_reserve_ratio = get_number(map, "contextReserveRatio", false, Some(0.0), Some(0.5), &mut issues).unwrap_or(0.15);
    let context_conversation_ratio = get_number(map, "contextConversationRatio", false, Some(0.0), Some(1.0), &mut issues).unwrap_or(0.45);
    let context_files_ratio = get_number(map, "contextFilesRatio", false, Some(0.0), Some(1.0), &mut issues).unwrap_or(0.25);
    let multi_agent_mode = get_enum(map, "multiAgentMode", &["off", "manual", "auto"], Some("manual"), &mut issues).unwrap();
    let multi_agent_rework_cap = get_number(map, "multiAgentReworkCap", true, Some(1.0), Some(10.0), &mut issues).unwrap_or(3.0) as u64;
    let multi_agent_max_concurrency = get_number(map, "multiAgentMaxConcurrency", true, Some(1.0), Some(8.0), &mut issues).unwrap_or(2.0) as u64;
    let multi_agent_mcp_tools = get_string_array(map, "multiAgentMcpTools", &mut issues);
    let embedder_provider = get_enum(map, "embedderProvider", &["hash", "openai", "ollama"], Some("hash"), &mut issues).unwrap();
    let embedder_model = match get_string(map, "embedderModel", &mut issues) {
        Some(m) => {
            let t = m.trim().to_string();
            if t.is_empty() || jsutil::char_len(&t) > 200 {
                issues.push("embedderModel", "Invalid length: expected 1-200 characters");
                None
            } else {
                Some(t)
            }
        }
        None => None,
    };
    let embedder_base_url = get_string(map, "embedderBaseUrl", &mut issues);
    if let Some(url) = &embedder_base_url {
        if !is_valid_url(url) {
            issues.push("embedderBaseUrl", "Invalid URL");
        }
    }
    if !issues.0.is_empty() {
        bail!("Invalid config:\n  {}", issues.0.join("\n  "));
    }
    Ok(Config {
        provider, base_url, model, effort, key_env, context_window, max_output_tokens, timeout_ms,
        run_token_budget, max_cost_usd, max_time_ms, pricing, inline_reasoning, show_reasoning,
        approval_mode, allow_net, allow_net_hosts, view_mode, verify, event_log, stub, stub_fixture,
        memory_enabled, rag_enabled, rag_top_k, rag_capacity, rag_threshold, rag_index_max_files,
        rag_index_max_file_bytes, context_reserve_ratio, context_conversation_ratio, context_files_ratio,
        multi_agent_mode, multi_agent_rework_cap, multi_agent_max_concurrency, multi_agent_mcp_tools,
        embedder_provider, embedder_model, embedder_base_url,
    })
}

impl VerifyConfig {
    pub fn to_value(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "commands": self.commands,
            "timeoutMs": self.timeout_ms,
            "maxRepairRounds": self.max_repair_rounds,
            "policy": {
                "requireTests": self.policy.require_tests,
                "requireLint": self.policy.require_lint,
                "requireTypecheck": self.policy.require_typecheck,
                "allowWarnings": self.policy.allow_warnings,
            }
        })
    }
}

impl Config {
    /// Serialized in schema order; `None` fields are omitted (like `undefined` in JSON.stringify).
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("provider".into(), json!(self.provider));
        if let Some(v) = &self.base_url {
            m.insert("baseUrl".into(), json!(v));
        }
        if let Some(v) = &self.model {
            m.insert("model".into(), json!(v));
        }
        if let Some(v) = &self.effort {
            m.insert("effort".into(), json!(v));
        }
        m.insert("keyEnv".into(), json!(self.key_env));
        m.insert("contextWindow".into(), json!(self.context_window));
        m.insert("maxOutputTokens".into(), json!(self.max_output_tokens));
        m.insert("timeoutMs".into(), json!(self.timeout_ms));
        m.insert("runTokenBudget".into(), json!(self.run_token_budget));
        m.insert("maxCostUsd".into(), num(self.max_cost_usd));
        m.insert("maxTimeMs".into(), json!(self.max_time_ms));
        if let Some(v) = &self.pricing {
            m.insert("pricing".into(), v.clone());
        }
        m.insert("inlineReasoning".into(), json!(self.inline_reasoning));
        m.insert("showReasoning".into(), json!(self.show_reasoning));
        if let Some(v) = &self.approval_mode {
            m.insert("approvalMode".into(), json!(v));
        }
        m.insert("allowNet".into(), json!(self.allow_net));
        m.insert("allowNetHosts".into(), json!(self.allow_net_hosts));
        if let Some(v) = &self.view_mode {
            m.insert("viewMode".into(), json!(v));
        }
        m.insert("verify".into(), self.verify.to_value());
        m.insert("eventLog".into(), json!(self.event_log));
        m.insert("stub".into(), json!(self.stub));
        if let Some(v) = &self.stub_fixture {
            m.insert("stubFixture".into(), json!(v));
        }
        m.insert("memoryEnabled".into(), json!(self.memory_enabled));
        m.insert("ragEnabled".into(), json!(self.rag_enabled));
        m.insert("ragTopK".into(), json!(self.rag_top_k));
        m.insert("ragCapacity".into(), json!(self.rag_capacity));
        m.insert("ragThreshold".into(), num(self.rag_threshold));
        m.insert("ragIndexMaxFiles".into(), json!(self.rag_index_max_files));
        m.insert("ragIndexMaxFileBytes".into(), json!(self.rag_index_max_file_bytes));
        m.insert("contextReserveRatio".into(), num(self.context_reserve_ratio));
        m.insert("contextConversationRatio".into(), num(self.context_conversation_ratio));
        m.insert("contextFilesRatio".into(), num(self.context_files_ratio));
        m.insert("multiAgentMode".into(), json!(self.multi_agent_mode));
        m.insert("multiAgentReworkCap".into(), json!(self.multi_agent_rework_cap));
        m.insert("multiAgentMaxConcurrency".into(), json!(self.multi_agent_max_concurrency));
        m.insert("multiAgentMcpTools".into(), json!(self.multi_agent_mcp_tools));
        m.insert("embedderProvider".into(), json!(self.embedder_provider));
        if let Some(v) = &self.embedder_model {
            m.insert("embedderModel".into(), json!(v));
        }
        if let Some(v) = &self.embedder_base_url {
            m.insert("embedderBaseUrl".into(), json!(v));
        }
        Value::Object(m)
    }

    pub fn price_table(&self) -> Option<&Value> {
        self.pricing.as_ref()
    }
}

// ---------------------------------------------------------------------------------------------
// Paths, credentials, atomic writes
// ---------------------------------------------------------------------------------------------

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub fn config_dir() -> PathBuf {
    let base = env_nonempty("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| jsutil::home_dir().join(".config"));
    base.join("goon")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

pub fn data_dir() -> PathBuf {
    let base = env_nonempty("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| jsutil::home_dir().join(".local").join("share"));
    base.join("goon")
}

/// Creates a directory tree with mode 0700 on POSIX (`mkdir(..., { recursive: true, mode: 0o700 })`).
pub fn mkdir_private(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Writes `value` to `file` through an exclusive temporary file and an atomic rename (mode 0600).
pub fn atomic_json(file: &Path, value: &Value, compact: bool) -> Result<()> {
    let body = if compact { jsutil::stringify(value) } else { jsutil::stringify_pretty(value) };
    atomic_write(file, format!("{body}\n").as_bytes())
}

pub fn atomic_write(file: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let temporary = file.with_file_name(format!("{name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut handle = options.open(&temporary)?;
        handle.write_all(bytes)?;
        handle.flush()?;
        drop(handle);
        std::fs::rename(&temporary, file)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(anyhow!(error));
    }
    Ok(())
}

pub fn load_credentials() -> Map<String, Value> {
    match std::fs::read_to_string(credentials_path()) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) if map.values().all(Value::is_string) => map,
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    }
}

pub fn save_credential(key_env: &str, api_key: &str) -> Result<()> {
    mkdir_private(&config_dir())?;
    let mut existing = load_credentials();
    let trimmed = api_key.trim();
    if !trimmed.is_empty() {
        existing.insert(key_env.to_string(), Value::String(trimmed.to_string()));
    }
    atomic_json(&credentials_path(), &Value::Object(existing), false)
}

pub fn resolve_api_key(config: &Config) -> Option<String> {
    crate::process::protect_secret_env(&config.key_env);
    if let Ok(v) = std::env::var(&config.key_env) {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let stored = load_credentials();
    stored.get(&config.key_env).and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn normalize_gateway_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/').to_string();
    if let Ok(parsed) = url::Url::parse(&trimmed) {
        let host = parsed.host_str().unwrap_or("");
        let host_port = match parsed.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        };
        let known = host == "api.yuristore.xyz" || host_port == "127.0.0.1:8000" || host_port == "localhost:8000";
        if (parsed.path() == "/" || parsed.path().is_empty()) && known {
            return format!("{trimmed}/v1");
        }
    }
    trimmed
}

fn bool_env(name: &str) -> Result<Option<bool>> {
    let Ok(raw) = std::env::var(name) else {
        return Ok(None);
    };
    let value = raw.trim().to_lowercase();
    if ["1", "true", "yes", "on"].contains(&value.as_str()) {
        return Ok(Some(true));
    }
    if ["0", "false", "no", "off", ""].contains(&value.as_str()) {
        return Ok(Some(false));
    }
    bail!("{name} must be a boolean (true/false).")
}

/// JS `Number(text)`: trims, accepts decimal/hex/exponent; empty string is 0; invalid is NaN.
pub fn js_number(text: &str) -> f64 {
    let t = text.trim();
    if t.is_empty() {
        return 0.0;
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).map(|v| v as f64).unwrap_or(f64::NAN);
    }
    if t == "Infinity" || t == "+Infinity" {
        return f64::INFINITY;
    }
    if t == "-Infinity" {
        return f64::NEG_INFINITY;
    }
    if t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
        return t.parse::<f64>().unwrap_or(f64::NAN);
    }
    f64::NAN
}

fn number_value(value: f64) -> Value {
    if value.is_finite() {
        num(value)
    } else {
        // zod rejects NaN as "received nan"; carry it as a string marker so the issue names the key.
        Value::String("NaN".into())
    }
}

/// `loadConfig(overrides)`: stored config, then env, then CLI overrides, validated again.
pub fn load_config(overrides: &Map<String, Value>) -> Result<Config> {
    let stored: Value = match std::fs::read_to_string(config_path()) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| anyhow!("{}", json_error_message(&e, &text)))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let validated = parse_config(&stored)?;
    let mut env = Map::new();
    let env_keys: [(&str, &str); 15] = [
        ("provider", "GOON_PROVIDER"), ("baseUrl", "GOON_BASE_URL"), ("model", "GOON_MODEL"), ("effort", "GOON_EFFORT"),
        ("keyEnv", "GOON_KEY_ENV"), ("contextWindow", "GOON_CONTEXT_WINDOW"), ("maxOutputTokens", "GOON_MAX_OUTPUT_TOKENS"),
        ("timeoutMs", "GOON_TIMEOUT_MS"), ("runTokenBudget", "GOON_RUN_TOKEN_BUDGET"), ("maxCostUsd", "GOON_MAX_COST_USD"),
        ("maxTimeMs", "GOON_MAX_TIME_MS"), ("inlineReasoning", "GOON_INLINE_REASONING"),
        ("embedderProvider", "GOON_EMBEDDER_PROVIDER"), ("embedderModel", "GOON_EMBEDDER_MODEL"),
        ("embedderBaseUrl", "GOON_EMBEDDER_BASE_URL"),
    ];
    let numeric = ["contextWindow", "maxOutputTokens", "timeoutMs", "runTokenBudget", "maxCostUsd", "maxTimeMs"];
    for (key, name) in env_keys {
        if let Ok(raw) = std::env::var(name) {
            let value = if numeric.contains(&key) { number_value(js_number(&raw)) } else { Value::String(raw) };
            env.insert(key.into(), value);
        }
    }
    if let Ok(raw) = std::env::var("GOON_PRICING_JSON") {
        let parsed: Value = serde_json::from_str(&raw).map_err(|_| anyhow!("GOON_PRICING_JSON is not valid JSON."))?;
        env.insert("pricing".into(), parsed);
    }
    let mut env_verify = Map::new();
    if let Some(enabled) = bool_env("GOON_VERIFY_ENABLED")? {
        env_verify.insert("enabled".into(), json!(enabled));
    }
    if let Ok(raw) = std::env::var("GOON_VERIFY_TIMEOUT_MS") {
        let value = js_number(&raw);
        if !value.is_finite() {
            bail!("GOON_VERIFY_TIMEOUT_MS must be a number.");
        }
        env_verify.insert("timeoutMs".into(), num(value));
    }
    if let Ok(raw) = std::env::var("GOON_VERIFY_MAX_REPAIR_ROUNDS") {
        let value = js_number(&raw);
        if !value.is_finite() {
            bail!("GOON_VERIFY_MAX_REPAIR_ROUNDS must be a number.");
        }
        env_verify.insert("maxRepairRounds".into(), num(value));
    }
    if let Ok(raw) = std::env::var("GOON_VERIFY_CMDS") {
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            env_verify.insert("commands".into(), json!([]));
        } else if raw.starts_with('[') {
            match serde_json::from_str::<Value>(&raw) {
                Ok(Value::Array(items)) if items.iter().all(Value::is_string) => {
                    env_verify.insert("commands".into(), Value::Array(items));
                }
                _ => bail!("GOON_VERIFY_CMDS must be a JSON array of strings or a comma-separated list."),
            }
        } else {
            let list: Vec<Value> = raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| json!(s)).collect();
            env_verify.insert("commands".into(), Value::Array(list));
        }
    }
    let mut policy_env = Map::new();
    for (key, name) in [
        ("requireTests", "GOON_VERIFY_REQUIRE_TESTS"),
        ("requireLint", "GOON_VERIFY_REQUIRE_LINT"),
        ("requireTypecheck", "GOON_VERIFY_REQUIRE_TYPECHECK"),
        ("allowWarnings", "GOON_VERIFY_ALLOW_WARNINGS"),
    ] {
        if let Some(value) = bool_env(name)? {
            policy_env.insert(key.into(), json!(value));
        }
    }
    if !policy_env.is_empty() {
        env_verify.insert("policy".into(), Value::Object(policy_env.clone()));
    }
    if let Some(v) = bool_env("GOON_ALLOW_NET")? {
        env.insert("allowNet".into(), json!(v));
    }
    if let Ok(raw) = std::env::var("GOON_ALLOW_NET_HOSTS") {
        let list: Vec<Value> = raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| json!(s)).collect();
        env.insert("allowNetHosts".into(), Value::Array(list));
    }
    if let Some(v) = bool_env("GOON_EVENT_LOG")? {
        env.insert("eventLog".into(), json!(v));
    }
    if let Some(v) = bool_env("GOON_STUB")? {
        env.insert("stub".into(), json!(v));
    }
    if let Ok(raw) = std::env::var("GOON_STUB_FIXTURE") {
        env.insert("stubFixture".into(), json!(raw));
    }
    if let Some(v) = bool_env("GOON_MEMORY_ENABLED")? {
        env.insert("memoryEnabled".into(), json!(v));
    }
    if let Some(v) = bool_env("GOON_RAG_ENABLED")? {
        env.insert("ragEnabled".into(), json!(v));
    }
    for (key, name) in [("ragTopK", "GOON_RAG_TOP_K"), ("ragCapacity", "GOON_RAG_CAPACITY")] {
        if let Ok(raw) = std::env::var(name) {
            let value = js_number(&raw);
            if !value.is_finite() || value.fract() != 0.0 {
                bail!("{name} must be an integer.");
            }
            env.insert(key.into(), num(value));
        }
    }
    let Value::Object(mut merged) = validated.to_value() else { unreachable!() };
    let stored_verify = merged.get("verify").cloned().unwrap_or(json!({}));
    for (k, v) in env.iter() {
        merged.insert(k.clone(), v.clone());
    }
    for (k, v) in overrides.iter() {
        // `undefined` in a TS override object (`{ ...config, effort: undefined }`) clears the key.
        if v.is_null() {
            merged.remove(k);
        } else {
            merged.insert(k.clone(), v.clone());
        }
    }
    // `verify` is nested: merge per key so one env var or flag never drops the other fields.
    let override_verify = overrides.get("verify").and_then(Value::as_object).cloned();
    let mut verify = stored_verify.as_object().cloned().unwrap_or_default();
    let stored_policy = verify.get("policy").and_then(Value::as_object).cloned().unwrap_or_default();
    for (k, v) in env_verify.iter() {
        verify.insert(k.clone(), v.clone());
    }
    if let Some(ov) = &override_verify {
        for (k, v) in ov {
            verify.insert(k.clone(), v.clone());
        }
    }
    let mut policy = stored_policy;
    if let Some(Value::Object(p)) = env_verify.get("policy") {
        for (k, v) in p {
            policy.insert(k.clone(), v.clone());
        }
    }
    if let Some(Value::Object(p)) = override_verify.as_ref().and_then(|o| o.get("policy")) {
        for (k, v) in p {
            policy.insert(k.clone(), v.clone());
        }
    }
    verify.insert("policy".into(), Value::Object(policy));
    merged.insert("verify".into(), Value::Object(verify));
    if !env.contains_key("runTokenBudget") && !overrides.contains_key("runTokenBudget") {
        merged.insert("runTokenBudget".into(), json!(0));
    }
    let explicit_max_out = stored.as_object().map(|m| m.contains_key("maxOutputTokens")).unwrap_or(false)
        || env.contains_key("maxOutputTokens")
        || overrides.contains_key("maxOutputTokens");
    if !explicit_max_out {
        let cw = merged.get("contextWindow").and_then(Value::as_f64).unwrap_or(0.0);
        let mo = merged.get("maxOutputTokens").and_then(Value::as_f64).unwrap_or(0.0);
        if cw > 0.0 && mo >= cw / 2.0 {
            merged.insert("maxOutputTokens".into(), json!(128f64.max((cw / 4.0).floor()) as u64));
        }
    }
    let mut config = parse_config(&Value::Object(merged))?;
    if (config.max_output_tokens as f64) >= config.context_window as f64 / 2.0 {
        bail!("maxOutputTokens must be less than half contextWindow.");
    }
    if let Some(base) = &config.base_url {
        let url = url::Url::parse(base).map_err(|_| anyhow!("Invalid URL"))?;
        if !["http", "https"].contains(&url.scheme()) || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            bail!("baseUrl must be HTTP(S), without credentials, query or fragment.");
        }
        config.base_url = Some(normalize_gateway_url(base));
    }
    Ok(config)
}

/// Node-style message for an invalid JSON document.
pub fn json_error_message(error: &serde_json::Error, _text: &str) -> String {
    format!("Unexpected token in JSON at line {} column {}: {}", error.line(), error.column(), error)
}

pub fn save_config(config: &Config) -> Result<()> {
    mkdir_private(&config_dir())?;
    let value = parse_config(&config.to_value())?.to_value();
    atomic_json(&config_path(), &value, false)
}

static STUB_CURSORS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, Arc<Mutex<usize>>>>> = std::sync::OnceLock::new();

/// Canonical (symlink-free) absolute path, like `realpath(resolve(cwd))`.
pub fn realpath(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    let canonical = std::fs::canonicalize(&absolute).map_err(|e| anyhow!("{}: {}", e, absolute.display()))?;
    Ok(strip_verbatim(canonical))
}

/// Windows `canonicalize` returns `\\?\C:\...`; Node's realpath returns `C:\...`.
pub fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().to_string();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

pub fn resolve_runtime(config: &Config, cwd: &Path) -> Result<RuntimeConfig> {
    crate::process::protect_secret_env(&config.key_env);
    let resolved_cwd = realpath(cwd)?;
    if config.stub {
        let key_path = resolved_cwd.to_string_lossy().to_string();
        let key = format!(
            "{}\0{}",
            if cfg!(windows) { key_path.to_lowercase() } else { key_path },
            config.stub_fixture.clone().unwrap_or_default()
        );
        let cursors = STUB_CURSORS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
        let cursor = cursors.lock().unwrap().entry(key).or_insert_with(|| Arc::new(Mutex::new(0))).clone();
        return Ok(RuntimeConfig {
            model: config.model.clone().unwrap_or_else(|| "stub-model".into()),
            config: config.clone(),
            cwd: resolved_cwd,
            api_key: String::new(),
            data_dir: data_dir(),
            base_url: "stub://local".into(),
            stub_cursor: Some(cursor),
        });
    }
    let Some(model) = config.model.clone() else {
        bail!("Missing model. Run \"goon setup\" or \"goon init --model MODEL --base-url URL\".");
    };
    let Some(api_key) = resolve_api_key(config) else {
        bail!("Missing {}. Run \"goon setup\" or set {} in your shell.", config.key_env, config.key_env);
    };
    let base_url = config.base_url.as_deref().map(normalize_gateway_url).unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string());
    Ok(RuntimeConfig { config: config.clone(), model, base_url, api_key, cwd: resolved_cwd, data_dir: data_dir(), stub_cursor: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let config = parse_config(&json!({})).unwrap();
        assert_eq!(config.provider, "openai-compatible");
        assert_eq!(config.context_window, 128_000);
        let value = config.to_value();
        let text = jsutil::stringify(&value);
        assert!(text.starts_with(r#"{"provider":"openai-compatible","keyEnv":"GOON_API_KEY","contextWindow":128000"#));
        assert!(text.contains(r#""maxCostUsd":0,"#));
        assert_eq!(parse_config(&value).unwrap(), config);
    }

    #[test]
    fn strict_and_ranges() {
        assert!(parse_config(&json!({"bogus": 1})).unwrap_err().to_string().contains("bogus"));
        assert!(parse_config(&json!({"multiAgentMode": "x"})).unwrap_err().to_string().contains("multiAgentMode"));
        assert!(parse_config(&json!({"contextWindow": 10})).is_err());
        assert!(parse_config(&json!({"pricing": {"m": {"inputPer1M": 1, "outputPer1M": 2}}})).is_ok());
        assert!(parse_config(&json!({"pricing": {"models": {"m": {"inputPer1M": 1, "outputPer1M": 2}}}})).is_ok());
        assert!(parse_config(&json!({"pricing": {"m": {"inputPer1M": -1, "outputPer1M": 2}}})).is_err());
    }

    #[test]
    fn gateway_url() {
        assert_eq!(normalize_gateway_url("https://api.yuristore.xyz/"), "https://api.yuristore.xyz/v1");
        assert_eq!(normalize_gateway_url("http://127.0.0.1:8000"), "http://127.0.0.1:8000/v1");
        assert_eq!(normalize_gateway_url("https://x.test/v1/"), "https://x.test/v1");
    }

    #[test]
    fn js_number_parsing() {
        assert_eq!(js_number(" 42 "), 42.0);
        assert!(js_number("abc").is_nan());
        assert_eq!(js_number(""), 0.0);
    }
}
