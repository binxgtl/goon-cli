//! Session files (`{data}/sessions/{id}.json`), locks, listing index and goals.
//!
//! The on-disk format is the one the TypeScript build wrote: compact JSON, AI SDK `ModelMessage`
//! values, ISO timestamps with milliseconds.

use crate::config::atomic_json;
use crate::files::FileBackup;
use crate::jsutil::{is_iso_datetime, js_f64, js_f64_opt, now_iso, now_millis, parse_iso_millis, take_chars, utf16_len};
use anyhow::{anyhow, bail, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnCheckpoint {
    pub id: u64,
    pub created_at: String,
    pub prompt: String,
    pub message_index: usize,
    #[serde(serialize_with = "js_f64::serialize")]
    pub tokens_before: f64,
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "js_f64_opt::serialize")]
    pub cost_usd_before: Option<f64>,
    pub files: Vec<FileBackup>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TodoItem {
    /// Set by `update_plan` (written to the session file, dropped again on load like zod did).
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub content: String,
    pub status: String,
    #[serde(default = "default_priority")]
    pub priority: String,
}

fn default_priority() -> String {
    "medium".into()
}

pub const GOAL_STATUSES: [&str; 6] = ["active", "paused", "blocked", "usage_limited", "budget_limited", "complete"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionGoal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    pub objective: String,
    #[serde(default = "default_goal_status")]
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, serialize_with = "js_f64::serialize")]
    pub tokens_at_start: f64,
    #[serde(default, serialize_with = "js_f64::serialize")]
    pub tokens_used: f64,
    #[serde(default, serialize_with = "js_f64::serialize")]
    pub time_used_seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "js_f64_opt::serialize")]
    pub token_budget: Option<f64>,
}

fn default_goal_status() -> String {
    "active".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub version: u8,
    pub id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub messages: Vec<Value>,
    #[serde(serialize_with = "js_f64::serialize")]
    pub tokens: f64,
    pub compactions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoints: Option<Vec<TurnCheckpoint>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todos: Option<Vec<TodoItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<SessionGoal>,
    #[serde(default, skip_serializing_if = "Option::is_none", serialize_with = "js_f64_opt::serialize")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejected_signatures: Option<indexmap::IndexMap<String, u64>>,
}

impl Session {
    pub fn cost(&self) -> f64 {
        self.cost_usd.unwrap_or(0.0)
    }
}

pub const GOAL_USAGE: &str = "Usage: /goal [<objective>|--budget <tokens> <objective>|clear|edit|pause|resume]";

pub fn goal_status_label(status: &str) -> &'static str {
    match status {
        "active" => "active",
        "paused" => "paused",
        "blocked" => "stalled",
        "usage_limited" => "usage limited",
        "budget_limited" => "limited by budget",
        _ => "complete",
    }
}

pub fn format_goal_elapsed_seconds(seconds: f64) -> String {
    let s = seconds.max(0.0).floor() as i64;
    if s < 60 {
        return format!("{s}s");
    }
    let minutes = s / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rem_minutes = minutes % 60;
    if hours >= 24 {
        return format!("{}d {}h {}m", hours / 24, hours % 24, rem_minutes);
    }
    if rem_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {rem_minutes}m")
    }
}

