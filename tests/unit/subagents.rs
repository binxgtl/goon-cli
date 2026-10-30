use crate::common::*;
use goon::policy::{ExecutionPolicy, NetworkMode};
use goon::subagents::Subagents;

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

fn deny_policy() -> Arc<ExecutionPolicy> {
    let mut p = ExecutionPolicy::with_mode(NetworkMode::Deny, &[]);
    p.limits.default_tool_timeout_ms = 5000;
    p.limits.max_tool_timeout_ms = 10_000;
    Arc::new(p)
}

struct Parent {
    rt: Arc<config::RuntimeConfig>,
    store: Arc<session::SessionStore>,
    tools: Arc<tools::ToolRegistry>,
    subagents: Arc<Subagents>,
    events: Arc<Mutex<Vec<Value>>>,
}

fn parent(f: &Fixture, url: &str, approve: tools::Approve, extra: Value) -> Parent {
    let mut cfg = json!({ "timeoutMs": 3000, "runTokenBudget": 100000 });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        cfg[k] = v;
    }
    let rt = runtime(&f.cwd, &f.data, url, cfg);
    let store = store(&rt);
    let tools = registry(&f.cwd, approve, false);
    let (emit, events) = events();
    let subagents = Subagents::new(rt.clone(), store.clone(), tools.clone(), emit);
    subagents.register().unwrap();
    Parent { rt, store, tools, subagents, events }
}

impl Parent {
    fn harness(&self) -> Arc<harness::Harness> {
        harness::Harness::new(self.rt.clone(), self.store.clone(), self.tools.clone(), harness::noop_emit(), None, Default::default()).unwrap()
    }
    async fn run(&self, prompt: &str) -> Result<String> {
        self.harness().run(prompt, &token(), None).await
    }
    fn child_dir(&self) -> PathBuf {
        self.subagents.subagents_root()
    }
}

fn recording(answer: impl Fn(&str) -> bool + Send + Sync + 'static) -> (tools::Approve, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(vec![]));
    let l = log.clone();
    let answer = Arc::new(answer);
    let approve: tools::Approve = Arc::new(move |req| {
        l.lock().unwrap().push(req.tool.clone());
        let ok = answer(&req.tool);
        Box::pin(async move { ok })
    });
    (approve, log)
}

fn locks_in(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|r| r.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".lock")).count()).unwrap_or(0)
}

#[tokio::test]
async fn a_subagent_write_denial_stops_the_parent_and_leaves_no_side_effect() {
    let f = fixture();
    let p = provider(|body, _| {
        if is_child(body) {
            return calls(vec![tc("write-child", "write_file", json!({ "path": "denied.txt", "content": "no" }))]);
        }
        calls(vec![tc("edit-agent", "delegate_edit", json!({ "tasks": ["Write denied.txt"] }))])
    })
    .await;
    let (approve, approvals) = recording(|tool| tool == "delegate_edit");
    let h = parent(&f, &p.url, approve, json!({}));
    let _ = h.run("parent-private-context").await;
    assert_eq!(*approvals.lock().unwrap(), vec!["delegate_edit", "write_file"]);
    assert_eq!(p.count(), 2);
    assert_eq!(h.store.session.lock().unwrap().messages.last().unwrap()["content"][0]["output"]["type"], "error-text");
    assert!(!f.exists("denied.txt"));
    assert_eq!(locks_in(&h.child_dir()), 0);
}

#[tokio::test]
async fn writable_delegated_tasks_run_sequentially_with_the_parent_approval_policy() {
    let f = fixture();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (a, m) = (active.clone(), maximum.clone());
    let p = provider(move |body, _| {
        if is_child(body) {
            assert!(!tool_names(body).iter().any(|n| n.starts_with("delegate_")));
            let task = body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap().rsplit('\n').next().unwrap().to_string();
            if !has_tool_results(body) {
                let now = a.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(now, Ordering::SeqCst);
                let mut r = calls(vec![tc("write-child", "write_file", json!({ "path": task, "content": task }))]);
                r.delay_ms = 30;
                return r;
            }
            a.fetch_sub(1, Ordering::SeqCst);
            return text(&format!("Created {task}"));
        }
        if !has_tool_results(body) {
            return calls(vec![tc("edit-agent", "delegate_edit", json!({ "tasks": ["a.txt", "b.txt"] }))]);
        }
        text("Finished edits.")
    })
    .await;
    let (approve, approvals) = recording(|_| true);
    let h = parent(&f, &p.url, approve, json!({}));
    assert_eq!(h.run("parent-private-context").await.unwrap(), "Finished edits.");
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    assert_eq!(*approvals.lock().unwrap(), vec!["delegate_edit", "write_file", "write_file"]);
    assert_eq!(f.read("a.txt"), "a.txt");
    assert_eq!(f.read("b.txt"), "b.txt");
}

