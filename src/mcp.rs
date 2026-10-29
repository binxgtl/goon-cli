//! MCP configuration (`~/.config/goon/mcp.json`), Codex import, schema sanitizing and the client
//! manager (stdio and streamable HTTP transports).

use crate::config::{atomic_json, config_dir, mkdir_private};
use crate::jsutil;
use anyhow::{anyhow, bail, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use crate::mcp_client::{McpManager, McpServerStatus, McpToolInfo};

pub const DEFAULT_MCP_MAX_BUFFER_SIZE: u64 = 32 * 1024 * 1024;
pub const DEFAULT_MAX_MCP_TOOLS: usize = 1024;
pub const DEFAULT_SCHEMA_MAX_BYTES: usize = 16_000;
pub const COMPACT_BULK_SCHEMA_MAX_BYTES: usize = 6_000;

#[derive(Clone, Debug, PartialEq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        cwd: Option<String>,
        env: Map<String, Value>,
        env_vars: Vec<String>,
        env_from: Map<String, Value>,
    },
    Http {
        url: String,
        bearer_env: Option<String>,
        headers: Map<String, Value>,
        env_headers: Map<String, Value>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpServerConfig {
    pub disabled: bool,
    pub required: bool,
    pub trust: bool,
    pub net: Option<bool>,
    pub allow_net: bool,
    pub read_only_tools: Vec<String>,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Option<Vec<String>>,
    pub timeout_ms: u64,
    pub tool_timeout_ms: Option<u64>,
    pub max_buffer_size: u64,
    pub max_tools: usize,
    pub tool_input_schema_max_bytes: Option<usize>,
    pub transport: McpTransport,
}

impl McpServerConfig {
    pub fn transport_name(&self) -> &'static str {
        match self.transport {
            McpTransport::Stdio { .. } => "stdio",
            McpTransport::Http { .. } => "http",
        }
    }

    /// Serialized like the zod output (common fields first, then transport fields).
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("disabled".into(), json!(self.disabled));
        m.insert("required".into(), json!(self.required));
        m.insert("trust".into(), json!(self.trust));
        m.insert("net".into(), json!(self.net.unwrap_or(false)));
        m.insert("allowNet".into(), json!(self.allow_net));
        m.insert("readOnlyTools".into(), json!(self.read_only_tools));
        if let Some(v) = &self.enabled_tools {
            m.insert("enabledTools".into(), json!(v));
        }
        if let Some(v) = &self.disabled_tools {
            m.insert("disabledTools".into(), json!(v));
        }
        m.insert("timeoutMs".into(), json!(self.timeout_ms));
        if let Some(v) = self.tool_timeout_ms {
            m.insert("toolTimeoutMs".into(), json!(v));
        }
        m.insert("maxBufferSize".into(), json!(self.max_buffer_size));
        m.insert("maxTools".into(), json!(self.max_tools));
        if let Some(v) = self.tool_input_schema_max_bytes {
            m.insert("toolInputSchemaMaxBytes".into(), json!(v));
        }
        match &self.transport {
            McpTransport::Stdio { command, args, cwd, env, env_vars, env_from } => {
                m.insert("transport".into(), json!("stdio"));
                m.insert("command".into(), json!(command));
                m.insert("args".into(), json!(args));
                if let Some(c) = cwd {
                    m.insert("cwd".into(), json!(c));
                }
                m.insert("env".into(), Value::Object(env.clone()));
                m.insert("envVars".into(), json!(env_vars));
                m.insert("envFrom".into(), Value::Object(env_from.clone()));
            }
            McpTransport::Http { url, bearer_env, headers, env_headers } => {
                m.insert("transport".into(), json!("http"));
                m.insert("url".into(), json!(url));
                if let Some(b) = bearer_env {
                    m.insert("bearerEnv".into(), json!(b));
                }
                m.insert("headers".into(), Value::Object(headers.clone()));
                m.insert("envHeaders".into(), Value::Object(env_headers.clone()));
            }
        }
        Value::Object(m)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpConfig {
    pub servers: indexmap::IndexMap<String, McpServerConfig>,
}

impl McpConfig {
    pub fn to_value(&self) -> Value {
        let mut servers = Map::new();
        for (name, server) in &self.servers {
            servers.insert(name.clone(), server.to_value());
        }
        json!({ "servers": Value::Object(servers) })
    }
}

fn env_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap())
}

