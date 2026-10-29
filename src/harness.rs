//! The agent loop: one user turn = model steps + tool batches until the model answers without
//! tool calls. Handles retries, compaction, context budget, cost/time caps, goal accounting,
//! verification hooks with rollback, and per-turn checkpoints.

use crate::config::RuntimeConfig;
use crate::context::{build_goal_budget_limit_prompt, compact_if_needed, compact_now, estimated_tokens, expand_file_mentions, recap_session, system_prompt, MentionOptions, SystemPromptCache};
use crate::context_budget::{allocate_context_budget, prepare_context_budget, render_memory_context, ContextBudgetConfig, MemoryPromptContext};
use crate::cost::{create_run_budget, to_usage_split, CostLedger, SharedBudget, UsageSplit};
use crate::errors::safe_error;
use crate::files::FileBackup;
use crate::jsutil::{canonicalize, now_iso, now_millis, stringify};
use crate::model::{ApiError, FinishReason, LanguageModel, ModelRequest, StreamPart, Usage};
use crate::provider::{auxiliary_effort, create_model, resolve_model_capabilities};
use crate::reasoning::{visible_answer, with_reasoning_replay, AnswerPart, AnswerProjection};
use crate::session::{repair_interrupted, SessionGoal, SessionStore, TurnCheckpoint};
use crate::tools::{normalize_tool_input, normalize_tool_name, ToolCall, ToolDefinition, ToolRegistry};
use crate::verify::{assess_verify_plan, format_verify_errors, format_verify_warnings, plan_verification, redact_text, redact_verdict, run_verification, CommandRunner, Severity, VerificationFailed, VerifyRunner, VerifyVerdict};
use crate::config::VerifyPolicy;
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;

pub fn noop_emit() -> Emit {
    Arc::new(|_| {})
}

/// The user cancelled (Ctrl-C / signal).
#[derive(Debug, Clone)]
pub struct Aborted(pub String);

impl Default for Aborted {
    fn default() -> Self {
        Aborted("This operation was aborted".into())
    }
}

impl std::fmt::Display for Aborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Aborted {}

pub fn is_aborted(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Aborted>().is_some()
}

#[derive(Clone, Debug, Default)]
pub struct RepoIndexOutcome {
    pub files: u64,
    pub chunks: u64,
    pub duration_ms: u64,
    pub reindexed: String,
}

/// Memory seam (P19): the store redacts and serializes writes; failures never fail a run.
pub trait MemoryHooks: Send + Sync {
    fn handle_event(&self, event: Value) -> BoxFuture<'static, Result<()>>;
    fn retrieve(&self, query: String, budget_tokens: u64) -> BoxFuture<'static, Result<MemoryPromptContext>>;
    fn ensure_repo_indexed(&self, cwd: PathBuf) -> BoxFuture<'static, Result<RepoIndexOutcome>>;
}

#[derive(Default, Clone)]
pub struct HarnessDeps {
    pub verify_enabled: Option<bool>,
    pub verify_runner: Option<Arc<dyn VerifyRunner>>,
    pub verify_policy: Option<VerifyPolicy>,
    pub memory: Option<Arc<dyn MemoryHooks>>,
}

pub const MAX_API_RETRIES: u32 = 3;
pub const MAX_PARALLEL_READS: usize = 6;
const RETRY_BASE_MS: f64 = 250.0;
const RETRY_MAX_MS: f64 = 8_000.0;
const RETRY_MAX_AFTER_MS: f64 = 20_000.0;

fn retry_delay_ms(error: &anyhow::Error, attempt: u32) -> u64 {
    if let Some(api) = error.downcast_ref::<ApiError>() {
        if let Some(raw) = api.headers.get("retry-after") {
            let ms = match raw.trim().parse::<f64>() {
                Ok(seconds) => seconds * 1000.0,
                Err(_) => chrono::DateTime::parse_from_rfc2822(raw.trim()).map(|d| (d.timestamp_millis() - now_millis()) as f64).unwrap_or(f64::NAN),
            };
            if ms.is_finite() && ms > 0.0 {
                return ms.min(RETRY_MAX_AFTER_MS) as u64;
            }
        }
    }
    let exponential = RETRY_MAX_MS.min(RETRY_BASE_MS * 2f64.powi(attempt as i32));
    let jitter = (uuid::Uuid::new_v4().as_u128() % 1000) as f64 / 1000.0;
    crate::jsutil::js_round(exponential + jitter * exponential * 0.25) as u64
}

fn is_non_retryable(error: &anyhow::Error, cancel: &CancellationToken) -> bool {
    if cancel.is_cancelled() {
        return true;
    }
    if let Some(api) = error.downcast_ref::<ApiError>() {
        if matches!(api.status, Some(400) | Some(401) | Some(403) | Some(404) | Some(422)) {
            return true;
        }
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)\b(?:401|403|authentication_error|invalid_api_key|unauthorized)\b").unwrap());
    let msg = error.to_string();
    re.is_match(&msg) || msg.starts_with("Model response hit the output limit") || msg.starts_with("Invalid or excessive tool call batch") || msg.starts_with("Model stopped unexpectedly (content-filter)")
}

pub struct RevertResult {
    pub turns_reverted: usize,
    pub messages_removed: usize,
    pub restored_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub reverted_prompts: Vec<String>,
    pub conflict_files: Vec<String>,
    pub unrecoverable_files: Vec<String>,
}

