//! Tool registry: built-in workspace tools, input normalization (aliases from other harnesses),
//! validation, risk classification and the approval/network gate every call passes through.

use crate::cost::{CostLedger, SharedBudget};
use crate::files::{limit_text_default, WorkspaceFiles};
use crate::model::ToolSpec;
use crate::policy::{is_network_allowed_for_command, network_denied_message, ExecutionPolicy, NetworkMode};
use crate::process::{run_command, CommandError, RunOptions, DEFAULT_COMMAND_TIMEOUT_MS, MAX_COMMAND_TIMEOUT_MS};
use crate::risk::{classify_bash_risk, RiskClass};
use crate::session::{SessionGoal, TodoItem};
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tokio_util::sync::CancellationToken;

/// A tool that asked the user and was refused (the run must stop).
#[derive(Debug, Clone)]
pub struct PermissionDenied(pub String);

impl std::fmt::Display for PermissionDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PermissionDenied {}

pub fn is_permission_denied(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PermissionDenied>().is_some()
}

#[derive(Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub input: Value,
    pub cancel: CancellationToken,
    pub risk: Option<RiskClass>,
    pub trusted: Option<bool>,
}

pub type Approve = Arc<dyn Fn(ApprovalRequest) -> BoxFuture<'static, bool> + Send + Sync>;
pub type ToolExec = Arc<dyn Fn(Value, CancellationToken) -> BoxFuture<'static, Result<String>> + Send + Sync>;

pub fn approve_all() -> Approve {
    Arc::new(|_| Box::pin(async { true }))
}

pub fn deny_all() -> Approve {
    Arc::new(|_| Box::pin(async { false }))
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub input: Value,
}

#[derive(Clone)]
pub enum Validator {
    /// zod semantics (defaults, stripping, trims) for schemas exported from zod.
    Zod(Vec<&'static str>),
    /// Ajv validation for MCP / JSON schemas.
    Json,
    /// Fallback when a JSON schema cannot be used: any object.
    ObjectOnly,
}

#[derive(Clone)]
pub struct ToolDefinition {
    pub description: String,
    pub schema: Value,
    pub read_only: bool,
    pub risk: Option<RiskClass>,
    pub net: Option<bool>,
    pub trusted: Option<bool>,
    pub validator: Validator,
    /// `None` for the registry's own built-in tools.
    pub exec: Option<ToolExec>,
}

impl ToolDefinition {
    pub fn parse(&self, name: &str, input: &Value) -> Result<Value> {
        match &self.validator {
            Validator::Zod(trims) => crate::schema::zod_parse(&self.schema, input, trims).map_err(|e| anyhow!(e)),
            Validator::Json => {
                crate::schema::ajv_validate(&self.schema, input).map_err(|e| anyhow!("Invalid arguments for {name}: {e}"))?;
                Ok(input.clone())
            }
            Validator::ObjectOnly => {
                if !input.is_object() {
                    bail!("Invalid arguments for {name}: expected an object.");
                }
                Ok(input.clone())
            }
        }
    }
}

pub struct ExecuteOutcome {
    pub text: String,
    pub denied: bool,
    pub loop_detected: bool,
}

/// Durable rejection bookkeeping owned by the caller (CLI session, tests).
pub trait RejectionGuard: Send + Sync {
    fn record_denied(&self, signature: &str) -> u64;
    fn reset(&self, signature: &str);
}

pub type AskUser = Arc<dyn Fn(Vec<Value>, CancellationToken) -> BoxFuture<'static, Result<IndexMap<String, Vec<String>>>> + Send + Sync>;

#[derive(Default, Clone)]
pub struct RegistryCallbacks {
    pub on_todo_update: Option<Arc<dyn Fn(Vec<TodoItem>) + Send + Sync>>,
    pub get_goal: Option<Arc<dyn Fn() -> Option<SessionGoal> + Send + Sync>>,
    pub on_goal_update: Option<Arc<dyn Fn(Option<SessionGoal>) -> BoxFuture<'static, Result<()>> + Send + Sync>>,
    pub on_plan_exit: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub ask_user: Option<AskUser>,
}

#[derive(Clone, Debug)]
pub struct PendingImage {
    pub path: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Default, Clone, Copy)]
pub struct RegistryFlags {
    pub read_only: bool,
    pub plan_mode: bool,
    pub allow_peer_tools_in_read_only: bool,
    pub only_peer_tools: bool,
}

pub fn sanitize_bash_command(command: &str) -> String {
    if !crate::jsutil::is_windows() {
        return command.to_string();
    }
    static RE: OnceLock<fancy_regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| fancy_regex::Regex::new(r"(?i)((?:^|[\s;&|()])(?:\d+|&)?>>?\s*)(?:nul|/dev/nul)(?=$|[\s;&|()><])").unwrap());
    re.replace_all(command, "${1}/dev/null").to_string()
}

pub fn tool_allowed_by_set(allowed: &HashSet<String>, name: &str) -> bool {
    if allowed.contains(name) {
        return true;
    }
    allowed.iter().any(|entry| entry.strip_suffix('*').map(|prefix| name.starts_with(prefix)).unwrap_or(false))
}

pub const PEER_TOOLS: [&str; 4] = ["peer_list", "peer_send", "peer_broadcast", "peer_read"];
const COLLAB_TOOLS: [&str; 17] = [
    "spawn_agent",
    "wait_agent",
    "send_input",
    "close_agent",
    "resume_agent",
    "list_agents",
    "agent_network",
    "run_workflow",
    "update_plan",
    "get_goal",
    "create_goal",
    "update_goal",
    "request_user_input",
    "view_image",
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];

