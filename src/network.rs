//! Agent Network: a shared peer-to-peer message bus + blackboard for agents in one session, and the
//! multi-persona brainstorm ("council room") built on it.

use crate::config::RuntimeConfig;
use crate::cost::{RunBudget, SharedBudget};
use crate::errors::safe_error;
use crate::files::limit_text;
use crate::harness::{is_aborted, noop_emit, Harness, HarnessDeps};
use crate::jsutil::{now_iso, now_millis, stringify, take_chars};
use crate::policy::ExecutionPolicy;
use crate::reasoning::last_assistant_answer;
use crate::session::{SessionRoute, SessionStore};
use crate::tools::{catalog_definition, deny_all, ToolRegistry};
use anyhow::Result;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct NetworkMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub topic: Option<String>,
    pub content: String,
    pub created_at: String,
    pub read_by: HashSet<String>,
}

#[derive(Clone, Debug)]
pub struct BlackboardEntry {
    pub id: String,
    pub author: String,
    pub role: String,
    pub topic: String,
    pub kind: String,
    pub content: String,
    pub round: Option<u32>,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct NetworkNodeInfo {
    pub id: String,
    pub nickname: String,
    pub role: String,
    pub status: String,
    pub task: Option<String>,
}

impl NetworkNodeInfo {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("nickname".into(), json!(self.nickname));
        m.insert("role".into(), json!(self.role));
        m.insert("status".into(), json!(self.status));
        if let Some(t) = &self.task {
            m.insert("task".into(), json!(t));
        }
        Value::Object(m)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NetworkPersona {
    pub id: String,
    pub name: String,
    pub role: String,
    pub focus: String,
}

pub fn default_persona(key: &str) -> Option<NetworkPersona> {
    let (name, role, focus) = match key {
        "architect" => ("Architect", "System Architect", "Modular architecture, clean abstractions, data flow, extensibility, and long-term maintainability."),
        "researcher" => ("Researcher", "Codebase & Domain Researcher", "Existing codebase patterns, concrete file/line evidence, reusable utilities, and edge-case discovery."),
        "critic" => ("Critic", "Devil’s Advocate & Risk Analyst", "Failure modes, race conditions, security flaws, over-engineering traps, and hidden assumptions."),
        "pragmatist" => ("Pragmatist", "Pragmatic Builder", "Simplicity, minimal dependencies, developer experience (DX), fast delivery, and testability."),
        "security" => ("Security", "Security & Reliability Engineer", "Auth, input validation, least privilege, secret handling, resource bounds, and safe defaults."),
        _ => return None,
    };
    Some(NetworkPersona { id: key.into(), name: name.into(), role: role.into(), focus: focus.into() })
}

fn short_id() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

fn collapse(text: &str) -> String {
    static WS: OnceLock<Regex> = OnceLock::new();
    WS.get_or_init(|| Regex::new(r"\s+").unwrap()).replace_all(text, " ").to_string()
}

#[derive(Default)]
struct BusState {
    messages: Vec<NetworkMessage>,
    blackboard: Vec<BlackboardEntry>,
    nodes: indexmap::IndexMap<String, NetworkNodeInfo>,
}

/// Shared P2P message bus and blackboard for all agents in a session.
#[derive(Default)]
pub struct AgentNetworkBus {
    state: Mutex<BusState>,
}

impl AgentNetworkBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register_node(&self, info: NetworkNodeInfo) {
        self.state.lock().unwrap().nodes.insert(info.nickname.to_lowercase(), info);
    }

    pub fn update_node_status(&self, nickname: &str, status: &str, task: Option<&str>) {
        if let Some(n) = self.state.lock().unwrap().nodes.get_mut(&nickname.to_lowercase()) {
            n.status = status.into();
            if let Some(t) = task {
                n.task = Some(t.into());
            }
        }
    }

    pub fn list_nodes(&self) -> Vec<NetworkNodeInfo> {
        self.state.lock().unwrap().nodes.values().cloned().collect()
    }

