//! Custom workflows and the agent network (the pairing server is tested in src/pair.rs).
use crate::common::*;
use goon::network::{AgentNetwork, AgentNetworkBus, BrainstormOptions, NetworkNodeInfo};
use goon::workflow::{self, InterpContext, WorkflowHooks};
use indexmap::IndexMap;

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn custom_workflows_escape_shell_keep_awk_fields_gate_approvals_and_isolate_discovery() {
    let f = fixture();
    let _env = fixture_env(&f);
    let named: IndexMap<String, String> = [("target".to_string(), "src/auth.ts".to_string())].into_iter().collect();
    let outputs: IndexMap<String, String> = [("s1".to_string(), "step1-done".to_string())].into_iter().collect();
    let positional = strings(&["auth", "module"]);
    let ctx = InterpContext { input: "auth module", positional: &positional, named: &named, prev_output: "prev-ok", step_outputs: &outputs };
    assert_eq!(
        workflow::interpolate_workflow_template("Task: $input | First: $1 | Named: {{target}} | Prev: $prev | Step1: {{steps.s1.output}}", &ctx, false),
        "Task: auth module | First: auth | Named: src/auth.ts | Prev: prev-ok | Step1: step1-done"
    );
    let empty = IndexMap::new();
    let fix = strings(&["fix"]);
    let ctx = InterpContext { input: "x\"; rm -rf ~; echo \"", positional: &fix, named: &empty, prev_output: "a; touch /tmp/pwned", step_outputs: &empty };
    let shell = workflow::interpolate_workflow_template("grep -rn \"{{input}}\" src | awk '{print $1, $2}' && echo $prev", &ctx, true);
    assert!(shell.contains("awk '{print $1, $2}'"), "{shell}");
    assert!(shell.contains("\"x\\\"; rm -rf ~; echo \\\"\""), "{shell}");
    assert!(shell.contains("'a; touch /tmp/pwned'"), "{shell}");

    let (_, created) = workflow::create_workflow_scaffold(&f.cwd, "my-pipeline", Some("Custom release pipeline"), false, false).unwrap();
    assert_eq!(created.name, "my-pipeline");
    assert_match(&workflow::create_workflow_scaffold(&f.cwd, "my-pipeline", Some("Attempt overwrite"), false, false).unwrap_err().to_string(), "already exists");

    f.write("workflows/untrusted.md", "## Step 1 [shell]\n```bash\necho pwned\n```\n");
    let md = [
        "---",
        "name: md-flow",
        "description: Markdown multi-step workflow",
        "---",
        "## Step 1: Check Environment [type=shell id=env_check]",
        "```bash",
        "echo \"env-ok-{{1}}\" | awk '{print $1}'",
        "```",
        "## Step 2: Brainstorm [type=network id=net roles=architect|critic rounds=1]",
        "Design $input with context: $prev",
        "## Step 3: Implement [type=prompt id=impl]",
        "Implement $input using:\n{{steps.net.output}}",
    ]
    .join("\n");
    f.write(".goon/workflows/md-flow.md", &md);
    let parsed = workflow::parse_markdown_workflow("md-flow.md", &md, "workspace", &f.cwd.join(".goon/workflows/md-flow.md"));
    let all = workflow::discover_workflows(&f.cwd);
    assert!(!all.iter().any(|w| w.name == "untrusted"), "root workflows/ must be ignored");
    assert!(all.iter().any(|w| w.name == "my-pipeline" && w.scope == "workspace"));
    assert!(all.iter().any(|w| w.name == "md-flow" && w.scope == "workspace"));
    assert!(all.iter().any(|w| w.name == "feature-dev" && w.scope == "builtin"));

    let noop: Arc<dyn Fn(String, workflow::WorkflowStep, CancellationToken) -> workflow::StepFuture + Send + Sync> = Arc::new(|_, _, _| Box::pin(async { Ok("noop".to_string()) }));
    let base = WorkflowHooks { cwd: f.cwd.clone(), run_prompt: Some(noop.clone()), ..Default::default() };
    let blocked = workflow::execute_workflow(&parsed, "login-page", &WorkflowHooks { effective_read_only: true, ..base.clone() }).await.unwrap();
    assert_eq!(blocked.status, "failed");
    assert_match(&blocked.final_output, "blocked in read-only or plan mode");
    let denied = workflow::execute_workflow(&parsed, "login-page", &WorkflowHooks { approve_shell: Some(Arc::new(|_, _, _| Box::pin(async { false }))), ..base.clone() }).await.unwrap();
    assert_eq!(denied.status, "failed");
    assert_match(&denied.final_output, "Permission denied");

    let marker = f.cwd.join("injected.txt");
    let approved = Arc::new(Mutex::new(vec![]));
    let prompts = Arc::new(Mutex::new(vec![]));
    let (a, p) = (approved.clone(), prompts.clone());
    let hooks = WorkflowHooks {
        cwd: f.cwd.clone(),
        approve_shell: Some(Arc::new(move |cmd, _, _| {
            a.lock().unwrap().push(cmd);
            Box::pin(async { true })
        })),
        run_prompt: Some(Arc::new(move |text, _, _| {
            p.lock().unwrap().push(text);
            Box::pin(async { Ok("implemented-ok".to_string()) })
        })),
        run_network: Some(Arc::new(|topic, roles, rounds, _| {
            Box::pin(async move {
                assert_match(&topic, "env-ok-");
                assert_eq!(roles, Some(strings(&["architect", "critic"])));
                assert_eq!(rounds, Some(1));
                Ok("consensus-blueprint-ready".to_string())
            })
        })),
        ..Default::default()
    };
    let payload = format!("\"login\\\"; touch {}; echo \\\"\"", marker.to_string_lossy().replace('\\', "/"));
    let run = workflow::execute_workflow(&parsed, &payload, &hooks).await.unwrap();
    assert_eq!(run.status, "completed", "{}", run.final_output);
    assert_eq!(approved.lock().unwrap().len(), 1);
    assert!(!marker.exists(), "shell injection payload must not execute");
    assert_eq!(prompts.lock().unwrap().len(), 1);

    workflow::remove_custom_workflow(&f.cwd, "my-pipeline").unwrap();
    assert!(!workflow::discover_workflows(&f.cwd).iter().any(|w| w.name == "my-pipeline"));
}

