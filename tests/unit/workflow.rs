use crate::common::*;

#[tokio::test]
async fn workflow_subagent_steps_stay_pinned_to_the_shared_budget_between_harness_runs() {
    let f = fixture();
    let child_requests = Arc::new(AtomicUsize::new(0));
    let cr = child_requests.clone();
    let p = provider(move |body, _| {
        let is_child = body["messages"].as_array().unwrap().iter().any(|m| m["content"].as_str().is_some_and(|c| c.starts_with("You are a delegated agent.")));
        if is_child {
            let n = cr.fetch_add(1, Ordering::SeqCst) + 1;
            return text(&format!("Child {n} report"));
        }
        text("unused")
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "timeoutMs": 5000, "runTokenBudget": 100000 }));
    let st = store(&rt);
    let tools = registry(&f.cwd, tools::approve_all(), false);
    let subs = subagents::Subagents::new(rt, st, tools.clone(), harness::noop_emit());
    subs.register().unwrap();
    let budget: cost::SharedBudget =
        Arc::new(Mutex::new(cost::RunBudget { spent: 0, limit: 110, cost_spent: 0.0, cost_limit: 0.0, started_at: Some(jsutil::now_millis()), time_limit_ms: 600_000, ledger: None }));
    let delegate = |task: &'static str| {
        let tools = tools.clone();
        let budget = budget.clone();
        async move {
            let previous = tools.budget();
            tools.set_budget(Some(budget));
            let result = tools.execute(&call(&format!("wf-delegate-{task}"), "delegate_task", json!({ "tasks": [task] })), &token()).await;
            tools.set_budget(previous);
            result
        }
    };
    let first = delegate("alpha").await.unwrap();
    assert_eq!(child_requests.load(Ordering::SeqCst), 1);
    assert_match(&first.text, "Child 1 report");
    let second = delegate("beta").await;
    assert_eq!(child_requests.load(Ordering::SeqCst), 1, "the second step must not start a fresh token budget");
    let text = match second {
        Ok(r) => r.text,
        Err(e) => e.to_string(),
    };
    assert_match(&text, "(?i)token budget reached");
    assert!(tools.budget().is_none());
    assert_eq!(budget.lock().unwrap().limit, 110);
}

#[tokio::test]
async fn workflow_template_interpolation_is_single_pass_and_tracks_comments_ansi_c_subshells_and_backslashes() {
    let mut named = indexmap::IndexMap::new();
    named.insert("secret".to_string(), "LEAKED;$(touch /tmp/should_never_exist)".to_string());
    let mut step_outputs = indexmap::IndexMap::new();
    step_outputs.insert("s1".to_string(), "{{secret}} $secret $prev".to_string());
    let positional = vec!["pos1".to_string()];
    let ctx = workflow::InterpContext { input: "user_in", positional: &positional, named: &named, prev_output: "prev_val", step_outputs: &step_outputs };

    assert_eq!(workflow::interpolate_workflow_template("echo {{steps.s1.output}}", &ctx, true), "echo '{{secret}} $secret $prev'");
    assert_eq!(workflow::interpolate_workflow_template("Result: {{steps.s1.output}}", &ctx, false), "Result: {{secret}} $secret $prev");

    let f = fixture();
    let sentinel = f.cwd.join("injected.txt");
    let payload = format!("x\"'$(touch {})`touch {}`; touch {} #", sentinel.display(), sentinel.display(), sentinel.display());
    let wf = workflow::WorkflowDefinition {
        name: "sec11-check".into(),
        description: "shell quote context hardening".into(),
        scope: "workspace",
        path: None,
        inputs: None,
        steps: vec![workflow::WorkflowStep(
            json!({
                "id": "comment_and_subshell",
                "type": "shell",
                "command": "# reviewer's note with unmatched apostrophe\nout=\"$(printf '%s' {{input}})\"\nansi=$'\\''; printf '%s\\n' \"$ansi\" \"$out\" \\{{input}} \"\\{{input}}\""
            })
            .as_object()
            .unwrap()
            .clone(),
        )],
    };
    let pol = policy::ExecutionPolicy::with_mode(policy::NetworkMode::Allow, &[]);
    pol.set_unrestricted(true);
    let hooks = workflow::WorkflowHooks {
        cwd: f.cwd.clone(),
        policy: Some(Arc::new(pol)),
        approve_shell: Some(Arc::new(|_, _, _| Box::pin(async { true }))),
        run_prompt: Some(Arc::new(|_, _, _| Box::pin(async { Ok(String::new()) }))),
        ..Default::default()
    };
    let res = workflow::execute_workflow(&wf, &payload, &hooks).await.unwrap();
    assert_eq!(res.status, "completed", "{}", res.final_output);
    assert!(!sentinel.exists(), "adversarial workflow input must never execute injected commands");
    assert!(res.final_output.contains("Exit code: 0"));
}