    pub fn messages(&self) -> Vec<NetworkMessage> {
        self.state.lock().unwrap().messages.clone()
    }

    pub fn blackboard(&self) -> Vec<BlackboardEntry> {
        self.state.lock().unwrap().blackboard.clone()
    }

    pub fn send(&self, from: &str, to: &str, content: &str, kind: Option<&str>, topic: Option<&str>) -> NetworkMessage {
        let msg = NetworkMessage {
            id: short_id(),
            from: from.into(),
            to: to.trim().into(),
            kind: kind.unwrap_or("message").into(),
            topic: topic.filter(|t| !t.is_empty()).map(|t| t.trim().to_string()),
            content: content.trim().into(),
            created_at: now_iso(),
            read_by: [from.to_lowercase()].into_iter().collect(),
        };
        let mut s = self.state.lock().unwrap();
        s.messages.push(msg.clone());
        if s.messages.len() > 500 {
            s.messages.remove(0);
        }
        msg
    }

    pub fn post_blackboard(&self, author: &str, role: &str, topic: &str, content: &str, kind: Option<&str>, round: Option<u32>) -> BlackboardEntry {
        let entry = BlackboardEntry {
            id: short_id(),
            author: author.into(),
            role: role.into(),
            topic: topic.trim().into(),
            kind: kind.unwrap_or("idea").into(),
            content: content.trim().into(),
            round,
            created_at: now_iso(),
        };
        let mut s = self.state.lock().unwrap();
        s.blackboard.push(entry.clone());
        if s.blackboard.len() > 200 {
            s.blackboard.remove(0);
        }
        entry
    }

    pub fn read_inbox(&self, nickname: &str, include_read: bool) -> Vec<NetworkMessage> {
        let lower = nickname.to_lowercase();
        let mut s = self.state.lock().unwrap();
        let self_id = s.nodes.get(&lower).map(|n| n.id.to_lowercase());
        let mut out = vec![];
        for m in s.messages.iter_mut() {
            let target = m.to.to_lowercase();
            let recipient = target == "*" || target == "all" || target == lower || self_id.as_ref().map(|id| id.starts_with(&target)).unwrap_or(false);
            if !recipient || m.from.to_lowercase() == lower {
                continue;
            }
            if !include_read && m.read_by.contains(&lower) {
                continue;
            }
            m.read_by.insert(lower.clone());
            out.push(m.clone());
        }
        out
    }

    pub fn drain_unread_prompt_block(&self, nickname: &str) -> String {
        let unread = self.read_inbox(nickname, false);
        if unread.is_empty() {
            return String::new();
        }
        let lines: Vec<String> =
            unread.iter().map(|m| format!("  • [{}] from {}{}: {}", m.kind.to_uppercase(), m.from, m.topic.as_ref().map(|t| format!(" ({t})")).unwrap_or_default(), m.content)).collect();
        format!("\n<peer_messages_inbox>\nPeer agents sent you the following messages:\n{}\n</peer_messages_inbox>\n", lines.join("\n"))
    }

    pub fn clear(&self) {
        let mut s = self.state.lock().unwrap();
        s.messages.clear();
        s.blackboard.clear();
    }

