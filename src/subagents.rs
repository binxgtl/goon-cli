//! Subagents: synchronous delegation (`delegate_task` / `delegate_edit`), Codex-style background
//! agents (`spawn_agent`, `wait_agent`, `send_input`, `close_agent`, `resume_agent`, `list_agents`)
//! and the Agent Network tools. Children never receive delegation tools (depth is one).

use crate::config::RuntimeConfig;
use crate::cost::{create_run_budget, RunBudget, SharedBudget};
use crate::errors::safe_error;
use crate::files::limit_text;
use crate::harness::{Aborted, Emit, Harness, HarnessDeps};
use crate::jsutil::{now_iso, stringify, take_chars};
use crate::network::{format_brainstorm_report, AgentNetwork, AgentNetworkBus, BrainstormOptions, NetworkNodeInfo};
use crate::reasoning::last_assistant_answer;
use crate::session::{SessionRoute, SessionStore};
use crate::tools::{catalog_definition, is_permission_denied, ApprovalRequest, Approve, PermissionDenied, ToolExec, ToolRegistry};
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const CODEX_AGENT_NICKNAMES: [&str; 33] = [
    "Euclid",
    "Archimedes",
    "Hypatia",
    "Galileo",
    "Descartes",
    "Pascal",
    "Fermat",
    "Leibniz",
    "Newton",
    "Euler",
    "Gauss",
    "Faraday",
    "Lovelace",
    "Boole",
    "Maxwell",
    "Curie",
    "Planck",
    "Tesla",
    "Noether",
    "Hilbert",
    "Einstein",
    "Bohr",
    "Turing",
    "Feynman",
    "Sagan",
    "Socrates",
    "Plato",
    "Aristotle",
    "Kant",
    "Godel",
    "Nash",
    "Ramanujan",
    "Erdos",
];

/// Child progress is a control channel: text/reasoning and per-request diagnostics stay internal.
pub fn is_child_progress_event(event: &Value) -> bool {
    !matches!(event.get("type").and_then(Value::as_str), Some("text" | "reasoning" | "tool_stream" | "context_budget" | "memory_indexed" | "memory_retrieved" | "memory_error"))
}

pub fn build_role_prompt(role: &str, task: &str) -> String {
    match role {
        "explorer" => format!("You are a delegated agent. Role: explorer. Inspect the codebase using read-only tools (read_file, list_files, search) to answer the question below quickly and authoritatively with exact file and line evidence. Do not ask other agents.\n\n{task}"),
        "worker" => format!("You are a delegated agent. Role: worker. Complete the assigned implementation or verification task below. Respect file ownership boundaries, do not revert unrelated changes made by other agents, and return a concise report listing changed files and verification evidence. Do not ask other agents.\n\n{task}"),
        "planner" => format!("You are a delegated agent. Role: planner. Using read-only tools (read_file, list_files, search), analyze the codebase for the task below and return a structured, actionable plan with concrete steps and file references. Do not modify files and do not ask other agents.\n\n{task}"),
        "coder" => format!("You are a delegated agent. Role: coder. Implement exactly the task below in this workspace with write/edit tools, keep the change minimal, and do not revert unrelated changes made by other agents. Return a concise report listing changed files and evidence. Do not ask other agents.\n\n{task}"),
        "reviewer" => format!("You are a delegated agent. Role: reviewer. Using read-only tools (read_file, list_files, search), review the work described below for correctness, edge cases, and regressions, and return a verdict (approve or reject) with evidence. Do not modify files and do not ask other agents.\n\n{task}"),
        "tester" => format!("You are a delegated agent. Role: tester. Verify the change described below by reading the affected files and running the relevant tests with bash. Shell commands still require the same user approval as anywhere else. Return a pass/fail verdict with evidence. Do not ask other agents.\n\n{task}"),
        "researcher" => format!("You are a delegated agent. Role: researcher. Using read-only tools (read_file, list_files, search), research the question below and return a factual report with exact file and line evidence. Do not modify files and do not ask other agents.\n\n{task}"),
        _ => format!("You are a delegated agent. Complete this task and return a concise report with evidence. Do not ask other agents.\n\n{task}"),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentStatus {
    Running,
    Completed,
    Errored,
    Shutdown,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::Completed => "completed",
            AgentStatus::Errored => "errored",
            AgentStatus::Shutdown => "shutdown",
        }
    }
}

pub struct ManagedSubagent {
    pub id: String,
    pub nickname: String,
    pub agent_type: String,
    pub read_only: bool,
    pub model: String,
    pub effort: Option<String>,
    pub task: String,
    pub status: AgentStatus,
    pub last_output: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
    pub controller: CancellationToken,
    /// Completion of the current turn (`true` once it finished).
    pub done: Option<tokio::sync::watch::Receiver<bool>>,
    pub notify: Arc<Notify>,
}