#[tokio::test]
async fn child_usage_consumes_the_shared_parent_run_budget() {
    let f = fixture();
    let p = provider(|body, _| if is_child(body) { text("Child finished.") } else { calls(vec![tc("delegate1", "delegate_task", json!({ "tasks": ["Inspect files"] }))]) }).await;
    let h = parent(&f, &p.url, tools::approve_all(), json!({ "runTokenBudget": 200 }));
    assert_match(&h.run("parent-private-context").await.unwrap_err().to_string(), "token budget reached");
    assert_eq!(p.count(), 2);
}

#[tokio::test]
async fn terminal_child_events_follow_lock_release_and_capacity_is_reclaimed() {
    let f = fixture();
    let p = provider(|body, _| {
        if is_child(body) {
            return text("child report");
        }
        if !has_tool_results(body) {
            return calls(vec![tc("four", "delegate_task", json!({ "tasks": ["one", "two", "three", "four"] }))]);
        }
        text("handled children")
    })
    .await;
    let h = parent(&f, &p.url, tools::approve_all(), json!({}));
    let dir = h.child_dir();
    assert_eq!(h.run("four tasks").await.unwrap(), "handled children");
    let events = h.events.lock().unwrap();
    assert_eq!(events.iter().filter(|e| e["state"] == "start").count(), 4);
    assert_eq!(events.iter().filter(|e| e["state"] == "done").count(), 4);
    assert_eq!(locks_in(&dir), 0);
}