    pub fn format_board_summary(&self, limit: usize) -> String {
        let (nodes, board, messages) = {
            let s = self.state.lock().unwrap();
            (s.nodes.values().cloned().collect::<Vec<_>>(), s.blackboard.clone(), s.messages.clone())
        };
        let mut lines = vec![format!("Agent Network Nodes ({}):", nodes.len())];
        if nodes.is_empty() {
            lines.push("  (no active peer nodes registered yet)".into());
        }
        for n in &nodes {
            lines.push(format!("  • {} [{}] — {}{}", n.nickname, n.role, n.status, n.task.as_ref().map(|t| format!(" · {}", take_chars(t, 70))).unwrap_or_default()));
        }
        lines.push(String::new());
        lines.push(format!("Shared Blackboard ({} entries):", board.len()));
        if board.is_empty() {
            lines.push("  (blackboard is empty)".into());
        }
        for b in board.iter().skip(board.len().saturating_sub(limit)) {
            let round = b.round.filter(|r| *r > 0).map(|r| format!(" · R{r}")).unwrap_or_default();
            lines.push(format!("  [{}] {} ({}{round}) [{}] — {}", b.id, b.author, b.role, b.kind, b.topic));
            lines.push(format!("    {}{}", take_chars(&collapse(&b.content), 220), if crate::jsutil::utf16_len(&b.content) > 220 { "…" } else { "" }));
        }
        lines.push(String::new());
        lines.push(format!("Direct Peer Messages ({}):", messages.len()));
        if messages.is_empty() {
            lines.push("  (no peer messages sent yet)".into());
        }
        for m in messages.iter().skip(messages.len().saturating_sub(limit)) {
            lines.push(format!("  [{}] {} → {} [{}]{}: {}", m.id, m.from, m.to, m.kind, m.topic.as_ref().map(|t| format!(" ({t})")).unwrap_or_default(), take_chars(&collapse(&m.content), 180)));
        }
        lines.join("\n")
    }

