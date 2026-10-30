//! Multi-agent DAG coordination: plan validation, scheduling, per-todo rollback, merge and verify;
//! plus the verify-driven scope loop and the multi-candidate runner.
//!
//! The module never calls a model or a shell by itself: model access, spawning and command
//! execution are hooks owned by the caller.

use crate::config::{VerifyConfig, VerifyPolicy};
use crate::cost::{format_duration_ms, format_usd, CostLedger, RunBudget, SharedBudget};
use crate::errors::safe_error;
use crate::files::{compute_unified_diff, FileBackup, RestoreResult, WorkspaceFiles};
use crate::jsutil::{now_iso, now_millis, take_chars};
use crate::verify::{format_verify_errors, plan_verification, redact_text, redact_verdict, run_verification, CommandRunner, Severity, VerifyPlan, VerifyRunner, VerifyVerdict};
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tokio_util::sync::CancellationToken;

pub const AGENT_ROLES: [&str; 5] = ["planner", "coder", "reviewer", "tester", "researcher"];

pub fn is_write_role(role: &str) -> bool {
    role == "coder"
}

pub fn role_toolset(role: &str, allowed_mcp: &[String]) -> Vec<String> {
    let base: &[&str] = match role {
        "coder" => &["read_file", "list_files", "search", "write_file", "edit_file", "apply_patch", "bash"],
        "tester" => &["read_file", "list_files", "search", "bash"],
        _ => &["read_file", "list_files", "search"],
    };
    base.iter().map(|s| s.to_string()).chain(allowed_mcp.iter().cloned()).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Todo {
    pub id: String,
    pub title: String,
    pub acceptance: String,
    pub depends_on: Vec<String>,
    pub status: String,
    pub role: String,
    pub assignee: Option<String>,
    pub artifacts: Vec<String>,
    pub rework_cycles: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub id: String,
    pub goal: String,
    pub todos: Vec<Todo>,
    pub created_at: String,
}

fn planner_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal": { "type": "string", "minLength": 1, "maxLength": 8000 },
            "todos": { "type": "array", "minItems": 1, "maxItems": 16, "items": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "minLength": 1, "maxLength": 64 },
                    "title": { "type": "string", "minLength": 1, "maxLength": 400 },
                    "acceptance": { "type": "string", "maxLength": 8000, "default": "" },
                    "dependsOn": { "type": "array", "maxItems": 16, "default": [], "items": { "type": "string", "minLength": 1, "maxLength": 64 } },
                    "role": { "type": "string", "enum": AGENT_ROLES },
                    "assignee": { "type": "string", "maxLength": 80 },
                    "artifacts": { "type": "array", "maxItems": 32, "default": [], "items": { "type": "string", "minLength": 1 } }
                },
                "required": ["id", "title", "role"]
            } }
        },
        "required": ["goal", "todos"]
    })
}

pub fn plan_from_planner(raw: &Value, id: &str) -> Result<Plan> {
    let parsed =
        crate::schema::zod_parse(&planner_schema(), raw, &["goal", "todos.*.id", "todos.*.title", "todos.*.dependsOn.*", "todos.*.assignee", "todos.*.artifacts.*"]).map_err(|e| anyhow!(e))?;
    let strs = |v: &Value| v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()).unwrap_or_default();
    let todos = parsed["todos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let mut deps = vec![];
            for d in strs(&t["dependsOn"]) {
                if !deps.contains(&d) {
                    deps.push(d);
                }
            }
            Todo {
                id: t["id"].as_str().unwrap().into(),
                title: t["title"].as_str().unwrap().into(),
                acceptance: t["acceptance"].as_str().unwrap_or("").into(),
                depends_on: deps,
                status: "pending".into(),
                role: t["role"].as_str().unwrap().into(),
                assignee: t.get("assignee").and_then(Value::as_str).map(str::to_string),
                artifacts: strs(&t["artifacts"]),
                rework_cycles: 0,
            }
        })
        .collect();
    Ok(Plan { id: id.into(), goal: parsed["goal"].as_str().unwrap().into(), todos, created_at: now_iso() })
}

#[derive(Debug)]
pub struct PlanValidationError(pub String);
impl std::fmt::Display for PlanValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PlanValidationError {}

#[derive(Debug)]
pub struct ErrNotVerified(pub String);
impl std::fmt::Display for ErrNotVerified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Plan result not verified: {}", self.0)
    }
}
impl std::error::Error for ErrNotVerified {}

/// Unique ids, existing deps, no self-deps, valid roles and acyclic (Kahn).
pub fn validate_plan(plan: &Plan) -> Result<(), PlanValidationError> {
    let fail = |m: String| Err(PlanValidationError(m));
    if plan.todos.is_empty() {
        return fail("Plan has no todos.".into());
    }
    let mut seen = HashSet::new();
    for t in &plan.todos {
        if t.id.trim().is_empty() {
            return fail("Todo id must not be empty.".into());
        }
        if !seen.insert(t.id.clone()) {
            return fail(format!("Duplicate todo id: {}.", t.id));
        }
    }
    for t in &plan.todos {
        if !AGENT_ROLES.contains(&t.role.as_str()) {
            return fail(format!("Todo {} has invalid role: {}.", t.id, t.role));
        }
        for d in &t.depends_on {
            if d == &t.id {
                return fail(format!("Todo {} depends on itself.", t.id));
            }
            if !seen.contains(d) {
                return fail(format!("Todo {} depends on unknown todo: {d}.", t.id));
            }
        }
    }
    let mut indegree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for t in &plan.todos {
        indegree.insert(&t.id, t.depends_on.len());
        for d in &t.depends_on {
            dependents.entry(d.as_str()).or_default().push(&t.id);
        }
    }
    let mut queue: Vec<&str> = plan.todos.iter().filter(|t| indegree[t.id.as_str()] == 0).map(|t| t.id.as_str()).collect();
    let mut sorted = HashSet::new();
    let mut i = 0;
    while i < queue.len() {
        let id = queue[i];
        sorted.insert(id);
        for next in dependents.get(id).cloned().unwrap_or_default() {
            let left = indegree[next] - 1;
            indegree.insert(next, left);
            if left == 0 {
                queue.push(next);
            }
        }
        i += 1;
    }
    if sorted.len() != plan.todos.len() {
        let cyclic: Vec<&str> = plan.todos.iter().filter(|t| !sorted.contains(t.id.as_str())).map(|t| t.id.as_str()).collect();
        return fail(format!("Plan contains a dependency cycle involving: {}.", cyclic.join(", ")));
    }
    Ok(())
}

