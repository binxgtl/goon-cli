use crate::common::*;
use goon::coord::*;
use goon::verify::{RunnerOptions, RunnerResult, Severity, VerificationApprovalError, VerifyPlan, VerifyRunner};

fn verify_config(commands: &[&str]) -> config::VerifyConfig {
    config::VerifyConfig { enabled: true, commands: commands.iter().map(|c| c.to_string()).collect(), timeout_ms: 30_000, max_repair_rounds: 2, policy: Default::default() }
}

struct FakeRunner {
    codes: Vec<i32>,
    index: Mutex<usize>,
    calls: Mutex<Vec<(String, Vec<String>)>>,
    output: Option<String>,
}

impl FakeRunner {
    fn new(codes: &[i32]) -> Arc<FakeRunner> {
        Arc::new(FakeRunner { codes: codes.to_vec(), index: Mutex::new(0), calls: Mutex::new(vec![]), output: None })
    }
    fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }
}

impl VerifyRunner for FakeRunner {
    fn run(&self, command: String, args: Vec<String>, _opts: RunnerOptions) -> BoxFuture<'static, RunnerResult> {
        self.calls.lock().unwrap().push((command, args));
        let mut index = self.index.lock().unwrap();
        let code = self.codes[(*index).min(self.codes.len() - 1)];
        *index += 1;
        let output = self.output.clone().unwrap_or_else(|| if code == 0 { "all tests passed".into() } else { format!("test.ts:3: error: expected {} (token: supersecret)", *index) });
        Box::pin(async move { RunnerResult { code, output, timed_out: false, not_found: false } })
    }
}

type Approve = Arc<dyn Fn(&VerifyPlan) -> BoxFuture<'static, bool> + Send + Sync>;

fn approve_with(answer: bool, log: Option<Arc<Mutex<Vec<Vec<String>>>>>) -> Option<Approve> {
    Some(Arc::new(move |plan: &VerifyPlan| {
        if let Some(l) = &log {
            l.lock().unwrap().push(plan.commands.iter().map(|c| c.display.clone()).collect());
        }
        Box::pin(async move { answer })
    }))
}

fn opts(f: &Fixture, files: &Arc<files::WorkspaceFiles>, runner: Arc<FakeRunner>, approve: Option<Approve>) -> ScopeLoopOptions {
    ScopeLoopOptions {
        cwd: f.cwd.clone(),
        verify_config: verify_config(&["npm test"]),
        verify_policy: None,
        runner: Some(runner),
        files: files.clone(),
        cancel: token(),
        budget: None,
        approve,
        secrets: vec![],
        emit: None,
        max_rounds: None,
    }
}

fn writer(files: &Arc<files::WorkspaceFiles>, name: impl Fn(u64) -> String + Send + Sync + 'static, content: impl Fn(u64) -> String + Send + Sync + 'static) -> impl Fn(ScopeLoopAttempt, CancellationToken) -> BoxFuture<'static, Result<String>> {
    let files = files.clone();
    move |ctx, _| {
        let r = files.write(&name(ctx.round), &content(ctx.round));
        Box::pin(async move { r.map(|_| "attempted".to_string()) })
    }
}

fn emit_log() -> (Option<Arc<dyn Fn(Value) + Send + Sync>>, Arc<Mutex<Vec<Value>>>) {
    let log = Arc::new(Mutex::new(vec![]));
    let l = log.clone();
    (Some(Arc::new(move |e| l.lock().unwrap().push(e))), log)
}

fn actions(log: &Arc<Mutex<Vec<Value>>>, with_round: bool) -> Vec<String> {
    log.lock().unwrap().iter().map(|e| if with_round { format!("{}:{}", e["round"], e["action"].as_str().unwrap()) } else { e["action"].as_str().unwrap().to_string() }).collect()
}

#[test]
fn scope_round_cap_is_max_repair_rounds_plus_one_capped_at_four() {
    assert_eq!(scope_round_cap(0), 1);
    assert_eq!(scope_round_cap(1), 2);
    assert_eq!(scope_round_cap(2), 3);
    assert_eq!(scope_round_cap(10), 4);
}

