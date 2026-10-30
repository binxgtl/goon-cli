use crate::common::*;
use goon::coord::*;
use std::time::Duration;

fn is_child(body: &Value) -> bool {
    body["messages"].as_array().map(|m| m.iter().any(|m| m["content"].as_str().is_some_and(|c| c.starts_with("You are a delegated agent.")))).unwrap_or(false)
}

fn has_tool_results(body: &Value) -> bool {
    body["messages"].as_array().map(|m| m.iter().any(|m| m["role"] == "tool")).unwrap_or(false)
}

fn tool_names(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body["tools"].as_array().map(|t| t.iter().filter_map(|t| t["function"]["name"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    names.sort();
    names
}

fn plan_json(todos: Value) -> Value {
    json!({ "goal": "g", "todos": todos })
}

fn planner(plan: Value) -> Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, Result<Value>> + Send + Sync> {
    Arc::new(move |_, _| {
        let p = plan.clone();
        Box::pin(async move { Ok(p) })
    })
}

type Spawn = Arc<dyn Fn(DagSpawnRequest) -> BoxFuture<'static, Result<String>> + Send + Sync>;

fn hooks(plan: Value, spawn: Spawn) -> DagHooks {
    DagHooks { planner: planner(plan), spawn, review: None, test: None, verify: None }
}

fn opts(files: &Arc<files::WorkspaceFiles>, config: DagConfig, budget: Option<cost::SharedBudget>, emit: Option<Arc<dyn Fn(Value) + Send + Sync>>, allow_empty: bool) -> DagOptions {
    DagOptions { config, cancel: token(), files: files.clone(), budget, allowed_mcp_tools: vec![], allow_empty, write_concurrency: None, emit }
}

fn emit_log() -> (Option<Arc<dyn Fn(Value) + Send + Sync>>, Arc<Mutex<Vec<Value>>>) {
    let log = Arc::new(Mutex::new(vec![]));
    let l = log.clone();
    (Some(Arc::new(move |e| l.lock().unwrap().push(e))), log)
}

fn budget(spent: u64, limit: u64, cost_limit: f64, started_at: Option<i64>, time_limit_ms: u64) -> cost::SharedBudget {
    Arc::new(Mutex::new(cost::RunBudget { spent, limit, cost_spent: 0.0, cost_limit, started_at, time_limit_ms, ledger: None }))
}

#[tokio::test]
async fn allowed_tools_hide_tools_from_schemas_and_execute() {
    let f = fixture();
    let allowed: HashSet<String> = ["read_file", "list_files", "search"].iter().map(|s| s.to_string()).collect();
    let reg = tools::ToolRegistry::new(&f.cwd, tools::approve_all(), false, Some(allowed));
    let mut names: Vec<String> = reg.schemas().into_iter().map(|s| s.name).collect();
    names.sort();
    assert_eq!(names, vec!["list_files", "read_file", "search"]);
    assert_match(&reg.execute(&call("c1", "bash", json!({ "command": "echo hi" })), &token()).await.err().unwrap().to_string(), "(?i)not available");
    assert_match(&reg.execute(&call("c2", "write_file", json!({ "path": "x.txt", "content": "no" })), &token()).await.err().unwrap().to_string(), "(?i)not available");
    let wildcard: HashSet<String> = ["read_file", "mcp__*"].iter().map(|s| s.to_string()).collect();
    let wild = tools::ToolRegistry::new(&f.cwd, tools::approve_all(), true, Some(wildcard));
    assert_eq!(wild.schemas().into_iter().map(|s| s.name).collect::<Vec<_>>(), vec!["read_file"]);
}

#[tokio::test]
async fn run_tasks_is_the_public_spawn_path_and_pins_allowed_tools_in_the_child_registry() {
    let f = fixture();
    let child_tools = Arc::new(Mutex::new(vec![]));
    let ct = child_tools.clone();
    let p = provider(move |body, _| {
        if is_child(body) {
            *ct.lock().unwrap() = tool_names(body);
            return text("child report");
        }
        text("unused")
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "timeoutMs": 3000, "runTokenBudget": 100000 }));
    let st = store(&rt);
    let tools = registry(&f.cwd, tools::approve_all(), false);
    let subs = subagents::Subagents::new(rt, st, tools, harness::noop_emit());
    let allowed: HashSet<String> = ["read_file", "list_files", "search"].iter().map(|s| s.to_string()).collect();
    let out = subs.run_tasks(vec!["inspect".into()], subagents::RunTasksOptions { read_only: false, cancel: token(), budget: None, allowed_tools: Some(allowed), role: None }).await.unwrap();
    assert_match(&out, "child report");
    assert_eq!(*child_tools.lock().unwrap(), vec!["list_files", "read_file", "search"]);
}

#[tokio::test]
async fn delegate_edit_ignores_model_supplied_allowed_tools_and_keeps_the_parent_toolset() {
    let f = fixture();
    let child_tools = Arc::new(Mutex::new(vec![]));
    let ct = child_tools.clone();
    let p = provider(move |body, _| {
        if is_child(body) {
            *ct.lock().unwrap() = tool_names(body);
            return text("edited.");
        }
        if !has_tool_results(body) {
            return calls(vec![tc("esc", "delegate_edit", json!({ "tasks": ["work"], "allowedTools": ["read_file"] }))]);
        }
        text("Done.")
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "timeoutMs": 3000, "runTokenBudget": 100000 }));
    let st = store(&rt);
    let tools = registry(&f.cwd, tools::approve_all(), false);
    // Registered delegation tools hold a weak reference: the owner keeps the manager alive.
    let subs = subagents::Subagents::new(rt.clone(), st.clone(), tools.clone(), harness::noop_emit());
    subs.register().unwrap();
    harness::Harness::new(rt, st, tools, harness::noop_emit(), None, Default::default()).unwrap().run("delegate", &token(), None).await.unwrap();
    let child = child_tools.lock().unwrap().clone();
    assert!(child.contains(&"write_file".to_string()) && child.contains(&"bash".to_string()) && child.len() > 5, "{child:?}");
}

#[tokio::test]
async fn run_dag_lifecycle_spawns_write_serially_rolls_back_on_test_failure_reworks_and_merges() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let shared = budget(0, 50_000, 0.0, Some(1234), 0);
    let test_runs = Arc::new(AtomicUsize::new(0));
    let (fw, sb) = (files.clone(), shared.clone());
    let spawn: Spawn = Arc::new(move |req| {
        assert!(Arc::ptr_eq(&req.budget, &sb));
        assert_eq!(req.allowed_tools, role_toolset("coder", &[]));
        assert!(!req.read_only);
        let attempt = req.todo.rework_cycles + 1;
        let r = fw.write("x.txt", &format!("attempt {attempt}"));
        let id = req.todo.id.clone();
        Box::pin(async move { r.map(|_| format!("report attempt {attempt} for {id}")) })
    });
    let mut h = hooks(plan_json(json!([{ "id": "t1", "title": "write x.txt", "acceptance": "final content", "role": "coder" }])), spawn);
    let tr = test_runs.clone();
    h.test = Some(Arc::new(move |_, changed: Vec<String>, cancel: CancellationToken| {
        assert!(!cancel.is_cancelled());
        assert_eq!(changed, vec!["x.txt"]);
        let n = tr.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(n > 1) })
    }));
    let (emit, log) = emit_log();
    let result = run_dag("write a file", h, opts(&files, DagConfig { rework_cap: Some(3), max_concurrency: Some(2), ..Default::default() }, Some(shared.clone()), emit, false)).await.unwrap();
    assert!(result.merge.verified);
    assert_eq!(result.plan.todos[0].rework_cycles, 1);
    assert_eq!(result.plan.todos[0].status, "done");
    assert_eq!(f.read("x.txt"), "attempt 2");
    assert!(result.merge.combined_diff.contains("x.txt"));
    let events = log.lock().unwrap();
    let attempts: Vec<u64> = events.iter().filter(|e| e["type"] == "dag_todo_update" && e["status"] == "in_progress").map(|e| e["attempt"].as_u64().unwrap()).collect();
    assert_eq!(attempts, vec![1, 2]);
    assert!(events.iter().any(|e| e["type"] == "dag_todo_update" && e["status"] == "done"));
    assert!(events.iter().any(|e| e["type"] == "dag_plan_ready" && e["todos"] == 1));
    assert!(events.iter().any(|e| e["type"] == "dag_merge" && e["verified"] == true && e["conflicts"] == 0));
    assert_eq!(shared.lock().unwrap().started_at, Some(1234));
}