    /// Registers `peer_list`, `peer_send`, `peer_broadcast` and `peer_read` for one agent identity.
    pub fn register_peer_tools(self: &Arc<Self>, registry: &ToolRegistry, self_nickname: &str, self_role: &str) -> Result<()> {
        let nick = self_nickname.to_string();
        let role = self_role.to_string();
        let bus = self.clone();
        let n = nick.clone();
        registry.register(
            "peer_list",
            catalog_definition(
                "peer_list",
                Some(Arc::new(move |_input, _cancel| {
                    let bus = bus.clone();
                    let n = n.clone();
                    Box::pin(async move {
                        let (count_b, count_m) = {
                            let s = bus.state.lock().unwrap();
                            (s.blackboard.len(), s.messages.len())
                        };
                        Ok(stringify(&json!({ "self": n, "agents": bus.list_nodes().iter().map(|x| x.to_value()).collect::<Vec<_>>(), "blackboard_count": count_b, "messages_count": count_m })))
                    })
                })),
            ),
        )?;
        let bus = self.clone();
        let n = nick.clone();
        registry.register(
            "peer_send",
            catalog_definition(
                "peer_send",
                Some(Arc::new(move |input: Value, _cancel| {
                    let bus = bus.clone();
                    let n = n.clone();
                    Box::pin(async move {
                        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
                        let msg = bus.send(&n, &s("to").unwrap_or_default(), &s("message").unwrap_or_default(), s("kind").as_deref(), s("topic").as_deref());
                        Ok(stringify(&json!({ "ok": true, "message_id": msg.id, "from": msg.from, "to": msg.to, "kind": msg.kind })))
                    })
                })),
            ),
        )?;
        let bus = self.clone();
        let n = nick.clone();
        let r = role.clone();
        registry.register(
            "peer_broadcast",
            catalog_definition(
                "peer_broadcast",
                Some(Arc::new(move |input: Value, _cancel| {
                    let bus = bus.clone();
                    let n = n.clone();
                    let r = r.clone();
                    Box::pin(async move {
                        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
                        let entry = bus.post_blackboard(&n, &r, &s("topic").unwrap_or_default(), &s("message").unwrap_or_default(), s("kind").as_deref(), None);
                        Ok(stringify(&json!({ "ok": true, "entry_id": entry.id, "author": entry.author, "topic": entry.topic, "kind": entry.kind })))
                    })
                })),
            ),
        )?;
        let bus = self.clone();
        let n = nick;
        registry.register(
            "peer_read",
            catalog_definition(
                "peer_read",
                Some(Arc::new(move |input: Value, _cancel| {
                    let bus = bus.clone();
                    let n = n.clone();
                    Box::pin(async move {
                        let include_read = input.get("include_read").and_then(Value::as_bool).unwrap_or(false);
                        let include_board = input.get("include_blackboard").and_then(Value::as_bool).unwrap_or(true);
                        let inbox: Vec<Value> = bus
                            .read_inbox(&n, include_read)
                            .into_iter()
                            .map(|m| {
                                let mut o = Map::new();
                                o.insert("id".into(), json!(m.id));
                                o.insert("from".into(), json!(m.from));
                                o.insert("to".into(), json!(m.to));
                                o.insert("kind".into(), json!(m.kind));
                                if let Some(t) = m.topic {
                                    o.insert("topic".into(), json!(t));
                                }
                                o.insert("content".into(), json!(m.content));
                                o.insert("createdAt".into(), json!(m.created_at));
                                Value::Object(o)
                            })
                            .collect();
                        let mut out = Map::new();
                        out.insert("self".into(), json!(n));
                        out.insert("inbox".into(), Value::Array(inbox));
                        if include_board {
                            let board = bus.blackboard();
                            let items: Vec<Value> = board
                                .iter()
                                .skip(board.len().saturating_sub(30))
                                .map(|b| {
                                    let mut o = Map::new();
                                    o.insert("id".into(), json!(b.id));
                                    o.insert("author".into(), json!(b.author));
                                    o.insert("role".into(), json!(b.role));
                                    if let Some(r) = b.round {
                                        o.insert("round".into(), json!(r));
                                    }
                                    o.insert("kind".into(), json!(b.kind));
                                    o.insert("topic".into(), json!(b.topic));
                                    o.insert("content".into(), json!(b.content));
                                    Value::Object(o)
                                })
                                .collect();
                            out.insert("blackboard".into(), Value::Array(items));
                        }
                        Ok(stringify(&Value::Object(out)))
                    })
                })),
            ),
        )?;
        Ok(())
    }
}

pub struct BrainstormOptions {
    pub topic: String,
    pub roles: Option<Vec<String>>,
    pub rounds: Option<u32>,
    pub explore_codebase: Option<bool>,
    pub cancel: CancellationToken,
    pub budget: Option<SharedBudget>,
    pub policy: Option<Arc<ExecutionPolicy>>,
    pub on_progress: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

#[derive(Clone, Debug)]
pub struct Contribution {
    pub round: u32,
    pub persona: NetworkPersona,
    pub response: String,
}

#[derive(Clone, Debug)]
pub struct BrainstormResult {
    pub topic: String,
    pub rounds: u32,
    pub participants: Vec<NetworkPersona>,
    pub contributions: Vec<Contribution>,
    pub synthesis: String,
}

pub fn resolve_personas(roles: Option<&[String]>) -> Vec<NetworkPersona> {
    let default: Vec<String> = ["architect", "researcher", "critic", "pragmatist"].iter().map(|s| s.to_string()).collect();
    let requested = roles.filter(|r| !r.is_empty()).map(|r| r.to_vec()).unwrap_or(default);
    let mut out = vec![];
    let mut seen = HashSet::new();
    for raw in requested {
        let key = raw.trim().to_lowercase();
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        match default_persona(&key) {
            Some(p) => out.push(p),
            None => {
                let t = raw.trim();
                let mut chars = t.chars();
                let display = match chars.next() {
                    Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                    None => String::new(),
                };
                out.push(NetworkPersona {
                    id: key,
                    name: display.clone(),
                    role: format!("{display} Specialist"),
                    focus: format!("Provide specialized {display} insights, constructive critique, and concrete recommendations."),
                });
            }
        }
    }
    if out.len() >= 2 {
        out
    } else {
        vec![default_persona("architect").unwrap(), default_persona("critic").unwrap()]
    }
}

pub const DEFAULT_NETWORK_TOKEN_BUDGET: u64 = 120_000;

/// Multi-agent brainstorm: personas across rounds of exploration, P2P debate and synthesis.
pub struct AgentNetwork {
    pub config: Arc<RuntimeConfig>,
    pub parent: Arc<SessionStore>,
    pub bus: Arc<AgentNetworkBus>,
}

struct PersonaTurn<'a> {
    persona: &'a NetworkPersona,
    all: &'a [NetworkPersona],
    topic: &'a str,
    round: u32,
    total_rounds: u32,
    prior: Vec<Contribution>,
    explore: bool,
}

impl AgentNetwork {
    pub fn new(config: Arc<RuntimeConfig>, parent: Arc<SessionStore>, bus: Arc<AgentNetworkBus>) -> Self {
        AgentNetwork { config, parent, bus }
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.config.data_dir.join("network").join(self.parent.id())
    }

