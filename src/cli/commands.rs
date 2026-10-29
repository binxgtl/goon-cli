//! Slash commands, the interactive main loop, `/dag`, workflows and the memory-store commands.

use super::app::{App, MODE_ORDER};
use super::{pad_end, pad_start, safe_error, write_json_line};
use crate::config::{self, EFFORT_LEVELS};
use crate::coord::{run_dag, DagConfig, DagHooks, DagOptions, DagSpawnRequest, DEFAULT_DAG_TOKEN_BUDGET};
use crate::cost::{format_duration_ms, format_usd, CostLedger, RunBudget, SharedBudget};
use crate::jsutil::{self, now_iso, now_millis, stringify_pretty, take_chars};
use crate::model::ModelRequest;
use crate::session::{format_goal_status_indicator, format_goal_summary_lines, format_goal_usage_summary, goal_status_label, parse_goal_draft_args, same_workspace, GoalDraft, SessionGoal, SessionRoute, SessionStore, GOAL_USAGE};
use crate::subagents::RunTasksOptions;
use crate::term::terminal_text;
use crate::tools::{catalog_definition, ApprovalRequest, ToolCall, ToolExec, ToolRegistry};
use crate::tui::{PickerItem, PickerTab, QuestionOpts};
use crate::workflow::{discover_workflows, execute_workflow, WorkflowHooks};
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const HELP_TEXT: &str = concat!(
    "\nCommands (can also run while agent is working):\n",
    "  /auto [on|off], /yolo             Toggle Auto-Approve mode (or press Shift+Tab)\n",
    "  /transcript [on|off]              Toggle Codex full-screen Transcript mode (or press Ctrl+T)\n",
    "  /view [normal|verbose|transcript] Switch view: normal (hide thinking) | verbose | transcript\n",
    "  /copy [last|code|<N>]             Copy last response, code block, or Nth response (or press Ctrl+O)\n",
    "  /btw <question>, /side <question> Ask an ephemeral side question without modifying history\n",
    "  /goal [--budget <N>] [<obj>|edit|pause|resume|complete|clear] Autonomous goal loop (Codex /goal)\n",
    "  /recap                            Generate a live summary of current session progress\n",
    "  /skills [list|<name>]             List or inspect SKILL.md skills (use $skill-name in prompt)\n",
    "  /memories [list|add <text>|rm <id>|clear] Manage persistent user memories across sessions\n",
    "  /memory [rag index|rag stats|rag search <q>|budget|pref set/get/rm/list] Memory store: lexical repo index & preferences\n",
    "  /presets [list|save <name> <prompt>|run <name>|rm <name>] Manage reusable prompt presets\n",
    "  /pair [start|stop|status|qr|lan]  Pair mobile/web device via Cloudflared tunnel (QR + link)\n",
    "  /workflow [list|show|create|rm|run <name> [args]] Multi-step custom & built-in workflows\n",
    "  /network [<topic>|board|send|clear] Multi-agent Brainstorm/Council room & P2P message bus\n",
    "  /dag <goal>                       Multi-agent todo DAG: planner -> serial coders -> merge + verify\n",
    "  /mention <filepath>               Insert @filepath mention into the composer\n",
    "  /cd <dir>, /pwd                   Change or show active workspace directory\n",
    "  /mode [ask|auto|plan|readonly]    Switch approval mode (or press Shift+Tab)\n",
    "  /setup, /config, /provider        Interactive provider, URL, key, model & effort setup\n",
    "  /key                              Replace the stored API key (input hidden)\n",
    "  /prompts [show <name>]            List custom prompt files; run one with /prompts:<name> [args]\n",
    "  /model [id] [--effort=<level>]    Discover/pick model from /models or set model ID\n",
    "  /effort [low|medium|high|xhigh|max|default] Switch reasoning effort (or Alt+./Alt+,)\n",
    "  /plan [off|execute|<prompt>]      Enter/exit Plan Mode or execute approved plan\n",
    "  /revert, /undo [code|msg|both] [N] Rewind last N turns & restore prompt to composer (or press Esc Esc)\n",
    "  /checkpoints                      Interactive picker to rewind to any earlier turn checkpoint\n",
    "  /todo [clear]                     View or clear the current task checklist\n",
    "  /resume [latest|<number>|<id>]    Pick a session from this folder or all projects (←→ / Tab switch)\n",
    "  /new [title]                      Start a fresh session in this workspace\n",
    "  /delete [<id>|current]            Delete a saved session or current session\n",
    "  /compact [focus instructions]     Summarize & archive earlier context history\n",
    "  /diff                             Show git status, staged/unstaged diff & untracked files\n",
    "  /review [branch <b?>|commit <c?>|<instructions>] Run code review on diff, branch, or commit\n",
    "  /fork [title]                     Clone current session into a new forked session ID\n",
    "  /rename <title>                   Set a title for the current session\n",
    "  /export [filename.md]             Export session transcript to a Markdown file\n",
    "  /init                             Generate or update AGENTS.md for this project\n",
    "  /status, /debug-config, /warnings Show session status, config layers, or runtime warnings\n",
    "  /mcp [list|check|import-codex|reload] Inspect, check, reload, or import Codex MCP servers\n",
    "  /subagents [list|show <id>|<task>] List spawned subagents, view transcript, or run a subagent\n",
    "  /tools                            List active tools in current mode\n",
    "  /sessions                         List saved workspace sessions\n",
    "  /doctor                           Check environment health\n",
    "  /clear                            Clear screen (or press Ctrl+L)\n",
    "  /exit                             Exit goon (or press Ctrl+D on empty input)\n",
    "  !<command>                        Execute shell command directly & record in context\n",
    "  @<filepath> / $<skill-name>       Attach workspace file or invoke SKILL.md in prompt\n",
);

const MEMORY_COMMAND_USAGE: &str = "Usage: /memory rag index | rag stats | rag search <query> | budget | pref set <key> <value> | pref get <key> | pref rm <key> | pref list\n      (plain /memories list|add|rm|clear still manages persistent user memories)\n";

fn starts_cmd(input: &str, cmd: &str) -> bool {
    input == cmd || input.starts_with(&format!("{cmd} "))
}

fn line_count(text: &str) -> usize {
    Regex::new(r"\r?\n").unwrap().split(text).count()
}

async fn run_cmd(command: &str, args: &[&str], cwd: &Path, cancel: &CancellationToken, timeout_ms: u64) -> Result<crate::process::CommandResult> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let options = crate::process::RunOptions { cwd, cancel, timeout_ms, env: None, output_limit_bytes: None, hard_output_bytes: None };
    crate::process::run_command(command, &args, options).await.map_err(crate::process::command_error_to_anyhow)
}

fn goal_context(cleared: bool, objective: Option<&str>, status: Option<&str>) -> Value {
    json!({ "role": "user", "content": crate::context::build_user_goal_context_message(cleared, objective, status) })
}

fn with_budget<T>(tools: &ToolRegistry, budget: SharedBudget, f: impl FnOnce() -> T) -> (T, Option<SharedBudget>) {
    let previous = tools.budget();
    tools.set_budget(Some(budget));
    (f(), previous)
}

impl App {
    // ------------------------------------------------------------------ run_workflow tool
    pub fn register_run_workflow(&self, reg: &Arc<ToolRegistry>) -> Result<()> {
        let me = self.me.clone();
        let reg_weak = Arc::downgrade(reg);
        let exec: ToolExec = Arc::new(move |input: Value, cancel: CancellationToken| {
            let me = me.clone();
            let reg_weak = reg_weak.clone();
            Box::pin(async move {
                let app = me.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                let reg = reg_weak.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                let runtime = app.runtime();
                let name = input.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let all = discover_workflows(&runtime.cwd);
                let Some(wf) = all.iter().find(|w| w.name.to_lowercase() == name.to_lowercase()).cloned() else {
                    bail!("Workflow \"{name}\" not found. Available: {}", all.iter().map(|w| w.name.clone()).collect::<Vec<_>>().join(", "));
                };
                let raw_args = input.get("input").and_then(Value::as_str).unwrap_or("").to_string();
                let hooks = app.workflow_hooks(&reg, cancel, None, true);
                let res = execute_workflow(&wf, &raw_args, &hooks).await?;
                Ok(stringify_pretty(&res.to_value()))
            })
        });
        reg.register("run_workflow", catalog_definition("run_workflow", Some(exec)))
    }