fn status_payload(agent: Option<&ManagedSubagent>) -> Value {
    match agent {
        None => json!("not_found"),
        Some(a) => match a.status {
            AgentStatus::Running => json!("running"),
            AgentStatus::Shutdown => json!("shutdown"),
            AgentStatus::Completed => json!({ "completed": a.last_output.clone().unwrap_or_default() }),
            AgentStatus::Errored => json!({ "errored": a.last_error.clone().unwrap_or_else(|| "Subagent failed.".into()) }),
        },
    }
}

#[derive(Clone, Debug)]
pub struct SubagentSummary {
    pub id: String,
    pub nickname: String,
    pub role: String,
    pub status: String,
    pub task: String,
    pub summary: Option<String>,
    pub updated_at: String,
    pub messages_count: usize,
}

#[derive(Default)]
struct Slots {
    active: usize,
}

pub struct RunTasksOptions {
    pub read_only: bool,
    pub cancel: CancellationToken,
    pub budget: Option<SharedBudget>,
    pub allowed_tools: Option<HashSet<String>>,
    pub role: Option<String>,
}

pub struct Subagents {
    pub config: Arc<RuntimeConfig>,
    pub parent: Arc<SessionStore>,
    pub registry: Arc<ToolRegistry>,
    pub emit: Emit,
    pub bus: Arc<AgentNetworkBus>,
    pub network: AgentNetwork,
    slots: Mutex<Slots>,
    slot_free: Notify,
    nickname_index: Mutex<usize>,
    used_nicknames: Mutex<HashSet<String>>,
    pub spawned: Mutex<IndexMap<String, ManagedSubagent>>,
    pub metadata: Mutex<IndexMap<String, (String, String, String)>>,
    me: Mutex<Weak<Subagents>>,
}

fn sync_exec<F>(weak: Weak<Subagents>, f: F) -> ToolExec
where
    F: Fn(Arc<Subagents>, Value, CancellationToken) -> BoxFuture<'static, Result<String>> + Send + Sync + 'static,
{
    let f = Arc::new(f);
    Arc::new(move |input, cancel| {
        let weak = weak.clone();
        let f = f.clone();
        Box::pin(async move {
            let Some(me) = weak.upgrade() else { bail!("Subagents are no longer available.") };
            f(me, input, cancel).await
        })
    })
}

impl Subagents {
    pub fn new(config: Arc<RuntimeConfig>, parent: Arc<SessionStore>, registry: Arc<ToolRegistry>, emit: Emit) -> Arc<Subagents> {
        let bus = AgentNetworkBus::new();
        let network = AgentNetwork::new(config.clone(), parent.clone(), bus.clone());
        let me = Arc::new(Subagents {
            config,
            parent,
            registry,
            emit,
            bus,
            network,
            slots: Mutex::new(Slots::default()),
            slot_free: Notify::new(),
            nickname_index: Mutex::new(0),
            used_nicknames: Mutex::new(HashSet::new()),
            spawned: Mutex::new(IndexMap::new()),
            metadata: Mutex::new(IndexMap::new()),
            me: Mutex::new(Weak::new()),
        });
        *me.me.lock().unwrap() = Arc::downgrade(&me);
        me
    }

    fn arc(&self) -> Arc<Subagents> {
        self.me.lock().unwrap().upgrade().expect("subagents alive")
    }

    pub fn subagents_root(&self) -> PathBuf {
        self.config.data_dir.join("subagents").join(self.parent.id())
    }

    fn allocate_nickname(&self, preferred: Option<&str>) -> String {
        let mut used = self.used_nicknames.lock().unwrap();
        for a in self.spawned.lock().unwrap().values() {
            used.insert(a.nickname.to_lowercase());
        }
        for (nick, _, _) in self.metadata.lock().unwrap().values() {
            used.insert(nick.to_lowercase());
        }
        if let Some(p) = preferred.filter(|p| !p.is_empty()) {
            if !used.contains(&p.to_lowercase()) {
                used.insert(p.to_lowercase());
                return p.to_string();
            }
        }
        let mut index = self.nickname_index.lock().unwrap();
        let n = CODEX_AGENT_NICKNAMES.len();
        for i in 0..n {
            let candidate = CODEX_AGENT_NICKNAMES[(*index + i) % n];
            if !used.contains(&candidate.to_lowercase()) {
                *index = (*index + i + 1) % n;
                used.insert(candidate.to_lowercase());
                return candidate.to_string();
            }
        }
        let fallback = format!("Agent-{}", used.len() + 1);
        used.insert(fallback.to_lowercase());
        fallback
    }

    /// The shared run budget for every child (the parent's when a run is active).
    fn run_budget(&self) -> SharedBudget {
        if let Some(b) = self.registry.budget() {
            return b;
        }
        let seeded = Arc::new(Mutex::new(RunBudget {
            spent: 0,
            limit: self.config.run_token_budget,
            cost_spent: self.parent.session.lock().unwrap().cost(),
            cost_limit: self.config.max_cost_usd,
            started_at: None,
            time_limit_ms: self.config.max_time_ms,
            ledger: None,
        }));
        create_run_budget(&self.config, Some(seeded))
    }