#[tokio::test]
async fn fail_twice_then_pass_keeps_the_workspace_and_approves_exact_commands() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let runner = FakeRunner::new(&[1, 1, 0]);
    let approvals = Arc::new(Mutex::new(vec![]));
    let attempted = Arc::new(Mutex::new(vec![]));
    let (emit, log) = emit_log();
    let mut o = opts(&f, &files, runner.clone(), approve_with(true, Some(approvals.clone())));
    o.emit = emit;
    let (fw, att) = (files.clone(), attempted.clone());
    let result = run_scope_loop(
        "make the tests pass",
        move |ctx, _| {
            att.lock().unwrap().push((ctx.round, ctx.related_files.clone(), ctx.verify_output.clone(), ctx.commands.clone()));
            let r = fw.write(&format!("fix-{}.ts", ctx.round), &format!("export const r{} = 1;\n", ctx.round));
            let round = ctx.round;
            Box::pin(async move { r.map(|_| format!("attempt {round}")) })
        },
        o,
    )
    .await
    .unwrap();
    assert!(result.ok, "{}", result.reason);
    assert_eq!(result.rounds, 3);
    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|(c, a)| c == "npm" && a == &vec!["test".to_string()]));
    assert_eq!(*approvals.lock().unwrap(), vec![vec!["npm test".to_string()]; 3]);
    assert_eq!(result.changed_files, vec!["fix-1.ts", "fix-2.ts", "fix-3.ts"]);
    let att = attempted.lock().unwrap();
    assert_eq!(att[1].0, 2);
    assert_match(&att[1].2, "Verification failed");
    assert_eq!(att[1].1, vec!["fix-1.ts"]);
    assert_eq!(att[1].3, vec!["npm test"]);
    assert_eq!(actions(&log, true), vec!["1:expand", "2:expand", "3:contract"]);
    assert!(result.rollback.is_none());
    assert_eq!(f.read("fix-3.ts"), "export const r3 = 1;\n");
}

#[tokio::test]
async fn secrets_are_redacted_in_the_expanded_verify_output() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let seen = Arc::new(Mutex::new(vec![]));
    let mut o = opts(&f, &files, FakeRunner::new(&[1, 0]), approve_with(true, None));
    o.secrets = vec!["supersecret".into()];
    let (fw, s) = (files.clone(), seen.clone());
    run_scope_loop(
        "fix it",
        move |ctx, _| {
            s.lock().unwrap().push(ctx.verify_output.clone());
            let r = fw.write(&format!("a-{}.ts", ctx.round), &format!("export const a = {};\n", ctx.round));
            Box::pin(async move { r.map(|_| "ok".to_string()) })
        },
        o,
    )
    .await
    .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0], "");
    assert!(!seen[1].contains("supersecret"));
    assert_match(&seen[1], r"\[redacted\]");
}

#[tokio::test]
async fn the_returned_verdict_is_redacted() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let runner = Arc::new(FakeRunner { codes: vec![1], index: Mutex::new(0), calls: Mutex::new(vec![]), output: Some("token: supersecret\nAuthorization: Bearer sk-live-abcdef123456\nnote: LITERALSECRETVALUE".into()) });
    let mut o = opts(&f, &files, runner, approve_with(true, None));
    o.secrets = vec!["LITERALSECRETVALUE".into()];
    let result = run_scope_loop("fix it", writer(&files, |r| format!("r-{r}.ts"), |_| "x\n".into()), o).await.unwrap();
    assert!(!result.ok);
    let verdict = result.verdict.expect("exhausted rounds return the verdict");
    let rendered = format!("{} {:?} {:?}", verdict.reason, verdict.errors, verdict.stages.iter().map(|s| (&s.output, &s.issues, &s.reason)).collect::<Vec<_>>());
    assert!(!rendered.contains("LITERALSECRETVALUE"), "{rendered}");
    assert!(!rendered.contains("supersecret"), "{rendered}");
    assert!(!rendered.contains("sk-live-abcdef123456"), "{rendered}");
    assert_match(&rendered, r"\[redacted\]");
}