fn compact_fixed(n: f64) -> String {
    let s = crate::jsutil::to_fixed(n, 1);
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

pub fn format_goal_tokens_compact(tokens: f64) -> String {
    let n = crate::jsutil::js_round(tokens).max(0.0);
    if n < 1_000.0 {
        return format!("{}", n as i64);
    }
    if n < 1_000_000.0 {
        return format!("{}K", compact_fixed(n / 1_000.0));
    }
    format!("{}M", compact_fixed(n / 1_000_000.0))
}

pub fn format_goal_usage_summary(goal: &SessionGoal) -> String {
    let mut parts = vec![format!("Objective: {}", goal.objective)];
    if goal.time_used_seconds > 0.0 {
        parts.push(format!("Time: {}.", format_goal_elapsed_seconds(goal.time_used_seconds)));
    }
    if let Some(budget) = goal.token_budget {
        parts.push(format!("Tokens: {}/{}.", format_goal_tokens_compact(goal.tokens_used), format_goal_tokens_compact(budget)));
    }
    parts.join(" ")
}

pub fn format_goal_status_indicator(goal: &SessionGoal, active_turn_seconds: f64) -> String {
    let tokens_used = goal.tokens_used;
    let time_used = goal.time_used_seconds + if goal.status == "active" { active_turn_seconds.max(0.0).floor() } else { 0.0 };
    match goal.status.as_str() {
        "active" => {
            let usage = match goal.token_budget.filter(|b| *b != 0.0) {
                Some(b) => format!("{} / {}", format_goal_tokens_compact(tokens_used), format_goal_tokens_compact(b)),
                None => format_goal_elapsed_seconds(time_used),
            };
            format!("Pursuing goal ({usage})")
        }
        "paused" => "Goal paused (/goal resume)".into(),
        "blocked" => "Goal stalled (/goal resume)".into(),
        "usage_limited" => "Goal hit usage limits (/goal resume)".into(),
        "budget_limited" => match goal.token_budget.filter(|b| *b != 0.0) {
            Some(b) => format!("Goal unmet ({} / {} tokens)", format_goal_tokens_compact(tokens_used), format_goal_tokens_compact(b)),
            None => "Goal abandoned".into(),
        },
        _ => {
            let usage = match goal.token_budget.filter(|b| *b != 0.0) {
                Some(_) => format!("{} tokens", format_goal_tokens_compact(tokens_used)),
                None => format_goal_elapsed_seconds(time_used),
            };
            format!("Goal achieved ({usage})")
        }
    }
}

pub fn format_goal_summary_lines(goal: &SessionGoal, current_session_tokens: f64) -> String {
    let tokens_used = if goal.tokens_used > 0.0 { goal.tokens_used } else { (current_session_tokens - goal.tokens_at_start).max(0.0) };
    let created = parse_iso_millis(&goal.created_at).unwrap_or_else(now_millis);
    let wall = ((now_millis() - created) as f64 / 1000.0).floor().max(0.0);
    let time = if goal.time_used_seconds > 0.0 { goal.time_used_seconds } else { wall };
    let mut lines = vec![
        "Goal".to_string(),
        format!("  Status: {}", goal_status_label(&goal.status)),
        format!("  Objective: {}", goal.objective),
        format!("  Time used: {}", format_goal_elapsed_seconds(time)),
        format!("  Tokens used: {}", format_goal_tokens_compact(tokens_used)),
    ];
    if let Some(budget) = goal.token_budget {
        lines.push(format!("  Token budget: {}", format_goal_tokens_compact(budget)));
    }
    let hint = match goal.status.as_str() {
        "active" => "Commands: /goal edit, /goal pause, /goal clear",
        "paused" | "blocked" | "usage_limited" => "Commands: /goal edit, /goal resume, /goal clear",
        _ => "Commands: /goal edit, /goal clear",
    };
    lines.push(String::new());
    lines.push(format!("  {hint}"));
    lines.join("\n")
}

#[derive(Clone, Debug, PartialEq)]
pub enum GoalDraft {
    Ok { objective: String, token_budget: Option<f64> },
    Error(String),
}

fn parse_token_number(value: &str) -> Option<f64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)^(\d+(?:\.\d+)?)\s*([km])?$").unwrap());
    let m = re.captures(value.trim())?;
    let base: f64 = m[1].parse().ok()?;
    let mult = match m.get(2).map(|g| g.as_str().to_lowercase()) {
        None => 1.0,
        Some(k) if k == "k" => 1_000.0,
        _ => 1_000_000.0,
    };
    let out = crate::jsutil::js_round(base * mult);
    (out.is_finite() && out > 0.0).then_some(out)
}

