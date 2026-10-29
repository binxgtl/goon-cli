//! The session app: harness lifecycle, approvals, event rendering, turns and the goal loop.
//! Slash commands live in `commands.rs`.

use super::args::Parsed;
use super::{ctx, display_clean, set_exit, stderr_write, stdout_write, write_json_line};
use crate::approval::{bash_approval_exact, bash_approval_prefix, can_auto_approve, is_approved_bash_exact, is_approved_bash_prefix};
use crate::config::{self, Config, RuntimeConfig};
use crate::cost::{create_run_budget, CostLedger, SharedBudget};
use crate::eventlog::{event_log_path, EventLogWriter};
use crate::harness::{Aborted, Harness, HarnessDeps};
use crate::jsutil::{self, now_iso, stringify_pretty, take_chars};
use crate::mcp_client::McpManager;
use crate::memory::MemoryStore;
use crate::pair::PairManager;
use crate::policy::ExecutionPolicy;
use crate::risk::{classify_bash_risk, RiskClass};
use crate::session::{format_goal_status_indicator, same_workspace, SessionRoute, SessionStore};
use crate::subagents::Subagents;
use crate::term::{terminal_text, InputCancelled};
use crate::tools::{ApprovalRequest, Approve, RejectionGuard, ToolRegistry};
use crate::tui::{QuestionOpts, Tui};
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio_util::sync::CancellationToken;

pub const MODE_ORDER: [&str; 4] = ["ask", "auto", "plan", "readonly"];

pub struct AppState {
    pub mode: String,
    pub pre_plan_mode: String,
    pub view_mode: String,
    pub denied: bool,
    pub exiting: bool,
    pub terminating: bool,
    pub starting: bool,
    pub goal_deferred: bool,
    pub rebuild_pending: bool,
    pub reasoning_source: Option<String>,
}

#[derive(Default)]
pub struct AllowSets {
    pub tools: HashSet<String>,
    pub prefixes: BTreeSet<String>,
    pub exact: BTreeSet<String>,
}

pub enum Ui {
    Tui(Arc<Tui>),
    Plain,
    None,
}

pub struct App {
    pub me: Weak<App>,
    pub values: Parsed,
    pub json: bool,
    pub interactive: bool,
    pub output_last_message: Option<String>,
    pub policy: Arc<ExecutionPolicy>,
    pub notify_enabled: bool,
    pub ui: Ui,
    pub memory_store: Option<Arc<MemoryStore>>,
    pub config: Mutex<Config>,
    pub runtime: Mutex<Arc<RuntimeConfig>>,
    pub store: Mutex<Arc<SessionStore>>,
    pub tools: Mutex<Option<Arc<ToolRegistry>>>,
    pub harness: Mutex<Option<Arc<Harness>>>,
    pub manager: Mutex<Option<Arc<McpManager>>>,
    pub subagents: Mutex<Option<Arc<Subagents>>>,
    pub pair: Mutex<Option<Arc<PairManager>>>,
    pub controller: Mutex<Option<CancellationToken>>,
    pub st: Mutex<AppState>,
    pub allowed: Mutex<AllowSets>,
    pub event_writer: Mutex<Option<EventLogWriter>>,
    pub input_cancel: Mutex<CancellationToken>,
    pub rebuild_lock: tokio::sync::Mutex<()>,
}

pub struct TurnResult {
    pub aborted: bool,
    pub had_error: bool,
    pub text: String,
}

fn is_aborted(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Aborted>().is_some() || error.downcast_ref::<InputCancelled>().is_some()
}

impl App {
    // ------------------------------------------------------------------ accessors
    pub fn runtime(&self) -> Arc<RuntimeConfig> {
        self.runtime.lock().unwrap().clone()
    }

    pub fn store(&self) -> Arc<SessionStore> {
        self.store.lock().unwrap().clone()
    }

    pub fn tools(&self) -> Arc<ToolRegistry> {
        self.tools.lock().unwrap().clone().expect("tools built")
    }

    pub fn harness(&self) -> Arc<Harness> {
        self.harness.lock().unwrap().clone().expect("harness built")
    }

    pub fn tui(&self) -> Option<Arc<Tui>> {
        match &self.ui {
            Ui::Tui(t) => Some(t.clone()),
            _ => None,
        }
    }

    pub fn has_rl(&self) -> bool {
        matches!(self.ui, Ui::Plain)
    }

    pub fn mode(&self) -> String {
        self.st.lock().unwrap().mode.clone()
    }

    pub fn view_mode(&self) -> String {
        self.st.lock().unwrap().view_mode.clone()
    }

    pub fn session_id(&self) -> String {
        self.store().id()
    }

    pub fn controller(&self) -> Option<CancellationToken> {
        self.controller.lock().unwrap().clone()
    }

    pub fn set_controller(&self, token: Option<CancellationToken>) {
        *self.controller.lock().unwrap() = token;
    }

    pub fn subagents(&self) -> Option<Arc<Subagents>> {
        self.subagents.lock().unwrap().clone()
    }

    pub fn manager(&self) -> Option<Arc<McpManager>> {
        self.manager.lock().unwrap().clone()
    }

    pub fn pair(&self) -> Option<Arc<PairManager>> {
        self.pair.lock().unwrap().clone()
    }

    pub fn mcp_tool_count(&self) -> usize {
        self.manager().map(|m| m.tools().len()).unwrap_or(0)
    }

    pub fn exiting(&self) -> bool {
        self.st.lock().unwrap().exiting
    }

    // ------------------------------------------------------------------ output
    pub fn display(&self, text: &str) {
        if let Some(p) = self.pair() {
            p.notify_system(&terminal_text(text));
        }
        match self.tui() {
            Some(t) => t.write(text),
            None => display_clean(text),
        }
    }

    pub fn notice(&self, text: &str) {
        if let Some(t) = self.tui() {
            t.set_notice(Some(text));
        }
    }

    pub fn notify_attention(&self) {
        if self.notify_enabled && crate::term::stdout_is_tty() && !self.json {
            stdout_write("\x07");
        }
    }

    pub async fn question(&self, label: &str, cancel: Option<&CancellationToken>, remember: bool) -> Result<String> {
        match &self.ui {
            Ui::Tui(t) => t.question(label, cancel, remember, QuestionOpts::default()).await,
            Ui::Plain => {
                let input_cancel = self.input_cancel.lock().unwrap().clone();
                let merged = CancellationToken::new();
                let m = merged.clone();
                let a = cancel.cloned().unwrap_or_default();
                let watcher = tokio::spawn(async move {
                    tokio::select! {
                        _ = a.cancelled() => m.cancel(),
                        _ = input_cancel.cancelled() => m.cancel(),
                    }
                });
                let result = crate::term::read_line(label, Some(&merged)).await;
                watcher.abort();
                match result {
                    Ok(Some(line)) => Ok(line),
                    Ok(None) => Err(anyhow!("Input closed.")),
                    Err(e) if e.downcast_ref::<InputCancelled>().is_some() => Err(anyhow!("Input cancelled.")),
                    Err(e) => Err(e),
                }
            }
            Ui::None => bail!("Input requires a TTY."),
        }
    }

    /// Plain-mode block + question, or a TUI transient question.
    pub async fn ask_block(&self, block: &str, label: &str, cancel: Option<&CancellationToken>, opts: QuestionOpts) -> Result<String> {
        match self.tui() {
            Some(t) => t.ask_transient(block, label, cancel, opts).await,
            None => {
                self.display(block);
                self.question(label, cancel, false).await
            }
        }
    }

    pub async fn question_hidden(&self, label: &str) -> Result<String> {
        match self.tui() {
            Some(t) => t.question(label, None, false, QuestionOpts { esc_cancels: true, mask: true, ..Default::default() }).await,
            None => Ok(crate::term::read_hidden(label).await?.unwrap_or_default()),
        }
    }

    // ------------------------------------------------------------------ events
    pub fn publish_event(&self, event: &Value, log_only: bool) {
        if let Some(w) = self.event_writer.lock().unwrap().as_mut() {
            w.append(event);
        }
        if !log_only {
            self.render_event(event);
        }
    }