pub fn format_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "Todo list is empty.".into();
    }
    let count = |s: &str| todos.iter().filter(|t| t.status == s).count();
    let lines: Vec<String> = todos
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let icon = match t.status.as_str() {
                "completed" => "[x]",
                "in_progress" => "[>]",
                _ => "[ ]",
            };
            format!("{}. {icon} ({}) {}", i + 1, t.priority, t.content)
        })
        .collect();
    format!("Todos ({}/{} completed, {} in progress, {} pending):\n{}", count("completed"), todos.len(), count("in_progress"), count("pending"), lines.join("\n"))
}

pub fn tool_name_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "read" => "read_file",
        "glob" | "ls" => "list_files",
        "grep" => "search",
        "edit" => "edit_file",
        "write" => "write_file",
        "wait" => "wait_agent",
        "send_message" | "followup_task" => "send_input",
        "interrupt_agent" => "close_agent",
        "subagent" | "task" => "spawn_agent",
        "ask_user" | "ask_question" => "request_user_input",
        "read_image" => "view_image",
        "brainstorm" | "brainstorm_network" | "agent_council" | "network" => "agent_network",
        "workflow" => "run_workflow",
        "peer_message" => "peer_send",
        "broadcast" => "peer_broadcast",
        "peer_read_inbox" => "peer_read",
        _ => return None,
    })
}

pub fn normalize_tool_name(name: &str, has_exact: Option<&dyn Fn(&str) -> bool>) -> String {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    let re = PREFIX.get_or_init(|| Regex::new(r"(?i)^(?:multi_agent_v1|multi_agent|functions)\.").unwrap());
    let clean = re.replace(name.trim(), "").to_string();
    if let Some(f) = has_exact {
        if f(&clean) {
            return clean;
        }
    }
    tool_name_alias(&clean).or_else(|| tool_name_alias(&clean.to_lowercase())).map(str::to_string).unwrap_or(clean)
}

fn take(obj: &mut Map<String, Value>, keys: &[&str]) {
    for k in keys {
        obj.remove(*k);
    }
}

fn first_of<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| obj.get(*k).filter(|v| !v.is_null()))
}