#[tokio::test]
async fn the_model_drives_codex_multi_agent_tools_and_flexible_delegate_inputs() {
    let f = fixture();
    let child_turns = Arc::new(AtomicUsize::new(0));
    let ct = child_turns.clone();
    let p = provider(move |body, _| {
        if is_child(body) {
            let n = ct.fetch_add(1, Ordering::SeqCst) + 1;
            return text(&format!("Child turn {n} report"));
        }
        let tool_msgs: Vec<&Value> = body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").collect();
        match tool_msgs.len() {
            0 => {
                let names = tool_names(body);
                for n in ["spawn_agent", "wait_agent", "send_input", "close_agent"] {
                    assert!(names.contains(&n.to_string()), "{n}");
                }
                let spawn = body["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "spawn_agent").unwrap();
                assert!(spawn["function"]["parameters"]["properties"].get("message").is_some());
                calls(vec![tc("spawn-1", "spawn_agent", json!({ "prompt": "Explore module A", "role": "explorer" })), tc("del-single", "delegate_task", json!({ "task": "Single string task" }))])
            }
            2 => {
                let spawn_out: Value = serde_json::from_str(tool_msgs[0]["content"].as_str().unwrap()).unwrap();
                assert_eq!(spawn_out["nickname"], "Euclid");
                assert_eq!(spawn_out["agent_type"], "explorer");
                assert_match(tool_msgs[1]["content"].as_str().unwrap(), "Child turn");
                calls(vec![tc("wait-1", "wait", json!({ "id": spawn_out["agent_id"] }))])
            }
            3 => {
                let wait_out: Value = serde_json::from_str(tool_msgs[2]["content"].as_str().unwrap()).unwrap();
                assert_eq!(wait_out["timed_out"], false);
                let status = wait_out["status"].as_object().unwrap().values().next().unwrap();
                assert_match(status["completed"].as_str().unwrap(), "Child turn");
                calls(vec![tc("send-1", "send_message", json!({ "id": "Euclid", "prompt": "Follow-up question", "wait": true })), tc("close-1", "close_agent", json!({ "id": "Euclid" }))])
            }
            _ => {
                assert_match(tool_msgs[3]["content"].as_str().unwrap(), "Child turn 3 report");
                text("All multi-agent steps complete.")
            }
        }
    })
    .await;
    let h = parent(&f, &p.url, tools::approve_all(), json!({}));
    assert_eq!(h.run("use subagents").await.unwrap(), "All multi-agent steps complete.");
    assert_eq!(child_turns.load(Ordering::SeqCst), 3);
    assert!(h.subagents.list_subagent_summaries().iter().any(|s| s.nickname == "Euclid" && s.status == "shutdown"));
    h.subagents.close_all().await;
}

#[tokio::test]
async fn codex_tools_skills_side_questions_and_copy_extraction_work_end_to_end() {
    use base64::Engine;
    let f = fixture();
    f.write(".codex/skills/audit-sec/SKILL.md", "---\nname: audit-sec\ndescription: Security audit checklist\n---\nAlways check SQL injection and SSRF.");
    let skills = context::discover_skills(&f.cwd);
    assert!(skills.iter().any(|s| s.name == "audit-sec" && s.description == "Security audit checklist"));
    let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==").unwrap();
    std::fs::write(f.cwd.join("sample.png"), png).unwrap();
    let p = provider(|body, _| {
        let messages = body["messages"].as_array().unwrap();
        let last_user = messages.iter().rev().find(|m| m["role"] == "user");
        if last_user.and_then(|m| m["content"].as_str()).is_some_and(|c| c.contains("Ephemeral Side Question (/btw)")) {
            return text("Side answer: 42");
        }
        if !has_tool_results(body) {
            assert!(body["messages"].to_string().contains("Always check SQL injection and SSRF."));
            return calls(vec![
                tc(
                    "plan-1",
                    "update_plan",
                    json!({ "explanation": "Initial steps", "steps": [{ "step": "Inspect sample.png", "status": "in_progress" }, { "step": "Ask user preference", "status": "pending" }] }),
                ),
                tc("ask-1", "ask_user", json!({ "id": "mode_choice", "question": "Which mode should we use?", "options": ["Strict (Recommended)", "Permissive"] })),
                tc("img-1", "view_image", json!("sample.png")),
            ]);
        }
        text("Completed task.\n```ts\nconst ok = true;\n```")
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "timeoutMs": 3000 }));
    let store = store(&rt);
    let tools = registry(&f.cwd, tools::approve_all(), false);
    tools.callbacks.lock().unwrap().ask_user = Some(Arc::new(|questions: Vec<Value>, _| {
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0]["id"], "mode_choice");
        Box::pin(async { Ok([("mode_choice".to_string(), vec!["Strict (Recommended)".to_string()])].into_iter().collect()) })
    }));
    let expanded = context::expand_file_mentions("Run $audit-sec now", &tools.files, &Default::default());
    assert!(expanded.attached_paths.contains(&"$audit-sec".to_string()));
    let h = harness::Harness::new(rt, store.clone(), tools.clone(), harness::noop_emit(), None, Default::default()).unwrap();
    assert_match(&h.run("Run $audit-sec now", &token(), None).await.unwrap(), "Completed task");
    let todos = tools.todos();
    assert_eq!(todos.len(), 2);
    assert_eq!(todos[0].content, "Inspect sample.png");
    let before = store.session.lock().unwrap().messages.len();
    let (side, _, _) = h.ask_side_question("What is the side answer?", &token()).await.unwrap();
    assert_eq!(side, "Side answer: 42");
    let messages = store.session.lock().unwrap().messages.clone();
    assert_eq!(messages.len(), before);
    assert!(cli::extract_copyable_from_messages(&messages, "last").unwrap().0.contains("Completed task."));
    assert_eq!(cli::extract_copyable_from_messages(&messages, "code").unwrap().0, "const ok = true;");
}

fn price_budget(ledger: Arc<cost::CostLedger>, started_at: i64) -> cost::SharedBudget {
    Arc::new(Mutex::new(cost::RunBudget { spent: 0, limit: 100_000, cost_spent: 0.0, cost_limit: 100.0, started_at: Some(started_at), time_limit_ms: 600_000, ledger: Some(ledger) }))
}

#[tokio::test]
async fn parallel_child_costs_are_aggregated_per_child_session() {
    let f = fixture();
    let child_requests = Arc::new(AtomicUsize::new(0));
    let cr = child_requests.clone();
    let p = provider(move |body, _| {
        if is_child(body) {
            let n = cr.fetch_add(1, Ordering::SeqCst) + 1;
            let mut r = text(&format!("Report from child {n}"));
            r.delay_ms = 150;
            return r;
        }
        if !has_tool_results(body) {
            return calls(vec![tc("delegate1", "delegate_task", json!({ "tasks": ["alpha", "beta"] }))]);
        }
        text("Combined.")
    })
    .await;
    let pricing = json!({ "test-model": { "inputPer1M": 10_000, "outputPer1M": 0 } });
    let h = parent(&f, &p.url, tools::approve_all(), json!({ "pricing": pricing }));
    let ledger = cost::CostLedger::from_config(&h.rt, Some(&h.rt.model)).unwrap();
    let started_at = jsutil::now_millis() - 5_000;
    let budget = price_budget(ledger.clone(), started_at);
    h.harness().run("delegate", &token(), Some(budget.clone())).await.unwrap();
    assert_eq!(child_requests.load(Ordering::SeqCst), 2);
    let cost = h.store.session.lock().unwrap().cost();
    assert!((cost - 4.0).abs() < 0.01, "parent session cost should be $4, got {cost}");
    assert!((ledger.total() - 4.0).abs() < 0.01, "shared ledger should be $4, got {}", ledger.total());
    let b = budget.lock().unwrap();
    assert!((b.cost_spent - 4.0).abs() < 0.01, "budget mirror should be $4, got {}", b.cost_spent);
    assert_eq!(b.started_at, Some(started_at));
    assert_eq!(b.time_limit_ms, 600_000);
}