    fn resolve_agent_id(&self, target: &str) -> Option<String> {
        let clean = target.trim();
        let spawned = self.spawned.lock().unwrap();
        if spawned.contains_key(clean) {
            return Some(clean.to_string());
        }
        let lower = clean.to_lowercase();
        let lower = lower.strip_prefix("/root/").unwrap_or(&lower).to_string();
        spawned.values().find(|a| a.id.to_lowercase() == lower || a.id.to_lowercase().starts_with(&lower) || a.nickname.to_lowercase() == lower).map(|a| a.id.clone())
    }

    pub fn register(self: &Arc<Self>) -> Result<()> {
        let weak = Arc::downgrade(self);
        let reg = &self.registry;
        reg.register(
            "delegate_task",
            catalog_definition(
                "delegate_task",
                Some(sync_exec(weak.clone(), |me, input, cancel| {
                    Box::pin(async move {
                        let tasks = string_list(&input, "tasks");
                        me.run_tasks(tasks, RunTasksOptions { read_only: true, cancel, budget: None, allowed_tools: None, role: Some("explorer".into()) }).await
                    })
                })),
            ),
        )?;
        reg.register(
            "delegate_edit",
            catalog_definition(
                "delegate_edit",
                Some(sync_exec(weak.clone(), |me, input, cancel| {
                    Box::pin(async move {
                        let tasks = string_list(&input, "tasks");
                        me.run_tasks(tasks, RunTasksOptions { read_only: false, cancel, budget: None, allowed_tools: None, role: Some("worker".into()) }).await
                    })
                })),
            ),
        )?;
        reg.register("spawn_agent", catalog_definition("spawn_agent", Some(sync_exec(weak.clone(), |me, input, cancel| Box::pin(async move { me.spawn_agent_tool(input, cancel).await })))))?;
        reg.register("wait_agent", catalog_definition("wait_agent", Some(sync_exec(weak.clone(), |me, input, cancel| Box::pin(async move { me.wait_agent_tool(input, cancel).await })))))?;
        reg.register("send_input", catalog_definition("send_input", Some(sync_exec(weak.clone(), |me, input, cancel| Box::pin(async move { me.send_input_tool(input, cancel).await })))))?;
        reg.register(
            "close_agent",
            catalog_definition("close_agent", Some(sync_exec(weak.clone(), |me, input, _| Box::pin(async move { me.close_agent_tool(input["target"].as_str().unwrap_or("")).await })))),
        )?;
        reg.register(
            "resume_agent",
            catalog_definition("resume_agent", Some(sync_exec(weak.clone(), |me, input, _| Box::pin(async move { me.resume_agent_tool(input["target"].as_str().unwrap_or("")) })))),
        )?;
        reg.register(
            "list_agents",
            catalog_definition(
                "list_agents",
                Some(sync_exec(weak.clone(), |me, _, _| {
                    Box::pin(async move {
                        let spawned = me.spawned.lock().unwrap();
                        let agents: Vec<Value> =
                            spawned.values().map(|a| json!({ "agent_id": a.id, "agent_name": a.nickname, "agent_type": a.agent_type, "agent_status": status_payload(Some(a)) })).collect();
                        Ok(stringify(&json!({ "agents": agents })))
                    })
                })),
            ),
        )?;
        reg.register(
            "agent_network",
            catalog_definition(
                "agent_network",
                Some(sync_exec(weak.clone(), |me, input, cancel| {
                    Box::pin(async move {
                        let budget = me.run_budget();
                        let res = me
                            .network
                            .brainstorm(BrainstormOptions {
                                topic: input["topic"].as_str().unwrap_or("").to_string(),
                                roles: input.get("roles").map(|_| string_list(&input, "roles")),
                                rounds: input.get("rounds").and_then(Value::as_u64).map(|r| r as u32),
                                explore_codebase: input.get("explore_codebase").and_then(Value::as_bool),
                                cancel,
                                budget: Some(budget),
                                policy: me.registry.policy(),
                                on_progress: None,
                            })
                            .await?;
                        Ok(limit_text(&format_brainstorm_report(&res), 24_000))
                    })
                })),
            ),
        )?;
        self.bus.register_peer_tools(&self.registry, "Lead", "lead")?;
        Ok(())
    }