fn items_text(obj: &Map<String, Value>) -> Option<String> {
    let items = obj.get("items")?.as_array()?;
    let parts: Vec<&str> = items.iter().filter_map(|it| it.get("text").and_then(Value::as_str)).filter(|t| !t.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Maps argument shapes used by other harnesses (Codex, Claude Code, OpenCode) to goon's.
pub fn normalize_tool_input(tool: &str, input: &Value) -> Value {
    if let Value::String(s) = input {
        return match tool {
            "delegate_task" | "delegate_edit" => json!({ "tasks": [s] }),
            "spawn_agent" => json!({ "message": s }),
            "wait_agent" => json!({ "targets": [s] }),
            "close_agent" | "resume_agent" => json!({ "target": s }),
            "view_image" => json!({ "path": s }),
            "agent_network" => json!({ "topic": s }),
            "run_workflow" => json!({ "name": s, "input": "" }),
            "peer_broadcast" => json!({ "topic": "General", "message": s }),
            _ => input.clone(),
        };
    }
    let Some(original) = input.as_object() else { return input.clone() };
    let mut obj = original.clone();
    let undefined = |o: &Map<String, Value>, k: &str| !o.contains_key(k);
    if ["read_file", "write_file", "edit_file", "list_files", "search", "view_image"].contains(&tool) {
        if undefined(&obj, "path") {
            if let Some(alias) = ["filePath", "file_path", "directory"].iter().find_map(|k| obj.get(*k).cloned()) {
                obj.insert("path".into(), alias);
            }
        }
        take(&mut obj, &["filePath", "file_path", "directory"]);
    }
    if tool == "search" {
        if undefined(&obj, "query") {
            if let Some(p) = obj.get("pattern").cloned() {
                obj.insert("query".into(), p);
            }
        }
        take(&mut obj, &["pattern"]);
    }
    if tool == "edit_file" {
        if undefined(&obj, "oldText") {
            if let Some(v) = ["old_string", "old_str"].iter().find_map(|k| obj.get(*k).cloned()) {
                obj.insert("oldText".into(), v);
            }
        }
        if undefined(&obj, "newText") {
            if let Some(v) = ["new_string", "new_str"].iter().find_map(|k| obj.get(*k).cloned()) {
                obj.insert("newText".into(), v);
            }
        }
        take(&mut obj, &["old_string", "old_str", "new_string", "new_str"]);
    }
    if tool == "bash" {
        if undefined(&obj, "command") {
            if let Some(v) = obj.get("cmd").cloned() {
                obj.insert("command".into(), v);
            }
        }
        take(&mut obj, &["cmd"]);
        if let Some(Value::String(c)) = obj.get("command") {
            let s = sanitize_bash_command(c);
            obj.insert("command".into(), Value::String(s));
        }
    }
    if tool == "delegate_task" || tool == "delegate_edit" {
        match obj.get("tasks").cloned() {
            None => match first_of(&obj, &["task", "prompt", "message", "instructions"]).cloned() {
                Some(Value::String(s)) => {
                    obj.insert("tasks".into(), json!([s]));
                }
                Some(Value::Array(a)) => {
                    obj.insert("tasks".into(), Value::Array(a));
                }
                _ => {}
            },
            Some(Value::String(s)) => {
                obj.insert("tasks".into(), json!([s]));
            }
            Some(Value::Array(items)) => {
                let mapped: Vec<Value> = items
                    .into_iter()
                    .map(|item| match &item {
                        Value::Object(rec) => first_of(rec, &["task", "prompt", "message", "description"]).filter(|v| v.is_string()).cloned().unwrap_or(item),
                        _ => item,
                    })
                    .collect();
                obj.insert("tasks".into(), Value::Array(mapped));
            }
            _ => {}
        }
        take(&mut obj, &["task", "prompt", "message", "instructions"]);
    }
    if tool == "spawn_agent" {
        if undefined(&obj, "message") {
            if let Some(Value::String(m)) = first_of(&obj, &["prompt", "task", "instruction", "instructions"]).cloned() {
                obj.insert("message".into(), json!(m));
            } else if let Some(text) = items_text(&obj) {
                obj.insert("message".into(), json!(text));
            }
        }
        if undefined(&obj, "agent_type") {
            if let Some(Value::String(r)) = first_of(&obj, &["role", "subagent_type", "type"]).cloned() {
                obj.insert("agent_type".into(), json!(r));
            }
        }
        if undefined(&obj, "nickname") {
            if let Some(Value::String(n)) = obj.get("task_name").cloned() {
                obj.insert("nickname".into(), json!(n));
            }
        }
        if undefined(&obj, "fork_context") {
            if let Some(ft) = obj.get("fork_turns").cloned() {
                obj.insert("fork_context".into(), json!(ft == json!("all") || ft == json!(true)));
            }
        }
        if obj.get("reasoning_effort") == Some(&json!("minimal")) {
            obj.insert("reasoning_effort".into(), json!("low"));
        }
        take(&mut obj, &["prompt", "task", "instruction", "instructions", "items", "role", "subagent_type", "type", "task_name", "fork_turns"]);
    }
    if tool == "wait_agent" {
        match first_of(&obj, &["targets", "ids", "target", "id", "agent_ids", "agent_id"]).cloned() {
            Some(Value::String(s)) => {
                obj.insert("targets".into(), json!([s]));
            }
            Some(Value::Array(a)) => {
                let list: Vec<Value> = a
                    .iter()
                    .map(|v| {
                        Value::String(match v {
                            Value::String(s) => s.clone(),
                            other => crate::jsutil::js_string(other),
                        })
                    })
                    .filter(|v| !v.as_str().unwrap().is_empty())
                    .collect();
                obj.insert("targets".into(), Value::Array(list));
            }
            _ => {}
        }
        if let Some(t) = obj.get("timeoutMs").filter(|v| v.is_number()).cloned() {
            if undefined(&obj, "timeout_ms") {
                obj.insert("timeout_ms".into(), t);
            }
        }
        take(&mut obj, &["ids", "target", "id", "agent_ids", "agent_id", "timeoutMs"]);
    }
    if tool == "send_input" {
        if undefined(&obj, "target") {
            if let Some(Value::String(t)) = first_of(&obj, &["id", "agent_id", "task_name", "recipient"]).cloned() {
                obj.insert("target".into(), json!(t));
            }
        }
        if undefined(&obj, "message") {
            if let Some(Value::String(m)) = first_of(&obj, &["prompt", "task", "input"]).cloned() {
                obj.insert("message".into(), json!(m));
            } else if let Some(text) = items_text(&obj) {
                obj.insert("message".into(), json!(text));
            }
        }
        take(&mut obj, &["id", "agent_id", "task_name", "recipient", "prompt", "task", "input", "items"]);
    }
    if tool == "close_agent" || tool == "resume_agent" {
        if undefined(&obj, "target") {
            if let Some(Value::String(t)) = first_of(&obj, &["id", "agent_id", "task_name"]).cloned() {
                obj.insert("target".into(), json!(t));
            }
        }
        take(&mut obj, &["id", "agent_id", "task_name"]);
    }
    if tool == "update_plan" {
        if undefined(&obj, "plan") {
            if let Some(Value::Array(alt)) = first_of(&obj, &["steps", "items", "todos"]).cloned() {
                let plan: Vec<Value> = alt
                    .into_iter()
                    .map(|item| match &item {
                        Value::String(s) => json!({ "step": s, "status": "pending" }),
                        Value::Object(rec) => {
                            let step = first_of(rec, &["step", "content", "title", "text"]).map(crate::jsutil::js_string).unwrap_or_default();
                            let status = match rec.get("status").and_then(Value::as_str) {
                                Some("in_progress") => "in_progress",
                                Some("completed") => "completed",
                                _ => "pending",
                            };
                            json!({ "step": step, "status": status })
                        }
                        _ => item,
                    })
                    .collect();
                obj.insert("plan".into(), Value::Array(plan));
            }
        }
        take(&mut obj, &["steps", "items", "todos"]);
    }
    if tool == "request_user_input" && undefined(&obj, "questions") {
        if let Some(Value::String(q)) = obj.get("question").cloned() {
            let raw: Vec<Value> = obj.get("options").and_then(Value::as_array).cloned().unwrap_or_else(|| vec![json!("Yes (Recommended)"), json!("No")]);
            let options: Vec<Value> = raw.into_iter().map(|o| if let Value::String(s) = o { json!({ "label": s, "description": "" }) } else { o }).collect();
            let id = obj.get("id").and_then(Value::as_str).unwrap_or("question_1").to_string();
            let header = obj.get("header").and_then(Value::as_str).unwrap_or("Question").to_string();
            obj.insert("questions".into(), json!([{ "id": id, "header": header, "question": q, "options": options }]));
            take(&mut obj, &["question", "options", "id", "header"]);
        }
    }
    if tool == "create_goal" {
        if undefined(&obj, "objective") {
            if let Some(Value::String(g)) = obj.get("goal").cloned() {
                obj.insert("objective".into(), json!(g));
                obj.remove("goal");
            }
        }
        if undefined(&obj, "token_budget") {
            if let Some(b) = obj.get("tokenBudget").filter(|v| v.is_number()).cloned() {
                obj.insert("token_budget".into(), b);
                obj.remove("tokenBudget");
            }
        }
    }
    if tool == "agent_network" {
        if undefined(&obj, "topic") {
            if let Some(Value::String(t)) = first_of(&obj, &["prompt", "message", "task", "question"]).cloned() {
                obj.insert("topic".into(), json!(t));
            }
        }
        take(&mut obj, &["prompt", "message", "task", "question"]);
    }
    if tool == "peer_send" {
        if undefined(&obj, "to") {
            if let Some(Value::String(t)) = first_of(&obj, &["target", "recipient", "agent", "id"]).cloned() {
                obj.insert("to".into(), json!(t));
            }
        }
        if undefined(&obj, "message") {
            if let Some(Value::String(m)) = first_of(&obj, &["content", "text", "prompt"]).cloned() {
                obj.insert("message".into(), json!(m));
            }
        }
        take(&mut obj, &["target", "recipient", "agent", "id", "content", "text", "prompt"]);
    }
    if tool == "peer_broadcast" {
        if undefined(&obj, "message") {
            if let Some(Value::String(m)) = first_of(&obj, &["content", "text", "prompt"]).cloned() {
                obj.insert("message".into(), json!(m));
            }
        }
        if undefined(&obj, "topic") {
            if let Some(Value::String(t)) = obj.get("title").cloned() {
                obj.insert("topic".into(), json!(t));
            }
        }
        if undefined(&obj, "topic") {
            obj.insert("topic".into(), json!("Shared Idea"));
        }
        take(&mut obj, &["content", "text", "prompt", "title"]);
    }
    Value::Object(obj)
}

/// Stable identity of a tool call for the rejection-loop guard (hashed: inputs may hold secrets).
pub fn tool_approval_signature(tool: &str, input: &Value) -> String {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(crate::jsutil::stable_stringify(input).as_bytes()));
    format!("{tool}:{}", &digest[..32])
}

fn builtin_catalog() -> &'static Map<String, Value> {
    static CATALOG: OnceLock<Map<String, Value>> = OnceLock::new();
    CATALOG.get_or_init(|| serde_json::from_str::<Value>(include_str!("builtin_tools.json")).unwrap().as_object().unwrap().clone())
}

