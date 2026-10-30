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