/// `${NAME}` / `${NAME:-fallback}` placeholders in env and header values.
pub fn parse_env_placeholder(value: &str) -> Option<(String, Option<String>)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-(.*))?\}$").unwrap());
    let c = re.captures(value.trim())?;
    Some((c[1].to_string(), c.get(2).map(|m| m.as_str().to_string())))
}

/// Accepts Codex/Claude/standard MCP field spellings and maps them to goon's names.
fn normalize_server_raw(raw: &Value) -> Value {
    let Some(source) = raw.as_object() else { return raw.clone() };
    let mut obj = source.clone();
    let raw_type = obj.get("transport").and_then(Value::as_str).or_else(|| obj.get("type").and_then(Value::as_str)).map(str::to_string);
    match raw_type.as_deref() {
        Some("streamable_http") | Some("streamable-http") | Some("sse") | Some("http") => {
            obj.insert("transport".into(), json!("http"));
        }
        Some("stdio") => {
            obj.insert("transport".into(), json!("stdio"));
        }
        _ => {
            if !obj.contains_key("transport") {
                if obj.get("command").map(Value::is_string).unwrap_or(false) {
                    obj.insert("transport".into(), json!("stdio"));
                } else if obj.get("url").map(Value::is_string).unwrap_or(false) {
                    obj.insert("transport".into(), json!("http"));
                }
            }
        }
    }
    obj.remove("type");
    if let Some(Value::Bool(enabled)) = obj.get("enabled").cloned() {
        if !obj.contains_key("disabled") {
            obj.insert("disabled".into(), json!(!enabled));
        }
        obj.remove("enabled");
    }
    let alias = |from: &str, to: &str, obj: &mut Map<String, Value>, pred: fn(&Value) -> bool| {
        if let Some(v) = obj.get(from).cloned() {
            if pred(&v) && !obj.contains_key(to) {
                obj.insert(to.into(), v);
            }
        }
        obj.remove(from);
    };
    alias("read_only_tools", "readOnlyTools", &mut obj, |_| true);
    alias("enabled_tools", "enabledTools", &mut obj, |_| true);
    alias("disabled_tools", "disabledTools", &mut obj, |_| true);
    alias("default_tools_approval_mode", "defaultToolsApprovalMode", &mut obj, |_| true);
    let auto = obj.get("autoApprove") == Some(&json!(true))
        || matches!(obj.get("defaultToolsApprovalMode").and_then(Value::as_str), Some("auto") | Some("approve"));
    if auto {
        obj.insert("trust".into(), json!(true));
    }
    obj.remove("autoApprove");
    obj.remove("defaultToolsApprovalMode");
    for (key, factor) in [("startup_timeout_sec", 1000.0), ("startupTimeoutSec", 1000.0), ("startup_timeout_ms", 1.0)] {
        if let Some(n) = obj.get(key).and_then(Value::as_f64) {
            if !obj.contains_key("timeoutMs") {
                obj.insert("timeoutMs".into(), json!(jsutil::js_round(n * factor) as i64));
            }
        }
    }
    if let Some(n) = obj.get("tool_timeout_sec").and_then(Value::as_f64) {
        if !obj.contains_key("toolTimeoutMs") {
            obj.insert("toolTimeoutMs".into(), json!(jsutil::js_round(n * 1000.0) as i64));
        }
    }
    for key in ["startup_timeout_sec", "startupTimeoutSec", "startup_timeout_ms", "tool_timeout_sec"] {
        obj.remove(key);
    }
    alias("tool_input_schema_max_bytes", "toolInputSchemaMaxBytes", &mut obj, Value::is_number);
    alias("max_buffer_size", "maxBufferSize", &mut obj, Value::is_number);
    alias("max_tools", "maxTools", &mut obj, Value::is_number);
    alias("env_vars", "envVars", &mut obj, Value::is_array);
    alias("bearer_token_env_var", "bearerEnv", &mut obj, Value::is_string);
    alias("http_headers", "headers", &mut obj, Value::is_object);
    alias("env_http_headers", "envHeaders", &mut obj, Value::is_object);
    Value::Object(obj)
}