fn node(id: &str, nickname: &str, role: &str) -> NetworkNodeInfo {
    NetworkNodeInfo { id: id.into(), nickname: nickname.into(), role: role.into(), status: "running".into(), task: None }
}

fn first_user(body: &Value) -> String {
    body["messages"].as_array().and_then(|m| m.iter().find(|m| m["role"] == "user")).and_then(|m| m["content"].as_str()).unwrap_or("").to_string()
}

#[tokio::test]
async fn agent_network_bus_isolated_brainstorm_live_round_two_and_cleanup() {
    let f = fixture();
    let bus = AgentNetworkBus::new();
    bus.register_node(node("a1", "Euclid", "explorer"));
    bus.register_node(node("a2", "Archimedes", "worker"));
    bus.send("Euclid", "Archimedes", "Found bug in line 42", Some("finding"), Some("auth"));
    bus.send("Euclid", "*", "Use token bucket rate limiting", Some("idea"), None);
    bus.post_blackboard("Euclid", "explorer", "Architecture", "Proposed schema v2", Some("idea"), None);
    assert!(bus.read_inbox("Euclid", false).is_empty());
    let inbox = bus.drain_unread_prompt_block("Archimedes");
    assert_match(&inbox, "Found bug in line 42");
    assert_match(&inbox, "Use token bucket rate limiting");
    assert_eq!(bus.drain_unread_prompt_block("Archimedes"), "");
    assert_match(&bus.format_board_summary(10), "Proposed schema v2");

    let p = provider(|body, _| {
        let names: Vec<String> = body["tools"].as_array().map(|t| t.iter().filter_map(|t| t["function"]["name"].as_str().map(String::from)).collect()).unwrap_or_default();
        let user = first_user(body);
        if user.contains("Lead Moderator") {
            assert!(names.is_empty());
            for needle in ["Architect proposal R1", "Critic proposal R1", "Architect debate R2"] {
                assert!(user.contains(needle), "{needle}");
            }
            return text("### Consensus\nUse clean event-driven architecture.");
        }
        for hidden in ["read_file", "list_files", "search"] {
            assert!(!names.iter().any(|n| n == hidden), "{hidden} must not be exposed");
        }
        assert!(names.iter().any(|n| n == "peer_send"));
        if user.contains("Instructions for Round 1:") {
            let arch = user.starts_with("You are **Architect**");
            let has_tool = body["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool");
            if arch && !has_tool {
                return calls(vec![tc("p_send_1", "peer_send", json!({ "to": "Critic", "message": "What edge cases do you see in SSE?", "kind": "question" }))]);
            }
            return text(if arch { "Architect proposal R1" } else { "Critic proposal R1" });
        }
        if user.contains("Instructions for Round 2:") {
            let critic = user.starts_with("You are **Critic**");
            if critic {
                assert!(user.contains("What edge cases do you see in SSE?"));
                assert!(user.contains("Architect debate R2"));
            }
            return text(if critic { "Critic debate R2" } else { "Architect debate R2" });
        }
        text("Fallback")
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "runTokenBudget": 0 }));
    let st = store(&rt);
    let net = AgentNetwork::new(rt.clone(), st.clone(), bus);
    let result = net
        .brainstorm(BrainstormOptions {
            topic: "Design real-time SSE pairing".into(),
            roles: Some(strings(&["architect", "critic"])),
            rounds: Some(2),
            explore_codebase: Some(false),
            cancel: token(),
            budget: None,
            policy: None,
            on_progress: None,
        })
        .await
        .unwrap();
    assert_eq!(result.rounds, 2);
    assert_eq!(result.participants.len(), 2);
    assert_eq!(result.contributions.len(), 4);
    assert_match(&result.synthesis, "Use clean event-driven architecture");
    assert!(p.count() >= 6);
    let leftover = std::fs::read_dir(net.sessions_root()).map(|d| d.count()).unwrap_or(0);
    assert_eq!(leftover, 0, "ephemeral network sessions must be cleaned up");

    let tools = registry(&f.cwd, tools::approve_all(), false);
    let subs = subagents::Subagents::new(rt, st, tools.clone(), harness::noop_emit());
    subs.register().unwrap();
    assert!(!tools.is_read_only_tool("agent_network"));
    tools.set_plan_mode(true);
    assert!(!tools.schemas().iter().any(|s| s.name == "agent_network"));
    tools.set_plan_mode(false);
    assert!(tools.schemas().iter().any(|s| s.name == "agent_network"));
    subs.close_all().await;
}