pub fn parse_goal_draft_args(raw: &str) -> GoalDraft {
    static EQ: OnceLock<Regex> = OnceLock::new();
    static FLAG: OnceLock<Regex> = OnceLock::new();
    static SP: OnceLock<Regex> = OnceLock::new();
    let eq = EQ.get_or_init(|| Regex::new(r"(?is)^--(?:token-)?budget=(\S+)(?:\s+(.*))?$").unwrap());
    let flag = FLAG.get_or_init(|| Regex::new(r"(?i)^--(?:token-)?budget(?:\s|$)").unwrap());
    let sp = SP.get_or_init(|| Regex::new(r"(?is)^--(?:token-)?budget\s+(\S+)(?:\s+(.*))?$").unwrap());
    let mut rest = raw.trim().to_string();
    let mut token_budget = None;
    let snapshot = rest.clone();
    if let Some(m) = eq.captures(&snapshot) {
        token_budget = parse_token_number(&m[1]);
        if token_budget.is_none() {
            return GoalDraft::Error(format!("Invalid --budget \"{}\". Expected a positive token count (e.g. 50000 or 50k).", &m[1]));
        }
        rest = m.get(2).map(|g| g.as_str().trim().to_string()).unwrap_or_default();
    } else if flag.is_match(&rest) {
        let Some(m) = sp.captures(&snapshot) else {
            return GoalDraft::Error(format!("Missing value for --budget.\n{GOAL_USAGE}"));
        };
        token_budget = parse_token_number(&m[1]);
        if token_budget.is_none() {
            return GoalDraft::Error(format!("Invalid --budget \"{}\". Expected a positive token count (e.g. 50000 or 50k).", &m[1]));
        }
        rest = m.get(2).map(|g| g.as_str().trim().to_string()).unwrap_or_default();
    }
    GoalDraft::Ok { objective: rest, token_budget }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub cwd: String,
    pub updated_at: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn first_user_preview(messages: &[Value]) -> Option<String> {
    static GOAL_RE: OnceLock<Regex> = OnceLock::new();
    static REF_RE: OnceLock<Regex> = OnceLock::new();
    let goal_re = GOAL_RE.get_or_init(|| Regex::new(r#"User set the goal:\s*"([^"]+)""#).unwrap());
    let ref_re = REF_RE.get_or_init(|| Regex::new(r"(?s)<referenced_file.*?</referenced_file>").unwrap());
    for m in messages {
        if m.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let raw = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts.iter().map(|p| p.get("text").and_then(Value::as_str).unwrap_or("")).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        };
        let cleaned = if raw.starts_with("<codex_internal_context source=\"goal\">") {
            String::new()
        } else if raw.starts_with("<codex_internal_context source=\"user_goal\">") {
            match goal_re.captures(&raw) {
                Some(c) if !c[1].trim().is_empty() => format!("Goal: {}", collapse_ws(&c[1])),
                _ => String::new(),
            }
        } else {
            collapse_ws(&ref_re.replace_all(&raw, ""))
        };
        if !cleaned.is_empty() {
            return Some(take_chars(&cleaned, 72).to_string());
        }
    }
    None
}

/// Converts legacy serialized Buffers/Uint8Arrays in image/file parts back to base64.
pub fn repair_binary_payloads(raw: &mut Value) -> bool {
    use base64::Engine;
    let Some(messages) = raw.get_mut("messages").and_then(Value::as_array_mut) else { return false };
    let mut changed = false;
    let to_base64 = |value: &Value| -> Option<String> {
        let record = value.as_object()?;
        if record.get("type").and_then(Value::as_str) == Some("Buffer") {
            if let Some(data) = record.get("data").and_then(Value::as_array) {
                let bytes: Vec<u8> = data.iter().map(|v| v.as_f64().unwrap_or(0.0) as i64 as u8).collect();
                return Some(base64::engine::general_purpose::STANDARD.encode(bytes));
            }
        }
        if !record.is_empty() && record.keys().enumerate().all(|(i, k)| *k == i.to_string()) && record.values().all(Value::is_number) {
            let bytes: Vec<u8> = record.values().map(|v| v.as_f64().unwrap_or(0.0) as i64 as u8).collect();
            return Some(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
        None
    };
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else { continue };
        for part in content {
            let key = match part.get("type").and_then(Value::as_str) {
                Some("image") => "image",
                Some("file") => "data",
                _ => continue,
            };
            if let Some(repaired) = part.get(key).and_then(to_base64) {
                part[key] = Value::String(repaired);
                changed = true;
            }
        }
    }
    changed
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap())
}

pub fn is_uuid(text: &str) -> bool {
    uuid_re().is_match(text)
}

fn validate_message(message: &Value, index: usize) -> Result<()> {
    let role = message.get("role").and_then(Value::as_str).ok_or_else(|| anyhow!("Invalid session: messages[{index}] has no role"))?;
    let content = message.get("content").ok_or_else(|| anyhow!("Invalid session: messages[{index}] has no content"))?;
    let valid = match role {
        "system" => content.is_string(),
        "user" | "assistant" => content.is_string() || content.is_array(),
        "tool" => content.is_array(),
        _ => false,
    };
    if !valid {
        bail!("Invalid session: messages[{index}] is not a valid model message");
    }
    if let Some(parts) = content.as_array() {
        for (j, part) in parts.iter().enumerate() {
            let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
            let ok = match kind {
                "text" | "reasoning" => part.get("text").map(Value::is_string).unwrap_or(false),
                "image" => part.get("image").is_some(),
                "file" => part.get("data").is_some() && part.get("mediaType").map(Value::is_string).unwrap_or(false),
                "tool-call" => part.get("toolCallId").map(Value::is_string).unwrap_or(false) && part.get("toolName").map(Value::is_string).unwrap_or(false),
                "tool-result" => part.get("toolCallId").map(Value::is_string).unwrap_or(false) && part.get("output").map(Value::is_object).unwrap_or(false),
                "tool-approval-request" | "tool-approval-response" => true,
                _ => false,
            };
            if !ok {
                bail!("Invalid session: messages[{index}].content[{j}] is not a valid {role} part");
            }
        }
    }
    Ok(())
}

fn validate_session(session: &Session) -> Result<()> {
    if session.version != 1 {
        bail!("Invalid session: unsupported version");
    }
    if !is_uuid(&session.id) {
        bail!("Invalid session: id is not a UUID");
    }
    for (name, value) in [("createdAt", &session.created_at), ("updatedAt", &session.updated_at)] {
        if !is_iso_datetime(value) {
            bail!("Invalid session: {name} is not an ISO datetime");
        }
    }
    if session.tokens < 0.0 {
        bail!("Invalid session: tokens must be nonnegative");
    }
    for (i, m) in session.messages.iter().enumerate() {
        validate_message(m, i)?;
    }
    for cp in session.checkpoints.iter().flatten() {
        if cp.id == 0 || !is_iso_datetime(&cp.created_at) {
            bail!("Invalid session: malformed checkpoint");
        }
    }
    for todo in session.todos.iter().flatten() {
        if todo.content.is_empty() || !["pending", "in_progress", "completed"].contains(&todo.status.as_str()) || !["high", "medium", "low"].contains(&todo.priority.as_str()) {
            bail!("Invalid session: malformed todo");
        }
    }
    if let Some(goal) = &session.goal {
        if goal.objective.is_empty() || !GOAL_STATUSES.contains(&goal.status.as_str()) || !is_iso_datetime(&goal.created_at) || !is_iso_datetime(&goal.updated_at) {
            bail!("Invalid session: malformed goal");
        }
    }
    Ok(())
}

/// Parses and validates a stored session, repairing legacy binary payloads first.
pub fn parse_session(text: &str) -> Result<Session> {
    let mut raw: Value = crate::jsutil::parse_json(text).map_err(|e| anyhow!("{e}"))?;
    repair_binary_payloads(&mut raw);
    let session: Session = serde_json::from_value(raw).map_err(|e| anyhow!("Invalid session: {e}"))?;
    validate_session(&session)?;
    Ok(session)
}

pub const INTERRUPTED_RESULT: &str = "Previous process was interrupted. Execution status unknown; inspect the workspace before retrying.";

/// Appends error results for tool calls left without a result by an interrupted process.
pub fn repair_interrupted(messages: &mut Vec<Value>) -> bool {
    let mut pending: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
    for message in messages.iter() {
        let Some(parts) = message.get("content").and_then(Value::as_array) else { continue };
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("tool-call") if part.get("providerExecuted") != Some(&Value::Bool(true)) => {
                    let id = part.get("toolCallId").and_then(Value::as_str).unwrap_or("").to_string();
                    pending.insert(id, part.get("toolName").and_then(Value::as_str).unwrap_or("").to_string());
                }
                Some("tool-result") => {
                    if let Some(id) = part.get("toolCallId").and_then(Value::as_str) {
                        pending.shift_remove(id);
                    }
                }
                _ => {}
            }
        }
    }
    if pending.is_empty() {
        return false;
    }
    let content: Vec<Value> =
        pending.into_iter().map(|(id, name)| json!({ "type": "tool-result", "toolCallId": id, "toolName": name, "output": { "type": "error-text", "value": INTERRUPTED_RESULT } })).collect();
    messages.push(json!({ "role": "tool", "content": content }));
    true
}