/// DAG dispatcher over a validated plan.
pub struct Scheduler {
    pub plan: Plan,
    inflight: HashSet<String>,
    write_concurrency: usize,
    max_concurrency: usize,
}

impl Scheduler {
    pub fn new(plan: Plan, write_concurrency: usize, max_concurrency: usize) -> Self {
        Scheduler { plan, inflight: HashSet::new(), write_concurrency, max_concurrency }
    }

    pub fn todo(&self, id: &str) -> Option<&Todo> {
        self.plan.todos.iter().find(|t| t.id == id)
    }

    fn todo_mut(&mut self, id: &str) -> Option<&mut Todo> {
        self.plan.todos.iter_mut().find(|t| t.id == id)
    }

    pub fn status_of(&self, id: &str) -> String {
        self.todo(id).map(|t| t.status.clone()).unwrap_or_else(|| "pending".into())
    }

    pub fn deps_met(&self, todo: &Todo) -> bool {
        todo.depends_on.iter().all(|d| self.status_of(d) == "done")
    }

    pub fn blocked_by(&self, todo: &Todo) -> Option<&Todo> {
        todo.depends_on.iter().filter_map(|d| self.todo(d)).find(|d| d.status == "failed" || d.status == "blocked")
    }

    pub fn ready_todos(&self) -> Vec<Todo> {
        self.plan.todos.iter().filter(|t| t.status == "pending" && self.deps_met(t)).cloned().collect()
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight.len()
    }

    pub fn dispatch_ready(&mut self) -> Vec<Todo> {
        let mut writers = self.inflight.iter().filter(|id| self.todo(id).map(|t| is_write_role(&t.role)).unwrap_or(false)).count();
        let mut readers = self.inflight.len() - writers;
        let mut out = vec![];
        let ids: Vec<String> = self.plan.todos.iter().map(|t| t.id.clone()).collect();
        for id in ids {
            let todo = self.todo(&id).unwrap().clone();
            if todo.status != "pending" || self.inflight.contains(&id) || !self.deps_met(&todo) {
                continue;
            }
            let write = is_write_role(&todo.role);
            if write && writers >= self.write_concurrency.max(1) {
                continue;
            }
            if !write && readers >= self.max_concurrency {
                continue;
            }
            self.todo_mut(&id).unwrap().status = "in_progress".into();
            self.inflight.insert(id.clone());
            out.push(self.todo(&id).unwrap().clone());
            if write {
                writers += 1;
            } else {
                readers += 1;
            }
        }
        out
    }

    pub fn requeue(&mut self, id: &str) {
        if let Some(t) = self.todo_mut(id) {
            t.status = "pending".into();
        }
        self.inflight.remove(id);
    }

    pub fn mark_done(&mut self, id: &str) {
        if let Some(t) = self.todo_mut(id) {
            t.status = "done".into();
        }
        self.inflight.remove(id);
    }

    pub fn mark_failed(&mut self, id: &str) -> Vec<String> {
        if self.todo(id).is_none() {
            return vec![];
        }
        self.todo_mut(id).unwrap().status = "failed".into();
        self.inflight.remove(id);
        self.propagate_blocks()
    }

    pub fn propagate_blocks(&mut self) -> Vec<String> {
        let mut blocked = vec![];
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..self.plan.todos.len() {
                if self.plan.todos[i].status != "pending" {
                    continue;
                }
                let t = self.plan.todos[i].clone();
                if self.blocked_by(&t).is_some() {
                    self.plan.todos[i].status = "blocked".into();
                    blocked.push(t.id);
                    changed = true;
                }
            }
        }
        blocked
    }

    pub fn check_deadlock(&self) -> Vec<String> {
        if !self.inflight.is_empty() || !self.ready_todos().is_empty() {
            return vec![];
        }
        self.plan.todos.iter().filter(|t| t.status == "pending").map(|t| t.id.clone()).collect()
    }
}

pub const DEFAULT_MAX_REWORK_CYCLES: u32 = 3;
pub const DEFAULT_DAG_MAX_CONCURRENCY: usize = 2;
pub const DEFAULT_DAG_WRITE_CONCURRENCY: usize = 1;
pub const DEFAULT_DAG_TOKEN_BUDGET: u64 = 200_000;