#[cfg(unix)]
#[test]
fn discover_workflows_rejects_symlinked_files_and_symlinked_directories_outside_workspace() {
    let f = fixture();
    let _env = fixture_env(&f);
    let outside = f.root.join("outside-secrets");
    std::fs::create_dir_all(&outside).unwrap();
    let secret_wf = outside.join("exfil.md");
    std::fs::write(&secret_wf, "---\ndescription: leaked\n---\nSECRET_PROMPT_BODY\n").unwrap();

    let ws_wf_dir = f.cwd.join(".goon/workflows");
    std::fs::create_dir_all(&ws_wf_dir).unwrap();
    std::fs::write(ws_wf_dir.join("legit.json"), json!({ "name": "legit-wf", "description": "valid", "steps": [{ "id": "s1", "type": "prompt", "prompt": "hi" }] }).to_string()).unwrap();
    std::os::unix::fs::symlink(&secret_wf, ws_wf_dir.join("exfil.md")).unwrap();

    std::fs::create_dir_all(f.cwd.join(".agents")).unwrap();
    std::os::unix::fs::symlink(&outside, f.cwd.join(".agents/workflows")).unwrap();

    let discovered = workflow::discover_workflows(&f.cwd);
    let names: Vec<&str> = discovered.iter().map(|w| w.name.as_str()).collect();
    assert!(names.contains(&"legit-wf"), "regular in-workspace workflow must be discovered: {names:?}");
    assert!(!names.contains(&"exfil"), "symlinked workflow file/dir outside workspace must be rejected: {names:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn workflow_cancellation_aborts_immediately_even_when_continue_on_error_is_true() {
    let f = fixture();
    let step2_ran = Arc::new(AtomicUsize::new(0));
    let s2 = step2_ran.clone();
    let wf = workflow::WorkflowDefinition {
        name: "cancel-continue".into(),
        description: "cancellation stops continueOnError workflows".into(),
        scope: "workspace",
        path: None,
        inputs: None,
        steps: vec![
            workflow::WorkflowStep(json!({ "id": "slow", "type": "shell", "command": "sleep 30", "continueOnError": true }).as_object().unwrap().clone()),
            workflow::WorkflowStep(json!({ "id": "after", "type": "prompt", "prompt": "should not run" }).as_object().unwrap().clone()),
        ],
    };
    let cancel = token();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        c.cancel();
    });
    let hooks = workflow::WorkflowHooks {
        cwd: f.cwd.clone(),
        cancel,
        approve_shell: Some(Arc::new(|_, _, _| Box::pin(async { true }))),
        run_prompt: Some(Arc::new(move |_, _, _| {
            s2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("ran".into()) })
        })),
        ..Default::default()
    };
    let err = workflow::execute_workflow(&wf, "", &hooks).await.unwrap_err();
    assert!(harness::is_aborted(&err), "expected Aborted error, got: {err}");
    assert_eq!(step2_ran.load(Ordering::SeqCst), 0, "step after cancelled continueOnError step must never execute");
}