/// Fields declared with zod `.trim()` (trimmed before validation).
pub fn builtin_trims(name: &str) -> Vec<&'static str> {
    match name {
        "delegate_task" | "delegate_edit" => vec!["tasks.*"],
        "spawn_agent" => vec!["message", "nickname", "model"],
        "wait_agent" => vec!["targets.*"],
        "send_input" => vec!["target", "message"],
        "close_agent" | "resume_agent" => vec!["target"],
        "agent_network" => vec!["topic", "roles.*"],
        "peer_send" => vec!["to", "message", "topic"],
        "peer_broadcast" => vec!["topic", "message"],
        "run_workflow" => vec!["name", "input"],
        _ => vec![],
    }
}

/// Definition of a built-in (or collaboration) tool from the exported zod catalog.
pub fn catalog_definition(name: &str, exec: Option<ToolExec>) -> ToolDefinition {
    let entry = builtin_catalog().get(name).unwrap_or_else(|| panic!("unknown catalog tool {name}"));
    let mut schema = entry["parameters"].clone();
    if let Some(o) = schema.as_object_mut() {
        o.remove("$schema");
    }
    ToolDefinition {
        description: entry["description"].as_str().unwrap_or("").to_string(),
        schema,
        read_only: entry["readOnly"].as_bool().unwrap_or(false),
        risk: None,
        net: None,
        trusted: None,
        validator: Validator::Zod(builtin_trims(name)),
        exec,
    }
}

const BUILTINS: [&str; 16] = [
    "read_file",
    "list_files",
    "search",
    "todo_write",
    "todo_read",
    "update_plan",
    "get_goal",
    "create_goal",
    "update_goal",
    "request_user_input",
    "view_image",
    "exit_plan_mode",
    "write_file",
    "edit_file",
    "apply_patch",
    "bash",
];

pub struct ToolRegistry {
    pub cwd: PathBuf,
    pub files: Arc<WorkspaceFiles>,
    pub approve: Approve,
    pub allowed_tools: Option<HashSet<String>>,
    flags: Mutex<RegistryFlags>,
    definitions: RwLock<IndexMap<String, ToolDefinition>>,
    pub todos: Mutex<Vec<TodoItem>>,
    pub callbacks: Mutex<RegistryCallbacks>,
    pending_images: Mutex<Vec<PendingImage>>,
    pub budget: Mutex<Option<SharedBudget>>,
    pub cost_ledger: Mutex<Option<Arc<CostLedger>>>,
    pub rejection_guard: Mutex<Option<Arc<dyn RejectionGuard>>>,
    pub policy: Mutex<Option<Arc<ExecutionPolicy>>>,
}

impl ToolRegistry {
    pub fn new(cwd: &Path, approve: Approve, read_only: bool, allowed_tools: Option<HashSet<String>>) -> Arc<ToolRegistry> {
        Self::with_files(cwd, Arc::new(WorkspaceFiles::new(cwd)), approve, read_only, allowed_tools)
    }

    pub fn with_files(cwd: &Path, files: Arc<WorkspaceFiles>, approve: Approve, read_only: bool, allowed_tools: Option<HashSet<String>>) -> Arc<ToolRegistry> {
        let mut definitions = IndexMap::new();
        for name in BUILTINS {
            definitions.insert(name.to_string(), catalog_definition(name, None));
        }
        Arc::new(ToolRegistry {
            cwd: cwd.to_path_buf(),
            files,
            approve,
            allowed_tools,
            flags: Mutex::new(RegistryFlags { read_only, ..Default::default() }),
            definitions: RwLock::new(definitions),
            todos: Mutex::new(vec![]),
            callbacks: Mutex::new(RegistryCallbacks::default()),
            pending_images: Mutex::new(vec![]),
            budget: Mutex::new(None),
            cost_ledger: Mutex::new(None),
            rejection_guard: Mutex::new(None),
            policy: Mutex::new(None),
        })
    }

    pub fn flags(&self) -> RegistryFlags {
        *self.flags.lock().unwrap()
    }