pub fn same_workspace(a: &str, b: &str) -> bool {
    use unicode_normalization::UnicodeNormalization;
    let na: String = a.nfc().collect::<String>().trim_end_matches(['/', '\\']).to_string();
    let nb: String = b.nfc().collect::<String>().trim_end_matches(['/', '\\']).to_string();
    if crate::jsutil::is_windows() {
        na.to_lowercase() == nb.to_lowercase()
    } else {
        na == nb
    }
}

fn remove_event_log_artifacts(root: &Path, id: &str) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let prefix = format!("{id}.events.jsonl");
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == prefix || name.starts_with(&format!("{prefix}.")) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub struct SessionRoute {
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub base_url: String,
}

impl SessionRoute {
    /// The explicit allowlist of runtime fields a session records (never credentials).
    pub fn from_runtime(config: &crate::config::RuntimeConfig) -> SessionRoute {
        SessionRoute { cwd: config.cwd.to_string_lossy().to_string(), provider: config.provider.clone(), model: config.model.clone(), base_url: config.base_url.clone() }
    }
}

fn acquire(lock: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(lock)?;
    file.write_all(std::process::id().to_string().as_bytes())
}

fn lock_pid(lock: &Path) -> std::io::Result<Option<u32>> {
    let text = std::fs::read_to_string(lock)?;
    let trimmed = text.trim();
    Ok(trimmed.parse::<u64>().ok().filter(|p| *p > 0 && *p <= u32::MAX as u64).map(|p| p as u32).filter(|_| !trimmed.is_empty()))
}