fn capture_budget(seen: &Arc<Mutex<Option<cost::SharedBudget>>>) -> Spawn {
    let s = seen.clone();
    Arc::new(move |req| {
        *s.lock().unwrap() = Some(req.budget.clone());
        Box::pin(async { Ok("r".to_string()) })
    })
}

fn researcher() -> Value {
    plan_json(json!([{ "id": "t1", "title": "read", "role": "researcher" }]))
}

#[tokio::test]
async fn run_dag_creates_a_fallback_budget_from_config_when_none_is_passed() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let seen = Arc::new(Mutex::new(None));
    run_dag("g", hooks(researcher(), capture_budget(&seen)), opts(&files, DagConfig { run_token_budget: 4242, ..Default::default() }, None, None, true)).await.unwrap();
    {
        let b = seen.lock().unwrap().clone().unwrap();
        let b = b.lock().unwrap();
        assert_eq!((b.limit, b.spent), (4242, 0));
    }
    run_dag("g", hooks(researcher(), capture_budget(&seen)), opts(&files, DagConfig::default(), None, None, true)).await.unwrap();
    assert_eq!(seen.lock().unwrap().clone().unwrap().lock().unwrap().limit, DEFAULT_DAG_TOKEN_BUDGET);
}

fn coder_writes(files: &Arc<files::WorkspaceFiles>, content: &'static str) -> Spawn {
    let fw = files.clone();
    Arc::new(move |_| {
        let r = fw.write("x.txt", content);
        Box::pin(async move { r.map(|_| "r".to_string()) })
    })
}