fn opt_bool(m: &Map<String, Value>, key: &str, default: bool, errors: &mut Vec<String>) -> bool {
    match m.get(key) {
        None => default,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            errors.push(format!("{key}: expected boolean"));
            default
        }
    }
}

fn opt_int(m: &Map<String, Value>, key: &str, min: f64, max: f64, errors: &mut Vec<String>) -> Option<u64> {
    let v = m.get(key)?;
    match v.as_f64() {
        Some(n) if n.fract() == 0.0 && n >= min && n <= max => Some(n as u64),
        _ => {
            errors.push(format!("{key}: expected integer between {min} and {max}"));
            None
        }
    }
}

fn opt_strings(m: &Map<String, Value>, key: &str, errors: &mut Vec<String>) -> Option<Vec<String>> {
    let v = m.get(key)?;
    match v.as_array() {
        Some(items) if items.iter().all(Value::is_string) && items.len() <= 2048 => Some(items.iter().map(|i| i.as_str().unwrap().to_string()).collect()),
        _ => {
            errors.push(format!("{key}: expected an array of strings"));
            None
        }
    }
}

fn string_record(m: &Map<String, Value>, key: &str, value_is_env: bool, key_is_env: bool, errors: &mut Vec<String>) -> Map<String, Value> {
    match m.get(key) {
        None => Map::new(),
        Some(Value::Object(obj)) => {
            for (k, v) in obj {
                let Some(s) = v.as_str() else {
                    errors.push(format!("{key}.{k}: expected string"));
                    continue;
                };
                if value_is_env && !env_name_re().is_match(s) {
                    errors.push(format!("{key}.{k}: invalid environment variable name"));
                }
                if key_is_env && !env_name_re().is_match(k) {
                    errors.push(format!("{key}: invalid environment variable name {k}"));
                }
            }
            obj.clone()
        }
        Some(_) => {
            errors.push(format!("{key}: expected an object"));
            Map::new()
        }
    }
}