    pub async fn brainstorm(&self, options: BrainstormOptions) -> Result<BrainstormResult> {
        let personas = resolve_personas(options.roles.as_deref());
        let rounds = options.rounds.unwrap_or(2).clamp(1, 4);
        let explore = options.explore_codebase != Some(false);
        let progress = |t: &str| {
            if let Some(p) = &options.on_progress {
                p(t);
            }
        };
        let parent_budget = options.budget.clone();
        let budget: SharedBudget = {
            let p = parent_budget.as_ref().map(|b| b.lock().unwrap());
            let configured = p.as_ref().map(|b| b.limit).filter(|l| *l > 0).unwrap_or(self.config.run_token_budget);
            Arc::new(Mutex::new(RunBudget {
                spent: p.as_ref().map(|b| b.spent).unwrap_or(0),
                limit: if configured > 0 { configured } else { DEFAULT_NETWORK_TOKEN_BUDGET },
                cost_spent: p.as_ref().map(|b| b.cost_spent).unwrap_or_else(|| self.parent.session.lock().unwrap().cost()),
                cost_limit: p.as_ref().map(|b| b.cost_limit).unwrap_or(self.config.max_cost_usd),
                started_at: Some(p.as_ref().and_then(|b| b.started_at).unwrap_or_else(now_millis)),
                time_limit_ms: p.as_ref().map(|b| b.time_limit_ms).unwrap_or(self.config.max_time_ms),
                ledger: p.as_ref().and_then(|b| b.ledger.clone()),
            }))
        };
        for p in &personas {
            self.bus.register_node(NetworkNodeInfo {
                id: format!("net-{}", p.id),
                nickname: p.name.clone(),
                role: p.role.clone(),
                status: "ready".into(),
                task: Some(take_chars(&options.topic, 120).to_string()),
            });
        }
        progress(&format!(
            "\n╭─── Agent Network Brainstorm · {} ({} round{}) ───╮\n  Topic: {}\n",
            personas.iter().map(|p| p.name.clone()).collect::<Vec<_>>().join(" · "),
            rounds,
            if rounds > 1 { "s" } else { "" },
            options.topic
        ));
        let result = async {
            let contributions: Arc<Mutex<Vec<Contribution>>> = Arc::new(Mutex::new(vec![]));
            for round in 1..=rounds {
                if options.cancel.is_cancelled() {
                    return Err(crate::harness::Aborted::default().into());
                }
                progress(&format!("\n◈ Round {round}/{rounds} — {}\n", if round == 1 { "Initial Exploration & Proposals" } else { "Cross-Critique & Peer Debate" }));
                let concurrency = if round == 1 { 2 } else { 1 };
                let next = Arc::new(Mutex::new(0usize));
                let worker = || async {
                    loop {
                        if options.cancel.is_cancelled() {
                            break;
                        }
                        let idx = {
                            let mut n = next.lock().unwrap();
                            if *n >= personas.len() {
                                break;
                            }
                            *n += 1;
                            *n - 1
                        };
                        let persona = &personas[idx];
                        self.bus.update_node_status(&persona.name, &format!("round-{round}"), None);
                        progress(&format!("  ▸ [{} · {}] thinking...\n", persona.name, persona.role));
                        let prior = contributions.lock().unwrap().clone();
                        let response = self
                            .run_persona_turn(
                                PersonaTurn { persona, all: &personas, topic: &options.topic, round, total_rounds: rounds, prior, explore },
                                &options.cancel,
                                &budget,
                                options.policy.clone(),
                            )
                            .await?;
                        self.sync_parent_budget(parent_budget.as_ref(), &budget);
                        contributions.lock().unwrap().push(Contribution { round, persona: persona.clone(), response: response.clone() });
                        self.bus.post_blackboard(
                            &persona.name,
                            &persona.role,
                            &format!("Round {round}: {}", take_chars(&options.topic, 60)),
                            &response,
                            Some(if round == 1 { "idea" } else { "critique" }),
                            Some(round),
                        );
                        self.bus.update_node_status(&persona.name, "idle", None);
                        let preview = take_chars(collapse(&response).trim(), 180).to_string();
                        progress(&format!("  ● [{}] {preview}{}\n", persona.name, if crate::jsutil::utf16_len(&response) > 180 { "…" } else { "" }));
                    }
                    Ok::<(), anyhow::Error>(())
                };
                let workers: Vec<_> = (0..concurrency.min(personas.len())).map(|_| worker()).collect();
                for r in futures_util::future::join_all(workers).await {
                    r?;
                }
            }
            if options.cancel.is_cancelled() {
                return Err(crate::harness::Aborted::default().into());
            }
            progress("\n◈ Synthesizing Agent Network Consensus...\n");
            let contributions = contributions.lock().unwrap().clone();
            let synthesis = self.run_synthesis(&options.topic, &personas, rounds, &contributions, &options.cancel, &budget, options.policy.clone()).await?;
            self.sync_parent_budget(parent_budget.as_ref(), &budget);
            self.bus.post_blackboard("Moderator", "Network Synthesizer", &format!("Consensus: {}", take_chars(&options.topic, 60)), &synthesis, Some("consensus"), None);
            progress("╰─── Agent Network Complete ──────────────────────────────────────╯\n");
            Ok(BrainstormResult { topic: options.topic.clone(), rounds, participants: personas.clone(), contributions, synthesis })
        }
        .await;
        let _ = std::fs::remove_dir(self.sessions_root());
        result
    }