    pub fn update_flags(&self, f: impl FnOnce(&mut RegistryFlags)) {
        f(&mut self.flags.lock().unwrap());
    }

    pub fn read_only(&self) -> bool {
        self.flags().read_only
    }

    pub fn plan_mode(&self) -> bool {
        self.flags().plan_mode
    }

    pub fn set_plan_mode(&self, value: bool) {
        self.update_flags(|f| f.plan_mode = value);
    }

    pub fn set_read_only(&self, value: bool) {
        self.update_flags(|f| f.read_only = value);
    }

    pub fn effective_read_only(&self) -> bool {
        let f = self.flags();
        f.read_only || f.plan_mode
    }

    pub fn policy(&self) -> Option<Arc<ExecutionPolicy>> {
        self.policy.lock().unwrap().clone()
    }

    pub fn set_policy(&self, policy: Option<Arc<ExecutionPolicy>>) {
        *self.policy.lock().unwrap() = policy;
    }

    pub fn budget(&self) -> Option<SharedBudget> {
        self.budget.lock().unwrap().clone()
    }

    pub fn set_budget(&self, budget: Option<SharedBudget>) {
        *self.budget.lock().unwrap() = budget;
    }

    pub fn cost_ledger(&self) -> Option<Arc<CostLedger>> {
        self.cost_ledger.lock().unwrap().clone()
    }

    pub fn set_cost_ledger(&self, ledger: Option<Arc<CostLedger>>) {
        *self.cost_ledger.lock().unwrap() = ledger;
    }

    pub fn callbacks(&self) -> RegistryCallbacks {
        self.callbacks.lock().unwrap().clone()
    }

    pub fn todos(&self) -> Vec<TodoItem> {
        self.todos.lock().unwrap().clone()
    }

    pub fn set_todos(&self, todos: Vec<TodoItem>) {
        *self.todos.lock().unwrap() = todos;
    }

    pub fn take_pending_images(&self) -> Vec<PendingImage> {
        std::mem::take(&mut *self.pending_images.lock().unwrap())
    }

    pub fn register(&self, name: &str, definition: ToolDefinition) -> Result<()> {
        static NAME: OnceLock<Regex> = OnceLock::new();
        let re = NAME.get_or_init(|| Regex::new(r"^[a-z][a-z0-9_]*$").unwrap());
        let mut defs = self.definitions.write().unwrap();
        if !re.is_match(name) || defs.contains_key(name) {
            bail!("Invalid or duplicate tool: {name}");
        }
        defs.insert(name.to_string(), definition);
        Ok(())
    }

    /// Registers a tool described by a JSON schema (MCP). Unusable schemas fall back to "any object".
    pub fn register_json(&self, name: &str, description: &str, schema: &Value, read_only: bool, risk: Option<RiskClass>, net: Option<bool>, trusted: Option<bool>, exec: ToolExec) -> Result<()> {
        let mut clean = schema.clone();
        if let Some(o) = clean.as_object_mut() {
            o.remove("$schema");
            o.remove("$id");
        }
        let validator = if clean.is_object() || clean.is_boolean() { Validator::Json } else { Validator::ObjectOnly };
        self.register(name, ToolDefinition { description: description.into(), schema: clean, read_only, risk, net, trusted, validator, exec: Some(exec) })
    }

    pub fn unregister(&self, name: &str) {
        self.definitions.write().unwrap().shift_remove(name);
    }

    pub fn has(&self, name: &str) -> bool {
        self.definitions.read().unwrap().contains_key(name)
    }

    pub fn definition(&self, name: &str) -> Option<ToolDefinition> {
        self.definitions.read().unwrap().get(name).cloned()
    }

    pub fn resolve_tool_name(&self, name: &str) -> String {
        let defs = self.definitions.read().unwrap();
        normalize_tool_name(name, Some(&|n: &str| defs.contains_key(n)))
    }

    fn is_exposed(&self, name: &str, d: &ToolDefinition) -> bool {
        if let Some(allowed) = &self.allowed_tools {
            if !tool_allowed_by_set(allowed, name) {
                return false;
            }
        }
        let f = self.flags();
        if f.only_peer_tools {
            return PEER_TOOLS.contains(&name);
        }
        if name == "exit_plan_mode" && !f.plan_mode {
            return false;
        }
        if (name == "get_goal" || name == "create_goal" || name == "update_goal") && (self.callbacks.lock().unwrap().get_goal.is_none() || f.plan_mode) {
            return false;
        }
        if PEER_TOOLS.contains(&name) && f.read_only && !f.plan_mode && !f.allow_peer_tools_in_read_only {
            return false;
        }
        if (name == "todo_write" || name == "todo_read" || COLLAB_TOOLS.contains(&name)) && f.read_only && !f.plan_mode {
            return false;
        }
        !(f.read_only || f.plan_mode) || d.read_only
    }

    pub fn exposed_tool_names(&self) -> Vec<String> {
        let defs = self.definitions.read().unwrap().clone();
        defs.iter().filter(|(n, d)| self.is_exposed(n, d)).map(|(n, _)| n.clone()).collect()
    }

    /// Tool specs offered to the model in the current mode.
    pub fn schemas(&self) -> Vec<ToolSpec> {
        let defs = self.definitions.read().unwrap().clone();
        defs.iter().filter(|(n, d)| self.is_exposed(n, d)).map(|(n, d)| ToolSpec { name: n.clone(), description: d.description.clone(), parameters: d.schema.clone() }).collect()
    }

    /// Validators for the exposed tools (the AI SDK parses tool-call input before the harness sees it).
    pub fn exposed_definitions(&self) -> IndexMap<String, ToolDefinition> {
        let defs = self.definitions.read().unwrap().clone();
        defs.into_iter().filter(|(n, d)| self.is_exposed(n, d)).collect()
    }