fn coder() -> Value {
    plan_json(json!([{ "id": "t1", "title": "write", "role": "coder" }]))
}

#[tokio::test]
async fn run_dag_verify_false_rolls_back_every_todo_and_errors_not_verified() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let mut h = hooks(coder(), coder_writes(&files, "should not survive"));
    h.verify = Some(Arc::new(|_, _, _| Box::pin(async { Ok(false) })));
    let (emit, log) = emit_log();
    let error = run_dag("g", h, opts(&files, DagConfig::default(), None, emit, false)).await.err().unwrap();
    assert!(error.downcast_ref::<ErrNotVerified>().is_some(), "{error}");
    assert_match(&error.to_string(), "(?i)verification failed");
    assert!(!f.exists("x.txt"));
    assert!(log.lock().unwrap().iter().any(|e| e["type"] == "dag_merge" && e["verified"] == false));
}

#[tokio::test]
async fn run_dag_test_hook_without_approval_fails_closed_and_rolls_back() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let mut h = hooks(coder(), coder_writes(&files, "temp"));
    let cwd = f.cwd.clone();
    h.test = Some(Arc::new(move |_, changed: Vec<String>, cancel: CancellationToken| {
        let cwd = cwd.clone();
        Box::pin(async move {
            let config = config::VerifyConfig { enabled: true, commands: vec!["node --version".into()], timeout_ms: 30_000, max_repair_rounds: 1, policy: Default::default() };
            let mut plan = verify::plan_verification(&cwd, &changed, &config, &config.policy);
            let verdict = verify::run_verification(&cwd, &changed, &config, &verify::CommandRunner, &cancel, &mut plan, None).await?;
            Ok(verdict.pass)
        })
    }));
    let (emit, log) = emit_log();
    let error = run_dag("g", h, opts(&files, DagConfig::default(), None, emit, false)).await.err().unwrap().to_string();
    assert_match(&error, r"(?s)Plan failed.*t1\(failed: Verification commands require approval before running\.\)");
    assert!(!f.exists("x.txt"));
    let events = log.lock().unwrap();
    let failed = events.iter().find(|e| e["type"] == "dag_todo_update" && e["status"] == "failed").expect("failed event");
    assert_match(failed["reason"].as_str().unwrap(), "(?i)approval");
}

#[tokio::test]
async fn run_dag_review_rework_cap_fails_the_todo_blocks_dependents_and_rolls_back() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (fw, a) = (files.clone(), attempts.clone());
    let spawn: Spawn = Arc::new(move |req| {
        let n = a.fetch_add(1, Ordering::SeqCst) + 1;
        let r = if req.todo.id == "t1" { fw.write("x.txt", &format!("attempt {n}")).map(|_| ()) } else { Ok(()) };
        Box::pin(async move { r.map(|_| "r".to_string()) })
    });
    let mut h = hooks(plan_json(json!([{ "id": "t1", "title": "write", "role": "coder" }, { "id": "t2", "title": "check", "role": "tester", "dependsOn": ["t1"] }])), spawn);
    h.review = Some(Arc::new(|_, _, _| Box::pin(async { Ok(false) })));
    let (emit, log) = emit_log();
    let error = run_dag("g", h, opts(&files, DagConfig { rework_cap: Some(1), ..Default::default() }, None, emit, false)).await.err().unwrap().to_string();
    assert_match(&error, r"(?s)Plan failed.*t1\(failed: rework cap.*t2\(blocked\)");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(!f.exists("x.txt"));
    let events = log.lock().unwrap();
    assert!(events.iter().any(|e| e["type"] == "dag_todo_update" && e["todoId"] == "t2" && e["status"] == "blocked"));
    let failed = events.iter().find(|e| e["type"] == "dag_todo_update" && e["todoId"] == "t1" && e["status"] == "failed").expect("failed event");
    assert_match(failed["reason"].as_str().unwrap(), "rework cap");
}