    fn aggregate_run_cost(&self, store: &SessionStore, before: f64) {
        let delta = store.session.lock().unwrap().cost() - before;
        if delta <= 0.0 {
            return;
        }
        {
            let mut s = self.parent.session.lock().unwrap();
            s.cost_usd = Some(s.cost() + delta);
        }
        let _ = self.parent.save();
    }

    fn sync_parent_budget(&self, parent: Option<&SharedBudget>, budget: &SharedBudget) {
        let Some(parent) = parent else { return };
        let (spent, cost) = {
            let b = budget.lock().unwrap();
            (b.spent, b.cost_spent)
        };
        let mut p = parent.lock().unwrap();
        p.spent = spent;
        p.cost_spent = p.cost_spent.max(cost);
    }

    fn child_tools(&self, policy: Option<Arc<ExecutionPolicy>>, budget: &SharedBudget) -> Arc<ToolRegistry> {
        let tools = ToolRegistry::new(&self.config.cwd, deny_all(), true, None);
        tools.set_policy(policy);
        if let Some(ledger) = budget.lock().unwrap().ledger.clone() {
            tools.set_cost_ledger(Some(ledger));
        }
        tools
    }

    async fn run_child(&self, title: String, tools: Arc<ToolRegistry>, prompt: String, cancel: &CancellationToken, budget: &SharedBudget) -> Result<(Arc<SessionStore>, Result<String>)> {
        let store = Arc::new(SessionStore::open(&self.sessions_root(), SessionRoute::from_runtime(&self.config), None, None)?);
        store.session.lock().unwrap().title = Some(title);
        let before = store.session.lock().unwrap().cost();
        let outcome = match Harness::new(self.config.clone(), store.clone(), tools, noop_emit(), None, HarnessDeps { verify_enabled: Some(false), ..Default::default() }) {
            Ok(h) => h.run(&prompt, cancel, Some(budget.clone())).await,
            Err(e) => Err(e),
        };
        self.aggregate_run_cost(&store, before);
        Ok((store, outcome))
    }