fn json_string_field(raw: &str, names: &str) -> Option<String> {
    let re = Regex::new(&format!(r#""(?:{names})"\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)""#)).ok()?;
    re.captures(raw).map(|c| c[1].to_string())
}

pub fn extract_stream_preview(raw: &str) -> String {
    let target = json_string_field(raw, "path|filePath|file_path|directory")
        .or_else(|| json_string_field(raw, "command|cmd"))
        .or_else(|| json_string_field(raw, "query|pattern"))
        .unwrap_or_default()
        .replace("\\\"", "\"")
        .replace("\\\\", "\\");
    static SPLIT: OnceLock<Regex> = OnceLock::new();
    let lines = SPLIT.get_or_init(|| Regex::new(r"\\n|\n").unwrap()).split(raw).count();
    let kb = crate::jsutil::to_fixed(raw.len() as f64 / 1024.0, 1);
    let len = crate::jsutil::utf16_len(raw);
    if !target.is_empty() && len > 120 {
        return format!("{target} ({lines} lines · {kb} KB)");
    }
    if !target.is_empty() {
        return target;
    }
    if len > 80 {
        return format!("streaming {lines} lines ({kb} KB)…");
    }
    String::new()
}

fn collapse(text: &str) -> String {
    static WS: OnceLock<Regex> = OnceLock::new();
    WS.get_or_init(|| Regex::new(r"\s+").unwrap()).replace_all(text.trim(), " ").to_string()
}

fn patch_files(patch: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\*\*\* (?:Update|Add|Delete) File:\s*(.+)$").unwrap()).captures_iter(patch).map(|c| c[1].trim().to_string()).collect()
}

/// Tool event metadata (`kind` + one-line `summary`).
pub fn build_tool_meta(tool_name: &str, raw_input: &Value) -> Map<String, Value> {
    let canonical = normalize_tool_name(tool_name, None);
    let norm = normalize_tool_input(&canonical, raw_input);
    let empty = Map::new();
    let rec = norm.as_object().unwrap_or(&empty);
    let str_of = |k: &str| rec.get(k).and_then(Value::as_str);
    let path = str_of("path").map(str::trim).filter(|p| !p.is_empty()).unwrap_or(".").to_string();
    let take = |s: &str, n: usize| crate::jsutil::take_chars(s, n).to_string();
    let (kind, summary) = match canonical.as_str() {
        "read_file" | "view_image" => {
            let range = match rec.get("startLine").and_then(Value::as_f64) {
                Some(start) => format!(":{}{}", stringify(&crate::jsutil::num(start)), rec.get("endLine").and_then(Value::as_f64).map(|e| format!("-{}", stringify(&crate::jsutil::num(e)))).unwrap_or_default()),
                None => String::new(),
            };
            ("read", format!("{path}{range}"))
        }
        "list_files" => ("list", path),
        "search" => {
            let q = str_of("query").map(str::trim).unwrap_or("").to_string();
            ("search", if path != "." { format!("{q} in {path}") } else { q })
        }
        "bash" => ("exec", take(&collapse(str_of("command").unwrap_or("")), 140)),
        "write_file" | "edit_file" | "apply_patch" => {
            if canonical == "apply_patch" {
                if let Some(p) = str_of("patch") {
                    let files = patch_files(p);
                    return meta("edit", if files.is_empty() { "patch".into() } else { files.join(", ") });
                }
            }
            ("edit", path)
        }
        "update_plan" => ("other", format!("{} step(s)", rec.get("plan").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0))),
        "get_goal" => ("other", "inspect active goal".into()),
        "create_goal" => ("other", str_of("objective").map(|o| take(o.trim(), 80)).unwrap_or_else(|| "new goal".into())),
        "update_goal" => ("other", format!("status -> {}", str_of("status").unwrap_or("status"))),
        "request_user_input" => ("other", format!("{} question(s)", rec.get("questions").and_then(Value::as_array).map(|a| a.len()).unwrap_or(1))),
        "list_mcp_resources" | "list_mcp_resource_templates" => ("list", str_of("server").unwrap_or("all servers").to_string()),
        "read_mcp_resource" => ("read", format!("{}:{}", str_of("server").unwrap_or(""), str_of("uri").unwrap_or(""))),
        "spawn_agent" => {
            let role = str_of("agent_type").or(str_of("role")).unwrap_or("default");
            let msg = str_of("message").or(str_of("prompt")).or(str_of("task")).unwrap_or("");
            ("other", format!("[{role}] {}", take(&collapse(msg), 100)).trim().to_string())
        }
        "wait_agent" => {
            let targets = match (rec.get("targets").and_then(Value::as_array), rec.get("ids").and_then(Value::as_array), str_of("target")) {
                (Some(t), _, _) => t.iter().map(crate::jsutil::js_string).collect::<Vec<_>>().join(", "),
                (None, Some(i), _) => i.iter().map(crate::jsutil::js_string).collect::<Vec<_>>().join(", "),
                (None, None, Some(t)) => t.to_string(),
                _ => "agents".into(),
            };
            ("other", format!("waiting for {targets}"))
        }
        "send_input" => ("other", format!("to {}", str_of("target").or(str_of("id")).unwrap_or("agent"))),
        "close_agent" => ("other", str_of("target").or(str_of("id")).unwrap_or("agent").to_string()),
        "delegate_task" | "delegate_edit" => ("other", format!("{} subagent task(s)", rec.get("tasks").and_then(Value::as_array).map(|a| a.len()).unwrap_or(1))),
        "agent_network" => {
            let topic = str_of("topic").map(|t| take(&collapse(t), 80)).unwrap_or_else(|| "brainstorm".into());
            let rounds = rec.get("rounds").and_then(Value::as_f64).map(|r| format!("{}R", stringify(&crate::jsutil::num(r)))).unwrap_or_else(|| "2R".into());
            ("other", format!("[{rounds}] {topic}"))
        }
        "peer_send" => ("other", format!("→ {} ({})", str_of("to").unwrap_or("peer"), str_of("kind").unwrap_or("message"))),
        "peer_broadcast" => ("other", format!("broadcast: {}", str_of("topic").map(|t| take(t, 60)).unwrap_or_else(|| "blackboard".into()))),
        "peer_read" => ("read", "agent network inbox & blackboard".into()),
        "run_workflow" => ("other", str_of("name").unwrap_or("workflow").to_string()),
        _ => ("other", canonical.clone()),
    };
    meta(kind, summary)
}

fn meta(kind: &str, summary: String) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("kind".into(), json!(kind));
    m.insert("summary".into(), json!(summary));
    m
}

pub fn summarize_diff_counts(diff: Option<&str>) -> (u64, u64, &'static str) {
    let Some(diff) = diff.filter(|d| !d.is_empty()) else { return (0, 0, "Edited") };
    static NEW: OnceLock<Regex> = OnceLock::new();
    static DEL: OnceLock<Regex> = OnceLock::new();
    static MORE_ADD: OnceLock<Regex> = OnceLock::new();
    static MORE_DEL: OnceLock<Regex> = OnceLock::new();
    let new_re = NEW.get_or_init(|| Regex::new(r"^--- a/.+ \(new \+\d+ -\d+\)$").unwrap());
    let del_re = DEL.get_or_init(|| Regex::new(r"^--- a/.+ \(deleted \+\d+ -\d+\)$").unwrap());
    let more_add = MORE_ADD.get_or_init(|| Regex::new(r"^\.\.\. \(\+(\d+) more added lines\)$").unwrap());
    let more_del = MORE_DEL.get_or_init(|| Regex::new(r"^\.\.\. \(-(\d+) more removed lines\)$").unwrap());
    let (mut added, mut removed, mut is_new, mut is_deleted) = (0u64, 0u64, false, false);
    for line in diff.split('\n') {
        if line.starts_with("--- /dev/null") || new_re.is_match(line) {
            is_new = true;
        } else if line.starts_with("+++ /dev/null") || del_re.is_match(line) {
            is_deleted = true;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        } else {
            if let Some(c) = more_add.captures(line) {
                added += c[1].parse::<u64>().unwrap_or(0);
            }
            if let Some(c) = more_del.captures(line) {
                removed += c[1].parse::<u64>().unwrap_or(0);
            }
        }
    }
    (added, removed, if is_deleted { "Deleted" } else if is_new { "Added" } else { "Edited" })
}

fn call_signature(call: &ToolCall) -> String {
    format!("{}:{}", call.tool_name, stringify(&canonicalize(&call.input)))
}

/// A cancellation token that also fires after `duration` (`AbortSignal.any([signal, timeout])`).
pub struct Deadline {
    pub token: CancellationToken,
    timer: tokio::task::JoinHandle<()>,
}

impl Deadline {
    pub fn new(parent: &CancellationToken, duration: Duration) -> Deadline {
        let token = parent.child_token();
        let t = token.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            t.cancel();
        });
        Deadline { token, timer }
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.timer.abort();
    }
}

/// Output of one model step (AI SDK `streamText` semantics).
struct StepResult {
    text: String,
    calls: Vec<ToolCall>,
    assistant_messages: Vec<Value>,
    usage: Usage,
}

#[derive(Default)]
struct HarnessState {
    running: bool,
    pending_context: Vec<Value>,
    last_call_sig: String,
    repeat_count: u32,
    consecutive_goal_exec_failure_turns: u32,
    turn_had_successful_tool: bool,
    turn_had_failed_exec: bool,
    verify_attempts: u64,
    turn_had_edit: bool,
    last_verify_verdict: Option<VerifyVerdict>,
    verify_rolled_back: bool,
    repo_indexed_once: bool,
}

#[derive(Default)]
struct GoalClock {
    active: bool,
    last_ms: i64,
    last_tokens: f64,
}

pub struct Harness {
    pub config: Arc<RuntimeConfig>,
    pub store: Arc<SessionStore>,
    pub tools: Arc<ToolRegistry>,
    pub emit: Emit,
    pub model: Arc<dyn LanguageModel>,
    pub cost_ledger: Arc<CostLedger>,
    pub deps: HarnessDeps,
    /// Prompts written by a model (subagent tasks) may only attach workspace images.
    pub untrusted_prompts: Mutex<bool>,
    state: Mutex<HarnessState>,
    goal_clock: Arc<Mutex<GoalClock>>,
}

/// Mirrors goal time/token usage for the active turn; `allow_stopped` also flushes paused goals.
fn flush_goal(store: &SessionStore, tools: &ToolRegistry, clock: &Mutex<GoalClock>, emit: &Emit, allow_stopped: bool) {
    let mut clock = clock.lock().unwrap();
    if !clock.active {
        return;
    }
    let mut session = store.session.lock().unwrap();
    let tokens = session.tokens;
    let Some(goal) = session.goal.as_mut() else { return };
    if tools.plan_mode() {
        return;
    }
    if goal.status != "active" && !allow_stopped {
        return;
    }
    let now = now_millis();
    let delta_sec = ((now - clock.last_ms) as f64 / 1000.0).floor().max(0.0);
    let delta_tokens = (tokens - clock.last_tokens).max(0.0);
    clock.last_ms = now;
    clock.last_tokens = tokens;
    if delta_sec == 0.0 && delta_tokens == 0.0 {
        return;
    }
    goal.time_used_seconds += delta_sec;
    goal.tokens_used += delta_tokens;
    goal.updated_at = crate::jsutil::iso_from_millis(now);
    if goal.status == "active" {
        if let Some(budget) = goal.token_budget {
            if goal.tokens_used >= budget {
                goal.status = "budget_limited".into();
            }
        }
    }
    let event = json!({ "type": "goal", "goal": serde_json::to_value(&*goal).unwrap() });
    drop(session);
    emit(event);
}