/// Returns `approved`, `rework` or `failed` (past the rework cap).
pub fn apply_review_verdict(todo: &mut Todo, approved: bool, cap: u32) -> &'static str {
    if approved {
        return "approved";
    }
    todo.rework_cycles += 1;
    if todo.rework_cycles > cap {
        todo.status = "failed".into();
        return "failed";
    }
    todo.status = "pending".into();
    "rework"
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergeConflict {
    pub path: String,
    pub todos: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct MergeResult {
    pub verified: bool,
    pub combined_diff: String,
    pub conflicts: Vec<MergeConflict>,
    pub artifacts: Vec<String>,
    pub reason: String,
}

pub fn diff_paths(diff: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?m)^--- a/(.+?) \((?:new |deleted )?\+\d+ -\d+\)$").unwrap());
    let mut out: Vec<String> = vec![];
    for c in re.captures_iter(diff) {
        let p = c[1].trim().replace('\\', "/");
        if !p.is_empty() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

pub type VerifyMergeFn<'a> = Option<Box<dyn FnOnce(String, Vec<String>) -> BoxFuture<'a, Result<bool>> + Send + 'a>>;

/// Validate/aggregate (never apply): conflicts when two done todos changed the same file.
pub async fn merge(plan: &Plan, diffs: &HashMap<String, String>, allow_empty: bool, verify: VerifyMergeFn<'_>) -> Result<MergeResult> {
    let done: Vec<&Todo> = plan.todos.iter().filter(|t| t.status == "done").collect();
    let mut parts = vec![];
    let mut files_by: HashMap<String, Vec<String>> = HashMap::new();
    let mut artifacts: Vec<String> = vec![];
    for t in &done {
        let diff = diffs.get(&t.id).cloned().unwrap_or_default();
        if diff.trim().is_empty() {
            continue;
        }
        let paths = diff_paths(&diff);
        parts.push(diff);
        for p in &paths {
            if !artifacts.contains(p) {
                artifacts.push(p.clone());
            }
        }
        files_by.insert(t.id.clone(), paths);
    }
    let combined = parts.join("\n");
    let mut conflicts = vec![];
    for i in 0..done.len() {
        for j in i + 1..done.len() {
            let a = files_by.get(&done[i].id).cloned().unwrap_or_default();
            let b: HashSet<String> = files_by.get(&done[j].id).cloned().unwrap_or_default().into_iter().collect();
            for p in a {
                if b.contains(&p) {
                    conflicts.push(MergeConflict { path: p, todos: vec![done[i].id.clone(), done[j].id.clone()] });
                }
            }
        }
    }
    let result = |verified: bool, reason: String| MergeResult { verified, combined_diff: combined.clone(), conflicts: conflicts.clone(), artifacts: artifacts.clone(), reason };
    if !conflicts.is_empty() {
        return Ok(result(false, format!("merge conflict: {}", conflicts[0].path)));
    }
    if combined.trim().is_empty() && artifacts.is_empty() {
        return Ok(if allow_empty { result(true, "empty (allowed)".into()) } else { result(false, "no changes".into()) });
    }
    if let Some(v) = verify {
        if !v(combined.clone(), artifacts.clone()).await? {
            return Ok(result(false, "verification failed".into()));
        }
    }
    Ok(result(true, "ok".into()))
}

#[derive(Clone)]
pub struct DagSpawnRequest {
    pub todo: Todo,
    pub read_only: bool,
    pub allowed_tools: Vec<String>,
    pub budget: SharedBudget,
    pub cancel: CancellationToken,
}

#[derive(Clone)]
pub struct DagHooks {
    pub planner: Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, Result<Value>> + Send + Sync>,
    pub spawn: Arc<dyn Fn(DagSpawnRequest) -> BoxFuture<'static, Result<String>> + Send + Sync>,
    pub review: Option<Arc<dyn Fn(Todo, String, CancellationToken) -> BoxFuture<'static, Result<bool>> + Send + Sync>>,
    pub test: Option<Arc<dyn Fn(Todo, Vec<String>, CancellationToken) -> BoxFuture<'static, Result<bool>> + Send + Sync>>,
    pub verify: Option<Arc<dyn Fn(Vec<String>, String, CancellationToken) -> BoxFuture<'static, Result<bool>> + Send + Sync>>,
}

#[derive(Clone, Default)]
pub struct DagConfig {
    pub run_token_budget: u64,
    pub max_cost_usd: f64,
    pub max_time_ms: u64,
    pub rework_cap: Option<u32>,
    pub max_concurrency: Option<usize>,
}

pub struct DagOptions {
    pub config: DagConfig,
    pub cancel: CancellationToken,
    pub files: Arc<WorkspaceFiles>,
    pub budget: Option<SharedBudget>,
    pub allowed_mcp_tools: Vec<String>,
    pub allow_empty: bool,
    pub write_concurrency: Option<usize>,
    pub emit: Option<Arc<dyn Fn(Value) + Send + Sync>>,
}

pub struct DagRunResult {
    pub plan: Plan,
    pub merge: MergeResult,
    pub reports: HashMap<String, String>,
    pub summary: String,
}

pub fn diff_from_backups(files: &WorkspaceFiles, backups: &[FileBackup]) -> String {
    let mut parts = vec![];
    for b in backups {
        let after = files.path(&b.path).and_then(|p| files.text(&p)).unwrap_or_default();
        let before = if b.existed { b.content.clone().unwrap_or_default() } else { String::new() };
        let diff = compute_unified_diff(&b.path, &before, &after, 3);
        if !diff.starts_with("No changes in ") {
            parts.push(diff);
        }
    }
    parts.join("\n")
}

struct DagState {
    scheduler: Scheduler,
    reports: HashMap<String, String>,
    diffs: HashMap<String, String>,
    failures: HashMap<String, String>,
    todo_backups: Vec<(String, Vec<FileBackup>)>,
    rolled_back: bool,
}

fn todo_event(id: &str, status: &str, role: &str, attempt: Option<u32>, reason: Option<String>) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!("dag_todo_update"));
    m.insert("todoId".into(), json!(id));
    m.insert("status".into(), json!(status));
    m.insert("role".into(), json!(role));
    if let Some(a) = attempt {
        m.insert("attempt".into(), json!(a));
    }
    if let Some(r) = reason {
        m.insert("reason".into(), json!(r));
    }
    Value::Object(m)
}