    fn cleanup(&self, store: &SessionStore) {
        let id = store.id();
        let _ = store.close();
        let _ = std::fs::remove_file(self.sessions_root().join(format!("{id}.json")));
    }

    async fn run_persona_turn(&self, p: PersonaTurn<'_>, cancel: &CancellationToken, budget: &SharedBudget, policy: Option<Arc<ExecutionPolicy>>) -> Result<String> {
        let tools = self.child_tools(policy, budget);
        tools.update_flags(|f| f.allow_peer_tools_in_read_only = true);
        self.bus.register_peer_tools(&tools, &p.persona.name, &p.persona.role)?;
        if !p.explore {
            tools.update_flags(|f| f.only_peer_tools = true);
        }
        let peer_names = p.all.iter().filter(|x| x.id != p.persona.id).map(|x| format!("{} ({})", x.name, x.role)).collect::<Vec<_>>().join(", ");
        let inbox = self.bus.drain_unread_prompt_block(&p.persona.name);
        let prior_text = p.prior.iter().map(|c| format!("### [Round {}] {} ({}):\n{}", c.round, c.persona.name, c.persona.role, c.response)).collect::<Vec<_>>().join("\n\n");
        let prompt = if p.round == 1 {
            format!(
                "You are **{}** ({}) in a collaborative Agent Network brainstorming session.\nYour focus: {}\nOther agents in this network: {peer_names}.\n\nTopic / Challenge:\n{}\n\nInstructions for Round 1:\n1. {}\n2. You may use `peer_send` to ask a pointed question or challenge another agent ({}).\n3. Present a sharp, concrete proposal (key ideas, architecture/design choices, and questions for peers) in 150-300 words.",
                p.persona.name,
                p.persona.role,
                p.persona.focus,
                p.topic,
                if p.explore { "Use read-only tools (`list_files`, `search`, `read_file`) if needed to ground your ideas in the actual codebase." } else { "Analyze the topic from your specialist angle." },
                p.all.iter().map(|x| x.name.clone()).collect::<Vec<_>>().join(", ")
            )
        } else {
            format!(
                "You are **{}** ({}) in Round {} of {} of the Agent Network deliberation.\nYour focus: {}\n\nTopic:\n{}\n\nPrevious Round Proposals from the Network:\n{prior_text}\n{inbox}\nInstructions for Round {}:\n1. Directly respond to your peers' proposals and any direct messages in your inbox.\n2. Point out specific strengths, blind spots, or trade-offs in other agents' ideas (reference them by name, e.g. @Architect, @Critic).\n3. Refine your recommendation so the group converges toward the strongest practical solution (150-300 words).",
                p.persona.name, p.persona.role, p.round, p.total_rounds, p.persona.focus, p.topic, p.round
            )
        };
        let title = format!("Network · {} (R{})", p.persona.name, p.round);
        let (store, outcome) = match self.run_child(title, tools, prompt, cancel, budget).await {
            Ok(v) => v,
            Err(err) => {
                if cancel.is_cancelled() {
                    return Err(err);
                }
                return Ok(format!("[{} error: {}]", p.persona.name, safe_error(&err.to_string(), Some(&self.config.api_key))));
            }
        };
        let result = match outcome {
            Ok(_) => {
                let answer = last_assistant_answer(&store.session.lock().unwrap().messages, &self.config.inline_reasoning);
                Ok(limit_text(if answer.is_empty() { "(no response)" } else { &answer }, 6_000))
            }
            Err(err) => {
                if cancel.is_cancelled() || is_aborted(&err) {
                    Err(err)
                } else {
                    Ok(format!("[{} error: {}]", p.persona.name, safe_error(&err.to_string(), Some(&self.config.api_key))))
                }
            }
        };
        self.cleanup(&store);
        result
    }