#[tokio::test]
async fn a_read_only_todo_completing_mid_write_never_steals_the_write_todo_snapshot() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let fw = files.clone();
    let spawn: Spawn = Arc::new(move |req| {
        let fw = fw.clone();
        Box::pin(async move {
            if req.todo.id == "r1" {
                tokio::time::sleep(Duration::from_millis(15)).await;
                return Ok("research notes".to_string());
            }
            fw.write("n1.txt", "one")?;
            fw.write("n2.txt", "two")?;
            tokio::time::sleep(Duration::from_millis(40)).await;
            Ok("coder report".to_string())
        })
    });
    let mut h = hooks(plan_json(json!([{ "id": "r1", "title": "explore", "role": "researcher" }, { "id": "w1", "title": "write", "role": "coder" }])), spawn);
    let tested = Arc::new(Mutex::new(vec![]));
    let t = tested.clone();
    h.test = Some(Arc::new(move |todo: Todo, changed: Vec<String>, _| {
        t.lock().unwrap().push((todo.id.clone(), changed));
        Box::pin(async { Ok(true) })
    }));
    let (emit, log) = emit_log();
    let result = run_dag("g", h, opts(&files, DagConfig::default(), None, emit, false)).await.unwrap();
    assert!(result.merge.verified);
    assert!(result.merge.conflicts.is_empty());
    let mut artifacts = result.merge.artifacts.clone();
    artifacts.sort();
    assert_eq!(artifacts, vec!["n1.txt", "n2.txt"]);
    assert_eq!(*tested.lock().unwrap(), vec![("w1".to_string(), vec!["n1.txt".to_string(), "n2.txt".to_string()])]);
    assert!(!log.lock().unwrap().iter().any(|e| e["type"] == "dag_todo_update" && e["status"] == "failed"));
}

#[tokio::test]
async fn run_dag_fills_cost_and_time_caps_from_config_for_caller_budgets_too() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let seen = Arc::new(Mutex::new(None));
    let config = DagConfig { max_cost_usd: 2.5, max_time_ms: 60_000, ..Default::default() };
    run_dag("g", hooks(researcher(), capture_budget(&seen)), opts(&files, config.clone(), Some(budget(0, 12345, 0.0, None, 0)), None, true)).await.unwrap();
    {
        let b = seen.lock().unwrap().clone().unwrap();
        let b = b.lock().unwrap();
        assert_eq!((b.limit, b.cost_limit, b.time_limit_ms), (12345, 2.5, 60_000));
        assert!(b.started_at.is_some());
    }
    run_dag("g", hooks(researcher(), capture_budget(&seen)), opts(&files, config, Some(budget(0, 1, 9.0, Some(42), 9)), None, true)).await.unwrap();
    let b = seen.lock().unwrap().clone().unwrap();
    let b = b.lock().unwrap();
    assert_eq!((b.cost_limit, b.time_limit_ms, b.started_at), (9.0, 9, Some(42)));
}

#[tokio::test]
async fn run_dag_surfaces_spawn_errors_in_the_plan_failure_detail() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let spawn: Spawn = Arc::new(|_| Box::pin(async { Err(anyhow!("child exploded: root cause detail")) }));
    let error = run_dag("g", hooks(coder(), spawn), opts(&files, DagConfig::default(), None, None, false)).await.err().unwrap().to_string();
    assert_match(&error, r"(?s)Plan failed.*t1\(failed.*child exploded: root cause detail");
}