#[tokio::test]
async fn fail_closed_without_approve_commands_never_run_and_writes_roll_back() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let runner = FakeRunner::new(&[0]);
    let error = run_scope_loop("fix it", writer(&files, |r| format!("leak-{r}.ts"), |_| "x\n".into()), opts(&f, &files, runner.clone(), None)).await.err().expect("approval error");
    assert!(error.downcast_ref::<VerificationApprovalError>().is_some(), "{error}");
    assert!(runner.calls().is_empty());
    assert!(!f.exists("leak-1.ts"));
}

#[tokio::test]
async fn approval_denial_is_a_skip_not_a_failure_and_keeps_changes() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let runner = FakeRunner::new(&[0]);
    let (emit, log) = emit_log();
    let mut o = opts(&f, &files, runner.clone(), approve_with(false, None));
    o.emit = emit;
    let result = run_scope_loop("fix it", writer(&files, |r| format!("keep-{r}.ts"), |_| "x\n".into()), o).await.unwrap();
    assert!(runner.calls().is_empty());
    assert!(result.ok);
    assert_eq!(result.verdict.as_ref().map(|v| v.severity), Some(Severity::Skip));
    assert_eq!(actions(&log, false).last().unwrap(), "contract");
    assert_eq!(f.read("keep-1.ts"), "x\n");
}

#[tokio::test]
async fn ast_only_plans_run_without_an_approve_callback() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let mut o = opts(&f, &files, FakeRunner::new(&[0]), None);
    o.verify_config = verify_config(&[]);
    let result = run_scope_loop("fix it", writer(&files, |_| "a.ts".into(), |_| "export const a = 1;\n".into()), o).await.unwrap();
    assert!(result.ok, "{}", result.reason);
    assert!(matches!(result.verdict.as_ref().map(|v| v.severity), Some(Severity::Pass) | Some(Severity::Warn)));
}

fn token_budget(spent: u64, limit: u64) -> cost::SharedBudget {
    Arc::new(Mutex::new(cost::RunBudget { spent, limit, cost_spent: 0.0, cost_limit: 0.0, started_at: None, time_limit_ms: 0, ledger: None }))
}

#[tokio::test]
async fn token_budget_stops_before_the_first_attempt() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (emit, log) = emit_log();
    let mut o = opts(&f, &files, FakeRunner::new(&[0]), None);
    o.budget = Some(token_budget(100, 100));
    o.emit = emit;
    let a = attempts.clone();
    let result = run_scope_loop(
        "fix it",
        move |_, _| {
            a.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("x".to_string()) })
        },
        o,
    )
    .await
    .unwrap();
    assert!(!result.ok);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_match(&result.reason, "token budget reached");
    assert_eq!(actions(&log, false), vec!["stop"]);
    let rollback = result.rollback.unwrap();
    assert!(rollback.restored.is_empty() && rollback.deleted.is_empty() && rollback.conflicts.is_empty() && rollback.pruned.is_empty());
}

#[tokio::test]
async fn budget_exhausted_mid_loop_stops_without_exceeding_the_shared_budget() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let budget = token_budget(0, 100);
    let attempts = Arc::new(AtomicUsize::new(0));
    let (emit, log) = emit_log();
    let mut o = opts(&f, &files, FakeRunner::new(&[1, 1, 1]), approve_with(true, None));
    o.budget = Some(budget.clone());
    o.emit = emit;
    let (fw, a, b) = (files.clone(), attempts.clone(), budget.clone());
    let result = run_scope_loop(
        "fix it",
        move |ctx, _| {
            a.fetch_add(1, Ordering::SeqCst);
            b.lock().unwrap().spent += 60;
            let r = fw.write(&format!("w-{}.ts", ctx.round), "x\n");
            Box::pin(async move { r.map(|_| "attempted".to_string()) })
        },
        o,
    )
    .await
    .unwrap();
    assert!(!result.ok);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_match(&result.reason, "token budget reached");
    assert_eq!(actions(&log, true), vec!["1:expand", "2:expand", "3:stop"]);
    let mut deleted = result.rollback.unwrap().deleted;
    deleted.sort();
    assert_eq!(deleted, vec!["w-1.ts", "w-2.ts"]);
    assert!(!f.exists("w-1.ts") && !f.exists("w-2.ts"));
}