/// `mcpServerSchema.parse(raw)`.
pub fn parse_server(raw: &Value) -> Result<McpServerConfig> {
    let normalized = normalize_server_raw(raw);
    let Some(m) = normalized.as_object() else { bail!("Invalid MCP server: expected an object") };
    let mut errors = vec![];
    let transport_name = m.get("transport").and_then(Value::as_str).unwrap_or("").to_string();
    let common_keys = ["disabled", "required", "trust", "net", "allowNet", "readOnlyTools", "enabledTools", "disabledTools", "timeoutMs", "toolTimeoutMs", "maxBufferSize", "maxTools", "toolInputSchemaMaxBytes", "transport"];
    let specific: &[&str] = match transport_name.as_str() {
        "stdio" => &["command", "args", "cwd", "env", "envVars", "envFrom"],
        "http" => &["url", "bearerEnv", "headers", "envHeaders"],
        _ => bail!("Invalid MCP server: transport must be \"stdio\" or \"http\""),
    };
    for key in m.keys() {
        if !common_keys.contains(&key.as_str()) && !specific.contains(&key.as_str()) {
            errors.push(format!("Unrecognized key: \"{key}\""));
        }
    }
    let read_only_tools = opt_strings(m, "readOnlyTools", &mut errors).unwrap_or_default();
    let transport = if transport_name == "stdio" {
        let command = m.get("command").and_then(Value::as_str).unwrap_or("").to_string();
        if command.is_empty() {
            errors.push("command: expected a non-empty string".into());
        }
        let cwd = match m.get("cwd") {
            None => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(_) => {
                errors.push("cwd: expected a non-empty string".into());
                None
            }
        };
        let env_vars = opt_strings(m, "envVars", &mut errors).unwrap_or_default();
        for v in &env_vars {
            if !env_name_re().is_match(v) {
                errors.push(format!("envVars: invalid environment variable name {v}"));
            }
        }
        McpTransport::Stdio {
            command,
            args: opt_strings(m, "args", &mut errors).unwrap_or_default(),
            cwd,
            env: string_record(m, "env", false, false, &mut errors),
            env_vars,
            env_from: string_record(m, "envFrom", true, true, &mut errors),
        }
    } else {
        let url = m.get("url").and_then(Value::as_str).unwrap_or("").to_string();
        match url::Url::parse(&url) {
            Ok(u) if ["http", "https"].contains(&u.scheme()) && u.username().is_empty() && u.password().is_none() && u.fragment().is_none() => {}
            _ => errors.push("url: Expected HTTP(S) URL without credentials or fragment".into()),
        }
        let bearer_env = m.get("bearerEnv").and_then(Value::as_str).map(str::to_string);
        if let Some(b) = &bearer_env {
            if !env_name_re().is_match(b) {
                errors.push("bearerEnv: invalid environment variable name".into());
            }
        }
        McpTransport::Http {
            url,
            bearer_env,
            headers: string_record(m, "headers", false, false, &mut errors),
            env_headers: string_record(m, "envHeaders", true, false, &mut errors),
        }
    };
    let config = McpServerConfig {
        disabled: opt_bool(m, "disabled", false, &mut errors),
        required: opt_bool(m, "required", true, &mut errors),
        trust: opt_bool(m, "trust", false, &mut errors),
        net: Some(opt_bool(m, "net", false, &mut errors)),
        allow_net: opt_bool(m, "allowNet", false, &mut errors),
        read_only_tools,
        enabled_tools: opt_strings(m, "enabledTools", &mut errors),
        disabled_tools: opt_strings(m, "disabledTools", &mut errors),
        timeout_ms: opt_int(m, "timeoutMs", 100.0, 600_000.0, &mut errors).unwrap_or(30_000),
        tool_timeout_ms: opt_int(m, "toolTimeoutMs", 100.0, 600_000.0, &mut errors),
        max_buffer_size: opt_int(m, "maxBufferSize", 65_536.0, 128.0 * 1024.0 * 1024.0, &mut errors).unwrap_or(DEFAULT_MCP_MAX_BUFFER_SIZE),
        max_tools: opt_int(m, "maxTools", 1.0, 4096.0, &mut errors).unwrap_or(DEFAULT_MAX_MCP_TOOLS as u64) as usize,
        tool_input_schema_max_bytes: opt_int(m, "toolInputSchemaMaxBytes", 512.0, 256_000.0, &mut errors).map(|v| v as usize),
        transport,
    };
    if !errors.is_empty() {
        bail!("Invalid MCP server config: {}", errors.join("; "));
    }
    Ok(config)
}

/// `mcpConfigSchema.parse(raw)` (accepts `mcpServers` as an alias of `servers`).
pub fn parse_mcp_config(raw: &Value) -> Result<McpConfig> {
    let Some(obj) = raw.as_object() else { bail!("Invalid MCP config: expected an object") };
    let mut obj = obj.clone();
    if let Some(Value::Object(mcp_servers)) = obj.remove("mcpServers") {
        let mut merged = mcp_servers;
        if let Some(Value::Object(existing)) = obj.get("servers") {
            for (k, v) in existing {
                merged.insert(k.clone(), v.clone());
            }
        }
        obj.insert("servers".into(), Value::Object(merged));
    }
    for key in obj.keys() {
        if key != "servers" {
            bail!("Invalid MCP config: Unrecognized key: \"{key}\"");
        }
    }
    let servers = obj.get("servers").and_then(Value::as_object).cloned().ok_or_else(|| anyhow!("Invalid MCP config: servers must be an object"))?;
    if servers.len() > 64 {
        bail!("At most 64 MCP servers");
    }
    static NAME: OnceLock<Regex> = OnceLock::new();
    let name_re = NAME.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$").unwrap());
    let mut config = McpConfig::default();
    for (name, server) in servers {
        if !name_re.is_match(&name) {
            bail!("Invalid MCP server name: {name}");
        }
        let parsed = parse_server(&server).map_err(|e| anyhow!("MCP server {name}: {e}"))?;
        config.servers.insert(name, parsed);
    }
    Ok(config)
}

pub fn mcp_config_path() -> PathBuf {
    config_dir().join("mcp.json")
}