    fn render_event(&self, event: &Value) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let s = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        match kind {
            "user" => return,
            "session" | "done" => {
                if self.json {
                    write_json_line(event);
                }
                return;
            }
            "error" => {
                if self.json {
                    write_json_line(event);
                } else {
                    self.display(&format!("\n{}\n", terminal_text(&s("message"))));
                }
                return;
            }
            _ => {}
        }
        if let Some(p) = self.pair() {
            p.notify_event(event);
        }
        if kind == "stopped" {
            self.st.lock().unwrap().denied = true;
        }
        if kind == "plan_exit" {
            let mode = {
                let mut st = self.st.lock().unwrap();
                st.mode = st.pre_plan_mode.clone();
                st.mode.clone()
            };
            if let Some(t) = self.tui() {
                t.set_mode(&mode);
            }
        }
        if kind == "retry" {
            let attempt = event.get("attempt").map(jsutil::js_string).unwrap_or_default();
            let max = event.get("maxRetries").map(jsutil::js_string).unwrap_or_default();
            if let Some(t) = self.tui() {
                t.set_notice(Some(&format!("↻ API error ({}) — retrying ({attempt}/{max})...", jsutil::slice_chars(&s("message"), 0, 64))));
            } else if !self.json {
                stderr_write(&format!("\n[Retry {attempt}/{max}: {}]\n", terminal_text(&s("message"))));
            }
            return;
        }
        if kind == "tool_stream" {
            if self.json {
                return;
            }
            if let Some(t) = self.tui() {
                t.event(event);
            }
            return;
        }
        if self.json {
            write_json_line(event);
            return;
        }
        if let Some(t) = self.tui() {
            t.event(event);
            return;
        }
        if kind == "reasoning" {
            let mut st = self.st.lock().unwrap();
            if st.view_mode == "normal" {
                return;
            }
            let source = s("source");
            if st.reasoning_source.as_deref() != Some(source.as_str()) {
                stderr_write(&format!("\n[reasoning · {source}]\n"));
            }
            stderr_write(&terminal_text(&s("text")));
            st.reasoning_source = Some(source);
            return;
        }
        {
            let mut st = self.st.lock().unwrap();
            if st.reasoning_source.is_some() {
                stderr_write("\n");
                st.reasoning_source = None;
            }
        }
        let clean = |t: &str| terminal_text(t);
        match kind {
            "text" => stdout_write(&clean(&s("text"))),
            "subagent" if s("state") != "progress" => stderr_write(&format!("\n[agent {}: {}]\n", take_chars(&s("id"), 8), s("state"))),
            "tool" if s("state") == "start" => stderr_write(&format!("\n[{}]\n", s("name"))),
            "todo" => {
                let todos = event.get("todos").and_then(Value::as_array).cloned().unwrap_or_default();
                let done = todos.iter().filter(|t| t.get("status").and_then(Value::as_str) == Some("completed")).count();
                stderr_write(&format!("\n[todo: {done}/{} completed]\n", todos.len()));
            }
            "goal" => {
                let label = match event.get("goal").and_then(|g| serde_json::from_value::<crate::session::SessionGoal>(g.clone()).ok()) {
                    Some(g) => format_goal_status_indicator(&g, 0.0),
                    None => "Goal cleared".into(),
                };
                stderr_write(&format!("\n[{label}]\n"));
            }
            "plan_exit" => stderr_write("\n[Plan approved — exited plan mode]\n"),
            "compaction" => stderr_write("\n[Context compacted; original history archived.]\n"),
            "verify_start" => {
                let files = event.get("files").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                let commands: Vec<String> = event.get("commands").and_then(Value::as_array).map(|a| a.iter().map(jsutil::js_string).collect()).unwrap_or_default();
                let suffix = if commands.is_empty() { String::new() } else { format!(" · {}", commands.join(" · ")) };
                stderr_write(&clean(&format!("\n[verify: {files} file(s){suffix}]\n")));
            }
            "verify_result" => stderr_write(&clean(&format!("\n[verify {}: {}]\n", s("status"), s("reason")))),
            "verify_rollback" => {
                let conflicts = event.get("conflicts").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                let kept = if conflicts > 0 { format!("; kept {conflicts} conflicting file(s)") } else { String::new() };
                stderr_write(&clean(&format!("\n[verify rollback: {}{kept}]\n", s("reason"))));
            }
            "dag_plan_invalid" => stderr_write(&clean(&format!("\n[dag plan invalid: {}]\n", s("message")))),
            "dag_plan_ready" => stderr_write(&clean(&format!("\n[dag plan ready: {} todo(s)]\n", event.get("todos").map(jsutil::js_string).unwrap_or_default()))),
            "dag_todo_update" => stderr_write(&clean(&format!("\n[dag {}: {} ({})]\n", s("todoId"), s("status"), s("role")))),
            "dag_merge" => {
                let conflicts = event.get("conflicts").and_then(Value::as_u64).unwrap_or(0);
                let detail: Vec<String> = [if conflicts > 0 { format!("{conflicts} conflict(s)") } else { String::new() }, s("reason")].into_iter().filter(|x| !x.is_empty()).collect();
                let verified = event.get("verified").and_then(Value::as_bool).unwrap_or(false);
                let detail = if detail.is_empty() { String::new() } else { format!(": {}", detail.join(" — ")) };
                stderr_write(&clean(&format!("\n[dag merge: {}{detail}]\n", if verified { "verified" } else { "NOT verified" })));
            }
            "workflow_classified" => stderr_write(&clean(&format!("\n[workflow {}: {}]\n", s("mode"), s("reason")))),
            "scope_round" => stderr_write(&clean(&format!("\n[scope round {} {}: {}]\n", event.get("round").map(jsutil::js_string).unwrap_or_default(), s("action"), s("reason")))),
            "cost_degrade" => stderr_write(&clean(&format!("\n[cost degrade: {} — {} candidate(s)]\n", s("reason"), event.get("candidates").map(jsutil::js_string).unwrap_or_default()))),
            "stopped" => stderr_write(&format!("\n{}\n", s("reason"))),
            _ => {}
        }
    }

    pub fn emit_fn(&self) -> crate::harness::Emit {
        let me = self.me.clone();
        Arc::new(move |event: Value| {
            if let Some(app) = me.upgrade() {
                app.publish_event(&event, false);
            }
        })
    }

    pub fn sync_tui_header(&self, reset_context: bool) {
        let Some(t) = self.tui() else { return };
        let runtime = self.runtime();
        t.set_workspace(&runtime.cwd, super::read_git_branch(&runtime.cwd).as_deref());
        t.set_context_window(runtime.context_window);
        let store = self.store();
        let (goal, messages) = {
            let s = store.session.lock().unwrap();
            (s.goal.clone(), s.messages.clone())
        };
        t.set_goal(goal);
        if reset_context {
            t.set_context_usage(if messages.is_empty() { None } else { Some(crate::context::estimated_tokens(&messages, "")) });
        }
    }

    pub fn refresh_workspace_files(&self) {
        let Some(t) = self.tui() else { return };
        let files = self.tools().files.clone();
        tokio::spawn(async move {
            if let Ok(list) = tokio::task::spawn_blocking(move || files.list_workspace_files(5000)).await {
                t.set_workspace_files(list);
            }
        });
    }

    // ------------------------------------------------------------------ approvals
    fn approve_fn(&self, reg_slot: Arc<OnceLock<Weak<ToolRegistry>>>) -> Approve {
        let me = self.me.clone();
        Arc::new(move |request: ApprovalRequest| {
            let me = me.clone();
            let reg_slot = reg_slot.clone();
            Box::pin(async move {
                let Some(app) = me.upgrade() else { return false };
                app.approve(request, reg_slot.get().and_then(Weak::upgrade)).await
            })
        })
    }

    async fn approve(&self, request: ApprovalRequest, reg: Option<Arc<ToolRegistry>>) -> bool {
        // Only the live mode counts: a mode captured when the harness was built must not keep
        // auto-approving after the user switches back to ask/plan.
        let command = request.input.get("command").map(jsutil::js_string).unwrap_or_default();
        let risk = request.risk.unwrap_or_else(|| if request.tool == "bash" { classify_bash_risk(if request.input.is_object() { &command } else { "" }) } else { RiskClass::Medium });
        if risk == RiskClass::Critical {
            return false;
        }
        if can_auto_approve(&self.mode(), risk) {
            return true;
        }
        // High-risk calls are never remembered, so they always prompt even after an "always".
        let remembered = risk != RiskClass::High;
        {
            let allowed = self.allowed.lock().unwrap();
            if remembered && allowed.tools.contains(&request.tool) {
                return true;
            }
            if remembered && request.tool == "bash" && (is_approved_bash_exact(&request.input, &allowed.exact) || is_approved_bash_prefix(&request.input, &allowed.prefixes)) {
                return true;
            }
        }
        if matches!(self.ui, Ui::None) {
            return false;
        }
        let cancel = request.cancel.clone();
        if request.tool == "exit_plan_mode" && request.input.get("plan").is_some() {
            let plan = jsutil::js_string(&request.input["plan"]);
            let indented = regex::Regex::new(r"\r?\n").unwrap().split(&plan).map(|l| if l.trim().is_empty() { l.to_string() } else { format!("  {l}") }).collect::<Vec<_>>().join("\n");
            let block = format!("\n≡ Proposed plan\n{indented}\n\n  Approve plan and exit plan mode?\n");
            let reply = self.ask_block(&block, "[y/N] ", Some(&cancel), QuestionOpts::default()).await.unwrap_or_default().trim().to_lowercase();
            return reply == "y" || reply == "yes";
        }
        let diff = reg.as_ref().and_then(|r| r.files.preview_diff(&request.tool, &request.input));
        let raw_preview = if let Some(diff) = diff {
            let lines: Vec<&str> = diff.split('\n').collect();
            if lines.len() > 80 {
                format!("{}\n... (+{} more diff lines)", lines[..80].join("\n"), lines.len() - 80)
            } else {
                diff
            }
        } else if request.tool == "bash" && request.input.get("command").is_some() {
            format!("  $ {}", command.trim())
        } else if request.tool == "verify" && request.input.get("commands").map(Value::is_array).unwrap_or(false) {
            request.input["commands"].as_array().unwrap().iter().map(|c| format!("  $ {}", jsutil::js_string(c))).collect::<Vec<_>>().join("\n")
        } else {
            stringify_pretty(&request.input)
        };
        let runtime = self.runtime();
        let redacted = if runtime.api_key.is_empty() { raw_preview } else { raw_preview.replace(&runtime.api_key, "[REDACTED]") };
        let preview = terminal_text(take_chars(&redacted, 8000));
        let high = risk == RiskClass::High;
        let prefix = if !high && request.tool == "bash" { bash_approval_prefix(&request.input) } else { None };
        let exact = if !high && request.tool == "bash" { bash_approval_exact(&request.input) } else { None };
        let full_command = if request.input.is_object() { command.split_whitespace().collect::<Vec<_>>().join(" ") } else { String::new() };
        let prefix_hint = match &prefix {
            None => String::new(),
            Some(p) if *p == full_command => format!("\n  p = always allow exactly \"{p}\" in this session"),
            Some(p) => format!("\n  p = always allow \"{p} …\" in this session (simple arguments inside the workspace only)"),
        };
        let exact_hint = match &exact {
            Some(e) if Some(e) != prefix.as_ref() => format!("\n  e = always allow exactly \"{e}\" in this session"),
            _ => String::new(),
        };
        let high_hint = if high { format!("\n  High risk ({}): type the tool name \"{}\" to allow it once; it cannot be remembered.", risk.as_str(), request.tool) } else { String::new() };
        let block = format!("\n▲ Allow {}? [{}]\n{preview}{prefix_hint}{exact_hint}{high_hint}\n", request.tool, risk.as_str());
        let label = if high {
            format!("[type \"{}\" to allow, N to deny] ", request.tool)
        } else if exact.is_some() {
            "[y/N/p(refix)/e(xact)/a(lways)] ".into()
        } else if prefix.is_some() {
            "[y/N/p(refix)/a(lways)] ".into()
        } else {
            "[y/N/a(lways)] ".into()
        };
        self.notify_attention();
        if let Some(pair) = self.pair().filter(|p| p.is_running()) {
            let race = CancellationToken::new();
            let parent = cancel.clone();
            let race_child = race.clone();
            let link = tokio::spawn(async move {
                parent.cancelled().await;
                race_child.cancel();
            });
            let remote = pair.request_remote_approval(&request.tool, &preview, race.clone());
            let terminal = async {
                let answer = self.ask_block(&block, &label, Some(&race), QuestionOpts::default()).await;
                let clean = answer.unwrap_or_default().trim().to_lowercase();
                if clean == "a" || clean == "always" {
                    "always".to_string()
                } else if prefix.is_some() && (clean == "p" || clean == "prefix") {
                    "prefix".into()
                } else if clean == "y" || clean == "yes" {
                    "yes".into()
                } else {
                    "no".into()
                }
            };
            let winner = tokio::select! {
                dec = terminal => {
                    remote.cancel(if dec == "prefix" { "yes" } else { &dec });
                    dec
                }
                dec = remote.wait() => {
                    race.cancel();
                    let dec = dec.unwrap_or_else(|| "no".into());
                    self.display(&format!("\n[Paired device approved/denied {}: {dec}]\n", request.tool));
                    dec
                }
            };
            link.abort();
            remote.cancel("closed");
            if winner == "always" {
                if !high {
                    self.allowed.lock().unwrap().tools.insert(request.tool.clone());
                }
                return true;
            }
            if winner == "prefix" {
                if let Some(p) = prefix {
                    self.allowed.lock().unwrap().prefixes.insert(p);
                    return true;
                }
            }
            return winner == "yes";
        }
        let reply = self.ask_block(&block, &label, Some(&cancel), QuestionOpts::default()).await.unwrap_or_default().trim().to_lowercase();
        if high {
            return reply == request.tool.to_lowercase();
        }
        if reply == "a" || reply == "always" {
            self.allowed.lock().unwrap().tools.insert(request.tool.clone());
            return true;
        }
        if let Some(e) = exact.filter(|_| reply == "e" || reply == "exact") {
            self.allowed.lock().unwrap().exact.insert(e);
            return true;
        }
        if let Some(p) = prefix.filter(|_| reply == "p" || reply == "prefix") {
            self.allowed.lock().unwrap().prefixes.insert(p);
            return true;
        }
        reply == "y" || reply == "yes"
    }

    fn ask_user_fn(&self) -> crate::tools::AskUser {
        let me = self.me.clone();
        Arc::new(move |questions: Vec<Value>, cancel: CancellationToken| {
            let me = me.clone();
            Box::pin(async move {
                let app = me.upgrade().ok_or_else(|| anyhow!("Terminal closed."))?;
                let mut out = indexmap::IndexMap::new();
                for q in questions {
                    let options = q.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
                    let label_of = |o: &Value| o.get("label").map(jsutil::js_string).unwrap_or_default();
                    let lines: Vec<String> = options
                        .iter()
                        .enumerate()
                        .map(|(i, o)| {
                            let desc = o.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()).map(|d| format!(" — {d}")).unwrap_or_default();
                            format!("  {}. {}{desc}", i + 1, label_of(o))
                        })
                        .collect();
                    let header = q.get("header").and_then(Value::as_str).filter(|h| !h.is_empty()).unwrap_or("Question");
                    let block = format!("\n◇ {header}\n  {}\n{}\n", q.get("question").map(jsutil::js_string).unwrap_or_default(), lines.join("\n"));
                    let prompt = format!("Select [1-{}] or type answer (Enter for 1): ", options.len());
                    let reply = app.ask_block(&block, &prompt, Some(&cancel), QuestionOpts { esc_hint: Some("picks 1".into()), ..Default::default() }).await?.trim().to_string();
                    let id = q.get("id").map(jsutil::js_string).unwrap_or_default();
                    let pick = if reply.is_empty() {
                        options.first().map(label_of).unwrap_or_default()
                    } else if let Some(n) = reply.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= options.len()) {
                        label_of(&options[n - 1])
                    } else {
                        reply
                    };
                    out.insert(id, vec![pick]);
                }
                Ok(out)
            }) as BoxFuture<'static, Result<indexmap::IndexMap<String, Vec<String>>>>
        })
    }

    // ------------------------------------------------------------------ harness lifecycle
    pub async fn build_harness(&self, current_mode: &str, reconnect_mcp: bool) -> Result<(Arc<ToolRegistry>, Arc<Harness>)> {
        let runtime = self.runtime();
        let slot: Arc<OnceLock<Weak<ToolRegistry>>> = Arc::new(OnceLock::new());
        let reg = ToolRegistry::new(&runtime.cwd, self.approve_fn(slot.clone()), current_mode == "readonly", None);
        let _ = slot.set(Arc::downgrade(&reg));
        reg.set_plan_mode(current_mode == "plan");
        *reg.rejection_guard.lock().unwrap() = Some(Arc::new(SessionRejectionGuard { app: self.me.clone() }));
        reg.set_policy(Some(self.policy.clone()));
        if !matches!(self.ui, Ui::None) {
            reg.callbacks.lock().unwrap().ask_user = Some(self.ask_user_fn());
        }
        // Running MCP servers are reused across rebuilds; they restart only on /mcp reload or a
        // workspace change.
        let no_mcp = self.values.flag("no-mcp");
        let existing = self.manager();
        let reuse = !no_mcp && existing.as_ref().map(|m| m.connected() && !reconnect_mcp && m.cwd == runtime.cwd).unwrap_or(false);
        if reuse {
            existing.as_ref().unwrap().attach(&reg);
        } else if !no_mcp {
            if let Some(m) = existing {
                m.close().await;
                *self.manager.lock().unwrap() = None;
            }
        }
        if !no_mcp && !reuse {
            let token = CancellationToken::new();
            self.set_controller(Some(token.clone()));
            let outcome: Result<()> = async {
                let manager = McpManager::new(crate::mcp::load_mcp_config()?, &runtime.cwd, Some(self.policy.clone()));
                *self.manager.lock().unwrap() = Some(manager.clone());
                let connecting = manager.connect(Some(&reg), Some(&token));
                match self.tui() {
                    Some(t) => t.with_status("Connecting MCP", connecting).await?,
                    None => connecting.await?,
                };
                for st in manager.statuses() {
                    if st.status == "error" {
                        self.display(&format!("[MCP warning: server \"{}\" failed to start: {}]\n", st.name, st.error.unwrap_or_else(|| "unknown error".into())));
                    }
                }
                Ok(())
            }
            .await;
            self.set_controller(None);
            if let Err(error) = outcome {
                if !self.interactive || token.is_cancelled() {
                    return Err(error);
                }
                self.display(&format!("[MCP warning: {}]\n", super::safe_error(&error)));
            }
        }
        let old = self.subagents.lock().unwrap().take();
        if let Some(old) = old {
            old.close_all().await;
        }
        let store = self.store();
        if !self.values.flag("no-subagents") {
            let subagents = Subagents::new(runtime.clone(), store.clone(), reg.clone(), self.emit_fn());
            subagents.register()?;
            *self.subagents.lock().unwrap() = Some(subagents);
            self.register_run_workflow(&reg)?;
        }
        let deps = HarnessDeps { memory: self.memory_store.as_ref().map(|m| Arc::new(crate::memory::MemoryAdapter(m.clone())) as Arc<dyn crate::harness::MemoryHooks>), ..Default::default() };
        let harness = Harness::new(runtime, store, reg.clone(), self.emit_fn(), None, deps)?;
        Ok((reg, harness))
    }

    pub async fn rebuild_harness(&self, reconnect_mcp: bool) -> Result<()> {
        let _guard = self.rebuild_lock.lock().await;
        let mode = self.mode();
        let (tools, harness) = self.build_harness(&mode, reconnect_mcp).await?;
        *self.tools.lock().unwrap() = Some(tools);
        *self.harness.lock().unwrap() = Some(harness);
        Ok(())
    }

    /// Rebuilding closes MCP servers and subagents a running turn uses, so it waits for the turn.
    pub async fn request_rebuild(&self) -> Result<bool> {
        if self.controller().is_some() || self.harness().busy() {
            self.st.lock().unwrap().rebuild_pending = true;
            return Ok(false);
        }
        self.st.lock().unwrap().rebuild_pending = false;
        self.rebuild_harness(false).await?;
        Ok(true)
    }

    pub async fn apply_pending_rebuild(&self) -> Result<()> {
        let pending = self.st.lock().unwrap().rebuild_pending;
        if pending && self.controller().is_none() && !self.harness().busy() {
            self.st.lock().unwrap().rebuild_pending = false;
            self.rebuild_harness(false).await?;
        }
        Ok(())
    }

    pub fn apply_permission_mode(&self, next: &str) {
        {
            let mut st = self.st.lock().unwrap();
            if next == "plan" && st.mode != "plan" {
                st.pre_plan_mode = st.mode.clone();
            }
            st.mode = next.to_string();
        }
        let tools = self.tools();
        tools.set_read_only(next == "readonly");
        tools.set_plan_mode(next == "plan");
        if let Some(t) = self.tui() {
            t.set_mode(next);
        }
        {
            let mut config = self.config.lock().unwrap();
            config.approval_mode = Some(next.to_string());
            let _ = config::save_config(&config);
        }
        let label = if next == "auto" { "auto-approve (low/medium risk auto-approved; high risk still asks, critical always denied)".to_string() } else { next.to_string() };
        self.notice(&format!("Mode: {label}"));
        self.display(&format!("\n[Mode switched to: {label}]\n"));
    }

    pub fn set_plan_flags(&self, mode: &str) {
        let tools = self.tools();
        tools.set_plan_mode(mode == "plan");
        tools.set_read_only(mode == "readonly");
        if let Some(t) = self.tui() {
            t.set_mode(mode);
        }
    }

    pub fn config_value(&self) -> Map<String, Value> {
        self.config.lock().unwrap().to_value().as_object().cloned().unwrap_or_default()
    }

    /// `loadConfig({ ...config, ...changes })` + save + runtime re-resolve.
    pub fn update_config(&self, changes: Map<String, Value>, save: bool) -> Result<()> {
        let mut merged = self.config_value();
        for (k, v) in changes {
            merged.insert(k, v);
        }
        let next = config::load_config(&merged)?;
        if save {
            config::save_config(&next)?;
        }
        *self.config.lock().unwrap() = next;
        Ok(())
    }

    pub fn resolve_runtime_here(&self) -> Result<()> {
        let cwd = self.runtime().cwd.clone();
        let config = self.config.lock().unwrap().clone();
        let runtime = config::resolve_runtime(&config, &cwd)?;
        *ctx().api_key.lock().unwrap() = Some(runtime.api_key.clone()).filter(|k| !k.is_empty());
        *self.runtime.lock().unwrap() = Arc::new(runtime);
        Ok(())
    }

    pub async fn apply_effort(&self, next: Option<String>) -> Result<()> {
        let mut changes = Map::new();
        changes.insert("effort".into(), next.map(Value::String).unwrap_or(Value::Null));
        self.update_config(changes, true)?;
        self.resolve_runtime_here()?;
        let effort = self.runtime().effort.clone();
        if let Some(t) = self.tui() {
            t.set_effort(effort.as_deref());
        }
        self.sync_tui_header(false);
        let applied = self.request_rebuild().await?;
        let suffix = if applied { "" } else { " (applies from the next turn)" };
        let label = effort.unwrap_or_else(|| "default".into());
        self.notice(&format!("Reasoning effort: {label}{suffix}"));
        self.display(&format!("\n[Reasoning effort set to: {label}{suffix}]\n"));
        Ok(())
    }

    // ------------------------------------------------------------------ turns
    pub async fn run_turn(&self, input: &str, goal_step: Option<usize>) -> Result<TurnResult> {
        if input.len() > 1024 * 1024 {
            bail!("Prompt exceeds 1 MiB.");
        }
        self.apply_pending_rebuild().await?;
        self.st.lock().unwrap().denied = false;
        let mut result = TurnResult { aborted: false, had_error: false, text: String::new() };
        let started = jsutil::now_millis();
        let controller = CancellationToken::new();
        self.set_controller(Some(controller.clone()));
        let store = self.store();
        if let Some(t) = self.tui() {
            t.set_status("Running");
            t.mark_turn_start();
            if let Some(step) = goal_step {
                let goal = store.session.lock().unwrap().goal.clone();
                let step_label = if step > 1 { format!(" · turn {step}") } else { String::new() };
                let indicator = goal.as_ref().map(|g| format_goal_status_indicator(g, 0.0)).unwrap_or_else(|| "Pursuing goal".into());
                let objective = goal.map(|g| format!(" — {}", g.objective)).unwrap_or_default();
                t.write(&format!("\n◆ {indicator}{step_label}{objective}\n"));
            } else {
                t.write(&format!("\n> {input}\n"));
            }
        } else if let Some(step) = goal_step.filter(|_| !self.json) {
            let goal = store.session.lock().unwrap().goal.clone();
            let step_label = if step > 1 { format!(" · turn {step}") } else { String::new() };
            self.display(&format!("\n[◆ Pursuing goal{step_label}{}]\n", goal.map(|g| format!(": {}", g.objective)).unwrap_or_default()));
        }
        if let Some(p) = self.pair() {
            p.notify_user_prompt(input);
        }
        self.publish_event(&json!({ "type": "user", "text": input }), true);
        let harness = self.harness();
        match harness.run(input, &controller, None).await {
            Ok(text) => {
                result.text = text.clone();
                let mut ok = true;
                if let Some(path) = &self.output_last_message {
                    let body = if text.ends_with('\n') { text.clone() } else { format!("{text}\n") };
                    if let Err(e) = std::fs::write(std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path)), body) {
                        ok = false;
                        self.turn_error(&anyhow!(e), &controller, &mut result);
                    }
                }
                if ok {
                    let denied = self.st.lock().unwrap().denied;
                    self.publish_event(&json!({ "type": "done", "text": text, "denied": denied, "sessionId": store.id() }), false);
                    if !self.json {
                        match self.tui() {
                            Some(t) => t.write("\n"),
                            None => stdout_write("\n"),
                        }
                    }
                    if denied && !self.interactive {
                        set_exit(2);
                    }
                }
            }
            Err(error) => self.turn_error(&error, &controller, &mut result),
        }
        self.set_controller(None);
        if self.interactive && jsutil::now_millis() - started >= 15_000 {
            self.notify_attention();
        }
        if let Some(t) = self.tui() {
            t.set_status("Ready");
        }
        if let Some(p) = self.pair() {
            p.sync_state();
        }
        self.sync_tui_header(false);
        self.refresh_workspace_files();
        Ok(result)
    }

    fn turn_error(&self, error: &anyhow::Error, controller: &CancellationToken, result: &mut TurnResult) {
        result.aborted = controller.is_cancelled() || is_aborted(error);
        result.had_error = true;
        let terminating = self.st.lock().unwrap().terminating;
        if !self.interactive {
            set_exit(if result.aborted { if terminating { 143 } else { 130 } } else { 1 });
        }
        let message = super::safe_error(error);
        self.publish_event(&json!({ "type": "error", "message": message }), false);
        let (exiting, goal_active) = {
            let st = self.st.lock().unwrap();
            (st.exiting, self.store().session.lock().unwrap().goal.as_ref().map(|g| g.status == "active").unwrap_or(false))
        };
        if result.aborted && !exiting && !terminating && goal_active {
            self.st.lock().unwrap().goal_deferred = true;
            if self.tui().is_some() {
                self.notice("Goal continuation paused after interrupt — run /goal resume to continue.");
            } else {
                self.display("[Goal continuation paused after interrupt — run /goal resume to continue.]\n");
            }
        }
    }

    pub async fn continue_goal_if_idle(&self) -> Result<()> {
        if self.controller().is_some() {
            return Ok(());
        }
        {
            let st = self.st.lock().unwrap();
            if st.exiting || st.terminating || st.denied {
                return Ok(());
            }
        }
        if self.tools().plan_mode() {
            return Ok(());
        }
        let mut empty_turns = 0;
        let mut step = 1;
        loop {
            {
                let st = self.st.lock().unwrap();
                if st.exiting || st.terminating || st.denied || st.goal_deferred {
                    break;
                }
            }
            let tools = self.tools();
            if tools.plan_mode() {
                break;
            }
            if self.tui().map(|t| t.has_queued_prompts()).unwrap_or(false) {
                break;
            }
            let store = self.store();
            let goal = store.session.lock().unwrap().goal.clone();
            let Some(goal) = goal.filter(|g| g.status == "active") else { break };
            if let Some(budget) = goal.token_budget {
                if goal.tokens_used >= budget {
                    let summary = {
                        let mut s = store.session.lock().unwrap();
                        let g = s.goal.as_mut().unwrap();
                        g.status = "budget_limited".into();
                        g.updated_at = now_iso();
                        format_goal_status_indicator(g, 0.0)
                    };
                    store.save()?;
                    self.sync_tui_header(false);
                    if self.tui().is_some() {
                        self.notice(&summary);
                    } else if !self.json {
                        self.display(&format!("\n[{summary}]\n"));
                    }
                    break;
                }
            }
            let prompt = crate::context::build_goal_continuation_prompt(&goal, !tools.plan_mode() && !tools.read_only());
            let res = self.run_turn(&prompt, Some(step)).await?;
            if res.aborted || res.had_error || self.st.lock().unwrap().denied {
                break;
            }
            let store = self.store();
            let after = store.session.lock().unwrap().goal.clone();
            match after {
                Some(g) if g.status == "active" => {
                    if res.text.trim().is_empty() {
                        empty_turns += 1;
                        if empty_turns >= 3 {
                            let (summary, objective) = {
                                let mut s = store.session.lock().unwrap();
                                let g = s.goal.as_mut().unwrap();
                                g.status = "blocked".into();
                                g.updated_at = now_iso();
                                (format_goal_status_indicator(g, 0.0), g.objective.clone())
                            };
                            store.save()?;
                            self.sync_tui_header(false);
                            self.notice(&summary);
                            if !self.json {
                                self.display(&format!("\n[{summary} — stalled after 3 empty continuation turns]\n"));
                            }
                            let _ = objective;
                            break;
                        }
                    } else {
                        empty_turns = 0;
                    }
                }
                Some(g) => {
                    let summary = format_goal_status_indicator(&g, 0.0);
                    self.notice(&summary);
                    if !self.json {
                        self.display(&format!("\n[{summary} — {}]\n", g.objective));
                    }
                    break;
                }
                None => break,
            }
            step += 1;
        }
        Ok(())
    }

    pub async fn run(&self, input: &str) -> Result<()> {
        self.st.lock().unwrap().goal_deferred = false;
        let res = self.run_turn(input, None).await?;
        let goal_active = self.store().session.lock().unwrap().goal.as_ref().map(|g| g.status == "active").unwrap_or(false);
        if !res.aborted && !res.had_error && !self.st.lock().unwrap().denied && goal_active {
            self.continue_goal_if_idle().await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ signals
    pub fn stop(&self) {
        let starting = self.st.lock().unwrap().starting;
        if starting {
            set_exit(130);
        }
        match self.controller() {
            Some(c) if !c.is_cancelled() => c.cancel(),
            _ => {
                self.st.lock().unwrap().exiting = true;
                self.close_input();
            }
        }
    }

    pub fn terminate(&self) {
        {
            let mut st = self.st.lock().unwrap();
            st.terminating = true;
            st.exiting = true;
        }
        if let Some(c) = self.controller() {
            c.cancel();
        }
        self.close_input();
        set_exit(143);
    }

    pub fn close_input(&self) {
        self.input_cancel.lock().unwrap().cancel();
        if let Some(t) = self.tui() {
            t.close();
        }
    }
}

/// Durable rejection streak: three identical denials stop the run even across resumes.
struct SessionRejectionGuard {
    app: Weak<App>,
}

impl RejectionGuard for SessionRejectionGuard {
    fn record_denied(&self, signature: &str) -> u64 {
        let Some(app) = self.app.upgrade() else { return 1 };
        let store = app.store();
        let count = {
            let mut s = store.session.lock().unwrap();
            let map = s.rejected_signatures.get_or_insert_with(Default::default);
            let count = map.shift_remove(signature).unwrap_or(0) + 1;
            map.insert(signature.to_string(), count);
            // Keep the most recent distinct denials only.
            while map.len() > 200 {
                let first = map.keys().next().cloned().unwrap();
                if first == signature {
                    break;
                }
                map.shift_remove(&first);
            }
            count
        };
        let _ = store.save();
        count
    }

    fn reset(&self, signature: &str) {
        let Some(app) = self.app.upgrade() else { return };
        let store = app.store();
        let changed = {
            let mut s = store.session.lock().unwrap();
            match s.rejected_signatures.as_mut() {
                Some(map) if map.get(signature).copied().unwrap_or(0) > 0 => {
                    map.shift_remove(signature);
                    true
                }
                _ => false,
            }
        };
        if changed {
            let _ = store.save();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Session entry
// ---------------------------------------------------------------------------------------------

async fn read_stdin_limited(limit: usize, error: &str) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut stdin = tokio::io::stdin();
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = stdin.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > limit {
            bail!("{error}");
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

pub async fn run_session(values: Parsed, mut config: Config, policy: ExecutionPolicy) -> Result<()> {
    let json_out = values.flag("json");
    let positionals = values.positionals.clone();
    let first = positionals.first().map(String::as_str);
    let mut resume_target = values.str("resume");
    let mut launch_resume_picker = false;
    let launch_pair = values.flag("pair") || first == Some("pair");
    let mut initial_command: Option<String> = None;
    let mut prompt_positionals = positionals.clone();
    if first == Some("pair") {
        prompt_positionals = positionals[1..].to_vec();
    } else if (first == Some("workflow") || first == Some("workflows")) && positionals.get(1).map(|s| s.to_lowercase()) == Some("run".into()) {
        let Some(name) = positionals.get(2) else { bail!("Usage: goon workflow run <name> [args...]") };
        initial_command = Some(format!("/workflow run {name} {}", positionals[3..].join(" ")).trim().to_string());
        prompt_positionals = vec![];
    } else if first == Some("network") || first == Some("brainstorm") {
        let topic = positionals[1..].join(" ").trim().to_string();
        if topic.is_empty() {
            bail!("Usage: goon network <topic>");
        }
        initial_command = Some(format!("/network {topic}"));
        prompt_positionals = vec![];
    } else if first == Some("dag") {
        let goal = positionals[1..].join(" ").trim().to_string();
        if goal.is_empty() {
            bail!("Usage: goon dag <goal>");
        }
        initial_command = Some(format!("/dag {goal}"));
        prompt_positionals = vec![];
    } else if first == Some("resume") {
        match positionals.get(1).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            Some(target) => {
                resume_target = Some(target);
                prompt_positionals = positionals[2..].to_vec();
            }
            None => {
                launch_resume_picker = true;
                prompt_positionals = vec![];
            }
        }
    }
    if values.str("prompt").is_some() && !prompt_positionals.is_empty() {
        bail!("Use either --prompt or positional prompt text.");
    }
    let mut prompt = values.str("prompt").or_else(|| (!prompt_positionals.is_empty()).then(|| prompt_positionals.join(" ")));
    if prompt.is_some() && crate::term::piped_stdin() {
        // `git diff | goon -p "review"`: attach piped data as context.
        let piped = read_stdin_limited(1024 * 1024, &format!("Piped input exceeds {} KiB.", 1024)).await?;
        if !piped.trim().is_empty() {
            prompt = Some(format!("{}\n\n<stdin>\n{}\n</stdin>", prompt.unwrap(), piped.trim_end()));
        }
    }
    let stdin_tty = crate::term::stdin_is_tty();
    if prompt.is_none() && !stdin_tty && initial_command.is_none() {
        let text = read_stdin_limited(1024 * 1024, "Piped prompt exceeds 1 MiB.").await?.trim().to_string();
        if text.is_empty() {
            bail!("Empty input. Supply --prompt or run in an interactive terminal.");
        }
        prompt = Some(text);
    }
    if !config.stub && prompt.is_none() && stdin_tty && !json_out && (config.model.is_none() || config::resolve_api_key(&config).is_none()) {
        config = crate::setup::run_setup_wizard(Some(config), Default::default()).await?;
    }
    let interactive = prompt.is_none();
    let output_last_message = values.str("output-last-message");
    if output_last_message.is_some() && interactive {
        bail!("--output-last-message requires a prompt (-p or piped input).");
    }
    let view_mode = config.view_mode.clone().unwrap_or_else(|| if config.show_reasoning { "verbose".into() } else { "normal".into() });
    if interactive && !json_out {
        config.show_reasoning = true;
    }
    let cwd = values.str("cwd").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut runtime = config::resolve_runtime(&config, &cwd)?;
    *ctx().api_key.lock().unwrap() = Some(runtime.api_key.clone()).filter(|k| !k.is_empty());
    if let Some(w) = super::insecure_base_url_warning(Some(&runtime.base_url)).filter(|_| !json_out) {
        stderr_write(&format!("goon: {w}\n"));
    }
    let sessions_root = runtime.data_dir.join("sessions");
    if launch_resume_picker && resume_target.is_none() {
        let resumable: Vec<_> = SessionStore::list(&sessions_root)?.into_iter().filter(|s| s.messages_count.unwrap_or(0) > 0 || s.title.is_some()).collect();
        if resumable.len() == 1 {
            resume_target = Some(resumable[0].id.clone());
            launch_resume_picker = false;
        } else if resumable.is_empty() {
            launch_resume_picker = false;
        }
    }
    if let Some(target) = resume_target.clone().filter(|t| t != "latest" && values.str("cwd").is_none()) {
        // Resuming by ID from another folder switches to that session's workspace.
        let all = SessionStore::list(&sessions_root)?;
        let wanted = target.to_lowercase();
        let is_index = regex::Regex::new(r"^\d{1,3}$").unwrap().is_match(&wanted);
        let cwd_text = runtime.cwd.to_string_lossy().to_string();
        let found = all.iter().find(|s| s.id.to_lowercase() == wanted).or_else(|| {
            if !is_index && wanted.chars().count() >= 4 {
                all.iter().find(|s| same_workspace(&s.cwd, &cwd_text) && s.id.to_lowercase().starts_with(&wanted)).or_else(|| all.iter().find(|s| s.id.to_lowercase().starts_with(&wanted)))
            } else {
                None
            }
        });
        if let Some(m) = found.filter(|m| !same_workspace(&m.cwd, &cwd_text)) {
            if let Ok(folder) = std::fs::canonicalize(&m.cwd) {
                runtime = config::resolve_runtime(&config, &config::strip_verbatim(folder))?;
                resume_target = Some(m.id.clone());
            }
        }
    }
    if let Err(error) = CostLedger::from_config(&runtime, Some(&runtime.model)) {
        if json_out {
            write_json_line(&json!({ "error": error.0 }));
        } else {
            stderr_write(&format!("goon: {}\n", error.0));
        }
        set_exit(2);
        return Ok(());
    }
    let store = Arc::new(SessionStore::open(&sessions_root, SessionRoute::from_runtime(&runtime), resume_target.as_deref(), None)?);
    let event_writer = config.event_log.then(|| {
        EventLogWriter::open(&event_log_path(&runtime.data_dir, &store.id()), Some(&store.id()), &[runtime.api_key.clone()].into_iter().filter(|k| !k.is_empty()).collect::<Vec<_>>(), Box::new(|m| stderr_write(&format!("goon: event log: {m}\n"))))
    });
    let memory_store = if runtime.memory_enabled { open_memory_store(&runtime).await } else { None };
    let mode = if values.flag("read-only") {
        "readonly".to_string()
    } else if values.flag("plan") {
        "plan".into()
    } else if values.flag("yes") {
        "auto".into()
    } else {
        config.approval_mode.clone().unwrap_or_else(|| "ask".into())
    };
    let pre_plan_mode = if values.flag("yes") || config.approval_mode.as_deref() == Some("auto") { "auto" } else { "ask" }.to_string();
    let notify_enabled = !matches!(std::env::var("GOON_NOTIFY").as_deref(), Ok("0") | Ok("off"));
    let use_tui = interactive && stdin_tty && crate::term::stdout_is_tty() && !json_out && !values.flag("no-tui") && std::env::var("TERM").as_deref() != Ok("dumb");
    let app_slot: Arc<OnceLock<Weak<App>>> = Arc::new(OnceLock::new());
    let ui = if use_tui {
        let title = {
            let s = store.session.lock().unwrap();
            match &s.title {
                Some(t) => format!("{} · {t}", runtime.model),
                None => format!("{} · {}", runtime.model, take_chars(&s.id, 8)),
            }
        };
        let slot = app_slot.clone();
        let tui = Tui::start(&title, Arc::new(move || {
            if let Some(app) = slot.get().and_then(Weak::upgrade) {
                app.stop();
            }
        }))?;
        tui.set_mode(&mode);
        tui.set_view_mode(&view_mode);
        tui.set_effort(runtime.effort.as_deref());
        let (tokens, cost) = {
            let s = store.session.lock().unwrap();
            (s.tokens, s.cost())
        };
        tui.set_tokens(tokens);
        tui.set_cost_usd(cost);
        Ui::Tui(tui)
    } else if stdin_tty {
        Ui::Plain
    } else {
        Ui::None
    };
    let app = Arc::new_cyclic(|me| App {
        me: me.clone(),
        values,
        json: json_out,
        interactive,
        output_last_message,
        policy: Arc::new(policy),
        notify_enabled,
        ui,
        memory_store,
        config: Mutex::new(config),
        runtime: Mutex::new(Arc::new(runtime)),
        store: Mutex::new(store),
        tools: Mutex::new(None),
        harness: Mutex::new(None),
        manager: Mutex::new(None),
        subagents: Mutex::new(None),
        pair: Mutex::new(None),
        controller: Mutex::new(None),
        st: Mutex::new(AppState { mode, pre_plan_mode, view_mode, denied: false, exiting: false, terminating: false, starting: true, goal_deferred: false, rebuild_pending: false, reasoning_source: None }),
        allowed: Mutex::new(AllowSets::default()),
        event_writer: Mutex::new(event_writer),
        input_cancel: Mutex::new(CancellationToken::new()),
        rebuild_lock: tokio::sync::Mutex::new(()),
    });
    let _ = app_slot.set(Arc::downgrade(&app));
    let weak = Arc::downgrade(&app);
    let handler: Arc<dyn Fn(&'static str) + Send + Sync> = Arc::new(move |sig| {
        if let Some(app) = weak.upgrade() {
            if sig == "SIGTERM" {
                app.terminate();
            } else {
                app.stop();
            }
        }
    });
    let _signals = super::on_signals(handler.clone());
    let _control = super::on_control_channel(handler);
    let outcome = app.clone().session_body(prompt, initial_command, launch_resume_picker, launch_pair).await;
    app.shutdown().await;
    outcome
}

async fn open_memory_store(runtime: &RuntimeConfig) -> Option<Arc<MemoryStore>> {
    let api_key = (!runtime.api_key.is_empty()).then(|| runtime.api_key.clone());
    let mut embedder = None;
    if runtime.embedder_provider != "hash" {
        let options = crate::embedder::EmbedderOptions {
            provider: runtime.embedder_provider.clone(),
            model: runtime.embedder_model.clone(),
            base_url: if runtime.embedder_provider == "openai" { Some(runtime.embedder_base_url.clone().unwrap_or_else(|| runtime.base_url.clone())) } else { runtime.embedder_base_url.clone() },
            api_key: if runtime.embedder_provider == "openai" { crate::embedder::embedder_api_key(runtime.embedder_base_url.as_deref(), Some(&runtime.base_url), api_key.as_deref()) } else { None },
            timeout_ms: None,
            batch_size: None,
        };
        match crate::embedder::create_embedder(options) {
            Ok(e) => embedder = Some(e),
            Err(error) => stderr_write(&format!("goon: embedder \"{}\" unavailable ({}) — falling back to the hash embedder.\n", runtime.embedder_provider, super::safe_error(&error))),
        }
    }
    let rag = crate::memory::RagSettings {
        top_k: Some(runtime.rag_top_k as usize),
        capacity: Some(runtime.rag_capacity as usize),
        threshold: Some(runtime.rag_threshold),
        max_files: Some(runtime.rag_index_max_files as usize),
        max_file_bytes: Some(runtime.rag_index_max_file_bytes),
        embedder,
    };
    match MemoryStore::open(&runtime.data_dir, &crate::rag::project_id_for(&runtime.cwd), Some(&runtime.cwd), &api_key.into_iter().collect::<Vec<_>>(), rag).await {
        Ok(store) => Some(store),
        Err(error) => {
            stderr_write(&format!("goon: memory store unavailable ({}) — continuing without memory.\n", super::safe_error(&error)));
            None
        }
    }
}

impl App {
    async fn session_body(self: Arc<Self>, prompt: Option<String>, initial_command: Option<String>, launch_resume_picker: bool, launch_pair: bool) -> Result<()> {
        self.sync_tui_header(true);
        let mode = self.mode();
        let (tools, harness) = self.build_harness(&mode, false).await?;
        *self.tools.lock().unwrap() = Some(tools);
        *self.harness.lock().unwrap() = Some(harness);
        if let Some(t) = self.tui() {
            self.refresh_workspace_files();
            let me = self.me.clone();
            t.set_on_copy(Arc::new(move || {
                let me = me.clone();
                Box::pin(async move {
                    let Some(app) = me.upgrade() else { return };
                    let messages = app.store().session.lock().unwrap().messages.clone();
                    let Some((text, label)) = super::extract_copyable_from_messages(&messages, "") else {
                        app.notice("Nothing to copy yet (no assistant output in this session).");
                        return;
                    };
                    let ok = super::copy_text_to_clipboard(&text).await;
                    let lines = regex::Regex::new(r"\r?\n").unwrap().split(&text).count();
                    let chars = jsutil::utf16_len(&text);
                    app.notice(&if ok { format!("Copied {label} ({chars} chars · {lines} lines) to clipboard") } else { format!("Failed to copy {label} to system clipboard") });
                })
            }));
            let me = self.me.clone();
            t.set_on_toggle_mode(Arc::new(move || {
                if let Some(app) = me.upgrade() {
                    let mode = app.mode();
                    let idx = MODE_ORDER.iter().position(|m| *m == mode).map(|i| i as i64).unwrap_or(-1);
                    let next = MODE_ORDER[((idx + 1) as usize) % MODE_ORDER.len()];
                    app.apply_permission_mode(next);
                }
            }));
            let me = self.me.clone();
            t.set_on_cycle_effort(Arc::new(move |direction: i32| {
                let Some(app) = me.upgrade() else { return };
                let mut ring: Vec<Option<String>> = vec![None];
                ring.extend(config::EFFORT_LEVELS.iter().map(|l| Some(l.to_string())));
                let current = app.runtime().effort.clone();
                let idx = ring.iter().position(|r| *r == current).map(|i| i as i64).unwrap_or(-1);
                let len = ring.len() as i64;
                let next = ring[((idx + direction as i64 + len) % len) as usize].clone();
                tokio::spawn(async move {
                    if let Err(error) = app.apply_effort(next).await {
                        app.display(&format!("\n[Effort change failed: {}]\n", super::safe_error(&error)));
                    }
                });
            }));
            let me = self.me.clone();
            t.set_on_immediate_command(Arc::new(move |cmd: String| {
                let me = me.clone();
                Box::pin(async move {
                    let Some(app) = me.upgrade() else { return };
                    let handled = match app.execute_immediate_command(&cmd).await {
                        Ok(h) => h,
                        Err(error) => {
                            app.display(&format!("{}\n", super::safe_error(&error)));
                            true
                        }
                    };
                    if !handled {
                        if cmd.starts_with("/revert") || cmd.starts_with("/undo") || cmd == "/checkpoints" {
                            if let Some(c) = app.controller() {
                                c.cancel();
                            }
                        }
                        if let Some(t) = app.tui() {
                            t.queue_prompt(&cmd);
                            t.set_notice(Some(&format!("Queued command \"{cmd}\" to run as soon as current step finishes.")));
                        }
                    }
                })
            }));
        }
        self.st.lock().unwrap().starting = false;
        let runtime = self.runtime();
        let store = self.store();
        self.publish_event(&json!({ "type": "session", "id": store.id(), "cwd": runtime.cwd.to_string_lossy() }), false);
        if !self.json {
            if self.interactive {
                self.display(&self.banner());
                self.render_resumed_history(true);
            } else {
                self.display(&format!("goon · {} · session {}\n", runtime.model, store.id()));
            }
        }
        if launch_pair {
            let pm = self.ensure_pair_manager();
            match self.tui() {
                Some(t) => {
                    t.set_notice(Some("Starting Cloudflared tunnel & waiting for DNS..."));
                    t.with_status("Starting Cloudflared tunnel", pm.start(false)).await?;
                }
                None => {
                    self.display("Starting Cloudflared tunnel & waiting for DNS...\n");
                    pm.start(false).await?;
                }
            }
            self.display(&format!("\n{}\n", pm.format_banner()));
        }
        if let Some(p) = &prompt {
            self.run(p).await?;
        }
        if prompt.is_none() {
            if matches!(self.ui, Ui::None) && initial_command.is_none() {
                bail!("Interactive mode requires a TTY.");
            }
            if let Some(t) = self.tui() {
                t.set_status("Ready");
            }
            self.main_loop(initial_command.or_else(|| launch_resume_picker.then(|| "/resume".to_string()))).await?;
        }
        Ok(())
    }

    pub fn banner(&self) -> String {
        let runtime = self.runtime();
        super::welcome_banner(&runtime.model, &runtime.provider, &runtime.base_url, runtime.effort.as_deref(), &runtime.cwd, &self.session_id(), &self.mode())
    }

    async fn shutdown(&self) {
        if let Some(t) = self.tui() {
            t.close();
        }
        self.input_cancel.lock().unwrap().cancel();
        if let Some(p) = self.pair() {
            p.stop().await;
        }
        let subagents = self.subagents.lock().unwrap().take();
        if let Some(s) = subagents {
            s.close_all().await;
        }
        if let Some(m) = self.manager() {
            m.close().await;
        }
        if let Some(w) = self.event_writer.lock().unwrap().as_mut() {
            w.close();
        }
        if let Some(m) = &self.memory_store {
            m.close().await;
        }
        let _ = self.store().close();
    }

    pub fn render_resumed_history(&self, show_banner: bool) {
        let store = self.store();
        let (messages, title, checkpoints, todos, id) = {
            let s = store.session.lock().unwrap();
            (s.messages.clone(), s.title.clone(), s.checkpoints.as_ref().map(|c| c.len()).unwrap_or(0), s.todos.clone(), s.id.clone())
        };
        if messages.is_empty() {
            return;
        }
        let referenced = regex::Regex::new(r#"(?s)<referenced_file\s+path="([^"]+)".*?</referenced_file>"#).unwrap();
        if let Some(t) = self.tui() {
            for m in &messages[messages.len().saturating_sub(30)..] {
                let role = m.get("role").and_then(Value::as_str).unwrap_or("");
                if role == "user" {
                    let raw = match m.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(parts)) => parts
                            .iter()
                            .map(|p| match p.get("text").and_then(Value::as_str) {
                                Some(t) => t.to_string(),
                                None if p.get("type").and_then(Value::as_str) == Some("image") => "[Image]".into(),
                                None => String::new(),
                            })
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" "),
                        _ => String::new(),
                    };
                    if raw.starts_with("<codex_internal_context source=\"") {
                        continue;
                    }
                    let cleaned = referenced.replace_all(&raw, "[@$1]").replace("\r\n", " ").replace('\n', " ").trim().to_string();
                    if !cleaned.is_empty() {
                        t.mark_turn_start();
                        self.display(&format!("\n> {cleaned}\n"));
                    }
                } else if role == "assistant" {
                    match m.get("content") {
                        Some(Value::String(s)) if !s.trim().is_empty() => self.display(&format!("\n[answer]\n{}\n", s.trim())),
                        Some(Value::Array(parts)) => {
                            for part in parts {
                                let text = part.get("text").and_then(Value::as_str).map(str::trim).unwrap_or("");
                                if text.is_empty() {
                                    continue;
                                }
                                match part.get("type").and_then(Value::as_str) {
                                    Some("reasoning") => self.display(&format!("\n[reasoning]\n{text}\n")),
                                    Some("text") => self.display(&format!("\n[answer]\n{text}\n")),
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if show_banner {
                let suffix = title.map(|t| format!(" ({t})")).unwrap_or_default();
                self.display(&format!("\n↻ Resumed session {}{suffix} · {} messages · {checkpoints} checkpoints\n", take_chars(&id, 8), messages.len()));
            }
            if let Some(todos) = todos.filter(|t| !t.is_empty()) {
                t.event(&json!({ "type": "todo", "todos": serde_json::to_value(todos).unwrap() }));
            }
            return;
        }
        let mut lines = vec![format!("\n↻ Resumed session {}{}: {} messages, {checkpoints} checkpoints", take_chars(&id, 8), title.map(|t| format!(" ({t})")).unwrap_or_default(), messages.len())];
        let preview: Vec<&Value> = messages.iter().filter(|m| matches!(m.get("role").and_then(Value::as_str), Some("user") | Some("assistant"))).collect();
        let attached = regex::Regex::new(r"(?s)<referenced_file.*?</referenced_file>").unwrap();
        for m in &preview[preview.len().saturating_sub(6)..] {
            let text = match m.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .map(|p| match p.get("text").and_then(Value::as_str) {
                        Some(t) => t.to_string(),
                        None if p.get("type").and_then(Value::as_str) == Some("tool-call") => format!("[tool:{}]", p.get("toolName").map(jsutil::js_string).unwrap_or_default()),
                        None => String::new(),
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            if text.starts_with("<codex_internal_context source=\"") {
                continue;
            }
            let cleaned = attached.replace_all(&text, "[attached file]").split_whitespace().collect::<Vec<_>>().join(" ");
            if !cleaned.is_empty() {
                let marker = if m.get("role").and_then(Value::as_str) == Some("user") { '›' } else { '◆' };
                let ellipsis = if jsutil::utf16_len(&cleaned) > 140 { "…" } else { "" };
                lines.push(format!("  {marker} {}{ellipsis}", jsutil::slice_chars(&cleaned, 0, 140)));
            }
        }
        self.display(&format!("{}\n", lines.join("\n")));
    }

    pub fn ensure_pair_manager(&self) -> Arc<PairManager> {
        if let Some(p) = self.pair() {
            return p;
        }
        let me = self.me.clone();
        let callbacks = crate::pair::PairCallbacks {
            get_state: Arc::new({
                let me = me.clone();
                move || {
                    let Some(app) = me.upgrade() else { return json!({}) };
                    let runtime = app.runtime();
                    let status = match app.tui() {
                        Some(t) => t.status(),
                        None => if app.controller().is_some() { "Running".into() } else { "Ready".into() },
                    };
                    let tokens = app.store().session.lock().unwrap().tokens;
                    json!({ "model": runtime.model, "provider": runtime.provider, "cwd": runtime.cwd.to_string_lossy(), "sessionId": app.session_id(), "mode": app.mode(), "status": status, "tokens": jsutil::num(tokens), "contextWindow": runtime.context_window })
                }
            }),
            get_history: Arc::new({
                let me = me.clone();
                move || me.upgrade().map(|app| app.pair_history()).unwrap_or_default()
            }),
            on_remote_prompt: Arc::new({
                let me = me.clone();
                move |text: String| {
                    let Some(app) = me.upgrade() else { return };
                    if crate::pair::is_forbidden_remote_input(&text) {
                        app.display("\n[Blocked remote attempt to run direct shell (!cmd) or escalate to auto-approve mode.]\n");
                        return;
                    }
                    if let Some(t) = app.tui() {
                        t.submit_external_prompt(&text);
                    } else {
                        tokio::spawn(async move {
                            let handled = app.execute_immediate_command(&text).await.unwrap_or(true);
                            if !handled && app.controller().is_none() {
                                if let Err(error) = app.run(&text).await {
                                    app.display(&format!("{}\n", super::safe_error(&error)));
                                }
                            }
                        });
                    }
                }
            }),
            on_remote_mode_change: Arc::new({
                let me = me.clone();
                move |mode: String| {
                    let Some(app) = me.upgrade() else { return };
                    if mode == "auto" {
                        app.display("\n[Blocked remote attempt to switch to auto-approve mode.]\n");
                        return;
                    }
                    app.apply_permission_mode(&mode);
                }
            }),
            on_remote_stop: Arc::new({
                let me = me.clone();
                move || {
                    if let Some(c) = me.upgrade().and_then(|a| a.controller()) {
                        c.cancel();
                    }
                }
            }),
            on_notice: Arc::new(move |msg: String| {
                if let Some(app) = me.upgrade() {
                    app.notice(&msg);
                }
            }),
        };
        let pm = PairManager::new(callbacks);
        *self.pair.lock().unwrap() = Some(pm.clone());
        pm
    }

    fn pair_history(&self) -> Vec<(String, String)> {
        let referenced = regex::Regex::new(r#"(?s)<referenced_file\s+path="([^"]+)".*?</referenced_file>"#).unwrap();
        let messages = self.store().session.lock().unwrap().messages.clone();
        let mut out = vec![];
        for m in &messages {
            match m.get("role").and_then(Value::as_str) {
                Some("user") => {
                    let raw = match m.get("content") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Array(parts)) => parts
                            .iter()
                            .map(|p| match p.get("text").and_then(Value::as_str) {
                                Some(t) => t.to_string(),
                                None if p.get("type").and_then(Value::as_str) == Some("image") => "[Image]".into(),
                                None => String::new(),
                            })
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(" "),
                        _ => String::new(),
                    };
                    let cleaned = referenced.replace_all(&raw, "[@$1]").trim().to_string();
                    if !cleaned.is_empty() {
                        out.push(("user".into(), cleaned));
                    }
                }
                Some("assistant") => match m.get("content") {
                    Some(Value::String(s)) if !s.trim().is_empty() => out.push(("assistant".into(), s.trim().to_string())),
                    Some(Value::Array(parts)) => {
                        let text = parts
                            .iter()
                            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        if !text.is_empty() {
                            out.push(("assistant".into(), text));
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        out
    }

    /// A shared run budget for workflow/DAG work that outlives a single harness run.
    pub fn fresh_budget(&self) -> SharedBudget {
        let runtime = self.runtime();
        create_run_budget(&runtime, None)
    }
}

pub fn path_text(path: &Path) -> String {
    path.to_string_lossy().to_string()
}