    pub fn is_read_only_tool(&self, name: &str) -> bool {
        let canonical = self.resolve_tool_name(name);
        if ["exit_plan_mode", "todo_write", "update_plan", "create_goal", "update_goal", "request_user_input", "wait_agent", "send_input", "close_agent", "resume_agent"].contains(&canonical.as_str())
        {
            return false;
        }
        self.definitions.read().unwrap().get(&canonical).map(|d| d.read_only).unwrap_or(false)
    }

    pub fn risk_of(&self, name: &str, input: Option<&Value>) -> RiskClass {
        let canonical = self.resolve_tool_name(name);
        if canonical == "bash" {
            if let Some(command) = input.and_then(|i| i.get("command")).and_then(Value::as_str) {
                return classify_bash_risk(command);
            }
        }
        let defs = self.definitions.read().unwrap();
        match defs.get(&canonical) {
            Some(d) if d.risk.is_some() => d.risk.unwrap(),
            Some(d) if d.read_only => RiskClass::Low,
            _ => RiskClass::Medium,
        }
    }

    async fn deny(&self, call_key: &str, message: String) -> ExecuteOutcome {
        let guard = self.rejection_guard.lock().unwrap().clone();
        let count = guard.map(|g| g.record_denied(call_key));
        let loop_detected = count.map(|c| c >= 3).unwrap_or(false);
        let text = if loop_detected { format!("{message} Repeated denials for identical arguments; stop this run.") } else { message };
        ExecuteOutcome { text, denied: true, loop_detected }
    }

    pub async fn execute(self: &Arc<Self>, call: &ToolCall, cancel: &CancellationToken) -> Result<ExecuteOutcome> {
        if cancel.is_cancelled() {
            bail!("This operation was aborted");
        }
        let canonical = self.resolve_tool_name(&call.tool_name);
        let Some(definition) = self.definition(&canonical) else { bail!("Unknown tool: {}", call.tool_name) };
        if canonical != "exit_plan_mode" && !self.is_exposed(&canonical, &definition) {
            let mode = if self.plan_mode() {
                "plan"
            } else if self.effective_read_only() {
                "read-only"
            } else {
                "the current"
            };
            bail!("Tool is not available in {mode} mode: {}", call.tool_name);
        }
        let normalized = normalize_tool_input(&canonical, &call.input);
        let input = definition.parse(&canonical, &normalized)?;
        let risk = self.risk_of(&canonical, Some(&input));
        let call_key = tool_approval_signature(&canonical, &normalized);
        if risk == RiskClass::Critical {
            return Ok(self.deny(&call_key, format!("Permission denied: {canonical} is critical risk and cannot run. Stop this run.")).await);
        }
        if let Some(policy) = self.policy() {
            if canonical == "bash" {
                if let Some(command) = input.get("command").and_then(Value::as_str) {
                    if !is_network_allowed_for_command(&policy, command) {
                        return Ok(self.deny(&call_key, network_denied_message(command)).await);
                    }
                }
            } else if definition.net == Some(true) && policy.mode != NetworkMode::Allow {
                return Ok(self
                    .deny(&call_key, "Permission denied: this MCP tool is marked as network/open-world and network access is not permitted. Use --allow-net or set allowNet on the MCP server.".into())
                    .await);
            }
        }
        let trusted_auto = definition.trusted == Some(true) && (risk == RiskClass::Medium || risk == RiskClass::Low);
        let needs_approval = !definition.read_only || risk != RiskClass::Low;
        if needs_approval && !trusted_auto {
            let approved =
                (self.approve)(ApprovalRequest { tool: canonical.clone(), input: input.clone(), cancel: cancel.clone(), risk: Some(risk), trusted: Some(definition.trusted == Some(true)) }).await;
            if !approved {
                return Ok(self.deny(&call_key, "Permission denied by the user. Stop this run.".into()).await);
            }
        }
        if let Some(g) = self.rejection_guard.lock().unwrap().clone() {
            g.reset(&call_key);
        }
        if cancel.is_cancelled() {
            bail!("This operation was aborted");
        }
        let result = match &definition.exec {
            Some(exec) => exec(input, cancel.clone()).await,
            None => self.run_builtin(&canonical, input, cancel).await,
        };
        match result {
            Ok(text) => Ok(ExecuteOutcome { text, denied: false, loop_detected: false }),
            Err(error) => {
                if let Some(d) = error.downcast_ref::<PermissionDenied>() {
                    return Ok(self.deny(&call_key, d.0.clone()).await);
                }
                Err(error)
            }
        }
    }

    fn goal_json(goal: Option<&SessionGoal>, report: Option<String>) -> String {
        let remaining = goal.and_then(|g| g.token_budget.map(|b| crate::jsutil::num((b - g.tokens_used).max(0.0)))).unwrap_or(Value::Null);
        crate::jsutil::stringify(&json!({
            "goal": goal.map(|g| serde_json::to_value(g).unwrap()).unwrap_or(Value::Null),
            "remainingTokens": remaining,
            "completionBudgetReport": report.map(Value::String).unwrap_or(Value::Null),
        }))
    }