pub fn load_mcp_config() -> Result<McpConfig> {
    match std::fs::read_to_string(mcp_config_path()) {
        Ok(text) => {
            let raw: Value = crate::jsutil::parse_json(&text).map_err(|e| anyhow!("{e}"))?;
            parse_mcp_config(&raw)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(McpConfig::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save_mcp_config(config: &McpConfig) -> Result<()> {
    mkdir_private(&config_dir())?;
    let value = parse_mcp_config(&config.to_value())?.to_value();
    atomic_json(&mcp_config_path(), &value, false)
}

/// Stable, provider-safe tool name for an MCP tool.
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    let suffix = &hex::encode(Sha256::digest(format!("{server}\0{tool}").as_bytes()))[..10];
    static NON: OnceLock<Regex> = OnceLock::new();
    static LEAD: OnceLock<Regex> = OnceLock::new();
    let non = NON.get_or_init(|| Regex::new(r"[^a-z0-9_]").unwrap());
    let lead = LEAD.get_or_init(|| Regex::new(r"^[^a-z]+").unwrap());
    let clean_server = non.replace_all(&server.to_lowercase(), "_").to_string();
    let clean_server = lead.replace(&clean_server, "s_").to_string();
    let clean_server = jsutil::take_chars(&clean_server, 16).to_string();
    let clean_tool = non.replace_all(&tool.to_lowercase(), "_").to_string();
    let clean_tool = jsutil::take_chars(&clean_tool, 28).to_string();
    format!("mcp_{clean_server}_{clean_tool}_{suffix}")
}

/// Resolves a command on Windows against PATH + PATHEXT so `npx`, `uvx` work without ENOENT.
pub fn resolve_mcp_command(command: &str, env: &std::collections::BTreeMap<String, String>, cwd: &Path) -> String {
    if !cfg!(windows) {
        return command.to_string();
    }
    let lower = command.to_lowercase();
    if [".exe", ".cmd", ".bat", ".com"].iter().any(|e| lower.ends_with(e)) {
        return command.to_string();
    }
    let pathext = env.get("PATHEXT").cloned().or_else(|| std::env::var("PATHEXT").ok()).unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    let exts: Vec<&str> = pathext.split(';').map(str::trim).filter(|e| !e.is_empty()).collect();
    if command.contains('\\') || command.contains('/') || Path::new(command).is_absolute() {
        let base = crate::files::resolve_lexical(cwd, command);
        if base.exists() {
            return base.to_string_lossy().to_string();
        }
        for ext in &exts {
            let candidate = PathBuf::from(format!("{}{ext}", base.display()));
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
        }
        return command.to_string();
    }
    let path_var = env.get("PATH").or_else(|| env.get("Path")).cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    let mut dirs = vec![cwd.to_path_buf()];
    dirs.extend(std::env::split_paths(&path_var));
    for dir in dirs {
        for ext in &exts {
            let candidate = dir.join(format!("{command}{ext}"));
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
        }
        let bare = dir.join(command);
        if bare.exists() {
            return bare.to_string_lossy().to_string();
        }
    }
    command.to_string()
}

const COMPOSITION_KEYS: [&str; 3] = ["anyOf", "oneOf", "allOf"];

fn sanitize_node(node: &Value) -> Value {
    match node {
        Value::Bool(_) => json!({ "type": "string" }),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_node).collect()),
        Value::Object(source) => {
            let mut map = Map::new();
            for (k, v) in source {
                if k == "$schema" || k == "$id" {
                    continue;
                }
                map.insert(k.clone(), v.clone());
            }
            if let Some(c) = map.remove("const") {
                map.insert("enum".into(), json!([c]));
            }
            match map.get("properties").cloned() {
                Some(Value::Object(props)) => {
                    let mut out = Map::new();
                    for (pk, pv) in props {
                        out.insert(pk, sanitize_node(&pv));
                    }
                    map.insert("properties".into(), Value::Object(out));
                }
                Some(Value::Null) => {
                    map.insert("properties".into(), json!({}));
                }
                _ => {}
            }
            for key in ["items", "prefixItems"] {
                if let Some(v) = map.get(key).cloned() {
                    map.insert(key.into(), sanitize_node(&v));
                }
            }
            if let Some(v @ Value::Object(_)) = map.get("additionalProperties").cloned() {
                map.insert("additionalProperties".into(), sanitize_node(&v));
            }
            for key in COMPOSITION_KEYS {
                if let Some(Value::Array(items)) = map.get(key).cloned() {
                    map.insert(key.into(), Value::Array(items.iter().map(sanitize_node).collect()));
                }
            }
            for key in ["$defs", "definitions"] {
                match map.get(key).cloned() {
                    Some(Value::Object(defs)) => {
                        let mut out = Map::new();
                        for (dk, dv) in defs {
                            out.insert(dk, sanitize_node(&dv));
                        }
                        map.insert(key.into(), Value::Object(out));
                    }
                    Some(_) => {
                        map.remove(key);
                    }
                    None => {}
                }
            }
            let has_type = map.get("type").map(|t| !t.is_null() && t != &json!("")).unwrap_or(false);
            if !has_type && !map.contains_key("$ref") && !COMPOSITION_KEYS.iter().any(|k| map.contains_key(*k)) {
                if map.contains_key("properties") || map.contains_key("required") || map.contains_key("additionalProperties") {
                    map.insert("type".into(), json!("object"));
                } else if map.contains_key("items") || map.contains_key("prefixItems") {
                    map.insert("type".into(), json!("array"));
                } else if map.contains_key("enum") || map.contains_key("format") {
                    map.insert("type".into(), json!("string"));
                } else if ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"].iter().any(|k| map.contains_key(*k)) {
                    map.insert("type".into(), json!("number"));
                }
            }
            if map.get("type") == Some(&json!("object")) && !matches!(map.get("properties"), Some(Value::Object(_))) {
                map.insert("properties".into(), json!({}));
            }
            if map.get("type") == Some(&json!("array")) && !map.contains_key("items") {
                map.insert("items".into(), json!({ "type": "string" }));
            }
            Value::Object(map)
        }
        other => other.clone(),
    }
}