    async fn acquire(&self, cancel: &CancellationToken, max: usize) -> Result<()> {
        loop {
            if cancel.is_cancelled() {
                return Err(Aborted::default().into());
            }
            let notified = self.slot_free.notified();
            {
                let mut slots = self.slots.lock().unwrap();
                if slots.active < max {
                    slots.active += 1;
                    return Ok(());
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = cancel.cancelled() => return Err(Aborted::default().into()),
            }
        }
    }

    fn release(&self) {
        self.slots.lock().unwrap().active -= 1;
        self.slot_free.notify_one();
    }

    fn child_approve(&self, denied: Arc<Mutex<bool>>) -> Approve {
        let parent = self.registry.approve.clone();
        Arc::new(move |request: ApprovalRequest| {
            let parent = parent.clone();
            let denied = denied.clone();
            Box::pin(async move {
                let allowed = parent(request).await;
                if !allowed {
                    *denied.lock().unwrap() = true;
                }
                allowed
            })
        })
    }

    fn child_emit(&self, id: String) -> Emit {
        let emit = self.emit.clone();
        Arc::new(move |event: Value| {
            if is_child_progress_event(&event) {
                emit(json!({ "type": "subagent", "id": id, "state": "progress", "event": event }));
            }
        })
    }

    fn add_parent_cost(&self, delta: f64) {
        if delta <= 0.0 {
            return;
        }
        {
            let mut s = self.parent.session.lock().unwrap();
            s.cost_usd = Some(s.cost() + delta);
        }
        let _ = self.parent.save();
    }

    async fn spawn_agent_tool(&self, input: Value, parent_cancel: CancellationToken) -> Result<String> {
        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
        let raw_role = s("agent_type").unwrap_or_else(|| if self.registry.effective_read_only() { "explorer".into() } else { "default".into() });
        let role = match raw_role.as_str() {
            "explorer" | "research" => "explorer",
            "worker" => "worker",
            _ => "default",
        };
        let child_read_only = role == "explorer" || self.registry.effective_read_only();
        if !child_read_only && self.registry.read_only() {
            bail!("Writable subagents unavailable in read-only mode. Use agent_type=\"explorer\".");
        }
        let mut child_config = (*self.config).clone();
        if let Some(m) = s("model") {
            child_config.model = m;
        }
        if let Some(e) = s("reasoning_effort") {
            child_config.config.effort = Some(e);
        }
        let child_config = Arc::new(child_config);
        let nickname = self.allocate_nickname(s("nickname").as_deref());
        let message = s("message").unwrap_or_default();
        let store = SessionStore::open(&self.subagents_root(), SessionRoute::from_runtime(&child_config), None, None)?;
        let id = store.id();
        store.session.lock().unwrap().title = Some(format!("{nickname} [{role}]"));
        if input.get("fork_context") == Some(&Value::Bool(true)) {
            let parent_messages = self.parent.session.lock().unwrap().messages.clone();
            let mut s = store.session.lock().unwrap();
            for m in parent_messages {
                let role = m["role"].as_str().unwrap_or("");
                if role == "user" || (role == "assistant" && m["content"].is_string()) {
                    s.messages.push(m);
                }
            }
        }
        store.save()?;
        store.close()?;
        drop(store);
        let controller = parent_cancel.child_token();
        let notify = Arc::new(Notify::new());
        self.spawned.lock().unwrap().insert(
            id.clone(),
            ManagedSubagent {
                id: id.clone(),
                nickname: nickname.clone(),
                agent_type: role.into(),
                read_only: child_read_only,
                model: child_config.model.clone(),
                effort: child_config.effort.clone(),
                task: message.clone(),
                status: AgentStatus::Running,
                last_output: None,
                last_error: None,
                updated_at: now_iso(),
                controller: controller.clone(),
                done: None,
                notify,
            },
        );
        self.metadata.lock().unwrap().insert(id.clone(), (nickname.clone(), role.into(), message.clone()));
        self.bus.register_node(NetworkNodeInfo { id: id.clone(), nickname: nickname.clone(), role: role.into(), status: "running".into(), task: Some(take_chars(&message, 120).to_string()) });
        let budget = self.run_budget();
        let prompt = build_role_prompt(role, &message);
        let done = self.start_managed_turn(&id, child_config, prompt, budget);
        if input.get("wait") == Some(&Value::Bool(true)) {
            wait_done(done).await;
            let spawned = self.spawned.lock().unwrap();
            return Ok(stringify(&json!({ "agent_id": id, "nickname": nickname, "agent_type": role, "status": status_payload(spawned.get(&id)) })));
        }
        Ok(stringify(&json!({ "agent_id": id, "nickname": nickname, "agent_type": role, "status": "running" })))
    }

    fn start_managed_turn(&self, id: &str, config: Arc<RuntimeConfig>, prompt: String, budget: SharedBudget) -> tokio::sync::watch::Receiver<bool> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        if let Some(a) = self.spawned.lock().unwrap().get_mut(id) {
            a.done = Some(rx.clone());
        }
        let me = self.arc();
        let id = id.to_string();
        tokio::spawn(async move {
            me.execute_managed_turn(&id, config, prompt, budget).await;
            let _ = tx.send(true);
        });
        rx
    }

