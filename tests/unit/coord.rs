use crate::common::*;
use goon::coord::*;

fn todo(id: &str, role: &str, deps: &[&str]) -> Todo {
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

fn plan(todos: Vec<Todo>) -> Plan {
    Plan { id: "plan-1".into(), goal: "ship it".into(), todos, created_at: "2026-01-01T00:00:00.000Z".into() }
}

fn invalid(p: Plan) -> String {
    validate_plan(&p).expect_err("expected a validation error").0
}

#[test]
fn validate_plan_accepts_a_well_formed_dag() {
    validate_plan(&plan(vec![todo("t1", "researcher", &[]), todo("t2", "coder", &["t1"]), todo("t3", "tester", &["t2"])])).unwrap();
}

#[test]
fn validate_plan_rejects_duplicate_empty_and_unknown_dependency_ids() {
    assert_match(&invalid(plan(vec![todo("t1", "coder", &[]), todo("t1", "coder", &[])])), "(?i)duplicate");
    invalid(plan(vec![todo("  ", "coder", &[])]));
    assert_match(&invalid(plan(vec![todo("t1", "coder", &["ghost"])])), "(?i)unknown");
    invalid(plan(vec![]));
}

#[test]
fn validate_plan_rejects_self_dependencies_and_cycles() {
    assert_match(&invalid(plan(vec![todo("t1", "coder", &["t1"])])), "(?i)itself");
    assert_match(&invalid(plan(vec![todo("t1", "coder", &["t2"]), todo("t2", "coder", &["t1"])])), "(?i)cycle");
    validate_plan(&plan(vec![todo("t1", "coder", &[]), todo("t2", "coder", &["t1"]), todo("t3", "coder", &["t1"]), todo("t4", "tester", &["t2", "t3"])])).unwrap();
}

#[test]
fn validate_plan_rejects_invalid_roles() {
    assert_match(&invalid(plan(vec![todo("t1", "wizard", &[])])), "(?i)role");
}

#[test]
fn plan_from_planner_applies_defaults_and_dedupes_deps() {
    let p =
        plan_from_planner(&json!({ "goal": "g", "todos": [{ "id": "t1", "title": "a", "role": "coder", "dependsOn": ["t2", "t2"] }, { "id": "t2", "title": "b", "role": "researcher" }] }), "plan-x")
            .unwrap();
    assert_eq!(p.id, "plan-x");
    assert_eq!(p.todos[0].depends_on, vec!["t2"]);
    assert_eq!(p.todos[0].status, "pending");
    assert_eq!(p.todos[0].rework_cycles, 0);
    let error = plan_from_planner(&json!({ "goal": "g", "todos": [{ "id": "t1", "title": "a", "role": "wizard" }] }), "p").unwrap_err();
    assert_match(&error.to_string(), "(?i)role");
}

fn ids(todos: &[Todo]) -> Vec<String> {
    todos.iter().map(|t| t.id.clone()).collect()
}

#[test]
fn scheduler_deps_met_only_when_all_deps_done_and_failures_block_transitively() {
    let mut s = Scheduler::new(plan(vec![todo("t1", "coder", &[]), todo("t2", "coder", &["t1"]), todo("t3", "tester", &["t2"])]), DEFAULT_DAG_WRITE_CONCURRENCY, DEFAULT_DAG_MAX_CONCURRENCY);
    assert_eq!(ids(&s.ready_todos()), vec!["t1"]);
    s.mark_done("t1");
    assert_eq!(ids(&s.ready_todos()), vec!["t2"]);
    let t2 = s.plan.todos[1].clone();
    assert!(s.deps_met(&t2));
    s.mark_done("t2");
    assert_eq!(ids(&s.ready_todos()), vec!["t3"]);

    let mut s2 = Scheduler::new(plan(vec![todo("a", "coder", &[]), todo("b", "coder", &["a"]), todo("c", "tester", &["b"])]), 1, 2);
    s2.mark_done("a");
    assert_eq!(s2.mark_failed("b"), vec!["c"]);
    assert_eq!(s2.plan.todos[2].status, "blocked");
    assert!(s2.ready_todos().is_empty());
    assert!(s2.check_deadlock().is_empty());
}

#[test]
fn scheduler_dispatch_ready_respects_write_serial_and_read_parallel_bounds() {
    let mut s = Scheduler::new(plan(vec![todo("r1", "researcher", &[]), todo("r2", "reviewer", &[]), todo("r3", "researcher", &[]), todo("w1", "coder", &[]), todo("w2", "coder", &[])]), 1, 2);
    let mut first = ids(&s.dispatch_ready());
    first.sort();
    assert_eq!(first, vec!["r1", "r2", "w1"]);
    assert_eq!(s.status_of("w2"), "pending");
    assert_eq!(s.status_of("r3"), "pending");
    s.mark_done("w1");
    assert_eq!(ids(&s.dispatch_ready()), vec!["w2"]);
    s.mark_done("r1");
    s.mark_done("r2");
    s.mark_done("w2");
    assert_eq!(ids(&s.dispatch_ready()), vec!["r3"]);
}

#[test]
fn scheduler_detects_deadlock_when_nothing_is_inflight_but_todos_remain_pending() {
    let mut s = Scheduler::new(plan(vec![todo("a", "coder", &[]), todo("b", "coder", &["a"])]), 1, 2);
    s.plan.todos[0].status = "in_progress".into();
    assert_eq!(s.check_deadlock(), vec!["b"]);
}

#[test]
fn apply_review_verdict_counts_rework_and_fails_past_the_cap() {
    let mut t = todo("t1", "coder", &[]);
    t.status = "in_progress".into();
    assert_eq!(apply_review_verdict(&mut t, true, DEFAULT_MAX_REWORK_CYCLES), "approved");
    assert_eq!(t.rework_cycles, 0);
    for _ in 0..DEFAULT_MAX_REWORK_CYCLES {
        assert_eq!(apply_review_verdict(&mut t, false, DEFAULT_MAX_REWORK_CYCLES), "rework");
        assert_eq!(t.status, "pending");
    }
    assert_eq!(t.rework_cycles, DEFAULT_MAX_REWORK_CYCLES);
    assert_eq!(apply_review_verdict(&mut t, false, DEFAULT_MAX_REWORK_CYCLES), "failed");
    assert_eq!(t.status, "failed");
    let mut strict = todo("t2", "coder", &[]);
    assert_eq!(apply_review_verdict(&mut strict, false, 1), "rework");
    assert_eq!(apply_review_verdict(&mut strict, false, 1), "failed");
}

fn done_plan() -> Plan {
    let mut p = plan(vec![todo("t1", "coder", &[]), todo("t2", "coder", &[])]);
    for t in &mut p.todos {
        t.status = "done".into();
    }
    p
}

fn diffs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
async fn merge_joins_done_diffs_detects_conflicts_and_rejects_empty_results() {
    let mut p = done_plan();
    let diff_a = ["--- a/src/a.ts (+1 -0)", "+++ b/src/a.ts", "@@ -1,1 +1,2 @@", " old", "+new"].join("\n");
    let diff_b = ["--- a/src/b.ts (+1 -0)", "+++ b/src/b.ts", "@@ -1,1 +1,2 @@", " x", "+y"].join("\n");
    let ok = merge(&p, &diffs(&[("t1", &diff_a), ("t2", &diff_b)]), false, None).await.unwrap();
    assert!(ok.verified);
    assert_eq!(ok.reason, "ok");
    let mut artifacts = ok.artifacts.clone();
    artifacts.sort();
    assert_eq!(artifacts, vec!["src/a.ts", "src/b.ts"]);
    assert!(ok.combined_diff.contains("src/a.ts") && ok.combined_diff.contains("src/b.ts"));

    let conflict_diff = ["--- a/src/a.ts (+1 -0)", "+++ b/src/a.ts", "@@ -1,1 +1,2 @@", " old", "+other"].join("\n");
    let conflict = merge(&p, &diffs(&[("t1", &diff_a), ("t2", &conflict_diff)]), false, None).await.unwrap();
    assert!(!conflict.verified);
    assert_eq!(conflict.conflicts.len(), 1);
    assert_eq!(conflict.conflicts[0].todos, vec!["t1", "t2"]);
    assert_match(&conflict.reason, "merge conflict");

    let empty = merge(&p, &HashMap::new(), false, None).await.unwrap();
    assert!(!empty.verified);
    assert_eq!(empty.reason, "no changes");
    assert!(merge(&p, &HashMap::new(), true, None).await.unwrap().verified);

    let both = diffs(&[("t1", &diff_a), ("t2", &diff_b)]);
    let verify_false = merge(&p, &both, false, Some(Box::new(|_, _| Box::pin(async { Ok(false) })))).await.unwrap();
    assert!(!verify_false.verified);
    assert_eq!(verify_false.reason, "verification failed");
    assert!(merge(&p, &both, false, Some(Box::new(|_, _| Box::pin(async { Ok(true) })))).await.unwrap().verified);

    p.todos[1].status = "pending".into();
    assert!(merge(&p, &diffs(&[("t1", &diff_a), ("t2", &conflict_diff)]), false, None).await.unwrap().verified);
}

#[tokio::test]
async fn merge_treats_an_empty_diff_on_a_shared_file_as_non_conflicting() {
    let p = done_plan();
    let diff_a = ["--- a/shared.ts (+1 -0)", "+++ b/shared.ts", "@@ -1,1 +1,2 @@", " old", "+new"].join("\n");
    let result = merge(&p, &diffs(&[("t1", &diff_a), ("t2", "")]), false, None).await.unwrap();
    assert!(result.verified);
    assert!(result.conflicts.is_empty());
}

#[test]
fn diff_paths_parses_unified_diff_headers_including_new_and_deleted_kinds() {
    let diff = ["--- a/src/new.ts (+3 -0)", "+++ b/src/new.ts", "@@ -0,0 +1,3 @@", "+a", "--- a/src/gone.ts (deleted +0 -3)", "+++ b/src/gone.ts", "@@ -1,3 +0,0 @@", "-b"].join("\n");
    let mut paths = diff_paths(&diff);
    paths.sort();
    assert_eq!(paths, vec!["src/gone.ts", "src/new.ts"]);
}

#[test]
fn role_toolsets_keep_read_only_roles_read_only_and_never_expose_mcp_or_peer_tools_by_default() {
    for role in ["planner", "reviewer", "researcher"] {
        for tool in role_toolset(role, &[]) {
            assert_match(&tool, "^(read_file|list_files|search)$");
        }
    }
    let coder = role_toolset("coder", &[]);
    assert!(coder.contains(&"bash".to_string()) && coder.contains(&"write_file".to_string()));
    let tester = role_toolset("tester", &[]);
    assert!(tester.contains(&"bash".to_string()) && !tester.contains(&"write_file".to_string()));
    for role in AGENT_ROLES {
        let set = role_toolset(role, &[]);
        assert!(!set.iter().any(|t| t.starts_with("mcp__")));
        assert!(!set.iter().any(|t| t == "spawn_agent" || t == "peer_send"));
    }
    let mut expected = tester.clone();
    expected.push("mcp__fs__read".into());
    assert_eq!(role_toolset("tester", &["mcp__fs__read".into()]), expected);
}

#[tokio::test]
async fn run_dag_validate_failure_emits_dag_plan_invalid_and_never_spawns() {
    let f = fixture();
    let spawned = Arc::new(AtomicUsize::new(0));
    let s = spawned.clone();
    let (emit, log) = events();
    let hooks = DagHooks {
        planner: Arc::new(|_, _| Box::pin(async { Ok(json!({ "goal": "g", "todos": [{ "id": "t1", "title": "a", "role": "coder", "dependsOn": ["t9"] }] })) })),
        spawn: Arc::new(move |_| {
            s.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("report".to_string()) })
        }),
        review: None,
        test: None,
        verify: None,
    };
    let opts = DagOptions {
        config: DagConfig::default(),
        cancel: token(),
        files: Arc::new(files::WorkspaceFiles::new(&f.cwd)),
        budget: None,
        allowed_mcp_tools: vec![],
        allow_empty: false,
        write_concurrency: None,
        emit: Some(emit),
    };
    let error = run_dag("g", hooks, opts).await.err().expect("plan validation error");
    assert!(error.downcast_ref::<PlanValidationError>().is_some(), "{error}");
    assert_eq!(spawned.load(Ordering::SeqCst), 0);
    assert!(log.lock().unwrap().iter().any(|e| e["type"] == "dag_plan_invalid"));
}