fn strip_descriptions(node: &mut Value) {
    match node {
        Value::Array(items) => items.iter_mut().for_each(strip_descriptions),
        Value::Object(obj) => {
            for key in ["description", "title", "examples", "$comment"] {
                obj.remove(key);
            }
            obj.values_mut().for_each(strip_descriptions);
        }
        _ => {}
    }
}

fn drop_definitions(node: &mut Value) {
    fn rewrite(cur: &mut Value) {
        match cur {
            Value::Array(items) => items.iter_mut().for_each(rewrite),
            Value::Object(obj) => {
                if let Some(Value::String(r)) = obj.get("$ref") {
                    if r.starts_with("#/$defs/") || r.starts_with("#/definitions/") {
                        obj.clear();
                        return;
                    }
                }
                for (k, v) in obj.iter_mut() {
                    if k == "$defs" || k == "definitions" {
                        continue;
                    }
                    rewrite(v);
                }
            }
            _ => {}
        }
    }
    rewrite(node);
    if let Value::Object(root) = node {
        root.remove("$defs");
        root.remove("definitions");
    }
}

fn collapse_deep(node: &mut Value, depth: usize) {
    match node {
        Value::Array(items) => items.iter_mut().for_each(|i| collapse_deep(i, depth)),
        Value::Object(obj) => {
            let complex = ["properties", "items", "additionalProperties", "$ref"].iter().any(|k| obj.contains_key(*k)) || COMPOSITION_KEYS.iter().any(|k| obj.contains_key(*k));
            if depth >= 3 && complex {
                let preserved = obj.get("type").and_then(Value::as_str).map(str::to_string);
                obj.clear();
                if let Some(t) = preserved {
                    if ["string", "number", "integer", "boolean"].contains(&t.as_str()) {
                        obj.insert("type".into(), json!(t));
                    }
                }
                return;
            }
            if let Some(Value::Object(props)) = obj.get_mut("properties") {
                props.values_mut().for_each(|v| collapse_deep(v, depth + 1));
            }
            if let Some(items) = obj.get_mut("items") {
                collapse_deep(items, depth + 1);
            }
            if let Some(ap @ Value::Object(_)) = obj.get_mut("additionalProperties") {
                collapse_deep(ap, depth + 1);
            }
            for key in COMPOSITION_KEYS {
                if let Some(Value::Array(items)) = obj.get_mut(key) {
                    items.iter_mut().for_each(|i| collapse_deep(i, depth + 1));
                }
            }
        }
        _ => {}
    }
}