    async fn execute_managed_turn(&self, id: &str, config: Arc<RuntimeConfig>, prompt: String, budget: SharedBudget) {
        let (cancel, read_only, nickname, agent_type, task) = {
            let s = self.spawned.lock().unwrap();
            let a = &s[id];
            (a.controller.clone(), a.read_only, a.nickname.clone(), a.agent_type.clone(), a.task.clone())
        };
        let denied = Arc::new(Mutex::new(false));
        let mut store: Option<Arc<SessionStore>> = None;
        let mut acquired = false;
        let mut failure: Option<String> = None;
        let outcome: Result<String> = async {
            self.acquire(&cancel, if read_only { 4 } else { 2 }).await?;
            acquired = true;
            {
                let b = budget.lock().unwrap();
                if b.limit > 0 && b.spent >= b.limit {
                    bail!("Shared run token budget reached.");
                }
            }
            let st = Arc::new(SessionStore::open(&self.subagents_root(), SessionRoute::from_runtime(&config), Some(id), None)?);
            store = Some(st.clone());
            (self.emit)(json!({ "type": "subagent", "id": id, "task": task, "state": "start" }));
            let tools = ToolRegistry::new(&config.cwd, self.child_approve(denied.clone()), read_only || self.registry.read_only(), None);
            tools.update_flags(|f| f.allow_peer_tools_in_read_only = true);
            tools.set_policy(self.registry.policy());
            tools.set_cost_ledger(self.registry.cost_ledger());
            self.bus.register_peer_tools(&tools, &nickname, &agent_type)?;
            let inbox = self.bus.drain_unread_prompt_block(&nickname);
            let effective = if inbox.is_empty() { prompt.clone() } else { format!("{prompt}\n{inbox}") };
            let harness = Harness::new(config.clone(), st.clone(), tools, self.child_emit(id.to_string()), None, HarnessDeps { verify_enabled: Some(false), ..Default::default() })?;
            harness.set_untrusted_prompts(true);
            let before = st.session.lock().unwrap().cost();
            let run = harness.run(&effective, &cancel, Some(budget.clone())).await;
            self.add_parent_cost(st.session.lock().unwrap().cost() - before);
            run?;
            if *denied.lock().unwrap() {
                return Err(PermissionDenied("Permission denied in a subagent. Stop this run.".into()).into());
            }
            let messages = st.session.lock().unwrap().messages.clone();
            Ok(last_assistant_answer(&messages, &config.inline_reasoning))
        }
        .await;
        match outcome {
            Ok(answer) => {
                let mut s = self.spawned.lock().unwrap();
                let a = s.get_mut(id).unwrap();
                a.last_output = Some(limit_text(&answer, 12_000));
                a.last_error = None;
                if a.status != AgentStatus::Shutdown {
                    a.status = AgentStatus::Completed;
                }
            }
            Err(error) => {
                let message = safe_error(&error.to_string(), Some(&config.api_key));
                failure = Some(message.clone());
                let mut s = self.spawned.lock().unwrap();
                let a = s.get_mut(id).unwrap();
                a.last_error = Some(message);
                if a.status != AgentStatus::Shutdown {
                    a.status = AgentStatus::Errored;
                }
            }
        }
        if let Some(st) = &store {
            let last = st.session.lock().unwrap().checkpoints.as_ref().and_then(|c| c.last().cloned());
            if let Some(cp) = last {
                self.registry.files.merge_backups(cp.files);
            }
            if let Err(error) = st.close() {
                let message = safe_error(&error.to_string(), Some(&config.api_key));
                failure.get_or_insert(message.clone());
                let mut s = self.spawned.lock().unwrap();
                let a = s.get_mut(id).unwrap();
                a.last_error = Some(message);
                if a.status != AgentStatus::Shutdown {
                    a.status = AgentStatus::Errored;
                }
            }
        }
        if acquired {
            self.release();
        }
        let (status, task, notify) = {
            let mut s = self.spawned.lock().unwrap();
            let a = s.get_mut(id).unwrap();
            a.updated_at = now_iso();
            (a.status.as_str().to_string(), a.task.clone(), a.notify.clone())
        };
        self.bus.update_node_status(&nickname, &status, Some(take_chars(&task, 120)));
        if store.is_some() {
            (self.emit)(json!({ "type": "subagent", "id": id, "state": if failure.is_some() { "error" } else { "done" } }));
        }
        notify.notify_waiters();
    }