#[tokio::test]
async fn run_dag_drains_parallel_siblings_and_rolls_back_when_a_hook_throws() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let finished = Arc::new(AtomicUsize::new(0));
    let (fw, done) = (files.clone(), finished.clone());
    let spawn: Spawn = Arc::new(move |req| {
        let (fw, done) = (fw.clone(), done.clone());
        Box::pin(async move {
            if req.todo.id == "r1" {
                return Ok("research notes".to_string());
            }
            fw.write("x.txt", "in flight")?;
            tokio::time::sleep(Duration::from_millis(40)).await;
            done.fetch_add(1, Ordering::SeqCst);
            Ok("coder report".to_string())
        })
    });
    let mut h = hooks(plan_json(json!([{ "id": "r1", "title": "explore", "role": "researcher" }, { "id": "w1", "title": "write", "role": "coder" }])), spawn);
    h.review = Some(Arc::new(|todo: Todo, _, _| {
        let id = todo.id.clone();
        Box::pin(async move {
            if id == "r1" {
                return Err(anyhow!("review exploded"));
            }
            Ok(true)
        })
    }));
    let (emit, log) = emit_log();
    let error = run_dag("g", h, opts(&files, DagConfig::default(), None, emit, false)).await.err().unwrap().to_string();
    assert_match(&error, r"(?s)Plan failed.*r1\(failed.*review exploded");
    assert!(!f.exists("x.txt"));
    assert!(!log.lock().unwrap().iter().any(|e| e["type"] == "dag_merge"));
    assert_eq!(finished.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn run_dag_drains_and_rolls_back_when_the_final_verify_hook_throws() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let mut h = hooks(coder(), coder_writes(&files, "temp"));
    h.verify = Some(Arc::new(|_, _, _| Box::pin(async { Err(anyhow!("verify exploded")) })));
    let error = run_dag("g", h, opts(&files, DagConfig::default(), None, None, false)).await.err().unwrap().to_string();
    assert_match(&error, "verify exploded");
    assert!(!f.exists("x.txt"));
}

#[tokio::test]
async fn run_dag_applies_max_cost_to_the_shared_budget_and_the_child_stops_at_the_cap() {
    let f = fixture();
    let p = provider(|body, _| {
        if is_child(body) {
            if !has_tool_results(body) {
                return calls(vec![tc("c1", "list_files", json!({}))]);
            }
            return text("never reached: the cost cap fires first");
        }
        text("unused")
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "timeoutMs": 3000, "runTokenBudget": 100000, "maxCostUsd": 0.5, "pricing": { "test-model": { "inputPer1M": 10_000, "outputPer1M": 0 } } }));
    let st = store(&rt);
    let tools = registry(&f.cwd, tools::approve_all(), false);
    let subs = subagents::Subagents::new(rt.clone(), st, tools, harness::noop_emit());
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let seen = Arc::new(Mutex::new(None::<cost::SharedBudget>));
    let report = Arc::new(Mutex::new(String::new()));
    let (s, r, sb) = (seen.clone(), report.clone(), subs.clone());
    let spawn: Spawn = Arc::new(move |req| {
        *s.lock().unwrap() = Some(req.budget.clone());
        let (r, sb) = (r.clone(), sb.clone());
        Box::pin(async move {
            let out = sb
                .run_tasks(vec!["inspect".into()], subagents::RunTasksOptions { read_only: true, cancel: req.cancel.clone(), budget: Some(req.budget.clone()), allowed_tools: None, role: None })
                .await?;
            *r.lock().unwrap() = out.clone();
            Ok(out)
        })
    });
    let config = DagConfig { run_token_budget: rt.run_token_budget, max_cost_usd: rt.max_cost_usd, max_time_ms: rt.max_time_ms, ..Default::default() };
    let error = run_dag("g", hooks(researcher(), spawn), opts(&files, config, None, None, false)).await.err().unwrap().to_string();
    assert_match(&error, "no changes");
    let b = seen.lock().unwrap().clone().unwrap();
    let b = b.lock().unwrap();
    assert_eq!(b.cost_limit, 0.5);
    assert!(b.cost_spent >= 0.5, "the shared mirror shows the child spend: {}", b.cost_spent);
    assert_match(&report.lock().unwrap(), "Run cost budget reached");
}

#[test]
fn multi_agent_config_keys_parse_with_the_documented_defaults() {
    let f = fixture();
    let _env = fixture_env(&f);
    let dir = f.config.join("goon");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), "{}").unwrap();
    let c = config::load_config(&Map::new()).unwrap();
    assert_eq!(c.multi_agent_mode, "manual");
    assert_eq!(c.multi_agent_rework_cap, 3);
    assert_eq!(c.multi_agent_max_concurrency, 2);
    assert!(c.multi_agent_mcp_tools.is_empty());
    std::fs::write(dir.join("config.json"), json!({ "multiAgentMode": "auto", "multiAgentReworkCap": 5, "multiAgentMaxConcurrency": 4, "multiAgentMcpTools": ["mcp__fs__read"] }).to_string()).unwrap();
    let tuned = config::load_config(&Map::new()).unwrap();
    assert_eq!(tuned.multi_agent_mode, "auto");
    assert_eq!(tuned.multi_agent_rework_cap, 5);
    assert_eq!(tuned.multi_agent_mcp_tools, vec!["mcp__fs__read"]);
    std::fs::write(dir.join("config.json"), json!({ "multiAgentMode": "sometimes" }).to_string()).unwrap();
    assert_match(&config::load_config(&Map::new()).err().unwrap().to_string(), "multiAgentMode");
}