    /// Hooks shared by the `run_workflow` tool (`tool_mode`) and the `/workflow run` command.
    fn workflow_hooks(&self, reg: &Arc<ToolRegistry>, cancel: CancellationToken, budget: Option<SharedBudget>, tool_mode: bool) -> WorkflowHooks {
        let runtime = self.runtime();
        let me = self.me.clone();
        let approve_reg = reg.clone();
        let approve_shell: Arc<dyn Fn(String, CancellationToken, crate::risk::RiskClass) -> BoxFuture<'static, bool> + Send + Sync> = Arc::new(move |command: String, step: CancellationToken, risk| {
            let approve = approve_reg.approve.clone();
            Box::pin(async move { approve(ApprovalRequest { tool: "bash".into(), input: json!({ "command": command }), cancel: step, risk: Some(risk), trusted: Some(false) }).await })
        });
        let shared_budget = budget.clone();
        let run_prompt: Arc<dyn Fn(String, crate::workflow::WorkflowStep, CancellationToken) -> crate::workflow::StepFuture + Send + Sync> = {
            let me = me.clone();
            let reg = reg.clone();
            Arc::new(move |prompt: String, step, sig: CancellationToken| {
                let me = me.clone();
                let reg = reg.clone();
                let shared_budget = shared_budget.clone();
                Box::pin(async move {
                    let app = me.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                    if !tool_mode {
                        return app.harness().run(&prompt, &sig, shared_budget).await;
                    }
                    app.run_workflow_child_prompt(&reg, &prompt, &step, &sig).await
                })
            })
        };
        let run_subagents: Option<Arc<dyn Fn(Vec<String>, bool, CancellationToken) -> crate::workflow::StepFuture + Send + Sync>> = (!tool_mode).then(|| {
            let me = me.clone();
            let budget = budget.clone();
            Arc::new(move |tasks: Vec<String>, read_only: bool, sig: CancellationToken| {
                let me = me.clone();
                let budget = budget.clone();
                Box::pin(async move {
                    let app = me.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                    let budget = budget.unwrap_or_else(|| app.fresh_budget());
                    if app.subagents().is_none() {
                        return app.harness().run(&tasks.join("\n\n"), &sig, Some(budget)).await;
                    }
                    let tools = app.tools();
                    let (call, previous) = with_budget(&tools, budget, || ToolCall { tool_call_id: format!("wf_{}", now_millis()), tool_name: if read_only { "delegate_task" } else { "delegate_edit" }.into(), input: json!({ "tasks": tasks }) });
                    let res = tools.execute(&call, &sig).await;
                    tools.set_budget(previous);
                    Ok(res?.text)
                }) as crate::workflow::StepFuture
            }) as Arc<dyn Fn(Vec<String>, bool, CancellationToken) -> crate::workflow::StepFuture + Send + Sync>
        });
        let run_network: Arc<dyn Fn(String, Option<Vec<String>>, Option<u32>, CancellationToken) -> crate::workflow::StepFuture + Send + Sync> = {
            let me = me.clone();
            let reg = reg.clone();
            let budget = budget.clone();
            Arc::new(move |topic: String, roles, rounds, sig: CancellationToken| {
                let me = me.clone();
                let reg = reg.clone();
                let budget = budget.clone();
                Box::pin(async move {
                    let app = me.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                    let Some(subagents) = app.subagents() else {
                        if tool_mode {
                            return Ok(String::new());
                        }
                        return app.harness().run(&topic, &sig, budget).await;
                    };
                    let progress: Option<Arc<dyn Fn(&str) + Send + Sync>> = (!tool_mode).then(|| {
                        let me = me.clone();
                        Arc::new(move |t: &str| {
                            if let Some(a) = me.upgrade() {
                                a.display(t);
                            }
                        }) as Arc<dyn Fn(&str) + Send + Sync>
                    });
                    let res = subagents
                        .network
                        .brainstorm(crate::network::BrainstormOptions { topic, roles, rounds, explore_codebase: None, cancel: sig, budget: if tool_mode { reg.budget() } else { budget.or_else(|| Some(app.fresh_budget())) }, policy: Some(app.policy.clone()), on_progress: progress })
                        .await?;
                    let report = crate::network::format_brainstorm_report(&res);
                    if !tool_mode {
                        app.display(&format!("\n{}\n", res.synthesis));
                    }
                    Ok(report)
                })
            })
        };
        let run_dag_hook: Arc<dyn Fn(String, CancellationToken) -> crate::workflow::StepFuture + Send + Sync> = {
            let me = me.clone();
            let reg = reg.clone();
            let budget = budget.clone();
            Arc::new(move |goal: String, sig: CancellationToken| {
                let me = me.clone();
                let reg = reg.clone();
                let budget = budget.clone();
                Box::pin(async move {
                    let app = me.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                    let budget = if tool_mode { reg.budget().unwrap_or_else(|| app.fresh_budget()) } else { budget.unwrap_or_else(|| app.fresh_budget()) };
                    let tools = app.tools();
                    let previous = tools.budget();
                    if !tool_mode {
                        tools.set_budget(Some(budget.clone()));
                    }
                    let res = app.run_dag_core(&goal, budget, &sig).await;
                    if !tool_mode {
                        tools.set_budget(previous);
                    }
                    res
                })
            })
        };
        let on_event: Arc<dyn Fn(Value) + Send + Sync> = {
            let me = me.clone();
            Arc::new(move |event: Value| {
                if let Some(a) = me.upgrade() {
                    a.publish_event(&event, false);
                }
            })
        };
        let on_progress: Option<Arc<dyn Fn(&str) + Send + Sync>> = (!tool_mode).then(|| {
            let me = me.clone();
            Arc::new(move |t: &str| {
                if let Some(a) = me.upgrade() {
                    a.display(t);
                }
            }) as Arc<dyn Fn(&str) + Send + Sync>
        });
        let confirm_step: Option<Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, bool> + Send + Sync>> = (!tool_mode).then(|| {
            let me = me.clone();
            Arc::new(move |msg: String, sig: CancellationToken| {
                let me = me.clone();
                Box::pin(async move {
                    let Some(app) = me.upgrade() else { return false };
                    if app.mode() == "auto" {
                        return true;
                    }
                    let block = format!("\n▲ Workflow approval gate: {msg}\n");
                    let answer = app.ask_block(&block, "[y/N] ", Some(&sig), QuestionOpts::default()).await.unwrap_or_default().trim().to_lowercase();
                    answer == "y" || answer == "yes"
                }) as BoxFuture<'static, bool>
            }) as Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, bool> + Send + Sync>
        });
        WorkflowHooks {
            cwd: runtime.cwd.clone(),
            cancel,
            effective_read_only: reg.effective_read_only(),
            policy: Some(self.policy.clone()),
            on_progress,
            approve_shell: Some(approve_shell),
            run_prompt: Some(run_prompt),
            run_subagents,
            run_network: Some(run_network),
            run_dag: Some(run_dag_hook),
            multi_agent_enabled: runtime.multi_agent_mode != "off",
            on_event: Some(on_event),
            confirm_step,
        }
    }

    async fn run_workflow_child_prompt(&self, reg: &Arc<ToolRegistry>, prompt: &str, step: &crate::workflow::WorkflowStep, sig: &CancellationToken) -> Result<String> {
        let runtime = self.runtime();
        let parent = self.store();
        let root = runtime.data_dir.join("workflows").join(parent.id());
        let child = Arc::new(SessionStore::open(&root, SessionRoute::from_runtime(&runtime), None, None)?);
        let child_id = child.id();
        let outcome: Result<String> = async {
            let step_read_only = reg.read_only() || step.bool("readOnly") == Some(true) || step.str("mode") == Some("readonly");
            let approve = reg.approve.clone();
            let child_tools = ToolRegistry::new(&runtime.cwd, Arc::new(move |req| approve(req)), step_read_only, None);
            child_tools.set_policy(reg.policy());
            child_tools.set_plan_mode(reg.plan_mode() || step.str("mode") == Some("plan"));
            child_tools.set_cost_ledger(reg.cost_ledger());
            *child_tools.rejection_guard.lock().unwrap() = reg.rejection_guard.lock().unwrap().clone();
            let before = child.session.lock().unwrap().cost();
            let deps = crate::harness::HarnessDeps { verify_enabled: Some(false), ..Default::default() };
            let harness = crate::harness::Harness::new(runtime.clone(), child.clone(), child_tools, Arc::new(|_| {}), None, deps)?;
            harness.run(prompt, sig, reg.budget()).await?;
            // Per-child session counter: a shared-ledger snapshot double-counts overlapping runs.
            let delta = child.session.lock().unwrap().cost() - before;
            if delta > 0.0 {
                {
                    let mut s = parent.session.lock().unwrap();
                    s.cost_usd = Some(s.cost() + delta);
                }
                let _ = parent.save();
            }
            let messages = child.session.lock().unwrap().messages.clone();
            Ok(crate::reasoning::last_assistant_answer(&messages, &runtime.inline_reasoning))
        }
        .await;
        let _ = child.close();
        let _ = std::fs::remove_file(root.join(format!("{child_id}.json")));
        outcome
    }

    // ------------------------------------------------------------------ DAG
    /// Planner + `run_dag` + fail-closed verification with the parent approval policy.
    pub async fn run_dag_core(&self, goal: &str, budget: SharedBudget, sig: &CancellationToken) -> Result<String> {
        let Some(subagents) = self.subagents() else { bail!("DAG requires subagents; restart without --no-subagents.") };
        let runtime = self.runtime();
        let me = self.me.clone();
        let planner_budget = budget.clone();
        let planner: Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, Result<Value>> + Send + Sync> = {
            let me = me.clone();
            Arc::new(move |planner_goal: String, planner_sig: CancellationToken| {
                let me = me.clone();
                let budget = planner_budget.clone();
                Box::pin(async move {
                    let app = me.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                    let runtime = app.runtime();
                    let model = crate::provider::create_model(&runtime, Some(&app.session_id()))?;
                    let system = "You are a planning assistant for a multi-agent coding DAG. Decompose the goal into 1-8 todos. Respond with ONLY a JSON object, no prose or code fences: {\"goal\": \"...\", \"todos\": [{\"id\": \"t1\", \"title\": \"short imperative step\", \"acceptance\": \"how to verify\", \"dependsOn\": [], \"role\": \"planner|coder|reviewer|tester|researcher\", \"assignee\": \"optional display label\"}]}. Use \"coder\" for todos that modify files and \"tester\" for todos that verify them; read-only analysis uses planner/reviewer/researcher. dependsOn may only reference earlier todo ids (acyclic); ids are t1, t2, ...";
                    let request = ModelRequest {
                        system: Some(system.into()),
                        messages: vec![json!({ "role": "user", "content": planner_goal })],
                        tools: vec![],
                        max_output_tokens: runtime.max_output_tokens.min(8_192),
                        effort: None,
                        cancel: planner_sig.clone(),
                        timeout: std::time::Duration::from_millis(runtime.timeout_ms),
                    };
                    let response = crate::generate::stream_text(&*model, request, 2).await.map_err(anyhow::Error::from)?;
                    app.harness().account_usage(Some(&response.usage), Some(&runtime.model), Some(&budget), 0, 0);
                    let raw = response.text;
                    let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) else { bail!("Planner did not return a JSON plan.") };
                    if end <= start {
                        bail!("Planner did not return a JSON plan.");
                    }
                    crate::jsutil::parse_json(&raw[start..=end]).map_err(|e| anyhow!("{e}"))
                })
            })
        };
        let goal_text = goal.to_string();
        let spawn_sub = subagents.clone();
        let spawn: Arc<dyn Fn(DagSpawnRequest) -> BoxFuture<'static, Result<String>> + Send + Sync> = Arc::new(move |req: DagSpawnRequest| {
            let subagents = spawn_sub.clone();
            let goal = goal_text.clone();
            Box::pin(async move {
                let task = [format!("Goal: {goal}"), format!("Todo {}: {}", req.todo.id, req.todo.title), if req.todo.acceptance.is_empty() { String::new() } else { format!("Acceptance criteria: {}", req.todo.acceptance) }, "Other todos may have already changed the workspace; do not revert their work. End with a short report.".to_string()]
                    .into_iter()
                    .filter(|l| !l.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                subagents.run_tasks(vec![task], RunTasksOptions { read_only: req.read_only, cancel: req.cancel, budget: Some(req.budget), allowed_tools: Some(req.allowed_tools.into_iter().collect::<HashSet<_>>()), role: Some(req.todo.role.clone()) }).await
            })
        });
        let verify_me = me.clone();
        let test: Arc<dyn Fn(crate::coord::Todo, Vec<String>, CancellationToken) -> BoxFuture<'static, Result<bool>> + Send + Sync> = Arc::new(move |_todo, files: Vec<String>, sig: CancellationToken| {
            let me = verify_me.clone();
            Box::pin(async move {
                let app = me.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                app.run_dag_verification(files, sig).await
            })
        });
        let verify_me = me.clone();
        let verify: Arc<dyn Fn(Vec<String>, String, CancellationToken) -> BoxFuture<'static, Result<bool>> + Send + Sync> = Arc::new(move |files: Vec<String>, _diff, sig: CancellationToken| {
            let me = verify_me.clone();
            Box::pin(async move {
                let app = me.upgrade().ok_or_else(|| anyhow!("Session closed."))?;
                app.run_dag_verification(files, sig).await
            })
        });
        let emit_me = me.clone();
        let options = DagOptions {
            config: DagConfig { run_token_budget: runtime.run_token_budget, max_cost_usd: runtime.max_cost_usd, max_time_ms: runtime.max_time_ms, rework_cap: Some(runtime.multi_agent_rework_cap as u32), max_concurrency: Some(runtime.multi_agent_max_concurrency as usize) },
            cancel: sig.clone(),
            files: self.tools().files.clone(),
            budget: Some(budget),
            allowed_mcp_tools: runtime.multi_agent_mcp_tools.clone(),
            allow_empty: false,
            write_concurrency: None,
            emit: Some(Arc::new(move |event: Value| {
                if let Some(a) = emit_me.upgrade() {
                    a.publish_event(&event, false);
                }
            })),
        };
        let result = run_dag(goal, DagHooks { planner, spawn, review: None, test: Some(test), verify: Some(verify) }, options).await?;
        Ok(result.summary)
    }

    async fn run_dag_verification(&self, files: Vec<String>, sig: CancellationToken) -> Result<bool> {
        use crate::verify::{assess_verify_plan, plan_verification, redact_verdict, run_verification, CommandRunner};
        let runtime = self.runtime();
        if !runtime.verify.enabled {
            return Ok(true);
        }
        let mut plan = plan_verification(&runtime.cwd, &files, &runtime.verify, &runtime.verify.policy);
        let commands: Vec<String> = plan.commands.iter().map(|c| c.display.clone()).collect();
        self.publish_event(&json!({ "type": "verify_start", "files": files, "commands": commands }), false);
        let started = now_millis();
        let tools = self.tools();
        let approve_files = files.clone();
        let approve_sig = sig.clone();
        let approve = move |plan: &crate::verify::VerifyPlan| -> BoxFuture<'static, bool> {
            // Same critical/network gates as the bash tool; the risk drives auto-mode approval.
            let assessment = assess_verify_plan(plan, tools.policy().as_deref());
            if let Some(denied) = assessment.denied {
                super::stderr_write(&format!("\n[verify blocked: {}]\n", terminal_text(&denied)));
                return Box::pin(async { false });
            }
            let approve = tools.approve.clone();
            let request = ApprovalRequest { tool: "verify".into(), input: json!({ "files": approve_files, "commands": plan.commands.iter().map(|c| c.display.clone()).collect::<Vec<_>>() }), cancel: approve_sig.clone(), risk: Some(assessment.risk), trusted: Some(false) };
            Box::pin(async move { approve(request).await })
        };
        let verdict = run_verification(&runtime.cwd, &files, &runtime.verify, &CommandRunner, &sig.clone(), &mut plan, Some(&approve)).await?;
        let verdict = redact_verdict(&verdict, &[&runtime.api_key]);
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
        self.publish_event(&Value::Object(event), false);
        Ok(verdict.pass)
    }

    // ------------------------------------------------------------------ memory store
    async fn run_memory_store_command(&self, rest: &str) {
        let runtime = self.runtime();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let head = parts.first().copied().unwrap_or("");
        let sub = parts.get(1).copied();
        let tail: Vec<&str> = parts.iter().skip(2).copied().collect();
        let api_key: Vec<String> = (!runtime.api_key.is_empty()).then(|| runtime.api_key.clone()).into_iter().collect();
        let project = crate::rag::project_id_for(&runtime.cwd);
        let rag = crate::memory::RagSettings {
            top_k: Some(runtime.rag_top_k as usize),
            capacity: Some(runtime.rag_capacity as usize),
            threshold: Some(runtime.rag_threshold),
            max_files: Some(runtime.rag_index_max_files as usize),
            max_file_bytes: Some(runtime.rag_index_max_file_bytes),
            embedder: None,
        };
        let store = match crate::memory::MemoryStore::open(&runtime.data_dir, &project, Some(&runtime.cwd), &api_key, rag).await {
            Ok(s) => s,
            Err(error) => {
                self.display(&format!("Memory error: {}\n", safe_error(&error)));
                return;
            }
        };
        let outcome: Result<()> = async {
            if head == "rag" && sub == Some("index") {
                let r = store.ensure_repo_indexed(None).await?;
                let suffix = if r.truncated { " (walk truncated at the file cap)" } else { "" };
                self.display(&format!("Indexed {} files / {} chunks in {}ms [{}]{suffix}\n", r.files, r.chunks, r.duration_ms, r.reindexed));
                return Ok(());
            }
            if head == "rag" && sub == Some("stats") {
                let stats = store.knowledge.stats().await;
                self.display(&format!(
                    "Knowledge index ({project}):\n  Files / chunks : {} / {}\n  Insights       : {}\n  Indexed at     : {}\n  Top-K / cap    : {} / {} chunks (threshold {})\n  Walk caps      : {} files, {} bytes/file\n  Exec lessons   : {}\n  Preferences    : {} ({})\n",
                    stats.files,
                    stats.chunks,
                    stats.insights,
                    stats.indexed_at.unwrap_or_else(|| "never (run /memory rag index)".into()),
                    runtime.rag_top_k,
                    runtime.rag_capacity,
                    jsutil::js_string(&jsutil::num(runtime.rag_threshold)),
                    runtime.rag_index_max_files,
                    runtime.rag_index_max_file_bytes,
                    store.exec_size(),
                    store.preference.len(),
                    store.preference.file.display()
                ));
                return Ok(());
            }
            if head == "rag" && sub == Some("search") {
                let query = tail.join(" ").trim().to_string();
                if query.is_empty() {
                    self.display("Usage: /memory rag search <query>\n");
                    return Ok(());
                }
                let hits = store.knowledge.retrieve(&query, runtime.rag_top_k as usize, None).await?;
                if hits.is_empty() {
                    self.display(&format!("No lexical matches for \"{query}\". Run \"/memory rag index\" first if the index is empty.\n"));
                    return Ok(());
                }
                let lines: Vec<String> = hits
                    .iter()
                    .map(|h| {
                        let preview = jsutil::slice_chars(&Regex::new(r"\s+").unwrap().replace_all(&h.chunk.content, " "), 0, 100).to_string();
                        format!("  {}:{}-{}  ({})  {preview}", h.chunk.path, h.chunk.start_line, h.chunk.end_line, jsutil::to_fixed(h.score, 3))
                    })
                    .collect();
                self.display(&format!("Top {} lexical (hashed term-frequency) matches:\n{}\n", hits.len(), lines.join("\n")));
                return Ok(());
            }
            if head == "budget" {
                let stats = store.knowledge.stats().await;
                let working = store.working.lock().unwrap().render().unwrap_or_default();
                let tree = store.repo_tree().unwrap_or_default();
                let wl = jsutil::utf16_len(&working);
                let tl = jsutil::utf16_len(&tree);
                self.display(&format!(
                    "Memory & retrieval budget:\n  memoryEnabled={}, ragEnabled={}\n  Retrieval      : topK={}, capacity={}, threshold={}\n  Walk caps      : {} files, {} bytes/file\n  Knowledge      : {} files / {} chunks / {} insights\n  Working notes  : {wl} chars (~{} tokens)\n  Repo tree      : {tl} chars (~{} tokens)\n  Exec lessons   : {} (rolling 50, summaries only)\n  Preferences    : {}\n  Injection      : retrieval is injected as an untrusted user block, bounded by the context files budget (v0.6).\n",
                    runtime.memory_enabled,
                    runtime.rag_enabled,
                    runtime.rag_top_k,
                    runtime.rag_capacity,
                    jsutil::js_string(&jsutil::num(runtime.rag_threshold)),
                    runtime.rag_index_max_files,
                    runtime.rag_index_max_file_bytes,
                    stats.files,
                    stats.chunks,
                    stats.insights,
                    wl.div_ceil(4),
                    tl.div_ceil(4),
                    store.exec_size(),
                    store.preference.len()
                ));
                return Ok(());
            }
            if head == "pref" {
                let action = sub.unwrap_or("list").to_lowercase();
                match action.as_str() {
                    "list" | "all" => {
                        let all = store.preference.all();
                        if all.is_empty() {
                            self.display(&format!("No preferences saved ({}).\nUse \"/memory pref set <key> <value>\".\n", store.preference.file.display()));
                            return Ok(());
                        }
                        let mut keys: Vec<&String> = all.keys().collect();
                        keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                        let lines: Vec<String> = keys.iter().map(|k| format!("  {k} = {}", all[*k])).collect();
                        self.display(&format!("Preferences ({}):\n{}\n", keys.len(), lines.join("\n")));
                    }
                    "set" => {
                        let key = tail.first().map(|k| k.trim().to_string()).unwrap_or_default();
                        let value = tail.iter().skip(1).copied().collect::<Vec<_>>().join(" ").trim().to_string();
                        if key.is_empty() || value.is_empty() {
                            self.display("Usage: /memory pref set <key> <value>\n");
                            return Ok(());
                        }
                        store.handle_event(json!({ "type": "user_preference", "key": key, "value": value })).await;
                        self.display(&format!("Saved preference \"{key}\" ({}).\n", store.preference.file.display()));
                    }
                    "get" => {
                        let key = tail.join(" ").trim().to_string();
                        match store.preference.get(&key) {
                            Some(v) => self.display(&format!("{key} = {v}\n")),
                            None => self.display(&format!("No preference \"{key}\".\n")),
                        }
                    }
                    "rm" | "remove" => {
                        let key = tail.join(" ").trim().to_string();
                        if store.preference.remove(&key)? {
                            self.display(&format!("Removed preference \"{key}\".\n"));
                        } else {
                            self.display(&format!("No preference \"{key}\".\n"));
                        }
                    }
                    _ => self.display(MEMORY_COMMAND_USAGE),
                }
                return Ok(());
            }
            self.display(MEMORY_COMMAND_USAGE);
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            self.display(&format!("Memory error: {}\n", safe_error(&error)));
        }
        store.close().await;
    }

    // ------------------------------------------------------------------ immediate commands
    /// Commands that may run while the agent works. `Ok(false)` hands the input to the idle loop.
    pub async fn execute_immediate_command(&self, raw: &str) -> Result<bool> {
        let input = raw.trim();
        if input.is_empty() {
            return Ok(true);
        }
        if input == "/exit" || input == "/quit" {
            self.st.lock().unwrap().exiting = true;
            if let Some(c) = self.controller() {
                c.cancel();
            }
            self.close_input();
            return Ok(true);
        }
        if input == "/clear" {
            if let Some(t) = self.tui() {
                t.clear();
            }
            self.display(&self.banner());
            return Ok(true);
        }
        if input == "/help" {
            self.display(HELP_TEXT);
            return Ok(true);
        }
        let store = self.store();
        if starts_cmd(input, "/copy") {
            let selector = input[5..].trim();
            let messages = store.session.lock().unwrap().messages.clone();
            let Some((text, label)) = super::extract_copyable_from_messages(&messages, selector) else {
                if self.tui().is_some() {
                    self.notice("Nothing to copy yet (no assistant output in this session).");
                } else {
                    self.display("Nothing to copy yet (no assistant output in this session).\n");
                }
                return Ok(true);
            };
            let ok = super::copy_text_to_clipboard(&text).await;
            let lines = line_count(&text);
            let chars = jsutil::utf16_len(&text);
            let msg = if ok { format!("Copied {label} ({chars} chars · {lines} lines) to clipboard.") } else { format!("Copied {label} ({chars} chars · {lines} lines).") };
            self.notice(&msg);
            self.display(&format!("{msg}\n"));
            return Ok(true);
        }
        if starts_cmd(input, "/btw") || starts_cmd(input, "/side") {
            let question = Regex::new(r"^/(?:btw|side)\b").unwrap().replace(input, "").trim().to_string();
            if question.is_empty() {
                self.display("Usage: /btw <question> (asks a side question without modifying the session history)\n");
                return Ok(true);
            }
            if self.tui().is_some() {
                self.notice("Answering side question (/btw)...");
            } else {
                self.display("\n[Answering side question (/btw)...]\n");
            }
            let me = self.me.clone();
            tokio::spawn(async move {
                let Some(app) = me.upgrade() else { return };
                let timeout = CancellationToken::new();
                let t2 = timeout.clone();
                let ms = app.runtime().timeout_ms;
                let timer = tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    t2.cancel();
                });
                match app.harness().ask_side_question(&question, &timeout).await {
                    Ok((text, _, _)) => {
                        let body = text.split('\n').map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n");
                        app.display(&format!("\n◇ Side answer: {question}\n{body}\n"));
                        app.notice("Side question (/btw) answered (history unchanged).");
                    }
                    Err(error) => app.display(&format!("\n[Side question failed: {}]\n", safe_error(&error))),
                }
                timer.abort();
            });
            return Ok(true);
        }
        if starts_cmd(input, "/goal") {
            return self.goal_command(input[5..].trim()).await;
        }
        if input == "/pwd" || input == "/cwd" {
            self.display(&format!("Workspace directory: {}\n", self.runtime().cwd.display()));
            return Ok(true);
        }
        if input == "/prompts" || input == "/prompts list" || input.starts_with("/prompts show ") {
            let prompts = crate::prompts::discover_prompts(&self.runtime().cwd);
            if let Some(name) = input.strip_prefix("/prompts show ") {
                let name = name.trim().trim_start_matches("prompts:").to_string();
                let name = Regex::new(r"^prompts:").unwrap().replace(&name, "").to_string();
                match prompts.iter().find(|p| p.name.to_lowercase() == name.to_lowercase()) {
                    Some(p) => self.display(&format!("\n/prompts:{} ({}):\n{}\n", p.name, p.path.display(), jsutil::slice_chars(&p.body, 0, 4000))),
                    None => self.display(&format!("Prompt \"{name}\" not found. Run /prompts to list them.\n")),
                }
                return Ok(true);
            }
            if prompts.is_empty() {
                self.display("No prompt files found. Create e.g. .goon/prompts/review.md or ~/.config/goon/prompts/review.md,\nthen run /prompts:review [args] ($ARGUMENTS and $1..$9 are substituted).\n");
            } else {
                let lines: Vec<String> = prompts.iter().map(|p| format!("  /prompts:{} [{}] {}", pad_end(&p.name, 20), p.scope, p.description)).collect();
                self.display(&format!("Custom prompts ({}) — run with /prompts:<name> [args]:\n{}\n", prompts.len(), lines.join("\n")));
            }
            return Ok(true);
        }
        if starts_cmd(input, "/skills") {
            let arg = input[7..].trim();
            let skills = crate::context::discover_skills(&self.runtime().cwd);
            if arg.is_empty() || arg.to_lowercase() == "list" {
                if skills.is_empty() {
                    self.display("No SKILL.md skills found in .agents/skills, .codex/skills, skills/, or ~/.config/goon/skills.\n");
                } else {
                    let lines: Vec<String> = skills.iter().map(|s| format!("  ${} [{}] — {}", pad_end(&s.name, 22), s.scope, s.description)).collect();
                    self.display(&format!("Available Skills ({}) — mention $<name> in any prompt to activate:\n{}\n", skills.len(), lines.join("\n")));
                }
                return Ok(true);
            }
            let clean = arg.trim_start_matches('$').to_lowercase();
            let Some(skill) = skills.iter().find(|s| s.name.to_lowercase() == clean) else {
                self.display(&format!("Skill \"{arg}\" not found. Run /skills to list discovered skills.\n"));
                return Ok(true);
            };
            let content = std::fs::read_to_string(&skill.path)?;
            self.display(&format!("\nSkill ${} ({}):\n{}\n", skill.name, skill.path.display(), jsutil::slice_chars(&content, 0, 4000)));
            return Ok(true);
        }
        if starts_cmd(input, "/memories") || starts_cmd(input, "/memory") {
            let rest = Regex::new(r"^/memor(?:ies|y)\b").unwrap().replace(input, "").trim().to_string();
            let head = rest.split_whitespace().next().unwrap_or("").to_lowercase();
            if head == "rag" || head == "budget" || head == "pref" {
                self.run_memory_store_command(&rest).await;
                return Ok(true);
            }
            let mut entries = crate::context::load_memories();
            let lower = rest.to_lowercase();
            if rest.is_empty() || lower == "list" {
                if entries.is_empty() {
                    self.display(&format!("No saved memories ({}).\nUse \"/memories add <note>\" to save a persistent preference across sessions.\n", crate::context::memories_path().display()));
                } else {
                    let lines: Vec<String> = entries.iter().enumerate().map(|(i, m)| format!("  [{}] {} ({})", i + 1, m.text, take_chars(&m.created_at, 10))).collect();
                    self.display(&format!("Saved Memories ({}):\n{}\n", entries.len(), lines.join("\n")));
                }
                return Ok(true);
            }
            if lower == "clear" || lower == "reset" {
                crate::context::save_memories(&[])?;
                self.display("Cleared all saved memories.\n");
                return Ok(true);
            }
            if lower.starts_with("rm ") || lower.starts_with("remove ") || lower.starts_with("del ") {
                let target = Regex::new(r"(?i)^(?:rm|remove|del)\s+").unwrap().replace(&rest, "").trim().to_string();
                match target.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= entries.len() && target.chars().all(|c| c.is_ascii_digit())) {
                    Some(n) => {
                        let removed = entries.remove(n - 1);
                        crate::context::save_memories(&entries)?;
                        self.display(&format!("Removed memory: \"{}\"\n", removed.text));
                    }
                    None => self.display(&format!("Invalid memory index \"{target}\". Use /memories list.\n")),
                }
                return Ok(true);
            }
            let note = if lower.starts_with("add ") { rest[4..].trim().to_string() } else { rest.clone() };
            entries.push(crate::context::MemoryEntry { id: now_millis().to_string(), text: note.clone(), created_at: now_iso() });
            crate::context::save_memories(&entries)?;
            self.display(&format!("Saved memory #{}: \"{note}\"\n", entries.len()));
            return Ok(true);
        }
        if starts_cmd(input, "/presets") || starts_cmd(input, "/preset") {
            let rest = Regex::new(r"^/presets?\b").unwrap().replace(input, "").trim().to_string();
            let mut presets = crate::context::load_presets();
            let lower = rest.to_lowercase();
            if rest.is_empty() || lower == "list" {
                if presets.is_empty() {
                    self.display(&format!("No saved prompt presets ({}).\nUse \"/presets save <name> <prompt>\" and \"/presets run <name>\".\n", crate::context::presets_path().display()));
                } else {
                    let lines: Vec<String> = presets.values().map(|p| format!("  {} — {}", pad_end(&p.name, 18), jsutil::slice_chars(&p.prompt.split_whitespace().collect::<Vec<_>>().join(" "), 0, 80))).collect();
                    self.display(&format!("Saved Presets ({}):\n{}\n", presets.len(), lines.join("\n")));
                }
                return Ok(true);
            }
            if lower.starts_with("save ") || lower.starts_with("add ") {
                let body = Regex::new(r"(?i)^(?:save|add)\s+").unwrap().replace(&rest, "").trim().to_string();
                let Some(sp) = body.find(char::is_whitespace).filter(|i| *i > 0) else {
                    self.display("Usage: /presets save <name> <prompt text>\n");
                    return Ok(true);
                };
                let name = body[..sp].trim().to_string();
                let prompt = body[sp..].trim().to_string();
                presets.insert(name.clone(), crate::context::PresetEntry { name: name.clone(), prompt, description: None, updated_at: now_iso() });
                crate::context::save_presets(&presets)?;
                self.display(&format!("Saved preset \"{name}\". Run it with: /presets run {name}\n"));
                return Ok(true);
            }
            if lower.starts_with("rm ") || lower.starts_with("remove ") {
                let name = Regex::new(r"(?i)^(?:rm|remove)\s+").unwrap().replace(&rest, "").trim().to_string();
                if presets.shift_remove(&name).is_none() {
                    self.display(&format!("Preset \"{name}\" not found.\n"));
                    return Ok(true);
                }
                crate::context::save_presets(&presets)?;
                self.display(&format!("Removed preset \"{name}\".\n"));
                return Ok(true);
            }
            return Ok(false);
        }
        if starts_cmd(input, "/mention") {
            let target = input[8..].trim();
            if target.is_empty() {
                self.display("Usage: /mention <filepath> (or type @ in the composer to autocomplete workspace files)\n");
                return Ok(true);
            }
            let formatted = if target.contains(' ') { format!("@\"{target}\" ") } else { format!("@{target} ") };
            match self.tui() {
                Some(t) => {
                    t.insert_text(&formatted);
                    t.set_notice(Some(&format!("Attached {} into composer.", formatted.trim())));
                }
                None => self.display(&format!("Tip: Include {} in your next prompt to attach the file.\n", formatted.trim())),
            }
            return Ok(true);
        }
        if input == "/debug-config" {
            let runtime = self.runtime();
            let caps = crate::provider::resolve_model_capabilities(&runtime.model, None);
            let config = self.config.lock().unwrap().clone();
            self.display(&format!(
                "\nResolved Configuration & Layers:\n  Config file      : {}\n  Credentials file : {} (keyEnv: {})\n  MCP config file  : {}\n  Memories file    : {}\n  Presets file     : {}\n  Data directory   : {}\n  Workspace (cwd)  : {}\n  Provider & URL   : {} ({})\n  Model & Effort   : {} (effort: {})\n  Capabilities     : ctx={}, maxOut={}, vision={}, defaultEffort={}\n  Approval & View  : mode={}, view={}, showReasoning={}\n  Registered Tools : {} active ({} from MCP)\n",
                config::config_path().display(),
                config::credentials_path().display(),
                runtime.key_env,
                crate::mcp::mcp_config_path().display(),
                crate::context::memories_path().display(),
                crate::context::presets_path().display(),
                runtime.data_dir.display(),
                runtime.cwd.display(),
                runtime.provider,
                runtime.base_url,
                runtime.model,
                runtime.effort.clone().unwrap_or_else(|| "default".into()),
                runtime.context_window,
                runtime.max_output_tokens,
                caps.supports_vision != Some(false),
                caps.default_effort.unwrap_or_else(|| "default".into()),
                self.mode(),
                self.view_mode(),
                config.show_reasoning,
                self.tools().schemas().len(),
                self.mcp_tool_count()
            ));
            return Ok(true);
        }
        if input == "/warnings" {
            let mut warns: Vec<String> = self.manager().map(|m| m.warnings()).unwrap_or_default();
            for st in self.manager().map(|m| m.statuses()).unwrap_or_default() {
                if st.status == "error" {
                    if let Some(e) = st.error {
                        let msg = format!("MCP {}: {e}", st.name);
                        if !warns.contains(&msg) {
                            warns.push(msg);
                        }
                    }
                }
            }
            if warns.is_empty() {
                self.display("No active runtime or MCP warnings.\n");
            } else {
                self.display(&format!("Warnings ({}):\n{}\n", warns.len(), warns.iter().map(|w| format!("  • {w}")).collect::<Vec<_>>().join("\n")));
            }
            return Ok(true);
        }
        if starts_cmd(input, "/auto") || input == "/yolo" {
            let arg = if input == "/yolo" { "on".to_string() } else { input[5..].trim().to_lowercase() };
            let mode = self.mode();
            let next = match arg.as_str() {
                "" | "toggle" => if mode == "auto" { "ask" } else { "auto" },
                "on" | "true" | "1" | "yes" => "auto",
                "off" | "false" | "0" | "ask" => "ask",
                _ => {
                    self.display("Usage: /auto [on | off] (or /yolo)\n");
                    return Ok(true);
                }
            };
            self.apply_permission_mode(next);
            return Ok(true);
        }
        if starts_cmd(input, "/mode") {
            let arg = input[5..].trim().to_lowercase();
            let next = match arg.as_str() {
                "" => {
                    let mode = self.mode();
                    let idx = MODE_ORDER.iter().position(|m| *m == mode).map(|i| i as i64).unwrap_or(-1);
                    MODE_ORDER[((idx + 1) as usize) % MODE_ORDER.len()]
                }
                "ask" => "ask",
                "plan" => "plan",
                "auto" | "auto-approve" | "yolo" => "auto",
                "readonly" | "read-only" => "readonly",
                _ => {
                    self.display("Usage: /mode [ask | auto | plan | readonly]\n");
                    return Ok(true);
                }
            };
            self.apply_permission_mode(next);
            return Ok(true);
        }
        if starts_cmd(input, "/transcript") {
            let arg = input[11..].trim().to_lowercase();
            match self.tui() {
                Some(t) => {
                    let next = match arg.as_str() {
                        "on" | "true" => true,
                        "off" | "false" => false,
                        _ => !t.is_transcript_overlay(),
                    };
                    t.set_transcript_overlay(next);
                    if next {
                        self.st.lock().unwrap().view_mode = "transcript".into();
                        t.set_view_mode("transcript");
                    }
                }
                None => {
                    let next = {
                        let mut st = self.st.lock().unwrap();
                        st.view_mode = if st.view_mode == "transcript" { "normal".into() } else { "transcript".into() };
                        st.view_mode.clone()
                    };
                    self.display(&format!("Transcript mode: {}\n", if next == "transcript" { "ON" } else { "OFF" }));
                }
            }
            return Ok(true);
        }
        if starts_cmd(input, "/view") {
            let arg = input[5..].trim().to_lowercase();
            let current = self.view_mode();
            let next = match arg.as_str() {
                "" => {
                    let order = ["normal", "verbose", "transcript"];
                    let idx = order.iter().position(|m| *m == current).map(|i| i as i64).unwrap_or(-1);
                    order[((idx + 1) as usize) % 3]
                }
                "normal" | "compact" | "default" | "hide" => "normal",
                "verbose" | "all" | "full" | "show" => "verbose",
                "transcript" | "tx" => "transcript",
                _ => {
                    self.display("Usage: /view [normal | verbose | transcript] (or press Ctrl+T for full-screen transcript)\n");
                    return Ok(true);
                }
            };
            self.st.lock().unwrap().view_mode = next.into();
            {
                let mut config = self.config.lock().unwrap();
                config.view_mode = Some(next.into());
                config.show_reasoning = next != "normal";
                config::save_config(&config)?;
            }
            if let Some(t) = self.tui() {
                t.set_view_mode(next);
                if arg == "transcript" {
                    t.set_transcript_overlay(true);
                } else if arg == "normal" || arg == "verbose" {
                    t.set_transcript_overlay(false);
                }
            }
            let desc = match next {
                "transcript" => "transcript (show thinking, uncollapsed tool calls, full outputs & diffs · Ctrl+T full-screen)",
                "verbose" => "verbose (show thinking & extended details)",
                _ => "normal (hide thinking blocks)",
            };
            if self.tui().is_some() {
                self.notice(&format!("View mode: {desc}"));
            } else {
                self.display(&format!("View mode set to: {desc}\n"));
            }
            return Ok(true);
        }
        if let Some(arg) = input.strip_prefix("/effort ") {
            let arg = arg.trim();
            let clean = arg.to_lowercase();
            let is_clear = ["default", "off", "none", "0", "1"].contains(&clean.as_str());
            let parsed = crate::setup::parse_effort_input(arg, None);
            if !is_clear && parsed.is_none() {
                self.display(&format!("Invalid effort \"{arg}\". Choose from: low, medium, high, xhigh, max, default.\n"));
                return Ok(true);
            }
            self.apply_effort(if is_clear { None } else { parsed }).await?;
            return Ok(true);
        }
        if starts_cmd(input, "/todo") {
            let arg = input[5..].trim().to_lowercase();
            let tools = self.tools();
            if arg == "clear" || arg == "reset" {
                tools.set_todos(vec![]);
                store.session.lock().unwrap().todos = Some(vec![]);
                store.save()?;
                if let Some(t) = self.tui() {
                    t.event(&json!({ "type": "todo", "todos": [] }));
                }
                self.display("Cleared todo checklist.\n");
            } else {
                self.display(&format!("\n{}\n", crate::tools::format_todos(&tools.todos())));
            }
            return Ok(true);
        }
        if input == "/diff" {
            let cwd = self.runtime().cwd.clone();
            let cancel = CancellationToken::new();
            let (status, diff, staged) = tokio::join!(
                run_cmd("git", &["status", "--short"], &cwd, &cancel, 10_000),
                run_cmd("git", &["diff", "--no-ext-diff"], &cwd, &cancel, 10_000),
                run_cmd("git", &["diff", "--cached", "--no-ext-diff"], &cwd, &cancel, 10_000)
            );
            match (status, diff, staged) {
                (Ok(status), Ok(diff), Ok(staged)) => {
                    let status_out = status.output.trim().to_string();
                    let diff_out = [staged.output.trim().to_string(), diff.output.trim().to_string()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n");
                    if status_out.is_empty() && diff_out.is_empty() {
                        self.display("Working tree is clean (no uncommitted changes).\n");
                    } else {
                        if !status_out.is_empty() {
                            self.display(&format!("\nGit Status:\n{status_out}\n"));
                        }
                        if !diff_out.is_empty() {
                            self.display(&format!("\n{}\n", jsutil::slice_chars(&diff_out, 0, 16_000)));
                        }
                    }
                }
                (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => self.display(&format!("Failed to run git diff: {}\n", safe_error(&e))),
            }
            return Ok(true);
        }
        if input == "/mcp" || input == "/mcp list" {
            let cfg = crate::mcp::load_mcp_config()?;
            let statuses = self.manager().map(|m| m.statuses()).unwrap_or_default();
            if cfg.servers.is_empty() {
                self.display(&format!("No MCP servers configured in {}.\nTip: Run \"/mcp import-codex\" or \"goon mcp add <name> -- <cmd> [args...]\"\n", crate::mcp::mcp_config_path().display()));
            } else {
                let lines: Vec<String> = cfg
                    .servers
                    .iter()
                    .map(|(name, s)| {
                        let target = match &s.transport {
                            crate::mcp::McpTransport::Stdio { command, args, .. } => {
                                if args.is_empty() {
                                    command.clone()
                                } else {
                                    format!("{command} {}", args.join(" "))
                                }
                            }
                            crate::mcp::McpTransport::Http { url, .. } => url.clone(),
                        };
                        let st = statuses.iter().find(|x| x.name == *name);
                        let badge = if s.disabled {
                            "disabled".to_string()
                        } else {
                            match st {
                                Some(x) if x.status == "ok" => format!("connected ({} tools)", x.tool_count),
                                Some(x) if x.status == "error" => format!("error: {}", x.error.clone().unwrap_or_default()),
                                _ => "configured".into(),
                            }
                        };
                        format!("  {name}  [{}]  ({badge})  {target}", s.transport_name())
                    })
                    .collect();
                self.display(&format!("MCP Servers ({}):\n{}\nActive MCP tools: {}\n", cfg.servers.len(), lines.join("\n"), self.mcp_tool_count()));
            }
            return Ok(true);
        }
        if input == "/subagents" || input == "/agents" || input == "/subagents list" || input.starts_with("/subagents show ") || input.starts_with("/agent show ") {
            let rest = Regex::new(r"^/(?:subagents|agents|agent)\s*").unwrap().replace(input, "").trim().to_string();
            if self.subagent_listing(&rest) {
                return Ok(true);
            }
        }
        if input == "/tools" {
            let tools = self.tools();
            let lines: Vec<String> = tools.exposed_tool_names().iter().map(|n| format!("  {} {n}", if tools.is_read_only_tool(n) { "● [read]" } else { "◆ [exec]" })).collect();
            self.display(&format!("{}\n", lines.join("\n")));
            return Ok(true);
        }
        if input == "/sessions" {
            let runtime = self.runtime();
            let list = SessionStore::list(&runtime.data_dir.join("sessions"))?;
            let current = store.id();
            let body = if list.is_empty() {
                "  No saved sessions.".to_string()
            } else {
                list.iter()
                    .map(|s| {
                        let cur = if s.id == current { " [current]" } else { "" };
                        let label = match (&s.title, &s.preview) {
                            (Some(t), _) => format!(" [{t}]"),
                            (None, Some(p)) => format!(" \"{p}\""),
                            _ => String::new(),
                        };
                        format!("  {}{cur}  ({} msgs)  {}  {}{label}  {}", take_chars(&s.id, 8), s.messages_count.unwrap_or(0), s.updated_at, s.model, s.cwd)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            self.display(&format!("{body}\n"));
            return Ok(true);
        }
        if input == "/doctor" {
            let runtime = self.runtime();
            let cancel = CancellationToken::new();
            let (b, r, discovered) = tokio::join!(
                async { matches!(run_cmd("bash", &["--version"], &runtime.cwd, &cancel, 5000).await, Ok(x) if x.code == 0) },
                async { matches!(run_cmd("rg", &["--version"], &runtime.cwd, &cancel, 5000).await, Ok(x) if x.code == 0) },
                crate::provider::discover_models(&runtime.base_url, Some(&runtime.api_key), &runtime.provider, None)
            );
            let in_list = discovered.iter().any(|m| m.id == runtime.model);
            let gateway = if discovered.is_empty() {
                "Unreachable or no /models response".to_string()
            } else {
                format!("OK ({} models: {} · active model {})", discovered.len(), discovered.iter().map(|m| m.id.clone()).collect::<Vec<_>>().join(", "), if in_list { "verified" } else { "NOT listed" })
            };
            self.display(&format!(
                "Provider: {} | Model: {} | Effort: {} | BaseURL: {}\nBash: {} | Ripgrep: {} | API Key: Configured\nGateway (/models): {gateway}\n",
                runtime.provider,
                runtime.model,
                runtime.effort.clone().unwrap_or_else(|| "default".into()),
                runtime.base_url,
                if b { "OK" } else { "Missing" },
                if r { "OK" } else { "JS fallback" }
            ));
            return Ok(true);
        }
        if input == "/status" {
            let runtime = self.runtime();
            let s = store.session.lock().unwrap().clone();
            let used = crate::context::estimated_tokens(&s.messages, "");
            let pct = ((used as f64 / runtime.context_window as f64) * 100.0).round().min(100.0) as u64;
            let goal_line = s.goal.as_ref().map(|g| format!("Goal           [{}] {} ({})\n", goal_status_label(&g.status), g.objective, format_goal_usage_summary(g))).unwrap_or_default();
            let cost_cap = if runtime.max_cost_usd > 0.0 { format_usd(runtime.max_cost_usd) } else { "unlimited".into() };
            let time_cap = if runtime.max_time_ms > 0 { format_duration_ms(runtime.max_time_ms as f64) } else { "unlimited".into() };
            let token_cap = if runtime.run_token_budget > 0 { runtime.run_token_budget.to_string() } else { "unlimited".into() };
            let mode = self.mode();
            self.display(&format!(
                "Session        {}{}\n{goal_line}Provider       {} ({})\nModel          {}\nEffort         {}\nMode           {}\nView           {}\nWorkspace      {}\nCheckpoints    {} turn(s)\nContext usage  ~{used} / {} tokens ({pct}%)\nCompactions    {}\nSession tokens {}\nCost           {} (cap {cost_cap})\nTime cap       {time_cap}\nToken cap      {token_cap}\nMCP tools      {}\n",
                s.id,
                s.title.as_ref().map(|t| format!(" ({t})")).unwrap_or_default(),
                runtime.provider,
                runtime.base_url,
                runtime.model,
                runtime.effort.clone().unwrap_or_else(|| "default".into()),
                if mode == "auto" { "auto-approve".to_string() } else { mode },
                self.view_mode(),
                runtime.cwd.display(),
                s.checkpoints.as_ref().map(|c| c.len()).unwrap_or(0),
                runtime.context_window,
                s.compactions,
                jsutil::js_string(&jsutil::num(s.tokens)),
                format_usd(s.cost()),
                self.mcp_tool_count()
            ));
            return Ok(true);
        }
        if input == "/cost" {
            let runtime = self.runtime();
            let harness = self.harness();
            let ledger = harness.cost_ledger.clone();
            let price = ledger.price_for(Some(&runtime.model)).map(|p| format!("input {}/1M, output {}/1M", format_usd(p.input_per_1m), format_usd(p.output_per_1m)));
            let budget = self.tools().budget();
            let guard = budget.as_ref().map(|b| b.lock().unwrap());
            let cost = store.session.lock().unwrap().cost();
            let described = CostLedger::describe(guard.as_deref(), cost);
            drop(guard);
            self.display(&format!(
                "{}\nPricing        {}\nCost cap       {}\nTime cap       {}\nToken cap      {}\n",
                described,
                price.unwrap_or_else(|| "missing (cost tracking inactive)".into()),
                if runtime.max_cost_usd > 0.0 { format_usd(runtime.max_cost_usd) } else { "unlimited".into() },
                if runtime.max_time_ms > 0 { format_duration_ms(runtime.max_time_ms as f64) } else { "unlimited".into() },
                if runtime.run_token_budget > 0 { format!("{} tokens", runtime.run_token_budget) } else { "unlimited".into() }
            ));
            return Ok(true);
        }
        if let Some(title) = input.strip_prefix("/rename ") {
            let title = title.trim();
            if !title.is_empty() {
                store.session.lock().unwrap().title = Some(title.to_string());
                store.save()?;
                if let Some(t) = self.tui() {
                    t.set_title(&format!("{} · {title}", self.runtime().model));
                }
                self.display(&format!("Session renamed to: {title}\n"));
            }
            return Ok(true);
        }
        if starts_cmd(input, "/export") {
            let s = store.session.lock().unwrap().clone();
            let out_name = input[7..].trim().to_string();
            let out_name = if out_name.is_empty() { format!("goon-session-{}.md", take_chars(&s.id, 8)) } else { out_name };
            let target = self.tools().files.path(&out_name)?;
            let runtime = self.runtime();
            let mut lines = vec![format!("# Goon Session {}", s.title.clone().unwrap_or_else(|| s.id.clone())), format!("- **Model**: {} ({})", runtime.model, runtime.provider), format!("- **Updated**: {}", s.updated_at), String::new()];
            for m in &s.messages {
                lines.push(format!("## {}", m.get("role").and_then(Value::as_str).unwrap_or("").to_uppercase()));
                lines.push(match m.get("content") {
                    Some(Value::String(c)) => c.clone(),
                    Some(other) => stringify_pretty(other),
                    None => String::new(),
                });
                lines.push(String::new());
            }
            std::fs::write(&target, lines.join("\n"))?;
            self.display(&format!("Exported session transcript to {out_name}\n"));
            return Ok(true);
        }
        if starts_cmd(input, "/fork") {
            let title = input[5..].trim();
            let forked = store.fork_to_new_file((!title.is_empty()).then_some(title))?;
            let count = store.session.lock().unwrap().messages.len();
            self.display(&format!("Forked current session ({count} messages) -> new session ID: {forked}\nSwitch to it now with: /resume {} (or goon --resume {})\n", take_chars(&forked, 8), take_chars(&forked, 8)));
            return Ok(true);
        }
        if starts_cmd(input, "/pair") {
            return self.pair_command(input[5..].trim()).await;
        }
        if starts_cmd(input, "/workflow") || starts_cmd(input, "/workflows") {
            return self.workflow_immediate(input);
        }
        if starts_cmd(input, "/network") || starts_cmd(input, "/brainstorm") || starts_cmd(input, "/council") {
            let explicit = Regex::new(r"(?i)^/(?:brainstorm|council)\b").unwrap().is_match(input);
            let rest = Regex::new(r"^/(?:network|brainstorm|council)\b").unwrap().replace(input, "").trim().to_string();
            let lower = rest.to_lowercase();
            if rest.is_empty() || (!explicit && ["status", "board", "list", "messages"].contains(&lower.as_str())) {
                let Some(subagents) = self.subagents() else {
                    self.display("Agent Network is disabled (--no-subagents).\n");
                    return Ok(true);
                };
                self.display(&format!(
                    "\n╭─── Agent Network & Peer-to-Peer Bus ────────────────────────────╮\n{}\n╰─────────────────────────────────────────────────────────────────╯\n  Tip: Run \"/network <topic>\" (or \"/brainstorm <topic>\") to convene a multi-agent ideation session.\n",
                    subagents.bus.format_board_summary(20)
                ));
                return Ok(true);
            }
            if !explicit && (lower == "clear" || lower == "reset") {
                if let Some(s) = self.subagents() {
                    s.bus.clear();
                }
                self.display("Cleared Agent Network blackboard and peer message history.\n");
                return Ok(true);
            }
            if !explicit && lower.starts_with("send ") {
                let body = rest[5..].trim().to_string();
                let sp = body.find(char::is_whitespace).filter(|i| *i > 0);
                let (Some(sp), Some(subagents)) = (sp, self.subagents()) else {
                    self.display("Usage: /network send <agent_nickname_or_*> <message>\n");
                    return Ok(true);
                };
                let to = body[..sp].trim();
                let msg = body[sp..].trim();
                let sent = subagents.bus.send("User", to, msg, Some("message"), None);
                self.display(&format!("Sent peer message [{}] User → {}: {}\n", sent.id, sent.to, sent.content));
                return Ok(true);
            }
            return Ok(false);
        }
        if starts_cmd(input, "/dag") {
            return self.dag_command(input[4..].trim()).await;
        }
        if let Some(raw_shell) = input.strip_prefix('!') {
            return self.shell_command(raw_shell.trim()).await;
        }
        Ok(false)
    }

    fn subagent_listing(&self, rest: &str) -> bool {
        if rest.is_empty() || rest == "list" {
            let list = self.subagents().map(|s| s.list_subagent_summaries()).unwrap_or_default();
            if list.is_empty() {
                self.display("  No subagents spawned in this session yet.\n  Tip: Use \"/agent <task>\" or ask the agent to spawn subagents (spawn_agent / delegate_task).\n");
            } else {
                let lines: Vec<String> = list
                    .iter()
                    .map(|s| format!("  {} ({})  [{}]  {}  ({} msgs)  {}\n    Task: {}{}", s.nickname, s.id, s.role, s.status, s.messages_count, s.updated_at, s.task, s.summary.as_ref().map(|x| format!("\n    Summary: {x}")).unwrap_or_default()))
                    .collect();
                self.display(&format!("Subagents ({}):\n{}\n", list.len(), lines.join("\n")));
            }
            return true;
        }
        if let Some(target) = rest.strip_prefix("show ") {
            let target = target.trim();
            let transcript = self.subagents().and_then(|s| s.read_subagent_transcript(target));
            self.display(&format!("{}\n", transcript.unwrap_or_else(|| format!("Subagent \"{target}\" not found."))));
            return true;
        }
        false
    }

    async fn goal_command(&self, arg: &str) -> Result<bool> {
        let lower = arg.to_lowercase();
        let now = now_iso();
        let store = self.store();
        let harness = self.harness();
        let tokens = store.session.lock().unwrap().tokens;
        let busy = self.controller().is_some();
        if arg.is_empty() {
            match store.session.lock().unwrap().goal.clone() {
                None => self.display(&format!("No active session goal.\n{GOAL_USAGE}\n")),
                Some(g) => self.display(&format!("\n{}\n", format_goal_summary_lines(&g, tokens))),
            }
            return Ok(true);
        }
        if lower == "clear" || lower == "off" || lower == "none" {
            if store.session.lock().unwrap().goal.take().is_none() {
                self.display("No active session goal.\n");
                return Ok(true);
            }
            harness.add_context_message(goal_context(true, None, None));
            store.save()?;
            self.sync_tui_header(false);
            self.notice("Goal cleared.");
            self.display("Goal cleared.\n");
            return Ok(true);
        }
        if ["pause", "resume", "complete", "done"].contains(&lower.as_str()) {
            let next_status = match lower.as_str() {
                "pause" => "paused",
                "resume" => "active",
                _ => "complete",
            };
            let goal = {
                let mut s = store.session.lock().unwrap();
                let Some(g) = s.goal.as_mut() else {
                    drop(s);
                    self.display(&format!("No session goal set yet.\n{GOAL_USAGE}\n"));
                    return Ok(true);
                };
                if next_status == "active" && g.status == "budget_limited" && g.token_budget.map(|b| g.tokens_used >= b).unwrap_or(false) {
                    g.token_budget = None;
                }
                g.status = next_status.into();
                g.updated_at = now.clone();
                g.clone()
            };
            if next_status == "active" {
                self.st.lock().unwrap().goal_deferred = false;
            }
            harness.add_context_message(goal_context(false, None, Some(next_status)));
            store.save()?;
            self.sync_tui_header(false);
            self.notice(&format_goal_status_indicator(&goal, 0.0));
            self.display(&format!("\n{}\n", format_goal_summary_lines(&goal, tokens)));
            if next_status == "active" && !busy {
                if self.mode() == "plan" {
                    self.display("Note: Plan mode is active. Switch to ask or auto mode (/mode ask or /auto) to pursue the goal autonomously.\n");
                } else {
                    self.continue_goal_if_idle().await?;
                }
            }
            return Ok(true);
        }
        if lower == "edit" || lower.starts_with("edit ") {
            if store.session.lock().unwrap().goal.is_none() {
                self.display(&format!("No session goal set yet.\n{GOAL_USAGE}\n"));
                return Ok(true);
            }
            let edit_rest = arg[4..].trim();
            if edit_rest.is_empty() {
                if busy {
                    self.display("Usage while working: /goal edit <objective>\n");
                    return Ok(true);
                }
                return Ok(false);
            }
            let (objective, budget) = match parse_goal_draft_args(edit_rest) {
                GoalDraft::Error(e) => {
                    self.display(&format!("{e}\n"));
                    return Ok(true);
                }
                GoalDraft::Ok { objective, token_budget } => ((!objective.is_empty()).then_some(objective), token_budget),
            };
            let goal = {
                let mut s = store.session.lock().unwrap();
                let g = s.goal.as_mut().unwrap();
                if let Some(b) = budget {
                    g.token_budget = Some(b);
                    if g.status == "budget_limited" && g.tokens_used < b {
                        g.status = "active".into();
                    }
                }
                if let Some(o) = objective.as_ref().filter(|o| !o.is_empty()) {
                    g.objective = o.clone();
                }
                g.updated_at = now.clone();
                g.clone()
            };
            let has_objective = objective.as_ref().map(|o| !o.is_empty()).unwrap_or(false);
            harness.add_context_message(goal_context(false, has_objective.then_some(goal.objective.as_str()), Some(&goal.status)));
            if has_objective {
                harness.add_context_message(json!({ "role": "user", "content": crate::context::build_goal_objective_updated_prompt(&goal) }));
            }
            store.save()?;
            self.sync_tui_header(false);
            self.notice(&format_goal_status_indicator(&goal, 0.0));
            self.display(&format!("\n{}\n", format_goal_summary_lines(&goal, tokens)));
            if goal.status == "active" && !busy && self.mode() != "plan" {
                self.st.lock().unwrap().goal_deferred = false;
                self.continue_goal_if_idle().await?;
            }
            return Ok(true);
        }
        let (objective, budget) = match parse_goal_draft_args(arg) {
            GoalDraft::Error(e) => {
                self.display(&format!("{e}\n"));
                return Ok(true);
            }
            GoalDraft::Ok { objective, token_budget } => ((!objective.is_empty()).then_some(objective), token_budget),
        };
        let Some(objective) = objective else {
            let existing = store.session.lock().unwrap().goal.is_some();
            if let (Some(b), true) = (budget, existing) {
                let goal = {
                    let mut s = store.session.lock().unwrap();
                    let g = s.goal.as_mut().unwrap();
                    g.token_budget = Some(b);
                    if g.status == "budget_limited" && g.tokens_used < b {
                        g.status = "active".into();
                    }
                    g.updated_at = now.clone();
                    g.clone()
                };
                store.save()?;
                self.sync_tui_header(false);
                self.notice(&format_goal_status_indicator(&goal, 0.0));
                self.display(&format!("\n{}\n", format_goal_summary_lines(&goal, tokens)));
                if goal.status == "active" && !busy && self.mode() != "plan" {
                    self.st.lock().unwrap().goal_deferred = false;
                    self.continue_goal_if_idle().await?;
                }
                return Ok(true);
            }
            self.display(&format!("{GOAL_USAGE}\n"));
            return Ok(true);
        };
        let had_existing = store.session.lock().unwrap().goal.is_some();
        let goal = SessionGoal {
            goal_id: Some(format!("goal_{}", radix36(now_millis() as u64))),
            objective: objective.clone(),
            status: "active".into(),
            created_at: now.clone(),
            updated_at: now.clone(),
            tokens_at_start: tokens,
            tokens_used: 0.0,
            time_used_seconds: 0.0,
            token_budget: budget,
        };
        store.session.lock().unwrap().goal = Some(goal.clone());
        self.st.lock().unwrap().goal_deferred = false;
        harness.add_context_message(goal_context(false, Some(&objective), Some("active")));
        if busy && had_existing {
            harness.add_context_message(json!({ "role": "user", "content": crate::context::build_goal_objective_updated_prompt(&goal) }));
        }
        store.save()?;
        self.sync_tui_header(false);
        self.notice(&format_goal_status_indicator(&goal, 0.0));
        self.display(&format!("\n{}\n", format_goal_summary_lines(&goal, tokens)));
        if self.mode() == "plan" {
            self.display("Note: Plan mode is active. Switch to ask or auto mode (/mode ask or /auto) to pursue the goal autonomously.\n");
        } else if !busy {
            self.continue_goal_if_idle().await?;
        }
        Ok(true)
    }

    async fn pair_command(&self, sub: &str) -> Result<bool> {
        let sub = sub.to_lowercase();
        if sub == "stop" || sub == "off" || sub == "close" {
            match self.pair().filter(|p| p.is_running()) {
                None => self.display("Device pairing is not currently active.\n"),
                Some(p) => {
                    p.stop().await;
                    self.notice("Stopped Cloudflared tunnel & device pairing.");
                    self.display("Stopped device pairing server and Cloudflared tunnel.\n");
                }
            }
            return Ok(true);
        }
        if sub == "status" {
            match self.pair().filter(|p| p.is_running()) {
                None => self.display("Device pairing is inactive. Run \"/pair\" to start a free Cloudflared tunnel + QR code.\n"),
                Some(p) => self.display(&format!("\n{}\n", p.format_banner())),
            }
            return Ok(true);
        }
        let pm = self.ensure_pair_manager();
        let skip_tunnel = sub == "lan" || sub == "local";
        if pm.is_running() && skip_tunnel && pm.listening_host() != "0.0.0.0" {
            pm.stop().await;
        }
        if !pm.is_running() {
            match self.tui() {
                Some(t) => {
                    t.set_notice(Some(if skip_tunnel { "Starting local/LAN pair server..." } else { "Starting Cloudflared tunnel & waiting for DNS..." }));
                    t.with_status(if skip_tunnel { "Starting LAN pair" } else { "Starting Cloudflared tunnel" }, pm.start(skip_tunnel)).await?;
                }
                None => {
                    self.display(if skip_tunnel { "Starting local/LAN pair server...\n" } else { "Starting Cloudflared tunnel & waiting for DNS...\n" });
                    pm.start(skip_tunnel).await?;
                }
            }
        }
        self.display(&format!("\n{}\n", pm.format_banner()));
        Ok(true)
    }

    fn workflow_immediate(&self, input: &str) -> Result<bool> {
        use crate::workflow::{create_workflow_scaffold, format_workflow_details, remove_custom_workflow};
        let rest = Regex::new(r"^/workflows?\b").unwrap().replace(input, "").trim().to_string();
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        let sub = tokens.first().map(|t| t.to_lowercase()).unwrap_or_else(|| "list".into());
        let cwd = self.runtime().cwd.clone();
        if rest.is_empty() || sub == "list" {
            let all = discover_workflows(&cwd);
            let lines: Vec<String> = all.iter().map(|w| format!("  {} [{}] ({} steps) — {}", pad_end(&w.name, 20), pad_end(&w.scope, 9), w.steps.len(), w.description)).collect();
            self.display(&format!("\nAvailable Workflows ({}) — run with \"/workflow run <name> [input]\":\n{}\n  Tip: Create a custom workflow with \"/workflow create <name> [description]\"\n", all.len(), lines.join("\n")));
            return Ok(true);
        }
        if ["show", "get", "info"].contains(&sub.as_str()) {
            let Some(name) = tokens.get(1) else {
                self.display("Usage: /workflow show <name>\n");
                return Ok(true);
            };
            match discover_workflows(&cwd).iter().find(|w| w.name.to_lowercase() == name.to_lowercase()) {
                Some(found) => self.display(&format!("\n{}\n", format_workflow_details(found))),
                None => self.display(&format!("Workflow \"{name}\" not found. Run \"/workflow list\" to see available workflows.\n")),
            }
            return Ok(true);
        }
        if ["create", "init", "new"].contains(&sub.as_str()) {
            let Some(name) = tokens.get(1) else {
                self.display("Usage: /workflow create <name> [description]\n");
                return Ok(true);
            };
            let desc = tokens[2..].join(" ").trim().to_string();
            match create_workflow_scaffold(&cwd, name, (!desc.is_empty()).then_some(desc.as_str()), false, false) {
                Ok((path, wf)) => self.display(&format!("Created custom workflow \"{}\" at {}\nEdit the JSON file to customize steps, or run it now with: /workflow run {} <your task>\n", wf.name, path.display(), wf.name)),
                Err(error) => self.display(&format!("Failed to create workflow: {}\n", safe_error(&error))),
            }
            return Ok(true);
        }
        if ["rm", "remove", "delete"].contains(&sub.as_str()) {
            let Some(name) = tokens.get(1) else {
                self.display("Usage: /workflow rm <name>\n");
                return Ok(true);
            };
            match remove_custom_workflow(&cwd, name) {
                Ok(path) => self.display(&format!("Removed custom workflow \"{name}\" ({}).\n", path.display())),
                Err(error) => self.display(&format!("Failed to remove workflow: {}\n", safe_error(&error))),
            }
            return Ok(true);
        }
        Ok(false)
    }

    async fn dag_command(&self, goal: &str) -> Result<bool> {
        if goal.is_empty() {
            self.display("Usage: /dag <goal> — decompose a goal into a role-based todo DAG (planner/coder/reviewer/tester/researcher) and execute it with subagents. Writes run serially with per-todo rollback and a final verification gate.\n");
            return Ok(true);
        }
        let runtime = self.runtime();
        if runtime.multi_agent_mode == "off" {
            self.display("/dag is disabled: set \"multiAgentMode\" to \"manual\" or \"auto\" in the goon config.\n");
            return Ok(true);
        }
        let tools = self.tools();
        if tools.effective_read_only() {
            self.display("/dag needs write access for coder todos; leave read-only/plan mode first.\n");
            return Ok(true);
        }
        if self.subagents().is_none() {
            self.display("/dag requires subagents; restart without --no-subagents.\n");
            return Ok(true);
        }
        if self.controller().is_some() || self.harness().busy() {
            self.display("/dag can only run while the agent is idle.\n");
            return Ok(true);
        }
        let token = CancellationToken::new();
        self.set_controller(Some(token.clone()));
        // Shared DAG budget seeded from the parent session/ledger so every spawn and the planner
        // account against the same caps.
        let previous = tools.budget();
        let cost_spent = self.store().session.lock().unwrap().cost();
        let budget: SharedBudget = Arc::new(Mutex::new(RunBudget { spent: 0, limit: if runtime.run_token_budget > 0 { runtime.run_token_budget } else { DEFAULT_DAG_TOKEN_BUDGET }, cost_spent, cost_limit: 0.0, started_at: None, time_limit_ms: 0, ledger: tools.cost_ledger() }));
        tools.set_budget(Some(budget.clone()));
        if let Some(t) = self.tui() {
            t.set_status("Running");
            t.mark_turn_start();
            t.write(&format!("\n> /dag {goal}\n"));
        } else if !self.json {
            self.display(&format!("\n[dag] {goal}\n"));
        }
        match self.run_dag_core(goal, budget, &token).await {
            Ok(summary) => self.display(&format!("\n{summary}\n")),
            Err(error) => self.display(&format!("\n[dag failed] {}\n", safe_error(&error))),
        }
        tools.set_budget(previous);
        self.set_controller(None);
        if let Some(t) = self.tui() {
            t.set_status("Ready");
        }
        Ok(true)
    }

    async fn shell_command(&self, raw: &str) -> Result<bool> {
        if raw.is_empty() {
            self.display("Usage: !<command> (runs a shell command directly in workspace and records output in session context)\n");
            return Ok(true);
        }
        let cmd = crate::tools::sanitize_bash_command(raw);
        self.display(&format!("\n$ {cmd}\n"));
        // When idle, Ctrl+C cancels this command instead of quitting goon.
        let token = CancellationToken::new();
        let owns = self.controller().is_none();
        if owns {
            self.set_controller(Some(token.clone()));
        }
        let cwd = self.runtime().cwd.clone();
        match run_cmd("bash", &["-c", &cmd], &cwd, &token, 600_000).await {
            Ok(res) => {
                self.display(&format!("{}\n[Exit code: {}]\n", if res.output.is_empty() { "(no output)" } else { &res.output }, res.code));
                self.harness().add_context_message(json!({ "role": "user", "content": format!("<user_shell_command>\n<command>{cmd}</command>\n<result>\nExit code: {}\n{}\n</result>\n</user_shell_command>", res.code, jsutil::slice_chars(&res.output, 0, 8000)) }));
                self.store().save()?;
            }
            Err(error) => self.display(&format!("Shell command failed: {}\n", safe_error(&error))),
        }
        if owns {
            self.set_controller(None);
        }
        Ok(true)
    }

    // ------------------------------------------------------------------ main loop
    pub async fn main_loop(&self, mut initial: Option<String>) -> Result<()> {
        loop {
            if self.exiting() {
                break;
            }
            let mut input = match initial.take() {
                Some(cmd) => {
                    if !crate::term::stdin_is_tty() && self.tui().is_none() && !self.has_rl() {
                        self.st.lock().unwrap().exiting = true;
                    }
                    cmd
                }
                None => {
                    let label = if self.tui().is_some() { "goon> " } else { "\ngoon> " };
                    match self.question(label, None, true).await {
                        Ok(line) => line.trim().to_string(),
                        Err(_) => break,
                    }
                }
            };
            if input.is_empty() {
                continue;
            }
            self.apply_pending_rebuild().await?;
            if self.execute_immediate_command(&input).await? {
                if self.exiting() {
                    break;
                }
                continue;
            }
            if input.to_lowercase() == "/goal edit" {
                let Some(goal) = self.store().session.lock().unwrap().goal.clone() else {
                    self.display(&format!("No session goal set yet.\n{GOAL_USAGE}\n"));
                    continue;
                };
                let block = format!("\n◇ Edit Goal Objective\n  Current: {}\n", goal.objective);
                let next = self.ask_block(&block, "New objective (Enter to cancel): ", None, QuestionOpts { esc_hint: Some("cancel".into()), ..Default::default() }).await.unwrap_or_default().trim().to_string();
                if next.is_empty() {
                    if self.tui().is_some() {
                        self.notice("Cancelled /goal edit.");
                    } else {
                        self.display("Cancelled /goal edit.\n");
                    }
                    continue;
                }
                self.execute_immediate_command(&format!("/goal edit {next}")).await?;
                continue;
            }
            if input == "/key" {
                let runtime = self.runtime();
                let entered = match self.tui() {
                    Some(t) => t.question(&format!("New API key for {} (hidden, Esc to cancel): ", runtime.key_env), None, false, QuestionOpts { esc_cancels: true, mask: true, ..Default::default() }).await.unwrap_or_default(),
                    None => self.question_hidden(&format!("New API key for {} (hidden): ", runtime.key_env)).await.unwrap_or_default(),
                };
                let entered = entered.trim().to_string();
                if entered.is_empty() {
                    self.display("API key unchanged.\n");
                    continue;
                }
                config::save_credential(&runtime.key_env, &entered)?;
                if std::env::var(&runtime.key_env).map(|v| !v.trim().is_empty()).unwrap_or(false) {
                    self.display(&format!("Saved to credentials.json, but {} is set in the environment and takes precedence.\n", runtime.key_env));
                }
                self.resolve_runtime_here()?;
                self.rebuild_harness(false).await?;
                let chars: Vec<char> = entered.chars().collect();
                let head: String = chars.iter().take(4).collect();
                let tail: String = chars[chars.len().saturating_sub(2)..].iter().collect();
                self.display(&format!("API key for {} updated ({head}…{tail}).\n", self.runtime().key_env));
                continue;
            }
            if input.starts_with("/prompts:") {
                let re = Regex::new(r"(?s)^/prompts:([A-Za-z0-9][A-Za-z0-9_.-]*)\s*(.*)$").unwrap();
                let caps = re.captures(&input);
                let prompts = crate::prompts::discover_prompts(&self.runtime().cwd);
                let found = caps.as_ref().and_then(|c| prompts.iter().find(|p| p.name.to_lowercase() == c[1].to_lowercase()));
                let Some(found) = found else {
                    let name = caps.as_ref().map(|c| c[1].to_string()).unwrap_or_else(|| input[9..].to_string());
                    self.display(&format!("Prompt \"{name}\" not found. Run /prompts to list them.\n"));
                    continue;
                };
                let args = caps.as_ref().map(|c| c[2].to_string()).unwrap_or_default();
                self.run(&crate::prompts::expand_prompt_template(&found.body, &args)).await?;
                continue;
            }
            if input == "/setup" || input == "/config" || input == "/provider" {
                if !self.setup_command().await? {
                    continue;
                }
                continue;
            }
            if starts_cmd(&input, "/model") {
                self.model_command(input[6..].trim()).await?;
                continue;
            }
            if starts_cmd(&input, "/effort") {
                let mut arg = input[7..].trim().to_string();
                let runtime = self.runtime();
                if arg.is_empty() {
                    self.display(&format!("\nReasoning Effort (current: {}):\n", runtime.effort.clone().unwrap_or_else(|| "default".into())));
                    self.display(&format!("  {} [1] default  — Use provider/model default\n", if runtime.effort.is_none() { '●' } else { ' ' }));
                    for (idx, level) in EFFORT_LEVELS.iter().enumerate() {
                        self.display(&format!("  {} [{}] {} — {}\n", if runtime.effort.as_deref() == Some(*level) { '●' } else { ' ' }, idx + 2, pad_end(level, 8), crate::setup::effort_description(level)));
                    }
                    arg = self.question(&format!("Select [1-{}] or effort name: ", EFFORT_LEVELS.len() + 1), None, false).await.unwrap_or_default().trim().to_string();
                    if arg.is_empty() {
                        continue;
                    }
                }
                let clean = arg.to_lowercase();
                let is_clear = ["default", "off", "none", "0", "1"].contains(&clean.as_str());
                let parsed = crate::setup::parse_effort_input(&arg, None);
                if !is_clear && parsed.is_none() {
                    self.display(&format!("Invalid effort \"{arg}\". Choose from: low, medium, high, xhigh, max, default.\n"));
                    continue;
                }
                let mut changes = Map::new();
                changes.insert("effort".into(), if is_clear { Value::Null } else { json!(parsed) });
                self.update_config(changes, true)?;
                self.resolve_runtime_here()?;
                let effort = self.runtime().effort.clone();
                if let Some(t) = self.tui() {
                    t.set_effort(effort.as_deref());
                }
                self.sync_tui_header(false);
                self.rebuild_harness(false).await?;
                self.display(&format!("Reasoning effort set to: {}\n", effort.unwrap_or_else(|| "default".into())));
                continue;
            }
            if starts_cmd(&input, "/plan") {
                self.plan_command(input[5..].trim()).await?;
                continue;
            }
            if ["/checkpoints", "/revert list", "/undo list", "/revert pick", "/undo pick"].contains(&input.as_str()) {
                let checkpoints = self.store().session.lock().unwrap().checkpoints.clone().unwrap_or_default();
                if checkpoints.is_empty() {
                    if self.tui().is_some() {
                        self.notice("↺ No previous turns to rewind in this session.");
                    } else {
                        self.display("No turn checkpoints recorded in this session yet.\n");
                    }
                    continue;
                }
                let Some(t) = self.tui() else {
                    self.display(&format!("\nTurn Checkpoints ({}):\n", checkpoints.len()));
                    for cp in &checkpoints {
                        let files = if cp.files.is_empty() { "no file changes".to_string() } else { format!("files: {}", cp.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>().join(", ")) };
                        self.display(&format!("  #{} [{}] \"{}\" ({files})\n", cp.id, jsutil::slice_chars(&cp.created_at, 11, 19), jsutil::slice_chars(&cp.prompt, 0, 60)));
                    }
                    continue;
                };
                let reversed: Vec<_> = checkpoints.iter().rev().collect();
                let rows: Vec<String> = reversed
                    .iter()
                    .take(15)
                    .enumerate()
                    .map(|(idx, cp)| {
                        let files = if cp.files.is_empty() { "no file changes".to_string() } else { format!("{} file(s): {}", cp.files.len(), cp.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>().join(", ")) };
                        let preview = jsutil::slice_chars(&cp.prompt.split_whitespace().collect::<Vec<_>>().join(" "), 0, 54).to_string();
                        format!("  {}. #{} [{}] \"{preview}\" ({files})", pad_start(&(idx + 1).to_string(), 2), cp.id, jsutil::slice_chars(&cp.created_at, 11, 19))
                    })
                    .collect();
                let block = format!("\n◇ Rewind to turn checkpoint · {} available\n{}\n", checkpoints.len(), rows.join("\n"));
                let choice = t.ask_transient(&block, &format!("Rewind [1-{}, or Enter to cancel]: ", reversed.len().min(15)), None, QuestionOpts { esc_hint: Some("cancel".into()), ..Default::default() }).await.unwrap_or_default().trim().to_string();
                match choice.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= reversed.len() && choice.chars().all(|c| c.is_ascii_digit())) {
                    Some(n) => input = format!("/revert {n}"),
                    None => continue,
                }
            }
            if starts_cmd(&input, "/revert") || starts_cmd(&input, "/undo") {
                self.revert_command(&input).await;
                continue;
            }
            if starts_cmd(&input, "/new") {
                self.new_session_command(input[4..].trim()).await?;
                continue;
            }
            if starts_cmd(&input, "/resume") {
                self.resume_command(input[7..].trim()).await?;
                continue;
            }
            if starts_cmd(&input, "/compact") {
                let focus = input[8..].trim().to_string();
                let token = CancellationToken::new();
                self.set_controller(Some(token.clone()));
                if let Some(t) = self.tui() {
                    t.set_status("Compacting");
                }
                match self.harness().compact(&token, (!focus.is_empty()).then_some(focus.as_str())).await {
                    Ok(res) => self.display(&format!("Context compacted (compaction #{}, {} tokens used).\n", res.compactions, res.tokens)),
                    Err(error) => self.display(&format!("{}\n", safe_error(&error))),
                }
                self.set_controller(None);
                if let Some(t) = self.tui() {
                    t.set_status("Ready");
                }
                continue;
            }
            if input == "/recap" {
                let token = CancellationToken::new();
                self.set_controller(Some(token.clone()));
                if let Some(t) = self.tui() {
                    t.set_status("Running");
                }
                match self.harness().recap(&token).await {
                    Ok(res) => self.display(&format!("\n◇ Session recap\n{}\n", res.text.split('\n').map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"))),
                    Err(error) => self.display(&format!("Recap failed: {}\n", safe_error(&error))),
                }
                self.set_controller(None);
                if let Some(t) = self.tui() {
                    t.set_status("Ready");
                }
                continue;
            }
            if starts_cmd(&input, "/cd") {
                self.cd_command(input[3..].trim()).await;
                continue;
            }
            if starts_cmd(&input, "/delete") {
                self.delete_command(input[7..].trim()).await?;
                continue;
            }
            if input.starts_with("/presets ") || input.starts_with("/preset ") {
                let rest = Regex::new(r"^/presets?\s+").unwrap().replace(&input, "").trim().to_string();
                let name = Regex::new(r"(?i)^run\s+").unwrap().replace(&rest, "").trim().to_string();
                let presets = crate::context::load_presets();
                let Some(preset) = presets.get(&name) else {
                    self.display(&format!("Preset \"{name}\" not found. Use \"/presets list\" or \"/presets save <name> <prompt>\".\n"));
                    continue;
                };
                self.display(&format!("Running preset \"{}\":\n", preset.name));
                self.run(&preset.prompt.clone()).await?;
                continue;
            }
            if input.starts_with("/workflow ") || input.starts_with("/workflows ") {
                self.workflow_run_command(&input).await;
                continue;
            }
            if input.starts_with("/network ") || input.starts_with("/brainstorm ") || input.starts_with("/council ") {
                self.network_command(&input).await?;
                continue;
            }
            if starts_cmd(&input, "/review") {
                let extra = input[7..].trim();
                let branch = Regex::new(r"(?is)^branch\s+(\S+)(?:\s+(.+))?$").unwrap().captures(extra);
                let commit = Regex::new(r"(?is)^commit\s+(\S+)(?:\s+(.+))?$").unwrap().captures(extra);
                let focus = "Focus on correctness, bugs, regressions, security issues, edge cases, and missing tests. Report findings ordered by severity with exact file and line references.";
                let notes = |c: &regex::Captures| c.get(2).map(|m| m.as_str().trim().to_string()).filter(|s| !s.is_empty()).map(|n| format!("\nAdditional review focus: {n}")).unwrap_or_default();
                let prompt = if let Some(c) = branch {
                    let base = &c[1];
                    format!("Review the changes on the current branch compared to \"{base}\" (run `git diff {base}...HEAD` and `git log --oneline {base}..HEAD`). {focus}{}", notes(&c))
                } else if let Some(c) = commit {
                    let sha = &c[1];
                    format!("Review commit \"{sha}\" in this repository (run `git show {sha}`). {focus}{}", notes(&c))
                } else {
                    format!("Review the current changes in this repository (run git status and git diff or inspect modified files). Focus on correctness, bugs, security issues, edge cases, and missing tests. Report findings ordered by severity with exact file and line references.{}", if extra.is_empty() { String::new() } else { format!("\nAdditional review focus: {extra}") })
                };
                self.run(&prompt).await?;
                continue;
            }
            if input == "/init" {
                self.run("Inspect this workspace (package manifests, build/test scripts, directory structure, existing docs) and create or update AGENTS.md with concise, accurate commands for building, linting, typechecking, testing, and key architectural invariants.").await?;
                continue;
            }
            if input.starts_with("/mcp ") {
                self.mcp_slash_command(input[5..].trim()).await?;
                continue;
            }
            if ["/subagents", "/agents", "/agent"].contains(&input.as_str()) || input.starts_with("/subagents ") || input.starts_with("/agents ") || input.starts_with("/agent ") {
                let rest = Regex::new(r"^/(?:subagents|agents|agent)\s*").unwrap().replace(&input, "").trim().to_string();
                if self.subagent_listing(&rest) {
                    continue;
                }
                self.run(&format!("Use a subagent (spawn_agent + wait_agent, or delegate_task / delegate_edit as appropriate) to execute the following task and report the result:\n\n{rest}")).await?;
                continue;
            }
            self.run(&input).await?;
        }
        Ok(())
    }

    /// Returns `false` when the wizard was cancelled.
    async fn setup_command(&self) -> Result<bool> {
        let me = self.me.clone();
        let write: crate::setup::WriteFn = {
            let me = me.clone();
            Arc::new(move |t: &str| {
                if let Some(a) = me.upgrade() {
                    a.display(t);
                }
            })
        };
        let ask: crate::setup::AskFn = {
            let me = me.clone();
            Arc::new(move |label: String| {
                let me = me.clone();
                Box::pin(async move {
                    let app = me.upgrade().ok_or_else(|| anyhow!("Terminal closed."))?;
                    match app.tui() {
                        Some(t) => t.question(&label, None, false, QuestionOpts { esc_cancels: true, ..Default::default() }).await,
                        None => app.question(&label, None, false).await,
                    }
                })
            })
        };
        let ask_secret: crate::setup::AskFn = {
            let me = me.clone();
            Arc::new(move |label: String| {
                let me = me.clone();
                Box::pin(async move {
                    let app = me.upgrade().ok_or_else(|| anyhow!("Terminal closed."))?;
                    app.question_hidden(&label).await
                })
            })
        };
        let current = self.config.lock().unwrap().clone();
        let io = crate::setup::SetupIo { write: Some(write), ask: Some(ask), ask_secret: Some(ask_secret), discover: None };
        let next = match crate::setup::run_setup_wizard(Some(current), io).await {
            Ok(c) => c,
            Err(error) if error.downcast_ref::<crate::term::InputCancelled>().is_some() => {
                self.display("\n↺ Setup cancelled — configuration unchanged.\n");
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let mut next = next;
        if self.interactive && !self.json {
            next.show_reasoning = true;
        }
        *self.config.lock().unwrap() = next;
        self.resolve_runtime_here()?;
        let runtime = self.runtime();
        let store = self.store();
        store.session.lock().unwrap().model = runtime.model.clone();
        if let Some(t) = self.tui() {
            t.set_title(&format!("{} · {}", runtime.model, take_chars(&store.id(), 8)));
            t.set_effort(runtime.effort.as_deref());
        }
        self.sync_tui_header(false);
        self.rebuild_harness(false).await?;
        Ok(true)
    }

    async fn model_command(&self, raw_args: &str) -> Result<()> {
        let runtime = self.runtime();
        let mut next_model: Option<String> = None;
        let mut selected: Option<crate::provider::DiscoveredModel> = None;
        let config = self.config.lock().unwrap().clone();
        let mut next_effort = config.effort.clone();
        let mut effort_changed = false;
        if !raw_args.is_empty() {
            for tok in raw_args.split_whitespace() {
                if tok.starts_with("--effort=") || tok.starts_with("--reasoning=") {
                    let val = tok.split('=').nth(1).unwrap_or("");
                    next_effort = crate::setup::parse_effort_input(val, None);
                    effort_changed = true;
                } else if !tok.starts_with('-') {
                    next_model = Some(tok.to_string());
                }
            }
        } else {
            self.display(&format!(
                "\nCurrent model: {} (ctx: {}k · out: {}k · effort: {} · {} · {})\nProbing {}/models ...\n",
                runtime.model,
                (runtime.context_window as f64 / 1024.0).round(),
                (runtime.max_output_tokens as f64 / 1000.0).round(),
                runtime.effort.clone().unwrap_or_else(|| "default".into()),
                runtime.provider,
                runtime.base_url,
                runtime.base_url
            ));
            let discovered = crate::provider::discover_models(&runtime.base_url, Some(&runtime.api_key), &runtime.provider, None).await;
            if !discovered.is_empty() {
                let shown: Vec<_> = discovered.iter().take(30).cloned().collect();
                for (idx, m) in shown.iter().enumerate() {
                    self.display(&format!("  [{}] {}{}\n", idx + 1, m.id, crate::provider::format_model_badge(m)));
                }
                let pick = self.question(&format!("Select [1-{}] or enter model ID (Enter to keep {}): ", shown.len(), runtime.model), None, false).await.unwrap_or_default().trim().to_string();
                match pick.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= shown.len() && pick.chars().all(|c| c.is_ascii_digit())) {
                    Some(n) => {
                        selected = Some(shown[n - 1].clone());
                        next_model = Some(shown[n - 1].id.clone());
                    }
                    None if !pick.is_empty() => {
                        selected = discovered.iter().find(|m| m.id == pick).cloned();
                        next_model = Some(pick);
                    }
                    None => {}
                }
            } else {
                let typed = self.question(&format!("Enter model ID (Enter to keep {}): ", runtime.model), None, false).await.unwrap_or_default().trim().to_string();
                if !typed.is_empty() {
                    next_model = Some(typed);
                }
            }
        }
        if next_model.is_none() && !effort_changed {
            return Ok(());
        }
        let target = next_model.clone().unwrap_or_else(|| runtime.model.clone());
        let caps = crate::provider::resolve_model_capabilities(&target, selected.as_ref());
        let auto_ctx = next_model.as_ref().and(caps.context_window).unwrap_or(config.context_window);
        let raw_out = next_model.as_ref().and(caps.max_output_tokens).unwrap_or(config.max_output_tokens);
        let auto_out = raw_out.min(auto_ctx / 2);
        let final_effort = if effort_changed { next_effort } else if next_model.is_some() && caps.default_effort.is_some() { caps.default_effort.clone() } else { config.effort.clone() };
        let mut changes = Map::new();
        if let Some(m) = &next_model {
            changes.insert("model".into(), json!(m));
            changes.insert("contextWindow".into(), json!(auto_ctx));
            changes.insert("maxOutputTokens".into(), json!(auto_out));
        }
        if let Some(e) = &final_effort {
            changes.insert("effort".into(), json!(e));
        }
        self.update_config(changes, true)?;
        self.resolve_runtime_here()?;
        let runtime = self.runtime();
        let store = self.store();
        store.session.lock().unwrap().model = runtime.model.clone();
        if let Some(t) = self.tui() {
            t.set_title(&format!("{} · {}", runtime.model, take_chars(&store.id(), 8)));
            t.set_effort(runtime.effort.as_deref());
        }
        self.sync_tui_header(false);
        self.rebuild_harness(false).await?;
        self.display(&format!(
            "Active model: {} (ctx: {}k · out: {}k · {} · effort: {})\n",
            runtime.model,
            (runtime.context_window as f64 / 1024.0).round(),
            (runtime.max_output_tokens as f64 / 1000.0).round(),
            if caps.supports_vision == Some(false) { "text-only" } else { "vision" },
            runtime.effort.clone().unwrap_or_else(|| "default".into())
        ));
        Ok(())
    }

    async fn plan_command(&self, arg: &str) -> Result<()> {
        let lower = arg.to_lowercase();
        let mode = self.mode();
        let restore = |app: &App| -> String {
            let mut st = app.st.lock().unwrap();
            st.mode = st.pre_plan_mode.clone();
            st.mode.clone()
        };
        let enter = |app: &App| {
            let mut st = app.st.lock().unwrap();
            st.pre_plan_mode = st.mode.clone();
            st.mode = "plan".into();
        };
        if arg.is_empty() {
            if mode == "plan" {
                let m = restore(self);
                self.set_plan_flags(&m);
                self.display(&format!("Exited plan mode -> restored mode: {m}\n"));
            } else {
                enter(self);
                self.tools().set_plan_mode(true);
                self.tools().set_read_only(false);
                if let Some(t) = self.tui() {
                    t.set_mode("plan");
                }
                self.display("Plan mode active (read-only exploration + structured planning; call /plan off or /plan execute when ready).\n");
            }
            return Ok(());
        }
        if lower == "off" || lower == "exit" {
            let m = restore(self);
            self.set_plan_flags(&m);
            self.display(&format!("Plan mode off -> restored mode: {m}\n"));
            return Ok(());
        }
        if ["execute", "approve", "run"].contains(&lower.as_str()) {
            let m = restore(self);
            self.set_plan_flags(&m);
            self.display(&format!("Plan mode exited ({m}) — executing plan...\n"));
            return self.run("Proceed to implement the proposed plan step by step. Use todo_write to track tasks and run verification checks.").await;
        }
        if mode != "plan" {
            enter(self);
            self.tools().set_plan_mode(true);
            self.tools().set_read_only(false);
            if let Some(t) = self.tui() {
                t.set_mode("plan");
            }
            self.display("Plan mode active.\n");
        }
        self.run(arg).await
    }

    async fn revert_command(&self, input: &str) {
        let raw = Regex::new(r"^/(?:revert|undo)\b").unwrap().replace(input, "").trim().to_string();
        let mut scope = "both";
        let mut steps = 1usize;
        let mut force = false;
        for p in raw.split_whitespace() {
            match p.to_lowercase().as_str() {
                "--force" | "-f" | "force" => force = true,
                "code" | "files" | "file" | "source" => scope = "code",
                "msg" | "messages" | "chat" => scope = "messages",
                "both" | "all" => scope = "both",
                _ => {
                    if let Ok(n) = p.parse::<usize>() {
                        if n > 0 {
                            steps = n;
                        }
                    }
                }
            }
        }
        let res = match self.harness().revert(Some(steps), scope, force) {
            Ok(r) => r,
            Err(error) => {
                if self.tui().is_some() {
                    self.notice(&format!("↺ {}", safe_error(&error)));
                } else {
                    self.display(&format!("{}\n", safe_error(&error)));
                }
                return;
            }
        };
        let kept = if res.conflict_files.is_empty() { String::new() } else { format!("\n⚠ Kept {} file(s) changed after that turn: {} (use /revert {steps} {scope} --force to overwrite)\n", res.conflict_files.len(), res.conflict_files.join(", ")) };
        let lost = if res.unrecoverable_files.is_empty() { String::new() } else { format!("\n⚠ Backup pruned (session size limit), not restored: {}\n", res.unrecoverable_files.join(", ")) };
        if !kept.is_empty() || !lost.is_empty() {
            self.display(&format!("{kept}{lost}"));
        }
        let plural = if res.turns_reverted > 1 { "s" } else { "" };
        if let Some(t) = self.tui() {
            let (tokens, cost) = {
                let s = self.store();
                let s = s.session.lock().unwrap();
                (s.tokens, s.cost())
            };
            t.set_tokens(tokens);
            t.set_cost_usd(cost);
            self.sync_tui_header(true);
            let changed: Vec<String> = res.restored_files.iter().chain(res.deleted_files.iter()).cloned().collect();
            if scope == "both" || scope == "messages" {
                if !t.rewind_turns(res.turns_reverted) {
                    t.clear();
                    self.display(&self.banner());
                    self.render_resumed_history(false);
                }
                if !changed.is_empty() {
                    self.display(&format!("\n↺ Reverted {} turn{plural} (files restored: {})\n", res.turns_reverted, changed.join(", ")));
                }
                if let Some(p) = res.reverted_prompts.first() {
                    t.set_input(p, true);
                }
                let note = if changed.is_empty() { String::new() } else { format!(" · restored {} file(s)", changed.len()) };
                t.set_notice(Some(&format!("↺ Rewound {} turn{plural}{note} — edit prompt below, Enter to resend, or Esc Esc to rewind further", res.turns_reverted)));
            } else {
                self.display(&format!("\n↺ Reverted file changes from {} turn{plural}{}", res.turns_reverted, if changed.is_empty() { " (no files modified)\n".to_string() } else { format!(" ({})\n", changed.join(", ")) }));
            }
        } else {
            let mut text = format!("\n↺ Reverted {} turn(s) [scope: {scope}] ({} messages removed)\n", res.turns_reverted, res.messages_removed);
            if !res.restored_files.is_empty() {
                text.push_str(&format!("  Restored source files: {}\n", res.restored_files.join(", ")));
            }
            if !res.deleted_files.is_empty() {
                text.push_str(&format!("  Deleted newly created files: {}\n", res.deleted_files.join(", ")));
            }
            if res.restored_files.is_empty() && res.deleted_files.is_empty() && scope != "messages" {
                text.push_str("  No source files were modified in the reverted turn(s).\n");
            }
            if let Some(p) = res.reverted_prompts.first() {
                text.push_str(&format!("  Reverted prompt: \"{}\"\n", jsutil::slice_chars(p, 0, 80)));
            }
            self.display(&text);
        }
    }

    async fn switch_store(&self, new_store: Arc<SessionStore>, reconnect: bool) -> Result<()> {
        let old = std::mem::replace(&mut *self.store.lock().unwrap(), new_store.clone());
        if let Some(w) = self.event_writer.lock().unwrap().as_mut() {
            w.switch_session(&new_store.id(), None);
        }
        let runtime = self.runtime();
        self.publish_event(&json!({ "type": "session", "id": new_store.id(), "cwd": runtime.cwd.to_string_lossy() }), false);
        let _ = old.discard_if_empty();
        let _ = old.close();
        self.rebuild_harness(reconnect).await
    }

    fn reset_tui_after_switch(&self, title: Option<String>) {
        let Some(t) = self.tui() else { return };
        let runtime = self.runtime();
        let store = self.store();
        t.set_title(&match title {
            Some(title) => format!("{} · {title}", runtime.model),
            None => format!("{} · {}", runtime.model, take_chars(&store.id(), 8)),
        });
        let (tokens, cost, todos) = {
            let s = store.session.lock().unwrap();
            (s.tokens, s.cost(), s.todos.clone().unwrap_or_default())
        };
        t.set_tokens(tokens);
        t.set_cost_usd(cost);
        self.sync_tui_header(true);
        t.event(&json!({ "type": "todo", "todos": serde_json::to_value(todos).unwrap() }));
    }

    async fn new_session_command(&self, title: &str) -> Result<()> {
        let runtime = self.runtime();
        let new_store = Arc::new(SessionStore::open(&runtime.data_dir.join("sessions"), SessionRoute::from_runtime(&runtime), None, None)?);
        if !title.is_empty() {
            new_store.session.lock().unwrap().title = Some(title.to_string());
            new_store.save()?;
        }
        self.switch_store(new_store.clone(), false).await?;
        if let Some(t) = self.tui() {
            t.clear();
        }
        self.reset_tui_after_switch((!title.is_empty()).then(|| title.to_string()));
        self.display(&self.banner());
        self.display(&format!("Started new session: {}{}\n", new_store.id(), if title.is_empty() { String::new() } else { format!(" ({title})") }));
        Ok(())
    }

    async fn resume_command(&self, arg: &str) -> Result<()> {
        let runtime = self.runtime();
        let root = runtime.data_dir.join("sessions");
        let all = SessionStore::list(&root)?;
        let project: Vec<_> = all.iter().filter(|s| s.messages_count.unwrap_or(0) > 0 || s.title.is_some()).cloned().collect();
        let cwd_text = runtime.cwd.to_string_lossy().to_string();
        let folder: Vec<_> = project.iter().filter(|s| same_workspace(&s.cwd, &cwd_text)).cloned().collect();
        let current = self.session_id();
        let mut arg = arg.to_string();
        if arg.is_empty() {
            if project.is_empty() {
                self.display("No saved sessions yet. Sessions appear here after the first prompt.\n");
                return Ok(());
            }
            let target = if let Some(t) = self.tui() {
                let to_item = |s: &crate::session::SessionSummary, show_folder: bool| PickerItem {
                    id: s.id.clone(),
                    label: format!("{}{}{}", if s.id == current { "(current) " } else { "" }, s.title.as_ref().map(|t| format!("{t}: ")).unwrap_or_default(), s.preview.clone().unwrap_or_else(|| "(no prompt yet)".into())),
                    meta: Some([if show_folder { super::folder_label(&s.cwd) } else { String::new() }, super::format_relative_time(&s.updated_at), format!("{} msgs", s.messages_count.unwrap_or(0))].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join("   ")),
                    detail: Some(format!("{}   {}   {}", s.cwd, take_chars(&s.id, 8), s.model)),
                    search: Some(format!("{} {} {}", s.cwd, s.model, s.title.clone().unwrap_or_default())),
                };
                let tabs = vec![
                    PickerTab { label: "This folder".into(), items: folder.iter().map(|s| to_item(s, false)).collect(), empty: Some("No sessions in this folder yet. Press → for all projects.".into()) },
                    PickerTab { label: "All projects".into(), items: project.iter().map(|s| to_item(s, true)).collect(), empty: None },
                ];
                match t.pick("Resume", tabs, if folder.is_empty() { 1 } else { 0 }, "resume").await {
                    Some((_, item)) => Some(item.id),
                    None => {
                        t.set_notice(Some("Resume cancelled."));
                        return Ok(());
                    }
                }
            } else {
                // Readline mode: a numbered list; "a" switches between this folder and all projects.
                let mut show_all = folder.is_empty();
                loop {
                    let pool = if show_all { &project } else { &folder };
                    let rows: Vec<String> = pool
                        .iter()
                        .take(20)
                        .enumerate()
                        .map(|(i, s)| {
                            let label = s.title.clone().or_else(|| s.preview.clone()).unwrap_or_else(|| "(no prompt yet)".into());
                            let label = jsutil::slice_chars(&label.split_whitespace().collect::<Vec<_>>().join(" "), 0, 60).to_string();
                            format!("  {}  {label}   {}{}", pad_start(&(i + 1).to_string(), 2), if show_all { format!("{}   ", super::folder_label(&s.cwd)) } else { String::new() }, super::format_relative_time(&s.updated_at))
                        })
                        .collect();
                    self.display(&format!("\n{} ({})\n{}\n", if show_all { "All projects" } else { "This folder" }, pool.len(), if rows.is_empty() { "  (none)".to_string() } else { rows.join("\n") }));
                    let choice = self.question(&format!("Resume [number, ID prefix, a = {}, Enter = cancel]: ", if show_all { "this folder" } else { "all projects" }), None, false).await.unwrap_or_default().trim().to_string();
                    if choice.to_lowercase() == "a" {
                        show_all = !show_all;
                        continue;
                    }
                    break match choice.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= pool.len() && choice.chars().all(|c| c.is_ascii_digit())) {
                        Some(n) => Some(pool[n - 1].id.clone()),
                        None => (!choice.is_empty()).then_some(choice),
                    };
                }
            };
            let Some(target) = target else {
                self.display("Resume cancelled.\n");
                return Ok(());
            };
            arg = target;
        }
        let outcome: Result<()> = async {
            let candidates = if folder.is_empty() { &project } else { &folder };
            let prefix = arg.to_lowercase();
            let hexish = Regex::new(r"(?i)^[0-9a-f]{4,}$").unwrap().is_match(&arg);
            let target_match = match arg.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= candidates.len() && !hexish && arg.chars().all(|c| c.is_ascii_digit())) {
                Some(n) => candidates[n - 1].id.clone(),
                None if arg == "latest" => candidates.iter().find(|s| s.id != current).or(candidates.first()).map(|s| s.id.clone()).unwrap_or_else(|| arg.clone()),
                None => all
                    .iter()
                    .find(|s| s.id.to_lowercase() == prefix)
                    .or_else(|| folder.iter().find(|s| s.id.to_lowercase().starts_with(&prefix)))
                    .or_else(|| all.iter().find(|s| s.id.to_lowercase().starts_with(&prefix)))
                    .map(|s| s.id.clone())
                    .unwrap_or_else(|| arg.clone()),
            };
            if target_match == current {
                self.display(&format!("Already in session {}.\n", take_chars(&current, 8)));
                self.render_resumed_history(true);
                return Ok(());
            }
            // A session from another project brings its workspace with it.
            let target = all.iter().find(|s| s.id == target_match);
            let previous_runtime = self.runtime();
            let switching = target.map(|t| !same_workspace(&t.cwd, &cwd_text)).unwrap_or(false);
            if switching {
                let t = target.unwrap();
                let Ok(folder) = std::fs::canonicalize(&t.cwd) else {
                    self.display(&format!("Cannot resume: the session's folder no longer exists ({}).\n", t.cwd));
                    return Ok(());
                };
                let config = self.config.lock().unwrap().clone();
                *self.runtime.lock().unwrap() = Arc::new(config::resolve_runtime(&config, &config::strip_verbatim(folder))?);
            }
            let runtime = self.runtime();
            let new_store = match SessionStore::open(&root, SessionRoute::from_runtime(&runtime), Some(&target_match), Some(&current)) {
                Ok(s) => Arc::new(s),
                Err(error) => {
                    *self.runtime.lock().unwrap() = previous_runtime;
                    return Err(error);
                }
            };
            self.switch_store(new_store.clone(), switching).await?;
            if let Some(t) = self.tui() {
                t.clear();
                self.display(&self.banner());
                if switching {
                    self.refresh_workspace_files();
                }
            }
            self.reset_tui_after_switch(new_store.session.lock().unwrap().title.clone());
            self.render_resumed_history(true);
            if switching {
                self.display(&format!("Workspace switched to {}\n", runtime.cwd.display()));
            }
            if new_store.session.lock().unwrap().messages.is_empty() {
                self.display(&format!("Resumed session {} (0 messages).\n", take_chars(&new_store.id(), 8)));
            }
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            self.display(&format!("Failed to resume session: {}\n", safe_error(&error)));
        }
        Ok(())
    }

    async fn cd_command(&self, target: &str) {
        let runtime = self.runtime();
        if target.is_empty() {
            self.display(&format!("Current workspace directory: {}\nUsage: /cd <directory>\n", runtime.cwd.display()));
            return;
        }
        let outcome: Result<()> = async {
            let candidate = crate::files::resolve_lexical(&runtime.cwd, target);
            let meta = std::fs::metadata(&candidate)?;
            if !meta.is_dir() {
                self.display(&format!("Not a directory: {}\n", candidate.display()));
                return Ok(());
            }
            let real = std::fs::canonicalize(&candidate).map(config::strip_verbatim).unwrap_or(candidate);
            let mut next = (*runtime).clone();
            next.cwd = real.clone();
            *self.runtime.lock().unwrap() = Arc::new(next);
            let store = self.store();
            store.session.lock().unwrap().cwd = real.to_string_lossy().to_string();
            store.save()?;
            self.rebuild_harness(true).await?;
            if self.tui().is_some() {
                self.refresh_workspace_files();
                self.sync_tui_header(false);
                self.notice(&format!("Changed directory to: {}", real.display()));
            }
            self.display(&format!("Changed workspace directory to: {}\n", real.display()));
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            self.display(&format!("Failed to change directory: {}\n", safe_error(&error)));
        }
    }

    async fn delete_command(&self, target: &str) -> Result<()> {
        let runtime = self.runtime();
        let root = runtime.data_dir.join("sessions");
        if target.is_empty() || target.to_lowercase() == "current" {
            let old_id = self.session_id();
            let new_store = Arc::new(SessionStore::open(&root, SessionRoute::from_runtime(&runtime), None, None)?);
            let old = std::mem::replace(&mut *self.store.lock().unwrap(), new_store.clone());
            if let Some(w) = self.event_writer.lock().unwrap().as_mut() {
                w.switch_session(&new_store.id(), None);
            }
            self.publish_event(&json!({ "type": "session", "id": new_store.id(), "cwd": runtime.cwd.to_string_lossy() }), false);
            let _ = old.close();
            let _ = SessionStore::delete_session(&root, &old_id);
            self.rebuild_harness(false).await?;
            if let Some(t) = self.tui() {
                t.clear();
            }
            self.reset_tui_after_switch(None);
            self.display(&self.banner());
            self.display(&format!("Deleted session {} and started fresh session {}.\n", take_chars(&old_id, 8), take_chars(&new_store.id(), 8)));
            return Ok(());
        }
        if self.session_id().to_lowercase().starts_with(&target.to_lowercase()) {
            self.display("To delete the currently active session, run: /delete current\n");
            return Ok(());
        }
        match SessionStore::delete_session(&root, target) {
            Ok(Some(id)) => self.display(&format!("Deleted saved session: {id}\n")),
            Ok(None) => self.display(&format!("Failed to delete session: Session \"{target}\" not found.\n")),
            Err(error) => self.display(&format!("Failed to delete session: {}\n", safe_error(&error))),
        }
        Ok(())
    }

    async fn workflow_run_command(&self, input: &str) {
        let rest = Regex::new(r"^/workflows?\s+").unwrap().replace(input, "").trim().to_string();
        let without_run = Regex::new(r"(?i)^run\s+").unwrap().replace(&rest, "").trim().to_string();
        let (name, args) = match without_run.find(char::is_whitespace).filter(|i| *i > 0) {
            Some(sp) => (without_run[..sp].trim().to_string(), without_run[sp..].trim().to_string()),
            None => (without_run.trim().to_string(), String::new()),
        };
        let runtime = self.runtime();
        let all = discover_workflows(&runtime.cwd);
        let Some(wf) = all.iter().find(|w| w.name.to_lowercase() == name.to_lowercase()).cloned() else {
            self.display(&format!("Workflow \"{name}\" not found. Run \"/workflow list\" to see available workflows.\n"));
            return;
        };
        let token = CancellationToken::new();
        self.set_controller(Some(token.clone()));
        if let Some(t) = self.tui() {
            t.set_status("Running");
        }
        // Every workflow step shares one run budget (token/cost/time caps).
        let tools = self.tools();
        let budget = tools.budget().unwrap_or_else(|| self.fresh_budget());
        let hooks = self.workflow_hooks(&tools, token.clone(), Some(budget), false);
        match execute_workflow(&wf, &args, &hooks).await {
            Ok(res) => {
                if self.json {
                    write_json_line(&json!({ "type": "workflow_done", "result": res.to_value(), "sessionId": self.session_id() }));
                }
            }
            Err(error) => self.display(&format!("\nWorkflow error: {}\n", safe_error(&error))),
        }
        self.set_controller(None);
        if let Some(t) = self.tui() {
            t.set_status("Ready");
        }
        self.sync_tui_header(false);
    }

    async fn network_command(&self, input: &str) -> Result<()> {
        let Some(subagents) = self.subagents() else {
            self.display("Agent Network is disabled (--no-subagents).\n");
            return Ok(());
        };
        let raw = Regex::new(r"^/(?:network|brainstorm|council)\s+").unwrap().replace(input, "").trim().to_string();
        let mut rounds = 2u32;
        let mut roles: Option<Vec<String>> = None;
        let mut topic_tokens = vec![];
        for tok in raw.split_whitespace() {
            if let Some(n) = tok.strip_prefix("--rounds=") {
                if let Ok(n) = n.parse::<u32>() {
                    if (1..=4).contains(&n) {
                        rounds = n;
                    }
                }
            } else if let Some(list) = tok.strip_prefix("--roles=") {
                roles = Some(list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect());
            } else {
                topic_tokens.push(tok);
            }
        }
        let topic = topic_tokens.join(" ").trim().to_string();
        if topic.is_empty() {
            self.display("Usage: /network [--rounds=2] [--roles=architect,critic,researcher,pragmatist] <topic>\n");
            return Ok(());
        }
        let token = CancellationToken::new();
        self.set_controller(Some(token.clone()));
        if let Some(t) = self.tui() {
            t.set_status("Running");
            t.mark_turn_start();
            t.write(&format!("\n> {input}\n"));
        }
        let me = self.me.clone();
        let progress: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |t: &str| {
            if let Some(a) = me.upgrade() {
                a.display(t);
            }
        });
        let budget = self.tools().budget().unwrap_or_else(|| self.fresh_budget());
        let outcome = subagents.network.brainstorm(crate::network::BrainstormOptions { topic: topic.clone(), roles, rounds: Some(rounds), explore_codebase: None, cancel: token.clone(), budget: Some(budget), policy: Some(self.policy.clone()), on_progress: Some(progress) }).await;
        match outcome {
            Ok(res) => {
                let report = crate::network::format_brainstorm_report(&res);
                self.display(&format!("\n[answer]\n{}\n", res.synthesis));
                let store = self.store();
                {
                    let mut s = store.session.lock().unwrap();
                    s.messages.push(json!({ "role": "user", "content": format!("[Agent Network Brainstorm]: {topic}") }));
                    s.messages.push(json!({ "role": "assistant", "content": report }));
                }
                store.save()?;
                if self.json {
                    write_json_line(&json!({ "type": "network_done", "synthesis": res.synthesis, "rounds": res.rounds }));
                }
            }
            Err(error) => self.display(&format!("\nAgent Network failed: {}\n", safe_error(&error))),
        }
        self.set_controller(None);
        if let Some(t) = self.tui() {
            t.set_status("Ready");
        }
        self.sync_tui_header(false);
        Ok(())
    }

    async fn mcp_slash_command(&self, rest: &str) -> Result<()> {
        let args: Vec<&str> = rest.split_whitespace().collect();
        let sub = args.first().map(|s| s.to_lowercase()).unwrap_or_else(|| "list".into());
        match sub.as_str() {
            "reload" | "reconnect" => {
                self.rebuild_harness(true).await?;
                self.display(&format!("Reloaded MCP servers ({} MCP tools active).\n", self.mcp_tool_count()));
            }
            "import-codex" | "sync-codex" => {
                let filter: Vec<String> = args[1..].iter().map(|s| s.to_string()).collect();
                match crate::mcp::import_codex_mcp_servers(None, &filter) {
                    Ok((imported, path)) if imported.is_empty() => self.display(&format!("No matching MCP servers found in ~/.codex/config.toml (target: {}).\n", path.display())),
                    Ok((imported, path)) => {
                        self.display(&format!("Imported {} MCP server(s) into {}: {}\n", imported.len(), path.display(), imported.join(", ")));
                        self.rebuild_harness(true).await?;
                        self.display(&format!("Active MCP tools: {}\n", self.mcp_tool_count()));
                    }
                    Err(error) => self.display(&format!("MCP import failed: {}\n", safe_error(&error))),
                }
            }
            "remove" | "rm" => {
                let Some(target) = args.get(1) else {
                    self.display("Usage: /mcp remove <name>\n");
                    return Ok(());
                };
                let mut cfg = crate::mcp::load_mcp_config()?;
                if cfg.servers.shift_remove(*target).is_none() {
                    self.display(&format!("MCP server \"{target}\" not found.\n"));
                    return Ok(());
                }
                crate::mcp::save_mcp_config(&cfg)?;
                self.rebuild_harness(true).await?;
                self.display(&format!("Removed MCP server \"{target}\" ({} MCP tools active).\n", self.mcp_tool_count()));
            }
            "check" => {
                self.rebuild_harness(true).await?;
                for st in self.manager().map(|m| m.statuses()).unwrap_or_default() {
                    let detail = if st.status == "ok" { format!(" ({} tools)", st.tool_count) } else { st.error.map(|e| format!(" — {e}")).unwrap_or_default() };
                    self.display(&format!("  {}: {}{detail}\n", st.name, st.status));
                }
            }
            _ => self.display("Usage: /mcp [list | check | reload | import-codex [names...] | remove <name>]\n"),
        }
        Ok(())
    }
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