    async fn wait_agent_tool(&self, input: Value, cancel: CancellationToken) -> Result<String> {
        let targets: Vec<String> = match input.get("targets").and_then(Value::as_array) {
            Some(t) if !t.is_empty() => t.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            _ => self.spawned.lock().unwrap().values().filter(|a| a.status != AgentStatus::Shutdown).map(|a| a.id.clone()).collect(),
        };
        if targets.is_empty() {
            return Ok(stringify(&json!({ "status": {}, "timed_out": false })));
        }
        let resolved: Vec<(String, Option<String>)> = targets.iter().map(|t| (t.clone(), self.resolve_agent_id(t))).collect();
        let all_done = |me: &Subagents| {
            let s = me.spawned.lock().unwrap();
            resolved.iter().all(|(_, id)| id.as_ref().and_then(|i| s.get(i)).map(|a| a.status != AgentStatus::Running).unwrap_or(true))
        };
        if !all_done(self) {
            let timeout = input.get("timeout_ms").and_then(Value::as_u64).unwrap_or(120_000).clamp(100, 600_000);
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout);
            loop {
                let notifies: Vec<Arc<Notify>> = {
                    let s = self.spawned.lock().unwrap();
                    resolved.iter().filter_map(|(_, id)| id.as_ref().and_then(|i| s.get(i)).map(|a| a.notify.clone())).collect()
                };
                let waits: Vec<_> = notifies.iter().map(|n| Box::pin(n.notified())).collect();
                if all_done(self) {
                    break;
                }
                tokio::select! {
                    _ = futures_util::future::select_all(waits) => {}
                    _ = tokio::time::sleep_until(deadline) => break,
                    _ = cancel.cancelled() => return Err(Aborted("Wait cancelled".into()).into()),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                }
                if all_done(self) {
                    break;
                }
            }
        }
        let s = self.spawned.lock().unwrap();
        let mut status = Map::new();
        let mut any_final = false;
        for (key, id) in &resolved {
            let agent = id.as_ref().and_then(|i| s.get(i));
            let payload = status_payload(agent);
            if payload != json!("running") {
                any_final = true;
            }
            status.insert(agent.map(|a| a.id.clone()).unwrap_or_else(|| key.clone()), payload);
        }
        Ok(stringify(&json!({ "status": status, "timed_out": !any_final })))
    }

    async fn send_input_tool(&self, input: Value, parent_cancel: CancellationToken) -> Result<String> {
        let target = input["target"].as_str().unwrap_or("").to_string();
        let message = input["message"].as_str().unwrap_or("").to_string();
        let Some(id) = self.resolve_agent_id(&target) else { bail!("Agent with target \"{target}\" not found.") };
        let (status, nickname, controller, done) = {
            let s = self.spawned.lock().unwrap();
            let a = &s[&id];
            (a.status.clone(), a.nickname.clone(), a.controller.clone(), a.done.clone())
        };
        if status == AgentStatus::Shutdown {
            bail!("Agent \"{nickname}\" ({id}) is closed. Call resume_agent first.");
        }
        if status == AgentStatus::Running {
            if input.get("interrupt") == Some(&Value::Bool(true)) {
                controller.cancel();
            }
            if let Some(d) = done {
                wait_done(d).await;
            }
        }
        let (model, effort) = {
            let mut s = self.spawned.lock().unwrap();
            let a = s.get_mut(&id).unwrap();
            a.controller = parent_cancel.child_token();
            a.status = AgentStatus::Running;
            a.task = message.clone();
            (a.model.clone(), a.effort.clone())
        };
        let mut child_config = (*self.config).clone();
        child_config.model = model;
        if effort.is_some() {
            child_config.config.effort = effort;
        }
        let budget = self.run_budget();
        let done = self.start_managed_turn(&id, Arc::new(child_config), message, budget);
        let submission = uuid::Uuid::new_v4().to_string();
        if input.get("wait") == Some(&Value::Bool(true)) {
            wait_done(done).await;
            let s = self.spawned.lock().unwrap();
            return Ok(stringify(&json!({ "submission_id": submission, "agent_id": id, "nickname": nickname, "status": status_payload(s.get(&id)) })));
        }
        Ok(stringify(&json!({ "submission_id": submission, "agent_id": id, "nickname": nickname, "status": "running" })))
    }

    async fn close_agent_tool(&self, target: &str) -> Result<String> {
        let Some(id) = self.resolve_agent_id(target) else { return Ok(stringify(&json!({ "previous_status": "not_found" }))) };
        let (previous, controller, done, notify) = {
            let mut s = self.spawned.lock().unwrap();
            let a = s.get_mut(&id).unwrap();
            let previous = status_payload(Some(a));
            a.status = AgentStatus::Shutdown;
            (previous, a.controller.clone(), a.done.clone(), a.notify.clone())
        };
        controller.cancel();
        if let Some(d) = done {
            wait_done(d).await;
        }
        notify.notify_waiters();
        Ok(stringify(&json!({ "previous_status": previous })))
    }

    fn resume_agent_tool(&self, target: &str) -> Result<String> {
        let Some(id) = self.resolve_agent_id(target) else { bail!("Agent with target \"{target}\" not found.") };
        let mut s = self.spawned.lock().unwrap();
        let a = s.get_mut(&id).unwrap();
        if a.status == AgentStatus::Shutdown {
            a.status = if a.last_error.is_some() { AgentStatus::Errored } else { AgentStatus::Completed };
        }
        Ok(stringify(&json!({ "status": status_payload(Some(a)) })))
    }

    pub async fn close_all(&self) {
        let mut waits = vec![];
        {
            let mut s = self.spawned.lock().unwrap();
            for a in s.values_mut() {
                if a.status == AgentStatus::Running {
                    a.status = AgentStatus::Shutdown;
                    a.controller.cancel();
                    if let Some(d) = a.done.clone() {
                        waits.push(d);
                    }
                }
                a.notify.notify_waiters();
            }
        }
        for d in waits {
            wait_done(d).await;
        }
    }

    pub fn list_subagent_summaries(&self) -> Vec<SubagentSummary> {
        let mut out = vec![];
        let mut seen = HashSet::new();
        for a in self.spawned.lock().unwrap().values() {
            seen.insert(a.id.clone());
            let text = a.last_output.clone().or(a.last_error.clone());
            out.push(SubagentSummary {
                id: a.id.clone(),
                nickname: a.nickname.clone(),
                role: a.agent_type.clone(),
                status: a.status.as_str().into(),
                task: a.task.clone(),
                summary: text.filter(|t| !t.is_empty()).map(|t| take_chars(&t.split_whitespace().collect::<Vec<_>>().join(" "), 140).to_string()),
                updated_at: a.updated_at.clone(),
                messages_count: 0,
            });
        }
        if let Ok(disk) = SessionStore::list(&self.subagents_root()) {
            let meta = self.metadata.lock().unwrap();
            for item in disk {
                if seen.contains(&item.id) {
                    if let Some(e) = out.iter_mut().find(|s| s.id == item.id) {
                        e.messages_count = item.messages_count.unwrap_or(0);
                    }
                    continue;
                }
                let m = meta.get(&item.id);
                out.push(SubagentSummary {
                    id: item.id.clone(),
                    nickname: m.map(|x| x.0.clone()).or(item.title.clone()).unwrap_or_else(|| item.id[..8].to_string()),
                    role: m.map(|x| x.1.clone()).unwrap_or_else(|| "default".into()),
                    status: "completed".into(),
                    task: m.map(|x| x.2.clone()).or(item.preview.clone()).unwrap_or_default(),
                    summary: None,
                    updated_at: item.updated_at.clone(),
                    messages_count: item.messages_count.unwrap_or(0),
                });
            }
        }
        out
    }

    pub fn read_subagent_transcript(&self, target: &str) -> Option<String> {
        let managed = self.resolve_agent_id(target);
        let root = self.subagents_root();
        let target_id = match &managed {
            Some(id) => id.clone(),
            None => {
                let entries = std::fs::read_dir(&root).ok()?;
                let lower = target.trim().to_lowercase();
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".json"))
                    .find(|n| n[..n.len() - 5].to_lowercase().starts_with(&lower))
                    .map(|n| n[..n.len() - 5].to_string())?
            }
        };
        let spawned = self.spawned.lock().unwrap();
        let agent = managed.as_ref().and_then(|id| spawned.get(id));
        match std::fs::read_to_string(root.join(format!("{target_id}.json"))).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(raw) => {
                let messages = raw.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
                let answer = last_assistant_answer(&messages, &self.config.inline_reasoning);
                let nickname = agent
                    .map(|a| a.nickname.clone())
                    .or_else(|| self.metadata.lock().unwrap().get(&target_id).map(|m| m.0.clone()))
                    .or_else(|| raw.get("title").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| take_chars(&target_id, 8).to_string());
                let report =
                    if !answer.is_empty() { answer } else { agent.and_then(|a| a.last_output.clone().filter(|o| !o.is_empty()).or(a.last_error.clone())).unwrap_or_else(|| "(no output yet)".into()) };
                Some(format!("Subagent {nickname} ({target_id}):\n{report}"))
            }
            None => agent.map(|a| {
                let report = a.last_output.clone().filter(|o| !o.is_empty()).or(a.last_error.clone()).unwrap_or_else(|| format!("Status: {}", a.status.as_str()));
                format!("Subagent {} ({}):\n{report}", a.nickname, a.id)
            }),
        }
    }

    /// Public spawn path for delegated tasks and the DAG; `allowed_tools` pins the child toolset.
    pub async fn run_tasks(&self, tasks: Vec<String>, opts: RunTasksOptions) -> Result<String> {
        if !opts.read_only && self.registry.read_only() {
            bail!("Writable subagents unavailable in read-only mode.");
        }
        let allowed: HashSet<String> = opts.allowed_tools.clone().unwrap_or_else(|| self.registry.exposed_tool_names().into_iter().collect());
        let budget = opts.budget.clone().unwrap_or_else(|| self.run_budget());
        let results = Arc::new(Mutex::new(vec!["Skipped.".to_string(); tasks.len()]));
        let next = Arc::new(Mutex::new(0usize));
        let denied = Arc::new(Mutex::new(false));
        let workers = if opts.read_only { 2.min(tasks.len()) } else { 1 };
        let worker = || async {
            loop {
                if opts.cancel.is_cancelled() || *denied.lock().unwrap() {
                    break;
                }
                let index = {
                    let mut n = next.lock().unwrap();
                    if *n >= tasks.len() {
                        break;
                    }
                    *n += 1;
                    *n - 1
                };
                let outcome = self.child(&tasks[index], opts.read_only, &opts.cancel, &budget, opts.role.as_deref(), Some(allowed.clone())).await;
                let text = match outcome {
                    Ok(report) => report,
                    Err(error) => {
                        if is_permission_denied(&error) {
                            *denied.lock().unwrap() = true;
                        }
                        format!("Task failed: {}", safe_error(&error.to_string(), Some(&self.config.api_key)))
                    }
                };
                results.lock().unwrap()[index] = text;
            }
        };
        futures_util::future::join_all((0..workers).map(|_| worker())).await;
        if opts.cancel.is_cancelled() {
            return Err(Aborted::default().into());
        }
        if *denied.lock().unwrap() {
            return Err(PermissionDenied("Permission denied in a subagent. Stop this run.".into()).into());
        }
        let joined = results.lock().unwrap().join("\n\n");
        Ok(limit_text(&joined, 32_000))
    }

    async fn child(&self, task: &str, read_only: bool, cancel: &CancellationToken, budget: &SharedBudget, role: Option<&str>, allowed: Option<HashSet<String>>) -> Result<String> {
        let role = role.map(str::to_string).unwrap_or_else(|| if read_only { "explorer".into() } else { "worker".into() });
        self.acquire(cancel, 2).await?;
        let denied = Arc::new(Mutex::new(false));
        let mut store: Option<Arc<SessionStore>> = None;
        let outcome: Result<String> = async {
            {
                let b = budget.lock().unwrap();
                if b.limit > 0 && b.spent >= b.limit {
                    bail!("Shared run token budget reached.");
                }
            }
            let st = Arc::new(SessionStore::open(&self.subagents_root(), SessionRoute::from_runtime(&self.config), None, None)?);
            store = Some(st.clone());
            let id = st.id();
            let nickname = self.allocate_nickname(None);
            self.metadata.lock().unwrap().insert(id.clone(), (nickname, role.clone(), task.to_string()));
            (self.emit)(json!({ "type": "subagent", "id": id, "task": task, "state": "start" }));
            let tools = ToolRegistry::new(&self.config.cwd, self.child_approve(denied.clone()), read_only || self.registry.read_only(), allowed);
            tools.set_policy(self.registry.policy());
            tools.set_cost_ledger(self.registry.cost_ledger());
            let harness = Harness::new(self.config.clone(), st.clone(), tools, self.child_emit(id.clone()), None, HarnessDeps { verify_enabled: Some(false), ..Default::default() })?;
            harness.set_untrusted_prompts(true);
            let before = st.session.lock().unwrap().cost();
            let run = harness.run(&build_role_prompt(&role, task), cancel, Some(budget.clone())).await;
            self.add_parent_cost(st.session.lock().unwrap().cost() - before);
            run?;
            if *denied.lock().unwrap() {
                return Err(PermissionDenied("Permission denied in a subagent. Stop this run.".into()).into());
            }
            let messages = st.session.lock().unwrap().messages.clone();
            let answer = last_assistant_answer(&messages, &self.config.inline_reasoning);
            Ok(format!("Session {id}\nTask: {task}\n{}", limit_text(&answer, 12_000)))
        }
        .await;
        let mut failure = outcome.as_ref().err().map(|e| anyhow!(e.to_string()));
        let mut primary = outcome;
        if let Some(st) = &store {
            let cps = st.session.lock().unwrap().checkpoints.clone().unwrap_or_default();
            for cp in cps {
                self.registry.files.merge_backups(cp.files);
            }
            if let Err(e) = st.close() {
                if failure.is_none() {
                    failure = Some(anyhow!(e.to_string()));
                    primary = Err(e);
                }
            }
        }
        self.release();
        if let Some(st) = &store {
            (self.emit)(json!({ "type": "subagent", "id": st.id(), "state": if failure.is_some() { "error" } else { "done" } }));
        }
        primary
    }
}

async fn wait_done(mut rx: tokio::sync::watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            break;
        }
    }
}

fn string_list(input: &Value, key: &str) -> Vec<String> {
    input.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_prompts_share_prefix() {
        for role in ["explorer", "worker", "planner", "coder", "reviewer", "tester", "researcher", "default"] {
            assert!(build_role_prompt(role, "t").starts_with("You are a delegated agent."));
        }
        assert!(is_child_progress_event(&json!({"type": "tool"})));
        assert!(!is_child_progress_event(&json!({"type": "text"})));
    }
}