impl Harness {
    pub fn new(config: Arc<RuntimeConfig>, store: Arc<SessionStore>, tools: Arc<ToolRegistry>, emit: Emit, model: Option<Arc<dyn LanguageModel>>, deps: HarnessDeps) -> Result<Arc<Harness>> {
        let session_id = store.id();
        let model = match model {
            Some(m) => m,
            None => create_model(&config, Some(&session_id))?,
        };
        let own_ledger = CostLedger::from_config(&config, Some(&config.model)).map_err(|e| anyhow!(e.0))?;
        let cost_ledger = tools.cost_ledger().unwrap_or(own_ledger);
        tools.set_cost_ledger(Some(cost_ledger.clone()));
        {
            let mut s = store.session.lock().unwrap();
            if s.cost_usd.is_none() {
                s.cost_usd = Some(0.0);
            }
            if let Some(todos) = s.todos.clone().filter(|t| !t.is_empty()) {
                tools.set_todos(todos);
            }
        }
        let goal_clock = Arc::new(Mutex::new(GoalClock::default()));
        {
            let mut cbs = tools.callbacks.lock().unwrap();
            let store_t = store.clone();
            let emit_t = emit.clone();
            cbs.on_todo_update = Some(Arc::new(move |todos| {
                store_t.session.lock().unwrap().todos = Some(todos.clone());
                emit_t(json!({ "type": "todo", "todos": serde_json::to_value(&todos).unwrap() }));
            }));
            if !tools.read_only() {
                let store_g = store.clone();
                cbs.get_goal = Some(Arc::new(move || store_g.session.lock().unwrap().goal.clone()));
                let store_u = store.clone();
                let emit_u = emit.clone();
                let tools_w = Arc::downgrade(&tools);
                let clock_u = goal_clock.clone();
                cbs.on_goal_update = Some(Arc::new(move |next: Option<SessionGoal>| {
                    let store_u = store_u.clone();
                    let emit_u = emit_u.clone();
                    let tools_w = tools_w.clone();
                    let clock_u = clock_u.clone();
                    Box::pin(async move {
                        if let Some(t) = tools_w.upgrade() {
                            flush_goal(&store_u, &t, &clock_u, &emit_u, true);
                        }
                        let mut next = next;
                        {
                            let mut s = store_u.session.lock().unwrap();
                            if let Some(g) = next.as_mut() {
                                if g.tokens_at_start == 0.0 && g.tokens_used == 0.0 {
                                    g.tokens_at_start = s.tokens;
                                }
                                if let Some(current) = &s.goal {
                                    g.tokens_used = current.tokens_used;
                                    g.time_used_seconds = current.time_used_seconds;
                                }
                            }
                            s.goal = next.clone();
                        }
                        store_u.save()?;
                        emit_u(json!({ "type": "goal", "goal": next.map(|g| serde_json::to_value(g).unwrap()) }).as_object().map(|o| {
                            let mut o = o.clone();
                            if o.get("goal") == Some(&Value::Null) {
                                o.remove("goal");
                            }
                            Value::Object(o)
                        }).unwrap());
                        Ok(())
                    })
                }));
            }
            let emit_p = emit.clone();
            cbs.on_plan_exit = Some(Arc::new(move |plan| emit_p(json!({ "type": "plan_exit", "plan": plan }))));
        }
        Ok(Arc::new(Harness { config, store, tools, emit, model, cost_ledger, deps, untrusted_prompts: Mutex::new(false), state: Mutex::new(HarnessState::default()), goal_clock }))
    }

    pub fn busy(&self) -> bool {
        self.state.lock().unwrap().running
    }

    pub fn set_untrusted_prompts(&self, value: bool) {
        *self.untrusted_prompts.lock().unwrap() = value;
    }

    fn emit(&self, event: Value) {
        (self.emit)(event);
    }

    /// Accounts one provider request against the shared ledger and this session.
    pub fn account_usage(&self, usage: Option<&Usage>, model: Option<&str>, budget: Option<&SharedBudget>, fallback_input: u64, fallback_output: u64) -> UsageSplit {
        let split = to_usage_split(usage, fallback_input, fallback_output);
        self.account_split(&split, model, budget)
    }

    pub fn account_split(&self, split: &UsageSplit, model: Option<&str>, budget: Option<&SharedBudget>) -> UsageSplit {
        let model = model.unwrap_or(&self.config.model);
        let cost = self.cost_ledger.account(budget, split, Some(model));
        {
            let mut s = self.store.session.lock().unwrap();
            s.cost_usd = Some(s.cost() + cost);
        }
        let mut event = Map::new();
        event.insert("type".into(), json!("usage"));
        event.insert("input".into(), json!(split.input_tokens));
        event.insert("output".into(), json!(split.output_tokens));
        event.insert("total".into(), json!(split.total_tokens));
        if let Some(c) = split.cached_input_tokens.filter(|c| *c > 0) {
            event.insert("cacheRead".into(), json!(c));
        }
        if let Some(c) = split.cache_write_tokens.filter(|c| *c > 0) {
            event.insert("cacheWrite".into(), json!(c));
        }
        if cost > 0.0 {
            event.insert("costUsd".into(), crate::jsutil::num(cost));
        }
        self.emit(Value::Object(event));
        *split
    }

    /// Adds a user-role context message, deferred to the next safe point while a run is active.
    pub fn add_context_message(&self, message: Value) {
        let mut state = self.state.lock().unwrap();
        if state.running {
            state.pending_context.push(message);
        } else {
            drop(state);
            self.store.session.lock().unwrap().messages.push(message);
        }
    }

    fn drain_pending_context(&self) -> bool {
        let pending = std::mem::take(&mut self.state.lock().unwrap().pending_context);
        if pending.is_empty() {
            return false;
        }
        self.store.session.lock().unwrap().messages.extend(pending);
        true
    }

    pub async fn compact(&self, cancel: &CancellationToken, focus: Option<&str>) -> Result<crate::context::CompactionOutcome> {
        {
            let mut state = self.state.lock().unwrap();
            if state.running {
                bail!("Cannot compact while a run is active.");
            }
            state.running = true;
        }
        let result = compact_now(&self.store, &self.config, &*self.model, cancel, focus).await;
        if let Ok(res) = &result {
            self.emit(json!({ "type": "compaction", "count": res.compactions }));
            if res.tokens > 0 {
                self.account_split(&res.usage, None, None);
            }
        }
        self.state.lock().unwrap().running = false;
        if self.drain_pending_context() {
            let _ = self.store.save();
        }
        result
    }

    pub async fn recap(&self, cancel: &CancellationToken) -> Result<crate::context::RecapOutcome> {
        let res = recap_session(&self.store, &self.config, &*self.model, cancel).await?;
        if res.tokens > 0 {
            self.account_split(&res.usage, None, None);
        }
        Ok(res)
    }