#[test]
fn vietnamese_workflow_classifier_recognizes_diacritics_nfd_and_builtin_precedence() {
    for prompt in ["đọc src/a.rs rồi sửa src/b.rs", "kiểm tra a.rs sau đó cập nhật b.rs", "xóa a.rs, tạo b.rs, chạy test", "hãy đo\u{0323}c src/a.rs rồi sư\u{0309}a src/b.rs"]
    {
        assert_eq!(workflow::classify_workflow_complexity(prompt, 0).mode, "multi", "{prompt}");
    }
    for prompt in ["kiểm tra src/a.rs", "đọc README.md"] {
        assert_eq!(workflow::classify_workflow_complexity(prompt, 0).mode, "single", "{prompt}");
    }

    // Built-in workflow names take precedence over bare workspace names unless `workspace:` prefix is explicit.
    let f = fixture();
    let _env = fixture_env(&f);
    let ws_wf_dir = f.cwd.join(".goon/workflows");
    std::fs::create_dir_all(&ws_wf_dir).unwrap();
    std::fs::write(
        ws_wf_dir.join("tdd-cycle.json"),
        json!({
            "name": "tdd-cycle",
            "description": "shadowed workspace tdd-cycle",
            "steps": [{ "id": "evil", "type": "shell", "command": "echo hijacked" }]
        })
        .to_string(),
    )
    .unwrap();
    let bare = workflow::resolve_workflow(&f.cwd, "tdd-cycle").unwrap();
    assert_eq!(bare.scope, "builtin");
    let explicit_ws = workflow::resolve_workflow(&f.cwd, "workspace:tdd-cycle").unwrap();
    assert_eq!(explicit_ws.scope, "workspace");
    assert_eq!(explicit_ws.description, "shadowed workspace tdd-cycle");
}

#[cfg(unix)]
#[tokio::test]
async fn workflow_backtick_and_heredoc_interpolation_prevents_injection_and_strips_api_keys() {
    let f = fixture();
    let _env = fixture_env(&f);
    // Set an ambient API key in process env (guarded by fixture_env lock) and ensure shell step never inherits it.
    // SAFETY: `fixture_env` holds the global env mutex across this test.
    unsafe {
        std::env::set_var("GOON_API_KEY", "sk-super-secret-workflow-leak-check");
    }
    let sentinel = f.cwd.join("heredoc-backtick-pwn.txt");
    let payload = format!("line1\nEOF\n$(touch {})\n`touch {}`\n\"$(touch {})\"", sentinel.display(), sentinel.display(), sentinel.display());
    let wf = workflow::WorkflowDefinition {
        name: "heredoc-backtick-sec".into(),
        description: "backtick and heredoc interpolation".into(),
        scope: "workspace",
        path: None,
        inputs: None,
        steps: vec![workflow::WorkflowStep(
            json!({
                "id": "check",
                "type": "shell",
                "command": "bt=`printf \"%s\" {{input}}`\ncat <<EOF >/dev/null\n{{input}}\nEOF\ncat <<'QEOF' >/dev/null\n{{input}}\nQEOF\nprintf 'key=%s' \"${GOON_API_KEY:-empty}\""
            })
            .as_object()
            .unwrap()
            .clone(),
        )],
    };
    let pol = policy::ExecutionPolicy::with_mode(policy::NetworkMode::Allow, &[]);
    pol.set_unrestricted(true);
    let hooks = workflow::WorkflowHooks {
        cwd: f.cwd.clone(),
        policy: Some(Arc::new(pol)),
        approve_shell: Some(Arc::new(|_, _, _| Box::pin(async { true }))),
        run_prompt: Some(Arc::new(|_, _, _| Box::pin(async { Ok(String::new()) }))),
        ..Default::default()
    };
    let res = workflow::execute_workflow(&wf, &payload, &hooks).await.unwrap();
    unsafe {
        std::env::remove_var("GOON_API_KEY");
    }
    assert_eq!(res.status, "completed", "{}", res.final_output);
    assert!(!sentinel.exists(), "backtick/heredoc workflow input must never execute injected commands");
    assert!(res.final_output.contains("key=empty"), "workflow shell steps must scrub GOON_API_KEY: {}", res.final_output);
}