#[tokio::test]
async fn exhausted_rounds_roll_back_all_attempts_and_report_the_verdict() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let result = run_scope_loop("fix it", writer(&files, |r| format!("bad-{r}.ts"), |_| "x\n".into()), opts(&f, &files, FakeRunner::new(&[1]), approve_with(true, None))).await.unwrap();
    assert!(!result.ok);
    assert_eq!(result.rounds, 3);
    assert_match(&result.reason, r"verification still failing after 3 round\(s\)");
    assert_eq!(result.rollback.unwrap().deleted.len(), 3);
    assert!(!f.exists("bad-1.ts"));
}

#[tokio::test]
async fn one_file_edited_by_several_attempts_rolls_back_to_the_pre_loop_state() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let result = run_scope_loop("fix it", writer(&files, |_| "a.ts".into(), |r| format!("export const v = {r};\n")), opts(&f, &files, FakeRunner::new(&[1]), approve_with(true, None))).await.unwrap();
    assert!(!result.ok);
    assert!(!f.exists("a.ts"));
    assert!(result.rollback.unwrap().conflicts.is_empty());
}

#[tokio::test]
async fn no_tracked_changes_stops_the_loop() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let (emit, log) = emit_log();
    let mut o = opts(&f, &files, FakeRunner::new(&[0]), None);
    o.emit = emit;
    let result = run_scope_loop("fix it", |_, _| Box::pin(async { Ok("nothing to do".to_string()) }), o).await.unwrap();
    assert!(!result.ok);
    assert_match(&result.reason, "no tracked file changes");
    assert_eq!(actions(&log, false), vec!["stop"]);
}

#[tokio::test]
async fn cancellation_propagates_and_rolls_back() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let o = opts(&f, &files, FakeRunner::new(&[0]), None);
    let cancel = o.cancel.clone();
    let fw = files.clone();
    let error = run_scope_loop(
        "fix it",
        move |_, _| {
            let _ = fw.write("partial.ts", "x\n");
            cancel.cancel();
            Box::pin(async { Err(anyhow!("user cancelled")) })
        },
        o,
    )
    .await
    .err()
    .expect("cancelled");
    assert_match(&error.to_string(), "user cancelled");
    assert!(!f.exists("partial.ts"));
}

#[test]
fn budget_stop_reason_checks_the_token_limit_first_then_ledger_caps() {
    assert_eq!(budget_stop_reason(None), None);
    assert_match(&budget_stop_reason(Some(&token_budget(5, 5))).unwrap(), "token budget reached");
    assert_eq!(budget_stop_reason(Some(&token_budget(4, 5))), None);
    let ledger = Arc::new(cost::CostLedger::new(cost::CostPricingTable { default: None, models: [("m".to_string(), cost::Price { input_per_1m: 1.0, output_per_1m: 1.0, cached_input_per_1m: None, cache_write_per_1m: None })].into_iter().collect() }, Some("m".into())));
    let now = jsutil::now_millis();
    let b = |cost_limit: f64, cost_spent: f64, time_limit_ms: u64, started_at: Option<i64>| -> cost::SharedBudget {
        Arc::new(Mutex::new(cost::RunBudget { spent: 0, limit: 0, cost_spent, cost_limit, started_at, time_limit_ms, ledger: Some(ledger.clone()) }))
    };
    assert_match(&budget_stop_reason(Some(&b(10.0, 10.0, 0, None))).unwrap(), "cost budget reached");
    assert_match(&budget_stop_reason(Some(&b(0.0, 0.0, 1000, Some(now - 2000)))).unwrap(), "time budget reached");
    assert_eq!(budget_stop_reason(Some(&b(10.0, 1.0, 60_000, Some(now)))), None);
}