    async fn run_builtin(self: &Arc<Self>, name: &str, input: Value, cancel: &CancellationToken) -> Result<String> {
        let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let n = |k: &str, d: u64| input.get(k).and_then(Value::as_u64).unwrap_or(d);
        match name {
            "read_file" => self.files.read(&s("path"), n("offset", 1) as usize, n("lines", 200) as usize),
            "list_files" => self.files.list(&s("path")),
            "search" => {
                let path = self.files.path(&s("path"))?;
                let rel = crate::files::relative_path(&self.files.root, &path);
                let target = if rel.is_empty() { ".".to_string() } else { rel };
                let args: Vec<String> =
                    ["--fixed-strings", "--line-number", "--hidden", "--glob", "!.git", "--glob", "!node_modules", "--max-count", "50", "--max-columns", "1000", "--max-columns-preview", "--"]
                        .iter()
                        .map(|a| a.to_string())
                        .chain([s("query"), target])
                        .collect();
                let result = run_command("rg", &args, RunOptions { cwd: &self.files.root, cancel, timeout_ms: 30_000, env: None, output_limit_bytes: None, hard_output_bytes: None }).await;
                match result {
                    Ok(r) => {
                        static HIT: OnceLock<Regex> = OnceLock::new();
                        let hit = HIT.get_or_init(|| Regex::new(r"(?m)^[^\n]+:\d+:").unwrap());
                        if r.code > 1 && !hit.is_match(&r.output) {
                            bail!("{}", if r.output.is_empty() { "ripgrep failed.".to_string() } else { r.output });
                        }
                        let note = if r.code > 1 { "\n[ripgrep reported errors for some paths; results may be incomplete.]" } else { "" };
                        Ok(format!("{}{note}", limit_text_default(if r.output.is_empty() { "No matches found." } else { &r.output })))
                    }
                    Err(CommandError::NotFound(_)) => {
                        if cancel.is_cancelled() {
                            bail!("This operation was aborted");
                        }
                        self.files.fallback_search(&s("query"), &s("path"), cancel)
                    }
                    Err(e) => Err(anyhow!(e.to_string())),
                }
            }
            "todo_write" => {
                let todos: Vec<TodoItem> = serde_json::from_value(input.get("todos").cloned().unwrap_or(json!([])))?;
                self.set_todos(todos.clone());
                if let Some(cb) = self.callbacks().on_todo_update {
                    cb(todos.clone());
                }
                Ok(format!("Updated todo list.\n{}", format_todos(&todos)))
            }
            "todo_read" => Ok(format_todos(&self.todos())),
            "update_plan" => {
                let plan = input.get("plan").and_then(Value::as_array).cloned().unwrap_or_default();
                let todos: Vec<TodoItem> = plan
                    .iter()
                    .enumerate()
                    .map(|(i, item)| TodoItem {
                        id: Some((i + 1).to_string()),
                        content: item["step"].as_str().unwrap_or("").into(),
                        status: item["status"].as_str().unwrap_or("pending").into(),
                        priority: "medium".into(),
                    })
                    .collect();
                self.set_todos(todos.clone());
                if let Some(cb) = self.callbacks().on_todo_update {
                    cb(todos.clone());
                }
                let header = match input.get("explanation").and_then(Value::as_str).filter(|e| !e.is_empty()) {
                    Some(e) => format!("Plan updated ({e})."),
                    None => "Plan updated.".into(),
                };
                Ok(format!("{header}\n{}", format_todos(&todos)))
            }
            "get_goal" => {
                let goal = self.callbacks().get_goal.and_then(|g| g());
                Ok(Self::goal_json(goal.as_ref(), None))
            }
            "create_goal" => {
                let cbs = self.callbacks();
                let (Some(get), Some(update)) = (cbs.get_goal, cbs.on_goal_update) else { bail!("Goal tools require a persistent thread.") };
                if let Some(existing) = get() {
                    if existing.status != "complete" {
                        bail!("cannot create a new goal because this thread has an unfinished goal; complete the existing goal first");
                    }
                }
                let objective = s("objective").trim().to_string();
                if objective.is_empty() {
                    bail!("Goal objective must not be empty.");
                }
                let now = crate::jsutil::now_iso();
                let goal = SessionGoal {
                    goal_id: Some(uuid::Uuid::new_v4().to_string()),
                    objective,
                    status: "active".into(),
                    created_at: now.clone(),
                    updated_at: now,
                    tokens_at_start: 0.0,
                    tokens_used: 0.0,
                    time_used_seconds: 0.0,
                    token_budget: input.get("token_budget").and_then(Value::as_f64),
                };
                update(Some(goal.clone())).await?;
                let updated = get().unwrap_or(goal);
                Ok(Self::goal_json(Some(&updated), None))
            }
            "update_goal" => {
                let cbs = self.callbacks();
                let (Some(get), Some(update)) = (cbs.get_goal, cbs.on_goal_update) else { bail!("Goal tools require a persistent thread.") };
                let Some(existing) = get() else { bail!("cannot update goal because this thread has no goal") };
                let status = s("status");
                let mut next = existing;
                next.status = status.clone();
                next.updated_at = crate::jsutil::now_iso();
                update(Some(next.clone())).await?;
                let updated = get().unwrap_or(next);
                let report = (status == "complete" && updated.token_budget.is_some()).then(|| {
                    format!(
                        "Consumed {} of {} budgeted tokens in {}s.",
                        crate::jsutil::stringify(&crate::jsutil::num(updated.tokens_used)),
                        crate::jsutil::stringify(&crate::jsutil::num(updated.token_budget.unwrap())),
                        crate::jsutil::stringify(&crate::jsutil::num(updated.time_used_seconds))
                    )
                });
                Ok(Self::goal_json(Some(&updated), report))
            }
            "request_user_input" => {
                let questions = input.get("questions").and_then(Value::as_array).cloned().unwrap_or_default();
                if let Some(ask) = self.callbacks().ask_user {
                    let answers = ask(questions, cancel.clone()).await?;
                    let mut formatted = Map::new();
                    for (id, list) in answers {
                        formatted.insert(id, json!({ "answers": list }));
                    }
                    return Ok(crate::jsutil::stringify(&json!({ "answers": formatted })));
                }
                let mut fallback = Map::new();
                for q in &questions {
                    let label = q.pointer("/options/0/label").and_then(Value::as_str).unwrap_or("");
                    fallback.insert(q["id"].as_str().unwrap_or("").to_string(), json!({ "answers": [label] }));
                }
                Ok(crate::jsutil::stringify(&json!({ "answers": fallback, "note": "Non-interactive mode: selected default first option." })))
            }
            "view_image" => {
                let path = s("path");
                let (resolved, inside) = self.files.locate_image(&path)?;
                if !inside {
                    let ok = (self.approve)(ApprovalRequest {
                        tool: "view_image".into(),
                        input: json!({ "path": resolved.to_string_lossy(), "outsideWorkspace": true }),
                        cancel: cancel.clone(),
                        risk: None,
                        trusted: None,
                    })
                    .await;
                    if !ok {
                        return Err(PermissionDenied("Permission denied: reading an image outside the workspace. Stop this run.".into()).into());
                    }
                }
                let img = self.files.read_image(&path, 5 * 1024 * 1024, !inside)?;
                let resolved = img.resolved_path.to_string_lossy().to_string();
                let len = img.bytes.len();
                let media = img.media_type.clone();
                self.pending_images.lock().unwrap().push(PendingImage { path: resolved.clone(), mime_type: img.media_type, bytes: img.bytes });
                Ok(format!("Attached image {resolved} ({media}, {len} bytes)."))
            }
            "exit_plan_mode" => {
                if !self.plan_mode() {
                    return Ok("Session is not in plan mode; you can already edit files and run tools directly.".into());
                }
                let ok = (self.approve)(ApprovalRequest { tool: "exit_plan_mode".into(), input: input.clone(), cancel: cancel.clone(), risk: None, trusted: None }).await;
                if !ok {
                    return Ok("User chose to stay in plan mode and revise the plan. Incorporate any feedback and call exit_plan_mode again when ready.".into());
                }
                self.set_plan_mode(false);
                let plan = s("plan");
                if let Some(cb) = self.callbacks().on_plan_exit {
                    cb(plan.clone());
                }
                Ok(format!(
                    "Plan approved — exited plan mode. You can now use write_file, edit_file, apply_patch, todo_write, and bash to implement the plan step by step.\n\n## Approved Plan:\n{plan}"
                ))
            }
            "write_file" => self.files.write(&s("path"), &s("content")),
            "edit_file" => self.files.edit(&s("path"), &s("oldText"), &s("newText"), input.get("replaceAll").and_then(Value::as_bool).unwrap_or(false)),
            "apply_patch" => self.files.apply_patch(&s("patch")),
            "bash" => {
                let command = sanitize_bash_command(&s("command"));
                let policy = self.policy();
                let limits = policy.as_ref().map(|p| p.limits.clone());
                let requested = input.get("timeoutMs").and_then(Value::as_u64).unwrap_or_else(|| limits.as_ref().map(|l| l.default_tool_timeout_ms).unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS));
                let max = limits.as_ref().map(|l| l.max_tool_timeout_ms).unwrap_or(MAX_COMMAND_TIMEOUT_MS);
                let timeout = requested.min(max).max(1000);
                let result = run_command(
                    "bash",
                    &["-c".to_string(), command],
                    RunOptions {
                        cwd: &self.cwd,
                        cancel,
                        timeout_ms: timeout,
                        env: None,
                        output_limit_bytes: limits.as_ref().map(|l| l.max_output_bytes),
                        hard_output_bytes: limits.as_ref().map(|l| l.hard_output_bytes),
                    },
                )
                .await;
                if crate::jsutil::is_windows() {
                    let _ = crate::files::unlink_safe(&self.cwd.join("nul"));
                }
                let r = result.map_err(|e| anyhow!(e.to_string()))?;
                Ok(format!("Exit code: {}\n{}", r.code, r.output))
            }
            other => bail!("Unknown tool: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_and_inputs() {
        assert_eq!(normalize_tool_name("functions.read", None), "read_file");
        assert_eq!(normalize_tool_name("Glob", None), "list_files");
        assert_eq!(normalize_tool_input("read_file", &json!({"file_path": "a"})), json!({"path": "a"}));
        assert_eq!(normalize_tool_input("edit_file", &json!({"path": "a", "old_string": "x", "new_string": "y"})), json!({"path": "a", "oldText": "x", "newText": "y"}));
        assert_eq!(normalize_tool_input("wait_agent", &json!({"id": "abc", "timeoutMs": 500})), json!({"targets": ["abc"], "timeout_ms": 500}));
        assert_eq!(normalize_tool_input("delegate_task", &json!("x")), json!({"tasks": ["x"]}));
        assert_eq!(
            normalize_tool_input("update_plan", &json!({"steps": ["a", {"title": "b", "status": "completed"}]})),
            json!({"plan": [{"step": "a", "status": "pending"}, {"step": "b", "status": "completed"}]})
        );
    }

    #[tokio::test]
    async fn gates_and_builtins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let registry = ToolRegistry::new(dir.path(), deny_all(), false, None);
        let cancel = CancellationToken::new();
        let read = registry.execute(&ToolCall { tool_call_id: "1".into(), tool_name: "read".into(), input: json!({"path": "a.txt"}) }, &cancel).await.unwrap();
        assert!(read.text.contains("one"), "{}", read.text);
        let denied = registry.execute(&ToolCall { tool_call_id: "2".into(), tool_name: "bash".into(), input: json!({"command": "echo hi"}) }, &cancel).await.unwrap();
        assert!(denied.denied);
        let critical = registry.execute(&ToolCall { tool_call_id: "3".into(), tool_name: "bash".into(), input: json!({"command": "rm -rf /"}) }, &cancel).await.unwrap();
        assert!(critical.text.contains("critical risk"));
        assert!(registry.execute(&ToolCall { tool_call_id: "4".into(), tool_name: "nope".into(), input: json!({}) }, &cancel).await.is_err());
        registry.set_read_only(true);
        assert!(!registry.exposed_tool_names().contains(&"bash".to_string()));
        assert!(registry.exposed_tool_names().contains(&"read_file".to_string()));
    }
}