    pub async fn ask_side_question(&self, question: &str, cancel: &CancellationToken) -> Result<(String, u64, UsageSplit)> {
        let (mut history, goal) = {
            let s = self.store.session.lock().unwrap();
            (s.messages.clone(), s.goal.clone())
        };
        repair_interrupted(&mut history);
        let expanded = expand_file_mentions(question, &self.tools.files, &MentionOptions { model: Some(&self.config.model), ..Default::default() });
        history.push(json!({ "role": "user", "content": format!("[Ephemeral Side Question (/btw) — answer concisely without modifying session state]:\n{}", expanded.expanded_prompt) }));
        let sys = system_prompt(&self.config, true, false, goal.as_ref(), None)?;
        let request = ModelRequest {
            system: Some(format!("{sys}\n\nYou are answering an ephemeral side question (/btw). Do not call tools; answer directly and concisely from the conversation context and your knowledge.")),
            messages: with_reasoning_replay(&history, self.model.provider(), self.config.effort.as_deref()),
            tools: vec![],
            max_output_tokens: 8_192u64.min(self.config.max_output_tokens),
            effort: if self.config.stub { None } else { auxiliary_effort(self.config.effort.as_deref()) },
            cancel: cancel.clone(),
            timeout: Duration::from_millis(self.config.timeout_ms),
        };
        let mut attempt = 0;
        let (raw, usage) = loop {
            match self.collect_text(request.clone()).await {
                Ok(v) => break v,
                Err(e) if attempt < 2 && e.retryable && !cancel.is_cancelled() => {
                    attempt += 1;
                    tokio::time::sleep(crate::generate::retry_delay(&e, 2000.0 * 2f64.powi(attempt - 1))).await;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let visible = visible_answer(&raw, &self.config.inline_reasoning);
        let text = if visible.trim().is_empty() { raw.trim().to_string() } else { visible.trim().to_string() };
        let split = to_usage_split(Some(&usage), 0, 0);
        self.account_split(&split, None, None);
        Ok((text, split.total_tokens, split))
    }

    async fn collect_text(&self, request: ModelRequest) -> Result<(String, Usage), ApiError> {
        let mut stream = self.model.stream(request).await?;
        let mut raw = String::new();
        let mut usage = Usage::default();
        while let Some(part) = stream.next().await {
            match part {
                StreamPart::Error(e) => return Err(e),
                StreamPart::TextDelta { delta, .. } => raw.push_str(&delta),
                StreamPart::Finish { usage: u, .. } => usage = u,
                _ => {}
            }
        }
        Ok((raw, usage))
    }

    pub fn revert(&self, steps: Option<usize>, scope: &str, force: bool) -> Result<RevertResult> {
        if self.busy() {
            bail!("Cannot revert while a run is active.");
        }
        let checkpoints = self.store.session.lock().unwrap().checkpoints.clone().unwrap_or_default();
        if checkpoints.is_empty() {
            bail!("No turn checkpoints available to revert.");
        }
        let count = steps.unwrap_or(1).max(1).min(checkpoints.len());
        let target: Vec<TurnCheckpoint> = checkpoints[checkpoints.len() - count..].to_vec();
        let mut restored: indexmap::IndexSet<String> = Default::default();
        let mut deleted: indexmap::IndexSet<String> = Default::default();
        let mut conflicts: indexmap::IndexSet<String> = Default::default();
        let mut pruned: indexmap::IndexSet<String> = Default::default();
        if scope == "both" || scope == "code" {
            for cp in target.iter().rev() {
                let files: Vec<FileBackup> = cp.files.iter().filter(|f| !conflicts.contains(&f.path)).cloned().collect();
                let res = self.tools.files.restore_backups(&files, force)?;
                for r in res.restored {
                    deleted.shift_remove(&r);
                    restored.insert(r);
                }
                for d in res.deleted {
                    restored.shift_remove(&d);
                    deleted.insert(d);
                }
                conflicts.extend(res.conflicts);
                pruned.extend(res.pruned);
            }
        }
        let mut messages_removed = 0;
        {
            let mut s = self.store.session.lock().unwrap();
            let first = &target[0];
            if scope == "both" || scope == "messages" {
                let prev = s.messages.len();
                if first.message_index <= prev {
                    messages_removed = prev - first.message_index;
                    s.messages.truncate(first.message_index);
                }
                s.tokens = first.tokens_before;
                let mut cps = checkpoints.clone();
                cps.truncate(cps.len() - count);
                s.checkpoints = Some(cps);
            } else if scope == "code" {
                let mut cps = checkpoints.clone();
                let n = cps.len();
                for cp in cps[n - count..].iter_mut() {
                    cp.files.clear();
                }
                s.checkpoints = Some(cps);
                let changed: Vec<String> = restored.iter().chain(deleted.iter()).cloned().collect();
                if !changed.is_empty() {
                    s.messages.push(json!({ "role": "user", "content": format!("[System Notice: User reverted workspace file changes from the last {count} turn(s). Restored/deleted files: {}]", changed.join(", ")) }));
                }
            }
        }
        self.store.save()?;
        Ok(RevertResult {
            turns_reverted: count,
            messages_removed,
            restored_files: restored.into_iter().collect(),
            deleted_files: deleted.into_iter().collect(),
            reverted_prompts: target.iter().map(|c| c.prompt.clone()).collect(),
            conflict_files: conflicts.into_iter().collect(),
            unrecoverable_files: pruned.into_iter().collect(),
        })
    }

    fn memory_event(&self, event: Value) {
        let Some(memory) = self.deps.memory.clone() else { return };
        let emit = self.emit.clone();
        tokio::spawn(async move {
            if memory.handle_event(event).await.is_err() {
                emit(json!({ "type": "memory_error", "message": "memory store rejected an event." }));
            }
        });
    }

    fn remember_assistant_message(&self, text: &str) {
        if self.deps.memory.is_none() || text.trim().is_empty() {
            return;
        }
        let clean = redact_text(text, &[&self.config.api_key]).trim().to_string();
        if !clean.is_empty() {
            self.memory_event(json!({ "type": "assistant_message", "text": crate::jsutil::take_chars(&clean, 2000) }));
        }
    }

    async fn retrieve_memory(&self, query: &str, budget: u64) -> Option<MemoryPromptContext> {
        let memory = self.deps.memory.clone()?;
        match memory.retrieve(query.to_string(), budget).await {
            Ok(context) => {
                let rendered = render_memory_context(Some(&context));
                let tokens = rendered.map(|r| crate::jsutil::utf16_len(&r).div_ceil(2)).unwrap_or(0);
                self.emit(json!({ "type": "memory_retrieved", "chunks": context.rag.len(), "tokens": tokens }));
                Some(context)
            }
            Err(error) => {
                self.emit(json!({ "type": "memory_error", "message": safe_error(&error.to_string(), Some(&self.config.api_key)) }));
                None
            }
        }
    }

    async fn ensure_repo_index_once(&self) {
        let Some(memory) = self.deps.memory.clone() else { return };
        {
            let mut state = self.state.lock().unwrap();
            if !self.config.rag_enabled || state.repo_indexed_once {
                return;
            }
            state.repo_indexed_once = true;
        }
        match memory.ensure_repo_indexed(self.config.cwd.clone()).await {
            Ok(i) => self.emit(json!({ "type": "memory_indexed", "files": i.files, "chunks": i.chunks, "durationMs": i.duration_ms })),
            Err(error) => self.emit(json!({ "type": "memory_error", "message": safe_error(&error.to_string(), Some(&self.config.api_key)) })),
        }
    }

    fn schedule_patch_reindex(&self) {
        let Some(memory) = self.deps.memory.clone() else { return };
        if !self.config.rag_enabled || !self.tools.files.has_turn_changes() {
            return;
        }
        let emit = self.emit.clone();
        let cwd = self.config.cwd.clone();
        let key = self.config.api_key.clone();
        tokio::spawn(async move {
            match memory.ensure_repo_indexed(cwd).await {
                Ok(i) if i.reindexed != "none" => emit(json!({ "type": "memory_indexed", "files": i.files, "chunks": i.chunks, "durationMs": i.duration_ms })),
                Ok(_) => {}
                Err(error) => emit(json!({ "type": "memory_error", "message": safe_error(&error.to_string(), Some(&key)) })),
            }
        });
    }

    fn remember_patch_applied(&self, call: &ToolCall) {
        if self.deps.memory.is_none() {
            return;
        }
        let canonical = normalize_tool_name(&call.tool_name, None);
        let norm = normalize_tool_input(&canonical, &call.input);
        let paths: Vec<String> = if canonical == "apply_patch" {
            norm.get("patch").and_then(Value::as_str).map(patch_files).unwrap_or_default()
        } else {
            norm.get("path").and_then(Value::as_str).map(|p| vec![p.to_string()]).unwrap_or_default()
        };
        let backups: HashMap<String, FileBackup> = self.tools.files.peek_turn_backups().into_iter().map(|b| (b.path.clone(), b)).collect();
        for raw in paths {
            let key = raw.replace('\\', "/");
            let before = backups.get(&key).and_then(|b| b.content.clone());
            let bytes: i64 = match self.tools.files.path(&raw).and_then(|p| self.tools.files.text(&p)) {
                Ok(after) => after.len() as i64 - before.as_ref().map(|b| b.len() as i64).unwrap_or(0),
                Err(_) => {
                    if backups.get(&key).and_then(|b| b.after_deleted).unwrap_or(false) {
                        before.as_ref().map(|b| -(b.len() as i64)).unwrap_or(0)
                    } else {
                        0
                    }
                }
            };
            self.memory_event(json!({ "type": "patch_applied", "path": key, "bytes": bytes }));
        }
    }

    fn run_timeout(&self, budget: &SharedBudget) -> Duration {
        let b = budget.lock().unwrap();
        let remaining = match b.started_at {
            Some(started) if b.time_limit_ms > 0 => (started + b.time_limit_ms as i64 - now_millis()).max(1) as u64,
            _ => self.config.timeout_ms,
        };
        Duration::from_millis(self.config.timeout_ms.min(remaining))
    }

    /// Parses tool calls like the AI SDK (`parseToolCall`) against the tools offered this step.
    fn parse_tool_call(defs: &IndexMap<String, ToolDefinition>, name: &str, raw: &str) -> (Value, bool) {
        let parsed_json = || serde_json::from_str::<Value>(raw).ok();
        let Some(def) = defs.get(name) else {
            return (parsed_json().unwrap_or_else(|| Value::String(raw.to_string())), false);
        };
        let candidate = if raw.trim().is_empty() { Some(json!({})) } else { parsed_json() };
        match candidate {
            Some(value) => match def.parse(name, &value) {
                Ok(parsed) => (parsed, true),
                Err(_) => (if raw.trim().is_empty() { Value::String(raw.to_string()) } else { value }, false),
            },
            None => (Value::String(raw.to_string()), false),
        }
    }

    async fn stream_step(&self, request: ModelRequest, defs: &IndexMap<String, ToolDefinition>, cancel: &CancellationToken) -> Result<StepResult> {
        let mut parts = self.model.stream(request).await.map_err(anyhow::Error::new)?;
        let mut projection = AnswerProjection::new(&self.config.inline_reasoning);
        let mut step_text = String::new();
        let show_reasoning = self.config.show_reasoning;
        let mut content: Vec<Value> = vec![];
        let mut active_text: HashMap<String, usize> = HashMap::new();
        let mut active_reasoning: HashMap<String, usize> = HashMap::new();
        let mut streaming: HashMap<String, (String, String, usize)> = HashMap::new();
        let mut calls: Vec<ToolCall> = vec![];
        let mut usage = Usage::default();
        let mut finish = FinishReason::Unknown;
        let present = |parts: Vec<AnswerPart>, step_text: &mut String| {
            for part in parts {
                match part {
                    AnswerPart::Text(t) => {
                        step_text.push_str(&t);
                        self.emit(json!({ "type": "text", "text": t }));
                    }
                    AnswerPart::Reasoning(t) => {
                        if show_reasoning {
                            self.emit(json!({ "type": "reasoning", "text": t, "source": "inline" }));
                        }
                    }
                }
            }
        };
        fn open_part(content: &mut Vec<Value>, active: &mut HashMap<String, usize>, id: &str, kind: &str, md: &Option<Value>) -> usize {
            if let Some(i) = active.get(id) {
                return *i;
            }
            let mut part = Map::new();
            part.insert("type".into(), json!(kind));
            part.insert("text".into(), json!(""));
            if let Some(m) = md {
                part.insert("providerOptions".into(), m.clone());
            }
            content.push(Value::Object(part));
            active.insert(id.to_string(), content.len() - 1);
            content.len() - 1
        }
        loop {
            let next = tokio::select! {
                p = parts.next() => p,
                _ = cancel.cancelled() => return Err(Aborted::default().into()),
            };
            let Some(part) = next else { break };
            match part {
                StreamPart::Error(error) => return Err(error.into()),
                StreamPart::TextDelta { id, delta, provider_metadata } => {
                    let i = open_part(&mut content, &mut active_text, &id, "text", &provider_metadata);
                    let t = content[i]["text"].as_str().unwrap_or("").to_string() + &delta;
                    content[i]["text"] = json!(t);
                    if let Some(m) = provider_metadata {
                        content[i]["providerOptions"] = m;
                    }
                    let projected = projection.push(&delta);
                    present(projected, &mut step_text);
                }
                StreamPart::TextEnd { id, provider_metadata } => {
                    if let Some(i) = active_text.remove(&id) {
                        if let Some(m) = provider_metadata {
                            content[i]["providerOptions"] = m;
                        }
                    }
                }
                StreamPart::ReasoningDelta { id, delta, provider_metadata } => {
                    let i = open_part(&mut content, &mut active_reasoning, &id, "reasoning", &provider_metadata);
                    let t = content[i]["text"].as_str().unwrap_or("").to_string() + &delta;
                    content[i]["text"] = json!(t);
                    if let Some(m) = provider_metadata {
                        content[i]["providerOptions"] = m;
                    }
                    if show_reasoning {
                        self.emit(json!({ "type": "reasoning", "text": delta, "source": "provider" }));
                    }
                }
                StreamPart::ReasoningEnd { id, provider_metadata } => {
                    let i = open_part(&mut content, &mut active_reasoning, &id, "reasoning", &None);
                    if let Some(m) = provider_metadata {
                        content[i]["providerOptions"] = m;
                    }
                    active_reasoning.remove(&id);
                }
                StreamPart::ToolInputStart { id, tool_name } => {
                    let name = normalize_tool_name(&tool_name, None);
                    streaming.insert(id.clone(), (name.clone(), String::new(), 0));
                    self.emit(json!({ "type": "tool_stream", "name": name, "id": id, "state": "start", "preview": "" }));
                }
                StreamPart::ToolInputDelta { id, delta } => {
                    if let Some((name, raw, last)) = streaming.get_mut(&id) {
                        raw.push_str(&delta);
                        let len = crate::jsutil::utf16_len(raw);
                        if len - *last >= 48 || delta.contains('\n') || *last == 0 {
                            *last = len;
                            self.emit(json!({ "type": "tool_stream", "name": name.clone(), "id": id, "state": "delta", "preview": extract_stream_preview(raw) }));
                        }
                    }
                }
                StreamPart::ToolCall { id, tool_name, input, provider_metadata } => {
                    let (parsed, valid) = Self::parse_tool_call(defs, &tool_name, &input);
                    let mut part = Map::new();
                    part.insert("type".into(), json!("tool-call"));
                    part.insert("toolCallId".into(), json!(id));
                    part.insert("toolName".into(), json!(tool_name));
                    part.insert("input".into(), if !valid && !(parsed.is_object() || parsed.is_array() || parsed.is_null()) { json!({}) } else { parsed.clone() });
                    if let Some(m) = provider_metadata {
                        part.insert("providerOptions".into(), m);
                    }
                    content.push(Value::Object(part));
                    calls.push(ToolCall { tool_call_id: id, tool_name, input: parsed });
                }
                StreamPart::Finish { usage: u, finish_reason } => {
                    usage = u;
                    finish = finish_reason;
                }
            }
        }
        if cancel.is_cancelled() {
            return Err(Aborted::default().into());
        }
        match finish {
            FinishReason::Length => bail!("Model response hit the output limit. No tools from this response were executed; increase --max-output-tokens."),
            FinishReason::Error | FinishReason::ContentFilter | FinishReason::Other | FinishReason::Unknown => bail!("Model stopped unexpectedly ({}).", finish.as_str()),
            _ => {}
        }
        let tail = projection.finish();
        present(tail, &mut step_text);
        content.retain(|p| !(p["type"] == "text" && p["text"].as_str().map(str::is_empty).unwrap_or(false)));
        let assistant_messages = if content.is_empty() { vec![] } else { vec![json!({ "role": "assistant", "content": content })] };
        Ok(StepResult { text: step_text, calls, assistant_messages, usage })
    }

    pub async fn run(self: &Arc<Self>, prompt: &str, cancel: &CancellationToken, shared_budget: Option<SharedBudget>) -> Result<String> {
        {
            let mut state = self.state.lock().unwrap();
            if state.running {
                bail!("This harness already has an active run.");
            }
            state.running = true;
            state.last_call_sig.clear();
            state.repeat_count = 0;
            state.turn_had_successful_tool = false;
            state.turn_had_failed_exec = false;
            state.verify_attempts = 0;
            state.turn_had_edit = false;
            state.last_verify_verdict = None;
            state.verify_rolled_back = false;
        }
        let budget = create_run_budget(&self.config, shared_budget.clone());
        self.tools.set_budget(Some(budget.clone()));
        if shared_budget.is_none() {
            let session_cost = self.store.session.lock().unwrap().cost();
            budget.lock().unwrap().cost_spent = session_cost;
        }
        self.cost_ledger.register_run(&budget);
        let (message_index_before, tokens_before, cost_before) = {
            let s = self.store.session.lock().unwrap();
            (s.messages.len(), s.tokens, s.cost())
        };
        let turn_started_at = now_iso();
        {
            let mut clock = self.goal_clock.lock().unwrap();
            clock.active = true;
            clock.last_ms = now_millis();
            clock.last_tokens = tokens_before;
        }
        self.tools.files.begin_turn();
        let mut text = String::new();
        let result = self.run_inner(prompt, cancel, &budget, &mut text).await;
        if let Err(error) = &result {
            self.flush_goal(false);
            if !cancel.is_cancelled() {
                let msg = error.to_string();
                static USAGE: OnceLock<Regex> = OnceLock::new();
                let usage_re = USAGE.get_or_init(|| Regex::new(r"(?i)\b(?:429|rate[_\s-]?limit|usage[_\s-]?limit|insufficient_quota|quota|token budget reached)\b").unwrap());
                let event = {
                    let mut s = self.store.session.lock().unwrap();
                    match s.goal.as_mut() {
                        Some(goal) if goal.status == "active" || goal.status == "budget_limited" => {
                            let is_usage = usage_re.is_match(&msg);
                            if goal.status == "active" || is_usage {
                                goal.status = if is_usage { "usage_limited".into() } else { "blocked".into() };
                                goal.updated_at = now_iso();
                                Some(json!({ "type": "goal", "goal": serde_json::to_value(&*goal).unwrap() }))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                };
                if let Some(e) = event {
                    self.emit(e);
                }
            }
            repair_interrupted(&mut self.store.session.lock().unwrap().messages);
            self.drain_pending_context();
            let _ = self.store.save();
        }
        // finally
        self.flush_goal(false);
        self.goal_clock.lock().unwrap().active = false;
        if self.deps.memory.is_some() {
            let outcome = match &result {
                Err(e) => format!("error: {}", crate::jsutil::take_chars(&safe_error(&e.to_string(), Some(&self.config.api_key)), 300)),
                Ok(_) => {
                    let t = text.trim();
                    if t.is_empty() { "completed".to_string() } else { crate::jsutil::tail_chars(t, 400).to_string() }
                }
            };
            self.memory_event(json!({ "type": "task_completed", "outcome": outcome }));
        }
        self.schedule_patch_reindex();
        {
            let (had_failed, had_success) = {
                let st = self.state.lock().unwrap();
                (st.turn_had_failed_exec, st.turn_had_successful_tool)
            };
            let mut s = self.store.session.lock().unwrap();
            let mut goal_event = None;
            if let Some(goal) = s.goal.as_mut() {
                if goal.status == "active" && !cancel.is_cancelled() && had_failed && !had_success {
                    let mut st = self.state.lock().unwrap();
                    st.consecutive_goal_exec_failure_turns += 1;
                    if st.consecutive_goal_exec_failure_turns >= 3 {
                        goal.status = "blocked".into();
                        goal.updated_at = now_iso();
                        goal_event = Some(json!({ "type": "goal", "goal": serde_json::to_value(&*goal).unwrap() }));
                    }
                }
            }
            let files = self.tools.files.take_turn_backups();
            let goal_label = s.goal.as_ref().map(|g| format!("[goal] {}", g.objective));
            let checkpoints = s.checkpoints.get_or_insert_with(Vec::new);
            let next_id = checkpoints.last().map(|c| c.id).unwrap_or(0) + 1;
            checkpoints.push(TurnCheckpoint {
                id: next_id,
                created_at: turn_started_at,
                prompt: if prompt.starts_with("<codex_internal_context source=\"goal\">") { goal_label.unwrap_or_else(|| "[goal continuation]".into()) } else { prompt.to_string() },
                message_index: message_index_before,
                tokens_before,
                cost_usd_before: Some(cost_before),
                files,
            });
            drop(s);
            if let Some(e) = goal_event {
                self.emit(e);
            }
        }
        self.drain_pending_context();
        let _ = self.store.save();
        self.state.lock().unwrap().running = false;
        self.tools.set_budget(None);
        result
    }

    fn flush_goal(&self, allow_stopped: bool) {
        flush_goal(&self.store, &self.tools, &self.goal_clock, &self.emit, allow_stopped);
    }

    async fn run_inner(self: &Arc<Self>, prompt: &str, cancel: &CancellationToken, budget: &SharedBudget, text: &mut String) -> Result<String> {
        if cancel.is_cancelled() {
            return Err(Aborted::default().into());
        }
        self.drain_pending_context();
        self.ensure_repo_index_once().await;
        self.memory_event(json!({ "type": "task_started", "task": prompt }));
        let budget_config = ContextBudgetConfig::from_config(&self.config);
        let pre = allocate_context_budget(&budget_config, "");
        let untrusted = *self.untrusted_prompts.lock().unwrap();
        let expanded = expand_file_mentions(prompt, &self.tools.files, &MentionOptions { model: Some(&self.config.model), supports_vision: None, allow_outside_workspace: !untrusted, max_chars: pre.files as usize * 3 });
        {
            let mut s = self.store.session.lock().unwrap();
            if expanded.image_parts.is_empty() {
                s.messages.push(json!({ "role": "user", "content": expanded.expanded_prompt }));
            } else {
                let mut parts = vec![json!({ "type": "text", "text": expanded.expanded_prompt })];
                parts.extend(expanded.image_parts.clone());
                s.messages.push(json!({ "role": "user", "content": parts }));
            }
        }
        self.store.save()?;
        let mut prompt_cache = SystemPromptCache::default();
        let mut spent: u64 = 0;
        let mut measured_input: u64 = 0;
        let mut budget_limit_steered = false;
        let run_token_budget = self.config.run_token_budget;
        let account = |tokens: u64, spent: &mut u64| {
            *spent += tokens;
            budget.lock().unwrap().spent += tokens;
        };
        let exceeded = |spent: u64| -> Option<String> {
            let b = budget.lock().unwrap();
            if run_token_budget > 0 && spent >= run_token_budget {
                return Some("Run token budget reached. Resume with a new prompt or increase --run-token-budget.".into());
            }
            if b.limit > 0 && b.spent >= b.limit {
                return Some("Run token budget reached. Resume with a new prompt or increase --run-token-budget.".into());
            }
            let cap = CostLedger::hard_cap_exceeded(&b);
            cap.exceeded.then_some(cap.reason)
        };
        loop {
            if cancel.is_cancelled() {
                return Err(Aborted::default().into());
            }
            if self.drain_pending_context() {
                self.store.save()?;
            }
            if let Some(reason) = exceeded(spent) {
                bail!("{reason}");
            }
            let goal = self.store.session.lock().unwrap().goal.clone();
            let flags = self.tools.flags();
            let system = system_prompt(&self.config, flags.read_only, flags.plan_mode, goal.as_ref(), Some(&mut prompt_cache))?;
            let allocation = allocate_context_budget(&budget_config, &system);
            let memory_context = self.retrieve_memory(&expanded.expanded_prompt, allocation.files).await;
            let old_count = self.store.session.lock().unwrap().compactions;
            let compacted = compact_if_needed(&self.store, &self.config, &*self.model, &system, cancel, measured_input).await?;
            account(compacted.tokens, &mut spent);
            if compacted.usage.total_tokens > 0 {
                self.account_split(&compacted.usage, None, Some(budget));
            }
            let new_count = self.store.session.lock().unwrap().compactions;
            if new_count != old_count {
                self.emit(json!({ "type": "compaction", "count": new_count }));
            }
            if let Some(reason) = exceeded(spent) {
                bail!("{reason} Original history is archived; resume to continue.");
            }
            let messages = self.store.session.lock().unwrap().messages.clone();
            let prepared = prepare_context_budget(&messages, &system, &budget_config, memory_context.as_ref()).map_err(|e| anyhow!(e.0))?;
            let mut event = Map::new();
            event.insert("type".into(), json!("context_budget"));
            event.insert("inputBudget".into(), json!(prepared.stats.input_budget));
            event.insert("system".into(), json!(prepared.stats.system_budget));
            event.insert("conversation".into(), json!(prepared.stats.conversation));
            event.insert("files".into(), json!(prepared.stats.files));
            event.insert("truncated".into(), json!(prepared.stats.truncated));
            if let Some(r) = &prepared.stats.reason {
                event.insert("reason".into(), json!(r));
            }
            self.emit(Value::Object(event));
            let defs = self.tools.exposed_definitions();
            let tool_specs = self.tools.schemas();
            let mut attempt: u32 = 0;
            let step = loop {
                let deadline = Deadline::new(cancel, self.run_timeout(budget));
                let request = ModelRequest {
                    system: Some(prepared.system.clone()),
                    messages: with_reasoning_replay(&prepared.messages, self.model.provider(), self.config.effort.as_deref()),
                    tools: tool_specs.clone(),
                    max_output_tokens: self.config.max_output_tokens,
                    effort: if self.config.stub { None } else { self.config.effort.clone() },
                    cancel: deadline.token.clone(),
                    timeout: Duration::from_secs(365 * 24 * 3600),
                };
                let outcome = self.stream_step(request, &defs, &deadline.token).await.and_then(|step| {
                    if step.calls.len() > 32 || step.calls.iter().map(|c| &c.tool_call_id).collect::<HashSet<_>>().len() != step.calls.len() {
                        bail!("Invalid or excessive tool call batch. No tools executed.");
                    }
                    Ok(step)
                });
                drop(deadline);
                let outcome = match outcome {
                    Err(e) if is_aborted(&e) && !cancel.is_cancelled() => Err(anyhow::Error::new(ApiError::aborted())),
                    other => other,
                };
                match outcome {
                    Ok(step) => {
                        let input = step.usage.input_tokens.unwrap_or_else(|| estimated_tokens(&prepared.messages, &prepared.system));
                        let output = step.usage.output_tokens.unwrap_or_else(|| (crate::jsutil::utf16_len(&stringify(&Value::Array(step.assistant_messages.clone()))) as u64).div_ceil(2));
                        let tokens = step.usage.total_tokens().unwrap_or(input + output);
                        account(tokens, &mut spent);
                        self.store.session.lock().unwrap().tokens += tokens as f64;
                        measured_input = input;
                        self.flush_goal(false);
                        self.account_usage(Some(&step.usage), None, Some(budget), input, output);
                        self.store.session.lock().unwrap().messages.extend(step.assistant_messages.clone());
                        text.push_str(&step.text);
                        self.store.save()?;
                        break step;
                    }
                    Err(error) => {
                        if cancel.is_cancelled() {
                            return Err(Aborted::default().into());
                        }
                        let msg = error.to_string();
                        static STOP: OnceLock<Regex> = OnceLock::new();
                        let stop = STOP.get_or_init(|| Regex::new(r"(?i)output limit|Model stopped unexpectedly|Invalid or excessive tool call batch").unwrap());
                        if attempt >= MAX_API_RETRIES || is_non_retryable(&error, cancel) || stop.is_match(&msg) {
                            return Err(error);
                        }
                        let next = attempt + 1;
                        self.emit(json!({ "type": "retry", "attempt": next, "maxRetries": MAX_API_RETRIES, "message": safe_error(&msg, Some(&self.config.api_key)) }));
                        let delay = retry_delay_ms(&error, attempt);
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
                            _ = cancel.cancelled() => return Err(Aborted::default().into()),
                        }
                        attempt = next;
                    }
                }
            };
            if step.calls.is_empty() {
                self.enforce_final_verification()?;
                if step.text.trim().is_empty() {
                    bail!("Model finished without a visible answer.");
                }
                self.remember_assistant_message(text);
                return Ok(text.clone());
            }
            let batch_deadline = Deadline::new(cancel, self.run_timeout(budget));
            let outcome = self.execute_batch(step.calls, &batch_deadline.token).await;
            let timed_out = batch_deadline.token.is_cancelled() && !cancel.is_cancelled();
            drop(batch_deadline);
            let (denied, loop_detected) = outcome?;
            if cancel.is_cancelled() {
                return Err(Aborted::default().into());
            }
            if timed_out {
                bail!("The operation was aborted due to timeout");
            }
            if denied {
                let reason = if loop_detected { "Repeated denials for identical arguments; stopping this run." } else { "Permission denied." };
                self.emit(json!({ "type": "stopped", "reason": reason }));
                self.enforce_final_verification()?;
                self.remember_assistant_message(text);
                return Ok(text.clone());
            }
            self.flush_goal(false);
            let verify_deadline = Deadline::new(cancel, self.run_timeout(budget));
            let verified = self.run_verification_hook(&verify_deadline.token).await;
            drop(verify_deadline);
            self.state.lock().unwrap().turn_had_edit = false;
            verified?;
            let limit_goal = {
                let s = self.store.session.lock().unwrap();
                s.goal.clone().filter(|g| g.status == "budget_limited")
            };
            if let Some(goal) = limit_goal {
                if !budget_limit_steered {
                    budget_limit_steered = true;
                    self.store.session.lock().unwrap().messages.push(json!({ "role": "user", "content": build_goal_budget_limit_prompt(&goal) }));
                    self.store.save()?;
                }
            }
        }
    }

    fn enforce_final_verification(&self) -> Result<()> {
        let verdict = {
            let st = self.state.lock().unwrap();
            if st.verify_rolled_back {
                return Ok(());
            }
            match &st.last_verify_verdict {
                Some(v) if v.severity == Severity::Fail => v.clone(),
                _ => return Ok(()),
            }
        };
        self.rollback_turn("Model ended the turn with unresolved verification failures.")?;
        Err(VerificationFailed(verdict).into())
    }

    async fn run_verification_hook(&self, cancel: &CancellationToken) -> Result<()> {
        let verify_config = self.config.verify.clone();
        let enabled = self.deps.verify_enabled.unwrap_or(verify_config.enabled);
        if !enabled || !self.state.lock().unwrap().turn_had_edit || self.tools.effective_read_only() {
            return Ok(());
        }
        let skip = |reason: &str, this: &Harness| {
            this.emit(json!({ "type": "verify_result", "status": "skip", "reason": reason, "errors": [], "warnings": [], "durationMs": 0 }));
            this.memory_event(json!({ "type": "verify_result", "status": "skip", "issues": 0 }));
            let mut st = this.state.lock().unwrap();
            st.last_verify_verdict = None;
            st.verify_attempts = 0;
        };
        let files = self.tools.files.turn_changed_files();
        if files.is_empty() {
            skip("no tracked files", self);
            return Ok(());
        }
        let policy = self.deps.verify_policy.clone().unwrap_or(verify_config.policy.clone());
        let mut effective = verify_config.clone();
        effective.policy = policy.clone();
        let mut plan = plan_verification(&self.config.cwd, &files, &effective, &policy);
        let commands: Vec<String> = plan.commands.iter().map(|c| c.display.clone()).collect();
        self.emit(json!({ "type": "verify_start", "files": files, "commands": commands }));
        if !plan.commands.is_empty() {
            let assessment = assess_verify_plan(&plan, self.tools.policy().as_deref());
            let allowed = match &assessment.denied {
                Some(_) => false,
                None => (self.tools.approve)(crate::tools::ApprovalRequest { tool: "verify".into(), input: json!({ "files": files, "commands": commands }), cancel: cancel.clone(), risk: Some(assessment.risk), trusted: Some(false) }).await,
            };
            if !allowed {
                skip(assessment.denied.as_deref().unwrap_or("approval denied"), self);
                return Ok(());
            }
            plan.approved = true;
        }
        let runner: Arc<dyn VerifyRunner> = self.deps.verify_runner.clone().unwrap_or_else(|| Arc::new(CommandRunner));
        let started = now_millis();
        let verdict = run_verification(&self.config.cwd, &files, &effective, &*runner, cancel, &mut plan, None).await?;
        let verdict = redact_verdict(&verdict, &[&self.config.api_key]);
        let mut event = Map::new();
        event.insert("type".into(), json!("verify_result"));
        event.insert("status".into(), json!(verdict.severity.as_str()));
        if let Some(stage) = verdict.failed_stage() {
            event.insert("stage".into(), json!(stage));
        }
        event.insert("reason".into(), json!(verdict.reason));
        event.insert("errors".into(), Value::Array(verdict.errors.iter().map(|i| i.to_value()).collect()));
        event.insert("warnings".into(), Value::Array(verdict.warnings.iter().map(|i| i.to_value()).collect()));
        event.insert("durationMs".into(), json!((now_millis() - started).max(0)));
        self.emit(Value::Object(event));
        self.memory_event(json!({ "type": "verify_result", "status": verdict.severity.as_str(), "issues": verdict.errors.len() + verdict.warnings.len() }));
        if verdict.severity == Severity::Fail {
            let attempts = {
                let mut st = self.state.lock().unwrap();
                st.verify_attempts += 1;
                st.last_verify_verdict = Some(verdict.clone());
                st.verify_attempts
            };
            let max_rounds = verify_config.max_repair_rounds.max(1);
            let error_text = redact_text(&format_verify_errors(&verdict), &[&self.config.api_key]);
            self.store.session.lock().unwrap().messages.push(json!({ "role": "user", "content": format!("[Verification failed (attempt {attempts}/{max_rounds})]\n{error_text}") }));
            self.store.save()?;
            if attempts >= max_rounds {
                self.rollback_turn("Verification failed after the allowed repair attempts.")?;
                return Err(VerificationFailed(verdict).into());
            }
            return Ok(());
        }
        {
            let mut st = self.state.lock().unwrap();
            st.verify_attempts = 0;
            st.last_verify_verdict = None;
        }
        if !verdict.warnings.is_empty() {
            let warning_text = redact_text(&format_verify_warnings(&verdict), &[&self.config.api_key]);
            self.store.session.lock().unwrap().messages.push(json!({ "role": "user", "content": warning_text }));
            self.store.save()?;
        }
        Ok(())
    }

    fn rollback_turn(&self, reason: &str) -> Result<()> {
        self.state.lock().unwrap().verify_rolled_back = true;
        let backups = self.tools.files.peek_turn_backups();
        let result = self.tools.files.restore_backups(&backups, false)?;
        let conflicts: HashSet<String> = result.conflicts.iter().map(|p| p.replace('\\', "/")).collect();
        self.tools.files.replace_turn_backups(backups.into_iter().filter(|b| conflicts.contains(&b.path.replace('\\', "/"))).collect());
        self.emit(json!({ "type": "verify_rollback", "reason": reason, "restored": result.restored, "deleted": result.deleted, "conflicts": result.conflicts }));
        let mut lines = vec![format!("[Verification rollback] {reason}")];
        if !result.restored.is_empty() {
            lines.push(format!("Restored: {}", result.restored.join(", ")));
        }
        if !result.deleted.is_empty() {
            lines.push(format!("Deleted: {}", result.deleted.join(", ")));
        }
        if !result.conflicts.is_empty() {
            lines.push(format!("Kept (changed by user): {}", result.conflicts.join(", ")));
        }
        if !result.pruned.is_empty() {
            lines.push(format!("Skipped (backup pruned): {}", result.pruned.join(", ")));
        }
        self.store.session.lock().unwrap().messages.push(json!({ "role": "user", "content": redact_text(&lines.join("\n"), &[&self.config.api_key]) }));
        self.store.save()
    }

    async fn execute_one(self: &Arc<Self>, call: ToolCall, cancel: CancellationToken, denied_flag: Arc<Mutex<(bool, bool)>>) -> Value {
        let mut meta = build_tool_meta(&call.tool_name, &call.input);
        let started = now_millis();
        let attempt: Result<Value> = async {
            let already_denied = denied_flag.lock().unwrap().0;
            if already_denied || cancel.is_cancelled() {
                bail!("{}", if already_denied { "Skipped after permission denial." } else { "Cancelled before dispatch." });
            }
            let sig = call_signature(&call);
            let repeat = {
                let mut st = self.state.lock().unwrap();
                if sig == st.last_call_sig {
                    st.repeat_count += 1;
                } else {
                    st.last_call_sig = sig;
                    st.repeat_count = 1;
                }
                st.repeat_count
            };
            if meta["kind"] == "edit" {
                if let Some(diff) = self.tools.files.preview_diff(&call.tool_name, &call.input) {
                    let (added, removed, verb) = summarize_diff_counts(Some(&diff));
                    meta.insert("diff".into(), json!(diff));
                    meta.insert("added".into(), json!(added));
                    meta.insert("removed".into(), json!(removed));
                    meta.insert("verb".into(), json!(verb));
                }
            }
            self.emit(json!({ "type": "tool", "name": call.tool_name, "id": call.tool_call_id, "state": "start", "meta": meta.clone() }));
            let result = self.tools.execute(&call, &cancel).await?;
            {
                let mut d = denied_flag.lock().unwrap();
                d.0 |= result.denied;
                d.1 |= result.loop_detected;
            }
            meta.insert("durationMs".into(), json!((now_millis() - started).max(0)));
            if !result.denied {
                if meta["kind"] == "edit" {
                    self.state.lock().unwrap().turn_had_edit = true;
                    self.remember_patch_applied(&call);
                }
                static FAILED: OnceLock<Regex> = OnceLock::new();
                let failed_re = FAILED.get_or_init(|| Regex::new(r"(?m)^Exit code:\s*(\d+)").unwrap());
                let failed_bash = meta["kind"] == "exec" && failed_re.captures(&result.text).map(|c| &c[1] != "0").unwrap_or(false);
                let mut st = self.state.lock().unwrap();
                if failed_bash {
                    st.turn_had_failed_exec = true;
                } else {
                    st.turn_had_successful_tool = true;
                    st.consecutive_goal_exec_failure_turns = 0;
                }
            }
            let summary = meta["summary"].as_str().unwrap_or("").to_string();
            self.memory_event(json!({ "type": "tool_result", "tool": call.tool_name, "summary": if result.denied { format!("{summary} (denied)") } else { summary } }));
            if meta["kind"] == "exec" && !result.text.is_empty() {
                let lines: Vec<String> = result.text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).trim_end().to_string()).filter(|l| !l.is_empty()).collect();
                meta.insert("outputPreview".into(), json!(lines.iter().take(80).collect::<Vec<_>>()));
                if lines.len() > 80 {
                    meta.insert("omittedLines".into(), json!(lines.len() - 80));
                }
            }
            let loop_note = if !result.denied && (repeat == 3 || repeat == 5 || repeat >= 8) {
                format!("\n[Loop warning: {} was called {repeat} times consecutively with identical arguments. Analyze the previous result and try a different approach.]", call.tool_name)
            } else {
                String::new()
            };
            let output = json!({ "type": if result.denied { "error-text" } else { "text" }, "value": format!("{}{loop_note}", result.text) });
            self.emit(json!({ "type": "tool", "name": call.tool_name, "id": call.tool_call_id, "state": if result.denied { "error" } else { "done" }, "meta": meta.clone() }));
            Ok(output)
        }
        .await;
        let output = match attempt {
            Ok(o) => o,
            Err(error) => {
                if !meta.contains_key("durationMs") {
                    meta.insert("durationMs".into(), json!((now_millis() - started).max(0)));
                }
                let message = if is_aborted(&error) { Aborted::default().0 } else { error.to_string() };
                let out = json!({ "type": "error-text", "value": safe_error(&message, Some(&self.config.api_key)) });
                self.emit(json!({ "type": "tool", "name": call.tool_name, "id": call.tool_call_id, "state": "error", "meta": meta.clone() }));
                out
            }
        };
        json!({ "type": "tool-result", "toolCallId": call.tool_call_id, "toolName": call.tool_name, "output": output })
    }

    async fn execute_batch(self: &Arc<Self>, calls: Vec<ToolCall>, cancel: &CancellationToken) -> Result<(bool, bool)> {
        let flags = Arc::new(Mutex::new((false, false)));
        let mut index = 0;
        while index < calls.len() {
            let mut group = vec![calls[index].clone()];
            index += 1;
            if self.tools.is_read_only_tool(&group[0].tool_name) {
                while group.len() < MAX_PARALLEL_READS && index < calls.len() && self.tools.is_read_only_tool(&calls[index].tool_name) {
                    group.push(calls[index].clone());
                    index += 1;
                }
            }
            let futures = group.into_iter().map(|call| self.execute_one(call, cancel.clone(), flags.clone()));
            let results = futures_util::future::join_all(futures).await;
            {
                let mut s = self.store.session.lock().unwrap();
                let append = matches!(s.messages.last(), Some(m) if m["role"] == "tool");
                if append {
                    let last = s.messages.last_mut().unwrap();
                    last["content"].as_array_mut().unwrap().extend(results);
                } else {
                    s.messages.push(json!({ "role": "tool", "content": results }));
                }
            }
            self.store.save()?;
        }
        let images = self.tools.take_pending_images();
        if !images.is_empty() && resolve_model_capabilities(&self.config.model, None).supports_vision.unwrap_or(true) {
            use base64::Engine;
            let mut parts = vec![json!({ "type": "text", "text": format!("[Attached image(s) from view_image: {}]", images.iter().map(|i| i.path.clone()).collect::<Vec<_>>().join(", ")) })];
            for img in images {
                parts.push(json!({ "type": "image", "image": base64::engine::general_purpose::STANDARD.encode(&img.bytes), "mediaType": img.mime_type }));
            }
            self.store.session.lock().unwrap().messages.push(json!({ "role": "user", "content": parts }));
            self.store.save()?;
        }
        let f = *flags.lock().unwrap();
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_and_meta() {
        assert_eq!(extract_stream_preview(r#"{"path":"src/a.ts"}"#), "src/a.ts");
        assert_eq!(extract_stream_preview(r#"{"pa"#), "");
        let m = build_tool_meta("bash", &json!({"command": "  echo   hi\n there"}));
        assert_eq!(Value::Object(m), json!({"kind": "exec", "summary": "echo hi there"}));
        let m = build_tool_meta("apply_patch", &json!({"patch": "*** Begin Patch\n*** Update File: a.txt\n*** Add File: b.txt\n*** End Patch"}));
        assert_eq!(m["summary"], json!("a.txt, b.txt"));
        assert_eq!(summarize_diff_counts(Some("--- /dev/null\n+++ b/x\n+a\n+b")), (2, 0, "Added"));
    }
}