/// Planner → scheduler (writes serial) → spawn → review/test with rollback + rework cap → merge →
/// verify. Any failure rolls back every per-todo snapshot (reverse order) before returning.
pub async fn run_dag(goal: &str, hooks: DagHooks, opts: DagOptions) -> Result<DagRunResult> {
    let emit = |e: Value| {
        if let Some(f) = &opts.emit {
            f(e);
        }
    };
    let internal = opts.cancel.child_token();
    let state: Arc<Mutex<Option<DagState>>> = Arc::new(Mutex::new(None));
    let rollback_all = |state: &Arc<Mutex<Option<DagState>>>| -> Vec<(String, RestoreResult)> {
        let entries = {
            let mut guard = state.lock().unwrap();
            let Some(s) = guard.as_mut() else { return vec![] };
            if s.rolled_back {
                return vec![];
            }
            s.rolled_back = true;
            s.todo_backups.clone()
        };
        let mut out = vec![];
        for (id, backups) in entries.into_iter().rev() {
            if backups.is_empty() {
                continue;
            }
            if let Ok(res) = opts.files.restore_backups(&backups, false) {
                out.push((id, res));
            }
        }
        out
    };
    let result: Result<DagRunResult> = async {
        let cap = opts.config.rework_cap.unwrap_or(DEFAULT_MAX_REWORK_CYCLES);
        let max_concurrency = opts.config.max_concurrency.unwrap_or(DEFAULT_DAG_MAX_CONCURRENCY);
        let write_concurrency = opts.write_concurrency.unwrap_or(DEFAULT_DAG_WRITE_CONCURRENCY);
        let budget = match &opts.budget {
            Some(b) => b.clone(),
            None => {
                let limit = if opts.config.run_token_budget > 0 { opts.config.run_token_budget } else { DEFAULT_DAG_TOKEN_BUDGET };
                Arc::new(Mutex::new(RunBudget {
                    spent: 0,
                    limit,
                    cost_spent: 0.0,
                    cost_limit: opts.config.max_cost_usd,
                    started_at: Some(now_millis()),
                    time_limit_ms: opts.config.max_time_ms,
                    ledger: None,
                }))
            }
        };
        {
            let mut b = budget.lock().unwrap();
            if b.cost_limit == 0.0 {
                b.cost_limit = opts.config.max_cost_usd;
            }
            if b.time_limit_ms == 0 {
                b.time_limit_ms = opts.config.max_time_ms;
            }
            if b.started_at.is_none() {
                b.started_at = Some(now_millis());
            }
        }
        let plan = match (hooks.planner)(goal.to_string(), internal.clone()).await.and_then(|raw| plan_from_planner(&raw, &format!("dag-{}", radix36(now_millis() as u64)))) {
            Ok(p) => p,
            Err(error) => {
                emit(json!({ "type": "dag_plan_invalid", "message": error.to_string() }));
                return Err(error);
            }
        };
        if let Err(error) = validate_plan(&plan) {
            emit(json!({ "type": "dag_plan_invalid", "message": error.0 }));
            return Err(error.into());
        }
        emit(json!({ "type": "dag_plan_ready", "planId": plan.id, "todos": plan.todos.len() }));
        *state.lock().unwrap() = Some(DagState {
            scheduler: Scheduler::new(plan, write_concurrency, max_concurrency),
            reports: HashMap::new(),
            diffs: HashMap::new(),
            failures: HashMap::new(),
            todo_backups: vec![],
            rolled_back: false,
        });
        let with = |f: &mut dyn FnMut(&mut DagState)| {
            let mut g = state.lock().unwrap();
            f(g.as_mut().unwrap());
        };
        let fail_todo = |todo: &Todo, backups: &[FileBackup], reason: Option<String>| {
            if let Some(r) = &reason {
                with(&mut |s| {
                    s.failures.insert(todo.id.clone(), r.clone());
                    s.reports.insert(todo.id.clone(), format!("Task failed: {r}"));
                });
            }
            if !backups.is_empty() {
                let _ = opts.files.restore_backups(backups, false);
                with(&mut |s| {
                    if let Some(i) = s.todo_backups.iter().position(|(id, b)| id == &todo.id && b.as_slice() == backups) {
                        s.todo_backups.remove(i);
                    }
                });
            }
            let mut blocked = vec![];
            with(&mut |s| {
                for id in s.scheduler.mark_failed(&todo.id) {
                    let role = s.scheduler.todo(&id).map(|t| t.role.clone()).unwrap_or_default();
                    blocked.push((id, role));
                }
            });
            for (id, role) in blocked {
                emit(todo_event(&id, "blocked", &role, None, None));
            }
            emit(todo_event(&todo.id, "failed", &todo.role, None, reason.map(|r| take_chars(&r, 200).to_string())));
        };
        let run_todo = |todo: Todo| {
            let hooks = hooks.clone();
            let budget = budget.clone();
            let internal = internal.clone();
            let files = opts.files.clone();
            let allowed_mcp = opts.allowed_mcp_tools.clone();
            async move {
                emit(todo_event(&todo.id, "in_progress", &todo.role, Some(todo.rework_cycles + 1), None));
                let write = is_write_role(&todo.role);
                if write {
                    files.begin_turn();
                }
                let mut backups: Vec<FileBackup> = vec![];
                let attempt: Result<()> = async {
                    let report = (hooks.spawn)(DagSpawnRequest {
                        todo: todo.clone(),
                        read_only: !write,
                        allowed_tools: role_toolset(&todo.role, &allowed_mcp),
                        budget: budget.clone(),
                        cancel: internal.clone(),
                    })
                    .await?;
                    backups = if write { files.take_turn_backups() } else { vec![] };
                    let diff = diff_from_backups(&files, &backups);
                    with(&mut |s| {
                        if !backups.is_empty() {
                            s.todo_backups.push((todo.id.clone(), backups.clone()));
                        }
                        s.diffs.insert(todo.id.clone(), diff.clone());
                        s.reports.insert(todo.id.clone(), report.clone());
                    });
                    if internal.is_cancelled() {
                        return Err(crate::harness::Aborted::default().into());
                    }
                    let mut rework = false;
                    if let Some(review) = &hooks.review {
                        if !review(todo.clone(), report.clone(), internal.clone()).await? {
                            rework = true;
                        }
                    }
                    if !rework && !backups.is_empty() {
                        if let Some(test) = &hooks.test {
                            if !test(todo.clone(), backups.iter().map(|b| b.path.clone()).collect(), internal.clone()).await? {
                                rework = true;
                            }
                        }
                    }
                    if rework {
                        let mut outcome = "";
                        with(&mut |s| {
                            let t = s.scheduler.plan.todos.iter_mut().find(|t| t.id == todo.id).unwrap();
                            outcome = apply_review_verdict(t, false, cap);
                        });
                        if outcome == "failed" {
                            fail_todo(&todo, &backups, Some(format!("rework cap ({cap}) exceeded")));
                            return Ok(());
                        }
                        let res = files.restore_backups(&backups, false)?;
                        if !res.conflicts.is_empty() {
                            fail_todo(&todo, &backups, Some("rollback conflicted with newer user edits".into()));
                            return Ok(());
                        }
                        with(&mut |s| {
                            s.diffs.remove(&todo.id);
                            s.reports.remove(&todo.id);
                            s.scheduler.requeue(&todo.id);
                        });
                        return Ok(());
                    }
                    let mut attempt_no = 1;
                    with(&mut |s| {
                        let t = s.scheduler.plan.todos.iter_mut().find(|t| t.id == todo.id).unwrap();
                        for b in &backups {
                            if !t.artifacts.contains(&b.path) {
                                t.artifacts.push(b.path.clone());
                            }
                        }
                        attempt_no = t.rework_cycles + 1;
                        s.scheduler.mark_done(&todo.id);
                    });
                    emit(todo_event(&todo.id, "done", &todo.role, Some(attempt_no), None));
                    Ok(())
                }
                .await;
                if let Err(error) = attempt {
                    fail_todo(&todo, &backups, Some(safe_error(&error.to_string(), None)));
                }
            }
        };
        loop {
            if internal.is_cancelled() {
                return Err(crate::harness::Aborted::default().into());
            }
            let mut newly_blocked = vec![];
            let mut dispatched = vec![];
            with(&mut |s| {
                for id in s.scheduler.propagate_blocks() {
                    newly_blocked.push((id.clone(), s.scheduler.todo(&id).unwrap().role.clone()));
                }
                dispatched = s.scheduler.dispatch_ready();
            });
            for (id, role) in newly_blocked {
                emit(todo_event(&id, "blocked", &role, None, None));
            }
            if dispatched.is_empty() {
                let mut deadlock = vec![];
                with(&mut |s| {
                    deadlock = s.scheduler.check_deadlock();
                    for id in &deadlock {
                        let t = s.scheduler.plan.todos.iter_mut().find(|t| &t.id == id).unwrap();
                        t.status = "blocked".into();
                    }
                });
                if !deadlock.is_empty() {
                    for id in &deadlock {
                        let role = state.lock().unwrap().as_ref().unwrap().scheduler.todo(id).unwrap().role.clone();
                        emit(todo_event(id, "blocked", &role, None, None));
                    }
                    bail!("Plan deadlocked: no dispatchable todo but {} still pending ({}).", deadlock.len(), deadlock.join(", "));
                }
                break;
            }
            futures_util::future::join_all(dispatched.into_iter().map(&run_todo)).await;
        }
        let (plan, diffs, failures) = {
            let g = state.lock().unwrap();
            let s = g.as_ref().unwrap();
            (s.scheduler.plan.clone(), s.diffs.clone(), s.failures.clone())
        };
        let unfinished: Vec<&Todo> = plan.todos.iter().filter(|t| t.status != "done").collect();
        if !unfinished.is_empty() {
            rollback_all(&state);
            let detail = unfinished.iter().map(|t| format!("{}({}{})", t.id, t.status, failures.get(&t.id).map(|f| format!(": {f}")).unwrap_or_default())).collect::<Vec<_>>().join(", ");
            bail!("Plan failed: {detail}. All todo changes were rolled back.");
        }
        let verify_hook = hooks.verify.clone();
        let cancel = internal.clone();
        let verify: VerifyMergeFn = verify_hook
            .map(|v| Box::new(move |combined: String, artifacts: Vec<String>| v(artifacts, combined, cancel)) as Box<dyn FnOnce(String, Vec<String>) -> BoxFuture<'static, Result<bool>> + Send>);
        let merged = merge(&plan, &diffs, opts.allow_empty, verify).await?;
        let mut ev = Map::new();
        ev.insert("type".into(), json!("dag_merge"));
        ev.insert("verified".into(), json!(merged.verified));
        ev.insert("conflicts".into(), json!(merged.conflicts.len()));
        if !merged.reason.is_empty() {
            ev.insert("reason".into(), json!(merged.reason));
        }
        emit(Value::Object(ev));
        if !merged.verified {
            rollback_all(&state);
            return Err(ErrNotVerified(merged.reason.clone()).into());
        }
        let done: Vec<&Todo> = plan.todos.iter().filter(|t| t.status == "done").collect();
        let mut lines = vec![format!("DAG {}: {}/{} todo(s) done.", plan.id, done.len(), plan.todos.len())];
        for t in &done {
            lines.push(format!("  [{}] {}: {} — artifacts: {}", t.role, t.id, t.title, if t.artifacts.is_empty() { "(none)".to_string() } else { t.artifacts.join(", ") }));
        }
        lines.push(format!("Merge: {}{}", merged.reason, if merged.artifacts.is_empty() { String::new() } else { format!(" ({} file(s))", merged.artifacts.len()) }));
        let reports = state.lock().unwrap().as_ref().unwrap().reports.clone();
        Ok(DagRunResult { plan, merge: merged, reports, summary: lines.join("\n") })
    }
    .await;
    if result.is_err() {
        internal.cancel();
        rollback_all(&state);
    }
    result
}

fn radix36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = vec![];
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

// ---------------------------------------------------------------------------------------------
// Scope loop
// ---------------------------------------------------------------------------------------------

pub const SCOPE_ROUND_CAP: u64 = 4;

pub fn scope_round_cap(max_repair_rounds: u64) -> u64 {
    (max_repair_rounds + 1).clamp(1, SCOPE_ROUND_CAP)
}

/// Token limit first, then the ledger's cost/time caps.
pub fn budget_stop_reason(budget: Option<&SharedBudget>) -> Option<String> {
    let b = budget?.lock().unwrap();
    if b.limit > 0 && b.spent >= b.limit {
        return Some(format!("Run token budget reached ({} of {} tokens).", b.spent, b.limit));
    }
    b.ledger.as_ref()?;
    let cap = CostLedger::hard_cap_exceeded(&b);
    cap.exceeded.then_some(cap.reason)
}

pub struct ScopeLoopAttempt {
    pub round: u64,
    pub goal: String,
    pub verify_output: String,
    pub related_files: Vec<String>,
    pub commands: Vec<String>,
}

pub struct ScopeLoopOptions {
    pub cwd: PathBuf,
    pub verify_config: VerifyConfig,
    pub verify_policy: Option<VerifyPolicy>,
    pub runner: Option<Arc<dyn VerifyRunner>>,
    pub files: Arc<WorkspaceFiles>,
    pub cancel: CancellationToken,
    pub budget: Option<SharedBudget>,
    pub approve: Option<Arc<dyn Fn(&VerifyPlan) -> BoxFuture<'static, bool> + Send + Sync>>,
    pub secrets: Vec<String>,
    pub emit: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    pub max_rounds: Option<u64>,
}

pub struct ScopeLoopResult {
    pub ok: bool,
    pub rounds: u64,
    pub reason: String,
    pub verdict: Option<VerifyVerdict>,
    pub reports: Vec<String>,
    pub changed_files: Vec<String>,
    pub rollback: Option<RestoreResult>,
}

fn unique_paths(backups: &[FileBackup]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for b in backups {
        if !out.contains(&b.path) {
            out.push(b.path.clone());
        }
    }
    out
}

pub async fn run_scope_loop(goal: &str, attempt: impl Fn(ScopeLoopAttempt, CancellationToken) -> BoxFuture<'static, Result<String>>, opts: ScopeLoopOptions) -> Result<ScopeLoopResult> {
    let emit = |round: u64, action: &str, reason: &str| {
        if let Some(e) = &opts.emit {
            e(json!({ "type": "scope_round", "round": round, "action": action, "reason": reason }));
        }
    };
    let max_rounds = opts.max_rounds.unwrap_or_else(|| scope_round_cap(opts.verify_config.max_repair_rounds));
    let mut cfg = opts.verify_config.clone();
    if let Some(p) = &opts.verify_policy {
        cfg.policy = p.clone();
    }
    let policy = cfg.policy.clone();
    let runner: Arc<dyn VerifyRunner> = opts.runner.clone().unwrap_or_else(|| Arc::new(CommandRunner));
    let secrets: Vec<&str> = opts.secrets.iter().map(String::as_str).collect();
    let accumulated: Arc<Mutex<Vec<FileBackup>>> = Arc::new(Mutex::new(vec![]));
    let rollback = |acc: &Arc<Mutex<Vec<FileBackup>>>| -> Result<RestoreResult> {
        let all = acc.lock().unwrap().clone();
        if all.is_empty() {
            return Ok(RestoreResult::default());
        }
        let reversed: Vec<FileBackup> = all.iter().rev().cloned().collect();
        let res = opts.files.restore_backups(&reversed, false)?;
        let conflicts: HashSet<String> = res.conflicts.iter().map(|p| p.replace('\\', "/")).collect();
        opts.files.replace_turn_backups(all.into_iter().filter(|b| conflicts.contains(&b.path)).collect());
        Ok(res)
    };
    let mut reports = vec![];
    let mut last_verdict: Option<VerifyVerdict> = None;
    let mut last_commands: Vec<String> = vec![];
    let mut changed: Vec<String> = vec![];
    let outcome: Result<ScopeLoopResult> = async {
        for round in 1..=max_rounds {
            if let Some(cap) = budget_stop_reason(opts.budget.as_ref()) {
                emit(round, "stop", &cap);
                return Ok(ScopeLoopResult {
                    ok: false,
                    rounds: round - 1,
                    reason: cap,
                    verdict: last_verdict.clone(),
                    reports: reports.clone(),
                    changed_files: changed.clone(),
                    rollback: Some(rollback(&accumulated)?),
                });
            }
            opts.files.begin_turn();
            let verify_output = last_verdict.as_ref().map(|v| redact_text(&format_verify_errors(v), &secrets)).unwrap_or_default();
            let report = attempt(ScopeLoopAttempt { round, goal: goal.into(), verify_output, related_files: changed.clone(), commands: last_commands.clone() }, opts.cancel.clone()).await?;
            reports.push(report);
            let backups = opts.files.take_turn_backups();
            accumulated.lock().unwrap().extend(backups);
            changed = unique_paths(&accumulated.lock().unwrap());
            if changed.is_empty() {
                let reason = "no tracked file changes to verify";
                emit(round, "stop", reason);
                return Ok(ScopeLoopResult {
                    ok: false,
                    rounds: round,
                    reason: reason.into(),
                    verdict: None,
                    reports: reports.clone(),
                    changed_files: changed.clone(),
                    rollback: Some(rollback(&accumulated)?),
                });
            }
            let mut plan = plan_verification(&opts.cwd, &changed, &cfg, &policy);
            last_commands = plan.commands.iter().map(|c| c.display.clone()).collect();
            let started = now_millis();
            let approve = opts.approve.clone();
            let approve_ref: Option<&(dyn Fn(&VerifyPlan) -> BoxFuture<'static, bool> + Send + Sync)> = approve.as_deref();
            let verdict = redact_verdict(&run_verification(&opts.cwd, &changed, &cfg, &*runner, &opts.cancel, &mut plan, approve_ref).await?, &secrets);
            last_verdict = Some(verdict.clone());
            if verdict.severity != Severity::Fail {
                emit(round, "contract", &format!("verification {} in {}ms: {}", verdict.severity.as_str(), now_millis() - started, verdict.reason));
                return Ok(ScopeLoopResult {
                    ok: true,
                    rounds: round,
                    reason: verdict.reason.clone(),
                    verdict: Some(verdict),
                    reports: reports.clone(),
                    changed_files: changed.clone(),
                    rollback: None,
                });
            }
            if round == max_rounds {
                break;
            }
            emit(round, "expand", &format!("verification failed: {}", verdict.reason));
        }
        let reason = match &last_verdict {
            Some(v) => format!("verification still failing after {max_rounds} round(s): {}", v.reason),
            None => format!("scope loop exhausted {max_rounds} round(s)"),
        };
        emit(max_rounds, "stop", &reason);
        Ok(ScopeLoopResult { ok: false, rounds: max_rounds, reason, verdict: last_verdict.clone(), reports: reports.clone(), changed_files: changed.clone(), rollback: Some(rollback(&accumulated)?) })
    }
    .await;
    if outcome.is_err() {
        accumulated.lock().unwrap().extend(opts.files.take_turn_backups());
        let _ = rollback(&accumulated);
    }
    outcome
}

// ---------------------------------------------------------------------------------------------
// Multi-candidate generation
// ---------------------------------------------------------------------------------------------

pub const MAX_CANDIDATES: usize = 2;
pub const CANDIDATE_DEGRADE_RATIO: f64 = 0.2;

pub fn candidate_cap(requested: Option<usize>) -> usize {
    requested.map(|r| r.clamp(1, MAX_CANDIDATES)).unwrap_or(MAX_CANDIDATES)
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub index: usize,
    pub task: String,
    pub report: String,
    pub cost_usd: f64,
    pub passed: bool,
    pub warnings: usize,
    pub reason: String,
    pub backups: Vec<FileBackup>,
}

/// Reflection: folds a verification verdict into a rankable candidate. A candidate passes
/// whenever the verdict does not `fail` (warn and skip count as pass).
pub fn reflect(index: usize, task: &str, report: &str, cost_usd: f64, backups: Vec<FileBackup>, verdict: &VerifyVerdict) -> Candidate {
    Candidate { index, task: task.into(), report: report.into(), cost_usd, passed: verdict.severity != Severity::Fail, warnings: verdict.warnings.len(), reason: verdict.reason.clone(), backups }
}

pub fn rerank(candidates: &[Candidate]) -> Vec<Candidate> {
    let mut out = candidates.to_vec();
    out.sort_by(|a, b| {
        (b.passed as u8).cmp(&(a.passed as u8)).then(a.warnings.cmp(&b.warnings)).then(a.cost_usd.partial_cmp(&b.cost_usd).unwrap_or(std::cmp::Ordering::Equal)).then(a.index.cmp(&b.index))
    });
    out
}

pub fn cost_degrade(budget: Option<&SharedBudget>, now: i64) -> (bool, String) {
    let Some(b) = budget else { return (false, String::new()) };
    let b = b.lock().unwrap();
    if b.cost_limit > 0.0 {
        let remaining = b.cost_limit - b.cost_spent;
        if remaining / b.cost_limit < CANDIDATE_DEGRADE_RATIO {
            return (true, format!("cost budget nearly exhausted ({} of {})", format_usd(b.cost_spent.max(0.0)), format_usd(b.cost_limit)));
        }
    }
    if b.time_limit_ms > 0 {
        if let Some(started) = b.started_at {
            let remaining = (started + b.time_limit_ms as i64 - now) as f64;
            if remaining / (b.time_limit_ms as f64) < CANDIDATE_DEGRADE_RATIO {
                return (true, format!("time budget nearly exhausted ({} of {} left)", format_duration_ms(remaining.max(0.0)), format_duration_ms(b.time_limit_ms as f64)));
            }
        }
    }
    (false, String::new())
}

pub struct CandidateRunResult {
    pub ok: bool,
    pub winner: Option<Candidate>,
    pub candidates: Vec<Candidate>,
    pub ranked: Vec<Candidate>,
    pub degraded: bool,
    pub reason: String,
}

pub async fn run_candidates(
    task: &str,
    generate: impl Fn(String, usize, bool, CancellationToken) -> BoxFuture<'static, Result<String>>,
    verify: impl Fn(Vec<String>, CancellationToken) -> BoxFuture<'static, Result<VerifyVerdict>>,
    files: &WorkspaceFiles,
    cancel: &CancellationToken,
    budget: Option<&SharedBudget>,
    requested: Option<usize>,
    emit: Option<&(dyn Fn(Value) + Send + Sync)>,
) -> CandidateRunResult {
    let (degraded, degrade_reason) = cost_degrade(budget, now_millis());
    let count = if degraded { 1 } else { candidate_cap(requested) };
    if degraded {
        if let Some(e) = emit {
            e(json!({ "type": "cost_degrade", "reason": degrade_reason, "candidates": 1 }));
        }
    }
    let spent = || budget.map(|b| b.lock().unwrap().cost_spent).unwrap_or(0.0);
    let mut generated = vec![];
    let mut stop_reason = String::new();
    for index in 0..count {
        if let Some(cap) = budget_stop_reason(budget) {
            stop_reason = cap;
            break;
        }
        let before = spent();
        files.begin_turn();
        let mut backups = vec![];
        let mut report = String::new();
        let outcome: Result<Candidate> = async {
            report = generate(task.to_string(), index, degraded, cancel.clone()).await?;
            backups = files.take_turn_backups();
            let verdict = verify(unique_paths(&backups), cancel.clone()).await?;
            Ok(reflect(index, task, &report, (spent() - before).max(0.0), backups.clone(), &verdict))
        }
        .await;
        match outcome {
            Ok(c) => {
                let passed = c.passed;
                generated.push(c);
                if passed {
                    break;
                }
                let _ = files.restore_backups(&backups, false);
            }
            Err(error) => {
                if backups.is_empty() {
                    backups = files.take_turn_backups();
                }
                if !backups.is_empty() {
                    let _ = files.restore_backups(&backups, false);
                }
                generated.push(Candidate {
                    index,
                    task: task.into(),
                    report,
                    cost_usd: (spent() - before).max(0.0),
                    passed: false,
                    warnings: 0,
                    reason: safe_error(&error.to_string(), None),
                    backups,
                });
            }
        }
    }
    let ranked = rerank(&generated);
    if let Some(winner) = generated.iter().find(|c| c.passed).cloned() {
        files.merge_backups(winner.backups.clone());
        let reason = winner.reason.clone();
        return CandidateRunResult { ok: true, winner: Some(winner), candidates: generated, ranked, degraded, reason };
    }
    let reason = if !stop_reason.is_empty() {
        stop_reason
    } else if !generated.is_empty() {
        format!("all {} candidate(s) failed verification: {}", generated.len(), generated.last().map(|c| c.reason.clone()).unwrap_or_default())
    } else {
        "no candidate generated".into()
    };
    CandidateRunResult { ok: false, winner: None, candidates: generated, ranked, degraded, reason }
}

/// Convenience for callers that only need the default verify runner.
pub fn default_runner() -> Arc<dyn VerifyRunner> {
    Arc::new(CommandRunner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(id: &str, deps: &[&str], role: &str) -> Todo {
        Todo {
            id: id.into(),
            title: id.into(),
            acceptance: String::new(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            status: "pending".into(),
            role: role.into(),
            assignee: None,
            artifacts: vec![],
            rework_cycles: 0,
        }
    }

    #[test]
    fn validation_and_scheduling() {
        let plan = Plan { id: "p".into(), goal: "g".into(), created_at: now_iso(), todos: vec![todo("t1", &[], "coder"), todo("t2", &[], "coder"), todo("t3", &["t1"], "reviewer")] };
        validate_plan(&plan).unwrap();
        let mut s = Scheduler::new(plan.clone(), 1, 2);
        let first = s.dispatch_ready();
        assert_eq!(first.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["t1"]);
        assert_eq!(s.mark_failed("t1"), vec!["t3"]);
        let cyclic = Plan { todos: vec![todo("a", &["b"], "coder"), todo("b", &["a"], "coder")], ..plan.clone() };
        assert!(validate_plan(&cyclic).unwrap_err().0.contains("cycle involving: a, b"));
        let parsed = plan_from_planner(&json!({"goal": " g ", "todos": [{"id": "t1", "title": "x", "role": "coder", "dependsOn": ["t0", "t0"]}]}), "p").unwrap();
        assert_eq!(parsed.goal, "g");
        assert_eq!(parsed.todos[0].depends_on, vec!["t0"]);
        assert!(plan_from_planner(&json!({"goal": "g", "todos": [{"id": "t1", "title": "x", "role": "boss"}]}), "p").is_err());
        assert_eq!(radix36(35), "z");
        assert_eq!(scope_round_cap(2), 3);
        assert_eq!(scope_round_cap(10), 4);
    }
}