fn prune_compositions(node: &mut Value, is_root: bool) {
    match node {
        Value::Array(items) => items.iter_mut().for_each(|i| prune_compositions(i, false)),
        Value::Object(obj) => {
            if !is_root && COMPOSITION_KEYS.iter().any(|k| obj.contains_key(*k)) {
                obj.clear();
                return;
            }
            obj.values_mut().for_each(|v| prune_compositions(v, false));
        }
        _ => {}
    }
}

/// Sanitizes an MCP tool `inputSchema` and applies progressive compaction when it exceeds
/// `max_bytes` so large MCP servers never fail registration.
pub fn sanitize_and_compact_tool_schema(raw: &Value, max_bytes: usize) -> Value {
    let base = if raw.is_object() { raw.clone() } else { json!({ "type": "object", "properties": {} }) };
    let mut sanitized = sanitize_node(&base);
    if let Value::Object(m) = &mut sanitized {
        if m.get("type") != Some(&json!("object")) {
            m.insert("type".into(), json!("object"));
        }
        if !matches!(m.get("properties"), Some(Value::Object(_))) {
            m.insert("properties".into(), json!({}));
        }
    }
    let fits = |v: &Value| jsutil::stringify(v).len() <= max_bytes;
    if fits(&sanitized) {
        return sanitized;
    }
    strip_descriptions(&mut sanitized);
    if fits(&sanitized) {
        return sanitized;
    }
    drop_definitions(&mut sanitized);
    if fits(&sanitized) {
        return sanitized;
    }
    collapse_deep(&mut sanitized, 0);
    if fits(&sanitized) {
        return sanitized;
    }
    prune_compositions(&mut sanitized, true);
    if fits(&sanitized) {
        return sanitized;
    }
    if let Value::Object(m) = &mut sanitized {
        if let Some(Value::Object(props)) = m.get("properties").cloned() {
            let mut flat = Map::new();
            for (k, v) in props {
                let t = v.get("type").and_then(Value::as_str).filter(|t| ["string", "number", "integer", "boolean"].contains(t));
                flat.insert(k, t.map(|t| json!({ "type": t })).unwrap_or_else(|| json!({})));
            }
            m.insert("properties".into(), Value::Object(flat));
        }
        m.remove("additionalProperties");
    }
    sanitized
}

fn parse_toml_value(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed == "true" {
        return json!(true);
    }
    if trimmed == "false" {
        return json!(false);
    }
    static NUM: OnceLock<Regex> = OnceLock::new();
    if NUM.get_or_init(|| Regex::new(r"^-?\d+(?:\.\d+)?$").unwrap()).is_match(trimmed) {
        return jsutil::num(trimmed.parse::<f64>().unwrap_or(0.0));
    }
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return serde_json::from_str(trimmed).unwrap_or_else(|_| json!(trimmed[1..trimmed.len() - 1]));
    }
    if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        return json!(trimmed[1..trimmed.len() - 1]);
    }
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = trimmed[1..trimmed.len() - 1].trim();
        if inner.is_empty() {
            return json!([]);
        }
        static ITEM: OnceLock<Regex> = OnceLock::new();
        let re = ITEM.get_or_init(|| Regex::new(r#""((?:\\.|[^"\\])*)"|'([^']*)'"#).unwrap());
        let items: Vec<Value> = re
            .captures_iter(inner)
            .map(|c| {
                if let Some(m) = c.get(1) {
                    serde_json::from_str::<String>(&format!("\"{}\"", m.as_str())).map(Value::String).unwrap_or_else(|_| json!(m.as_str()))
                } else {
                    json!(c.get(2).map(|m| m.as_str()).unwrap_or(""))
                }
            })
            .collect();
        return Value::Array(items);
    }
    json!(trimmed)
}