    async fn run_synthesis(
        &self,
        topic: &str,
        personas: &[NetworkPersona],
        rounds: u32,
        contributions: &[Contribution],
        cancel: &CancellationToken,
        budget: &SharedBudget,
        policy: Option<Arc<ExecutionPolicy>>,
    ) -> Result<String> {
        let tools = self.child_tools(policy, budget);
        tools.update_flags(|f| f.only_peer_tools = true);
        let transcript = contributions.iter().map(|c| format!("### [Round {}] {} ({})\n{}", c.round, c.persona.name, c.persona.role, c.response)).collect::<Vec<_>>().join("\n\n");
        let messages = self.bus.messages();
        let peer = messages.iter().skip(messages.len().saturating_sub(20)).map(|m| format!("- {} → {} [{}]: {}", m.from, m.to, m.kind, m.content)).collect::<Vec<_>>().join("\n");
        let prompt = format!(
            "You are the **Lead Moderator** synthesizing a {rounds}-round Agent Network deliberation among {}.\n\nTopic:\n{topic}\n\nFull Agent Network Deliberation:\n{transcript}\n\n{}Produce a clear, high-signal synthesis structured with:\n1. **Consensus Solution & Core Idea** (what the agents agreed is the best approach and why)\n2. **Key Debates & Trade-offs Resolved** (how critiques from Critic/Pragmatist/Architect shaped the design)\n3. **Actionable Implementation Plan** (concrete files, components, and verification steps)\nRespond in the same language as the user's Topic.",
            personas.iter().map(|p| format!("{} ({})", p.name, p.role)).collect::<Vec<_>>().join(", "),
            if peer.is_empty() { String::new() } else { format!("Direct Peer-to-Peer Messages Exchanged:\n{peer}\n\n") }
        );
        let (store, outcome) = self.run_child("Network · Moderator Synthesis".into(), tools, prompt, cancel, budget).await?;
        let result = outcome.map(|_| {
            let answer = last_assistant_answer(&store.session.lock().unwrap().messages, &self.config.inline_reasoning);
            limit_text(if answer.is_empty() { "(no synthesis generated)" } else { &answer }, 12_000)
        });
        self.cleanup(&store);
        result
    }
}

pub fn format_brainstorm_report(res: &BrainstormResult) -> String {
    let mut lines = vec![
        format!("# Agent Network Report: {}", res.topic),
        format!("Participants: {} · Rounds: {}", res.participants.iter().map(|p| format!("**{}** ({})", p.name, p.role)).collect::<Vec<_>>().join(", "), res.rounds),
        String::new(),
    ];
    for c in &res.contributions {
        lines.push(format!("## [Round {}] {} ({})", c.round, c.persona.name, c.persona.role));
        lines.push(c.response.trim().to_string());
        lines.push(String::new());
    }
    lines.push("## Consensus & Action Plan (Moderator Synthesis)".into());
    lines.push(res.synthesis.trim().to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_inbox_and_personas() {
        let bus = AgentNetworkBus::new();
        bus.register_node(NetworkNodeInfo { id: "abc123".into(), nickname: "Euclid".into(), role: "worker".into(), status: "running".into(), task: None });
        bus.send("Lead", "euclid", " hi ", None, Some("t"));
        bus.send("Lead", "abc", "by id", Some("idea"), None);
        bus.send("Euclid", "*", "self", None, None);
        assert_eq!(bus.read_inbox("Euclid", false).len(), 2);
        assert!(bus.read_inbox("Euclid", false).is_empty());
        assert!(bus.drain_unread_prompt_block("Lead").contains("[MESSAGE] from Euclid: self"));
        let p = resolve_personas(Some(&["critic".into(), "data".into()]));
        assert_eq!(p[1].name, "Data");
        assert_eq!(resolve_personas(Some(&["x".into()])).len(), 2);
    }
}