pub struct SessionStore {
    pub root: PathBuf,
    pub session: Mutex<Session>,
    lock: PathBuf,
    closed: Mutex<bool>,
    save_lock: Mutex<()>,
}

pub const SESSION_INDEX_FILE: &str = ".index.json";
const SESSION_INDEX_VERSION: u64 = 1;

impl SessionStore {
    pub fn open(root: &Path, input: SessionRoute, resume: Option<&str>, exclude_id: Option<&str>) -> Result<SessionStore> {
        crate::config::mkdir_private(root)?;
        let mut id = resume.map(|r| r.trim().to_string());
        static DIGITS: OnceLock<Regex> = OnceLock::new();
        static PREFIX: OnceLock<Regex> = OnceLock::new();
        let digits = DIGITS.get_or_init(|| Regex::new(r"^\d{1,3}$").unwrap());
        let prefix_re = PREFIX.get_or_init(|| Regex::new(r"(?i)^[0-9a-f-]{4,35}$").unwrap());
        if id.as_deref() == Some("latest") {
            let list = SessionStore::list(root)?;
            let ws: Vec<&SessionSummary> = list.iter().filter(|s| same_workspace(&s.cwd, &input.cwd) && Some(s.id.as_str()) != exclude_id).collect();
            id = ws
                .iter()
                .find(|s| s.messages_count.unwrap_or(0) > 0)
                .or(ws.first())
                .map(|s| s.id.clone())
                .or_else(|| list.iter().find(|s| same_workspace(&s.cwd, &input.cwd) && s.messages_count.unwrap_or(0) > 0).map(|s| s.id.clone()))
                .or_else(|| list.iter().find(|s| same_workspace(&s.cwd, &input.cwd)).map(|s| s.id.clone()));
        } else if let Some(raw) = id.clone().filter(|i| digits.is_match(i)) {
            let idx = raw.parse::<i64>().unwrap_or(0) - 1;
            let list = SessionStore::list(root)?;
            let non_empty: Vec<&SessionSummary> = list.iter().filter(|s| same_workspace(&s.cwd, &input.cwd) && s.messages_count.unwrap_or(0) > 0 && Some(s.id.as_str()) != exclude_id).collect();
            let all: Vec<&SessionSummary> = list.iter().filter(|s| same_workspace(&s.cwd, &input.cwd) && Some(s.id.as_str()) != exclude_id).collect();
            let candidates = if non_empty.is_empty() { all } else { non_empty };
            if idx >= 0 {
                if let Some(s) = candidates.get(idx as usize) {
                    id = Some(s.id.clone());
                }
            }
        } else if let Some(raw) = id.clone().filter(|i| !is_uuid(i) && prefix_re.is_match(i)) {
            let prefix = raw.to_lowercase();
            let list = SessionStore::list(root)?;
            if let Some(s) =
                list.iter().find(|s| same_workspace(&s.cwd, &input.cwd) && s.id.to_lowercase().starts_with(&prefix)).or_else(|| list.iter().find(|s| s.id.to_lowercase().starts_with(&prefix)))
            {
                id = Some(s.id.clone());
            }
        }
        if resume.is_some() && id.as_deref().map(str::is_empty).unwrap_or(true) {
            bail!("No matching session to resume in this workspace.");
        }
        if let Some(i) = &id {
            if !is_uuid(i) {
                bail!("Invalid session ID.");
            }
        }
        let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let lock = root.join(format!("{id}.lock"));
        if let Err(error) = acquire(&lock) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
            let recovery = PathBuf::from(format!("{}.recovery", lock.display()));
            if acquire(&recovery).is_err() {
                bail!("Session is locked or undergoing recovery.");
            }
            let outcome = (|| -> Result<()> {
                let pid = lock_pid(&lock)?.ok_or_else(|| anyhow!("Invalid session lock: {}", lock.display()))?;
                if crate::process::process_alive(pid) {
                    bail!("Session is already open in another process.");
                }
                std::fs::remove_file(&lock)?;
                acquire(&lock)?;
                Ok(())
            })();
            let _ = std::fs::remove_file(&recovery);
            outcome?;
        }
        SessionStore::load(root, input, &id, lock, resume.is_some())
    }

    fn load(root: &Path, input: SessionRoute, id: &str, lock: PathBuf, resume: bool) -> Result<SessionStore> {
        let result = (|| -> Result<SessionStore> {
            let now = now_iso();
            let mut session = if resume {
                parse_session(&std::fs::read_to_string(root.join(format!("{id}.json")))?)?
            } else {
                Session {
                    version: 1,
                    id: id.into(),
                    cwd: input.cwd.clone(),
                    title: None,
                    created_at: now.clone(),
                    updated_at: now,
                    provider: input.provider.clone(),
                    model: input.model.clone(),
                    base_url: input.base_url.clone(),
                    messages: vec![],
                    tokens: 0.0,
                    compactions: 0,
                    checkpoints: None,
                    todos: None,
                    goal: None,
                    cost_usd: Some(0.0),
                    rejected_signatures: Some(indexmap::IndexMap::new()),
                }
            };
            let real = crate::config::realpath(Path::new(&session.cwd)).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            if session.id != id || !same_workspace(&real, &input.cwd) {
                bail!("Session belongs to a different workspace.");
            }
            repair_interrupted(&mut session.messages);
            session.cwd = input.cwd;
            session.provider = input.provider;
            session.model = input.model;
            session.base_url = input.base_url;
            let store = SessionStore { root: root.to_path_buf(), session: Mutex::new(session), lock: lock.clone(), closed: Mutex::new(false), save_lock: Mutex::new(()) };
            store.save()?;
            Ok(store)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&lock);
        }
        result
    }

    pub fn id(&self) -> String {
        self.session.lock().unwrap().id.clone()
    }

    pub fn file(&self) -> PathBuf {
        self.root.join(format!("{}.json", self.id()))
    }

    /// Lists session summaries, cached in `.index.json` keyed by size and mtime.
    pub fn list(root: &Path) -> Result<Vec<SessionSummary>> {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let index_path = root.join(SESSION_INDEX_FILE);
        let mut index: serde_json::Map<String, Value> = serde_json::Map::new();
        if let Ok(text) = std::fs::read_to_string(&index_path) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                if parsed.get("version").and_then(Value::as_u64) == Some(SESSION_INDEX_VERSION) {
                    if let Some(e) = parsed.get("entries").and_then(Value::as_object) {
                        index = e.clone();
                    }
                }
            }
        }
        static NAME: OnceLock<Regex> = OnceLock::new();
        let name_re = NAME.get_or_init(|| Regex::new(r"^[0-9a-f-]{36}\.json$").unwrap());
        let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| name_re.is_match(n)).collect();
        names.sort();
        let mut next = serde_json::Map::new();
        let mut dirty = false;
        let mut summaries = vec![];
        let now = now_millis() as f64;
        for name in names {
            let file = root.join(&name);
            let Ok(meta) = std::fs::metadata(&file) else { continue };
            let size = meta.len();
            let mtime_ms = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
            let settled = now - mtime_ms > 3_000.0;
            if let Some(cached) = index.get(&name) {
                if settled && cached.get("size").and_then(Value::as_u64) == Some(size) && cached.get("mtimeMs").and_then(Value::as_f64) == Some(mtime_ms) {
                    next.insert(name.clone(), cached.clone());
                    if let Some(summary) = cached.get("summary").filter(|v| !v.is_null()) {
                        if let Ok(s) = serde_json::from_value::<SessionSummary>(summary.clone()) {
                            summaries.push(s);
                        }
                    }
                    continue;
                }
            }
            dirty = true;
            let summary = summarize_session_file(&file);
            next.insert(name, json!({ "size": size, "mtimeMs": crate::jsutil::num(mtime_ms), "summary": summary.as_ref().map(|s| serde_json::to_value(s).unwrap()).unwrap_or(Value::Null) }));
            if let Some(s) = summary {
                summaries.push(s);
            }
        }
        if dirty || index.len() != next.len() {
            let _ = atomic_json(&index_path, &json!({ "version": SESSION_INDEX_VERSION, "entries": next }), true);
        }
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(summaries)
    }

    pub fn delete_session(root: &Path, id_or_prefix: &str) -> Result<Option<String>> {
        let trimmed = id_or_prefix.trim().to_lowercase();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let list = SessionStore::list(root)?;
        let matches: Vec<&SessionSummary> = match list.iter().find(|s| s.id.to_lowercase() == trimmed) {
            Some(exact) => vec![exact],
            None => list.iter().filter(|s| s.id.to_lowercase().starts_with(&trimmed)).collect(),
        };
        if matches.is_empty() {
            return Ok(None);
        }
        if matches.len() > 1 {
            let ids: Vec<&str> = matches.iter().take(4).map(|m| &m.id[..8]).collect();
            bail!("Session prefix \"{}\" is ambiguous ({} matches: {}{}). Use more characters.", id_or_prefix.trim(), matches.len(), ids.join(", "), if matches.len() > 4 { ", ..." } else { "" });
        }
        let id = matches[0].id.clone();
        let lock = root.join(format!("{id}.lock"));
        match lock_pid(&lock) {
            Ok(Some(pid)) if crate::process::process_alive(pid) => bail!("Session {} is open in another goon process (pid {pid}).", &id[..8]),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        match std::fs::remove_file(root.join(format!("{id}.json"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let _ = std::fs::remove_file(&lock);
        remove_event_log_artifacts(root, &id);
        Ok(Some(id))
    }

    pub fn fork_to_new_file(&self, title: Option<&str>) -> Result<String> {
        let mut forked = {
            let mut session = self.session.lock().unwrap();
            prune_checkpoint_backups(&mut session, MAX_CHECKPOINT_BACKUP_BYTES);
            session.clone()
        };
        let forked_id = uuid::Uuid::new_v4().to_string();
        let now = now_iso();
        forked.id = forked_id.clone();
        forked.created_at = now.clone();
        forked.updated_at = now;
        if let Some(t) = title.filter(|t| !t.is_empty()) {
            forked.title = Some(t.into());
        } else if let Some(t) = forked.title.clone().filter(|t| !t.is_empty()) {
            forked.title = Some(format!("{t} (fork)"));
        }
        atomic_json(&self.root.join(format!("{forked_id}.json")), &serde_json::to_value(&forked)?, true)?;
        Ok(forked_id)
    }

    /// Writes the session atomically. Saves are serialized so the latest state lands last.
    pub fn save(&self) -> Result<()> {
        let _guard = self.save_lock.lock().unwrap();
        let (value, file) = {
            let mut session = self.session.lock().unwrap();
            session.updated_at = now_iso();
            prune_checkpoint_backups(&mut session, MAX_CHECKPOINT_BACKUP_BYTES);
            (serde_json::to_value(&*session)?, self.root.join(format!("{}.json", session.id)))
        };
        atomic_json(&file, &value, true)
    }

    pub fn discard_if_empty(&self) -> Result<()> {
        let (empty, file) = {
            let s = self.session.lock().unwrap();
            (s.messages.is_empty() && s.title.as_deref().map(str::is_empty).unwrap_or(true), self.root.join(format!("{}.json", s.id)))
        };
        if empty {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn close(&self) -> Result<()> {
        let mut closed = self.closed.lock().unwrap();
        if *closed {
            return Ok(());
        }
        *closed = true;
        match std::fs::remove_file(&self.lock) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for SessionStore {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn summarize_session_file(file: &Path) -> Option<SessionSummary> {
    let text = std::fs::read_to_string(file).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    if v.get("version").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let id = v.get("id")?.as_str()?;
    if !is_uuid(id) {
        return None;
    }
    let cwd = v.get("cwd")?.as_str()?;
    let updated_at = v.get("updatedAt")?.as_str()?;
    if !is_iso_datetime(updated_at) {
        return None;
    }
    let model = v.get("model")?.as_str()?;
    let messages = v.get("messages")?.as_array()?;
    let title = match v.get("title") {
        None => None,
        Some(Value::String(t)) => Some(t.clone()).filter(|t| !t.is_empty()),
        Some(_) => return None,
    };
    Some(SessionSummary { id: id.into(), cwd: cwd.into(), updated_at: updated_at.into(), model: model.into(), messages_count: Some(messages.len()), title, preview: first_user_preview(messages) })
}

/// Total bytes of file content kept for /undo across all checkpoints of one session.
pub const MAX_CHECKPOINT_BACKUP_BYTES: usize = 32 * 1024 * 1024;

/// Beyond the budget the oldest checkpoints lose their file contents (marked `pruned`).
pub fn prune_checkpoint_backups(session: &mut Session, max_bytes: usize) -> usize {
    let Some(checkpoints) = session.checkpoints.as_mut() else { return 0 };
    let mut total: usize = checkpoints.iter().flat_map(|cp| cp.files.iter()).map(|f| f.content.as_deref().map(utf16_len).unwrap_or(0)).sum();
    if total <= max_bytes {
        return 0;
    }
    let mut pruned = 0;
    for cp in checkpoints.iter_mut() {
        if total <= max_bytes {
            break;
        }
        for f in cp.files.iter_mut() {
            let Some(content) = f.content.take() else { continue };
            total -= utf16_len(&content);
            f.pruned = Some(true);
            pruned += 1;
        }
    }
    pruned
}

/// Last `n` characters helper used by previews elsewhere.
pub fn short_id(id: &str) -> &str {
    take_chars(id, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(cwd: &str) -> SessionRoute {
        SessionRoute { cwd: cwd.into(), provider: "openai-compatible".into(), model: "m".into(), base_url: "http://x".into() }
    }

    #[test]
    fn open_save_resume_and_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = crate::config::realpath(dir.path()).unwrap().to_string_lossy().to_string();
        let root = dir.path().join("sessions");
        let store = SessionStore::open(&root, route(&cwd), None, None).unwrap();
        let id = store.id();
        store.session.lock().unwrap().messages.push(json!({"role": "user", "content": "hello world"}));
        store.session.lock().unwrap().messages.push(json!({"role": "assistant", "content": [{"type": "tool-call", "toolCallId": "c1", "toolName": "bash", "input": {}}]}));
        store.save().unwrap();
        assert!(SessionStore::open(&root, route(&cwd), Some(&id), None).is_err());
        store.close().unwrap();
        let resumed = SessionStore::open(&root, route(&cwd), Some("latest"), None).unwrap();
        let s = resumed.session.lock().unwrap().clone();
        assert_eq!(s.id, id);
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[2]["content"][0]["output"]["value"], json!(INTERRUPTED_RESULT));
        let list = SessionStore::list(&root).unwrap();
        assert_eq!(list[0].preview.as_deref(), Some("hello world"));
        drop(resumed);
        assert_eq!(SessionStore::delete_session(&root, &id[..6]).unwrap(), Some(id));
    }

    #[test]
    fn goal_args_and_format() {
        match parse_goal_draft_args("--budget 50k ship it") {
            GoalDraft::Ok { objective, token_budget } => {
                assert_eq!(objective, "ship it");
                assert_eq!(token_budget, Some(50_000.0));
            }
            GoalDraft::Error(e) => panic!("{e}"),
        }
        assert!(matches!(parse_goal_draft_args("--budget"), GoalDraft::Error(_)));
        assert_eq!(format_goal_tokens_compact(1500.0), "1.5K");
        assert_eq!(format_goal_tokens_compact(2_000_000.0), "2M");
        assert_eq!(format_goal_elapsed_seconds(3660.0), "1h 1m");
        assert_eq!(format_goal_elapsed_seconds(90_000.0), "1d 1h 0m");
    }
}