/// Parses `[mcp_servers.<name>]` blocks from Codex's `~/.codex/config.toml`.
pub fn parse_codex_mcp_servers(toml_text: &str) -> indexmap::IndexMap<String, McpServerConfig> {
    let mut raw_servers: indexmap::IndexMap<String, Map<String, Value>> = indexmap::IndexMap::new();
    let mut current: Option<String> = None;
    let mut subtable: Option<String> = None;
    static HEADER: OnceLock<Regex> = OnceLock::new();
    let header_re = HEADER.get_or_init(|| Regex::new(r#"^mcp_servers\.(?:"([^"]+)"|'([^']+)'|([A-Za-z0-9_-]+))(?:\.(env|http_headers|env_http_headers))?$"#).unwrap());
    for raw_line in toml_text.replace("\r\n", "\n").split('\n') {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let header = line[1..line.len() - 1].trim();
            if let Some(c) = header_re.captures(header) {
                let name = c.get(1).or(c.get(2)).or(c.get(3)).unwrap().as_str().to_string();
                subtable = c.get(4).map(|m| m.as_str().to_string());
                raw_servers.entry(name.clone()).or_default();
                current = Some(name);
            } else {
                current = None;
                subtable = None;
            }
            continue;
        }
        let Some(server) = &current else { continue };
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        let val = parse_toml_value(&line[eq + 1..]);
        let entry = raw_servers.get_mut(server).unwrap();
        if let Some(sub) = &subtable {
            let table = entry.entry(sub.clone()).or_insert_with(|| json!({}));
            let text = match &val {
                Value::String(s) => s.clone(),
                other => jsutil::stringify(other),
            };
            if let Value::Object(t) = table {
                t.insert(key, json!(text));
            }
        } else {
            entry.insert(key, val);
        }
    }
    let mut parsed = indexmap::IndexMap::new();
    for (name, raw) in raw_servers {
        if let Ok(config) = parse_server(&Value::Object(raw)) {
            parsed.insert(name, config);
        }
    }
    parsed
}

pub fn import_codex_mcp_servers(path: Option<PathBuf>, filter: &[String]) -> Result<(Vec<String>, PathBuf)> {
    let codex_path = path.unwrap_or_else(|| jsutil::home_dir().join(".codex").join("config.toml"));
    if !codex_path.exists() {
        bail!("Codex config not found at {}", codex_path.display());
    }
    let text = std::fs::read_to_string(&codex_path)?;
    let servers = parse_codex_mcp_servers(&text);
    let mut current = load_mcp_config()?;
    let mut imported = vec![];
    for (name, mut server) in servers {
        if !filter.is_empty() && !filter.contains(&name) {
            continue;
        }
        // Imported servers are optional so offline local tools never block CLI runs.
        server.required = false;
        current.servers.insert(name.clone(), server);
        imported.push(name);
    }
    save_mcp_config(&current)?;
    Ok((imported, mcp_config_path()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_and_defaults() {
        let config = parse_mcp_config(&json!({"mcpServers": {"fs": {"command": "npx", "args": ["x"], "enabled": true, "autoApprove": true, "startup_timeout_sec": 5}}})).unwrap();
        let fs = &config.servers["fs"];
        assert!(fs.trust);
        assert_eq!(fs.timeout_ms, 5000);
        assert_eq!(fs.transport_name(), "stdio");
        assert!(parse_mcp_config(&json!({"servers": {"x": {"url": "ftp://x"}}})).is_err());
        assert!(parse_mcp_config(&json!({"servers": {"x": {"command": "a", "bogus": 1}}})).is_err());
    }

    #[test]
    fn tool_names_are_stable() {
        let name = mcp_tool_name("My-Server", "do.thing");
        assert!(name.starts_with("mcp_my_server_do_thing_"));
        assert_eq!(name.len(), "mcp_my_server_do_thing_".len() + 10);
    }

    #[test]
    fn schema_sanitize() {
        let schema = sanitize_and_compact_tool_schema(&json!({"$schema": "x", "properties": {"a": {"const": 1}}}), 16_000);
        assert_eq!(schema, json!({"properties": {"a": {"enum": [1], "type": "string"}}, "type": "object"}));
    }

    #[test]
    fn codex_toml_import() {
        let servers = parse_codex_mcp_servers("[mcp_servers.play]\ncommand = \"npx\"\nargs = [\"@playwright/mcp\"]\n[mcp_servers.play.env]\nX = \"1\"\n");
        assert_eq!(servers.len(), 1);
        match &servers["play"].transport {
            McpTransport::Stdio { command, args, env, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &vec!["@playwright/mcp".to_string()]);
                assert_eq!(env.get("X"), Some(&json!("1")));
            }
            _ => panic!(),
        }
    }
}