#[tokio::test]
async fn child_registries_inherit_the_parent_network_policy_and_deny_before_approval() {
    let f = fixture();
    let p = provider(|body, _| {
        if is_child(body) {
            if !has_tool_results(body) {
                return calls(vec![tc("child-curl", "bash", json!({ "command": "curl https://example.com" }))]);
            }
            return text("Child stopped after the denial.");
        }
        if !has_tool_results(body) {
            return calls(vec![tc("edit-agent", "delegate_edit", json!({ "tasks": ["fetch example.com"] }))]);
        }
        text("Parent done.")
    })
    .await;
    let (approve, approvals) = recording(|_| true);
    let h = parent(&f, &p.url, approve, json!({}));
    h.tools.set_policy(Some(deny_policy()));
    assert_eq!(h.run("delegate").await.unwrap(), "Parent done.");
    assert_eq!(*approvals.lock().unwrap(), vec!["delegate_edit"]);
    let transcripts: Vec<String> =
        std::fs::read_dir(h.child_dir()).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".json")).map(|e| std::fs::read_to_string(e.path()).unwrap()).collect();
    assert!(transcripts.iter().any(|t| t.to_lowercase().contains("network access")));
}

#[tokio::test]
async fn delegate_tools_ignore_model_supplied_allowed_tools() {
    let f = fixture();
    let child_tools = Arc::new(Mutex::new(vec![]));
    let ct = child_tools.clone();
    let p = provider(move |body, _| {
        if is_child(body) {
            *ct.lock().unwrap() = tool_names(body);
            return text("Read-only report.");
        }
        if !has_tool_results(body) {
            return calls(vec![tc("delegate-escalate", "delegate_task", json!({ "tasks": ["inspect"], "allowedTools": ["bash", "write_file", "mcp__server__tool"] }))]);
        }
        text("Done.")
    })
    .await;
    let h = parent(&f, &p.url, tools::approve_all(), json!({}));
    assert_eq!(h.run("parent-private-context").await.unwrap(), "Done.");
    assert_eq!(*child_tools.lock().unwrap(), vec!["list_files", "read_file", "search"]);
}

#[tokio::test]
async fn agent_network_shares_the_parent_ledger_and_mirrors_cost_back_to_the_budget() {
    let f = fixture();
    let p = provider(|_, _| text("Persona view.")).await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "timeoutMs": 3000, "runTokenBudget": 100000, "pricing": { "test-model": { "inputPer1M": 10_000, "outputPer1M": 0 } } }));
    let st = store(&rt);
    let ledger = cost::CostLedger::from_config(&rt, Some(&rt.model)).unwrap();
    let started_at = jsutil::now_millis() - 1_000;
    let budget = price_budget(ledger.clone(), started_at);
    let net = network::AgentNetwork::new(rt.clone(), st.clone(), network::AgentNetworkBus::new());
    let result = net
        .brainstorm(network::BrainstormOptions {
            topic: "Review the design".into(),
            roles: Some(vec!["architect".into(), "critic".into()]),
            rounds: Some(1),
            explore_codebase: Some(false),
            cancel: token(),
            budget: Some(budget.clone()),
            policy: Some(deny_policy()),
            on_progress: None,
        })
        .await
        .unwrap();
    assert_eq!(result.participants.len(), 2);
    let cost = st.session.lock().unwrap().cost();
    assert!((cost - 3.0).abs() < 0.01, "session cost was {cost}");
    let b = budget.lock().unwrap();
    assert!((b.cost_spent - 3.0).abs() < 0.01, "budget mirror was {}", b.cost_spent);
    assert!((ledger.total() - 3.0).abs() < 0.01, "ledger total was {}", ledger.total());
    assert_eq!(b.started_at, Some(started_at));
    assert_eq!(b.time_limit_ms, 600_000);
}
