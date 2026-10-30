use crate::common::*;
use goon::coord::*;
use goon::verify::{Severity, VerifyIssue, VerifyVerdict};

fn verdict(severity: Severity, warnings: usize, reason: &str) -> VerifyVerdict {
    let issue = |m: &str| VerifyIssue { stage: "test".into(), path: "a.ts".into(), line: None, column: None, message: m.into() };
    VerifyVerdict {
        pass: severity != Severity::Fail,
        severity,
        stages: vec![],
        errors: if severity == Severity::Fail { vec![issue("boom")] } else { vec![] },
        warnings: (0..warnings).map(|_| issue("style")).collect(),
        reason: reason.into(),
    }
}

#[test]
fn candidate_cap_clamps_to_max_candidates_minimum_one() {
    assert_eq!(MAX_CANDIDATES, 2);
    assert_eq!(candidate_cap(None), 2);
    assert_eq!(candidate_cap(Some(5)), 2);
    assert_eq!(candidate_cap(Some(2)), 2);
    assert_eq!(candidate_cap(Some(1)), 1);
    assert_eq!(candidate_cap(Some(0)), 1);
}

#[test]
fn reflect_folds_a_verdict_into_a_rankable_candidate() {
    let r = |v: VerifyVerdict| reflect(0, "t", "r", 0.5, vec![], &v);
    assert!(!r(verdict(Severity::Fail, 0, "fail")).passed);
    assert!(r(verdict(Severity::Pass, 0, "pass")).passed);
    let warn = r(verdict(Severity::Warn, 1, "warn"));
    assert!(warn.passed);
    assert_eq!(warn.warnings, 1);
    assert!(r(verdict(Severity::Skip, 0, "skip")).passed);
    assert_eq!(r(verdict(Severity::Fail, 0, "fail")).reason, "fail");
}

fn mk(index: usize, passed: bool, warnings: usize, cost_usd: f64) -> Candidate {
    Candidate { index, task: "t".into(), report: format!("r{index}"), cost_usd, passed, warnings, reason: String::new(), backups: vec![] }
}

#[test]
fn rerank_orders_pass_then_warnings_then_cost_and_ties_keep_generation_order() {
    let ranked = rerank(&[mk(0, true, 2, 0.1), mk(1, false, 0, 0.01), mk(2, true, 0, 0.9), mk(3, true, 2, 0.05), mk(4, false, 0, 0.0)]);
    assert_eq!(ranked.iter().map(|c| c.index).collect::<Vec<_>>(), vec![2, 3, 0, 4, 1]);
    let tied = rerank(&[mk(7, true, 0, 0.2), mk(3, true, 0, 0.2), mk(5, true, 0, 0.2)]);
    assert_eq!(tied.iter().map(|c| c.index).collect::<Vec<_>>(), vec![3, 5, 7]);
    assert!(rerank(&[]).is_empty());
}

fn budget(cost_limit: f64, cost_spent: f64, time_limit_ms: u64, started_at: Option<i64>) -> cost::SharedBudget {
    Arc::new(Mutex::new(cost::RunBudget { spent: 0, limit: 0, cost_spent, cost_limit, started_at, time_limit_ms, ledger: None }))
}

#[test]
fn cost_degrade_below_twenty_percent_remaining_degrades_to_one_candidate() {
    let now = jsutil::now_millis();
    assert_eq!(cost_degrade(None, now), (false, String::new()));
    assert!(!cost_degrade(Some(&budget(0.0, 0.0, 0, None)), now).0);
    let low = cost_degrade(Some(&budget(100.0, 85.0, 0, None)), now);
    assert!(low.0);
    assert_match(&low.1, "cost budget nearly exhausted");
    assert!(!cost_degrade(Some(&budget(100.0, 50.0, 0, None)), now).0);
    assert!(!cost_degrade(Some(&budget(100.0, 100.0 * (1.0 - CANDIDATE_DEGRADE_RATIO), 0, None)), now).0);
    let time = cost_degrade(Some(&budget(0.0, 0.0, 1000, Some(now - 900))), now);
    assert!(time.0);
    assert_match(&time.1, "time budget nearly exhausted");
}

type Generate = Arc<dyn Fn(String, usize, bool, CancellationToken) -> BoxFuture<'static, Result<String>> + Send + Sync>;

async fn run(
    f: &Fixture,
    files: &Arc<files::WorkspaceFiles>,
    generate: Generate,
    verify: Arc<dyn Fn(Vec<String>) -> VerifyVerdict + Send + Sync>,
    budget: Option<cost::SharedBudget>,
    requested: Option<usize>,
    log: Option<Arc<Mutex<Vec<Value>>>>,
) -> CandidateRunResult {
    let _ = f;
    let emit = log.map(|l| move |e: Value| l.lock().unwrap().push(e));
    let emit_ref: Option<&(dyn Fn(Value) + Send + Sync)> = emit.as_ref().map(|e| e as &(dyn Fn(Value) + Send + Sync));
    run_candidates(
        "ship the fix",
        move |task, index, degraded, cancel| generate(task, index, degraded, cancel),
        move |changed, _| {
            let v = verify(changed);
            Box::pin(async move { Ok(v) })
        },
        files,
        &token(),
        budget.as_ref(),
        requested,
        emit_ref,
    )
    .await
}

#[tokio::test]
async fn failing_first_candidate_is_rolled_back_and_the_passing_second_wins() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let generated = Arc::new(Mutex::new(vec![]));
    let (fw, g) = (files.clone(), generated.clone());
    let generate: Generate = Arc::new(move |_, index, _, _| {
        g.lock().unwrap().push(index);
        let r = fw.write(&format!("cand-{index}.ts"), &format!("export const v{index} = {};\n", if index == 0 { "broken" } else { "fixed" }));
        Box::pin(async move { r.map(|_| format!("report {index}")) })
    });
    let g2 = generated.clone();
    let verify = Arc::new(move |changed: Vec<String>| {
        let list = g2.lock().unwrap();
        assert_eq!(changed, vec![format!("cand-{}.ts", list.last().unwrap())]);
        if list.len() == 1 {
            verdict(Severity::Fail, 0, "fail")
        } else {
            verdict(Severity::Pass, 0, "pass")
        }
    });
    let log = Arc::new(Mutex::new(vec![]));
    let result = run(&f, &files, generate, verify, None, None, Some(log.clone())).await;
    assert!(result.ok);
    assert_eq!(result.winner.as_ref().unwrap().index, 1);
    assert_eq!(*generated.lock().unwrap(), vec![0, 1]);
    assert_eq!(result.candidates.len(), 2);
    assert_eq!(result.ranked[0].index, 1);
    assert!(!f.exists("cand-0.ts"));
    assert_eq!(f.read("cand-1.ts"), "export const v1 = fixed;\n");
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(files.turn_changed_files(), vec!["cand-1.ts"]);
}

#[tokio::test]
async fn both_candidates_failing_rolls_everything_back() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let fw = files.clone();
    let generate: Generate = Arc::new(move |_, index, _, _| {
        let r = fw.write(&format!("bad-{index}.ts"), "export const x = 1;\n");
        Box::pin(async move { r.map(|_| format!("report {index}")) })
    });
    let result = run(&f, &files, generate, Arc::new(|_| verdict(Severity::Fail, 0, "tests fail")), None, None, None).await;
    assert!(!result.ok);
    assert_eq!(result.candidates.len(), 2);
    assert!(result.winner.is_none());
    assert_match(&result.reason, r"all 2 candidate\(s\) failed verification");
    assert!(!f.exists("bad-0.ts") && !f.exists("bad-1.ts"));
    assert!(files.turn_changed_files().is_empty());
}

#[tokio::test]
async fn low_budget_degrades_to_one_candidate_with_a_cost_degrade_event() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let efforts = Arc::new(Mutex::new(vec![]));
    let (fw, e) = (files.clone(), efforts.clone());
    let generate: Generate = Arc::new(move |_, index, degraded, _| {
        e.lock().unwrap().push(degraded);
        let r = fw.write(&format!("one-{index}.ts"), "export const x = 1;\n");
        Box::pin(async move { r.map(|_| "report".to_string()) })
    });
    let log = Arc::new(Mutex::new(vec![]));
    let result = run(&f, &files, generate, Arc::new(|_| verdict(Severity::Pass, 0, "pass")), Some(budget(100.0, 90.0, 0, None)), Some(2), Some(log.clone())).await;
    assert!(result.ok);
    assert!(result.degraded);
    assert_eq!(*efforts.lock().unwrap(), vec![true]);
    let events = log.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "cost_degrade");
    assert_eq!(events[0]["candidates"], 1);
    assert_match(events[0]["reason"].as_str().unwrap(), "cost budget nearly exhausted");
}

#[tokio::test]
async fn hard_budget_cap_stops_generation_before_a_candidate_runs() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let generated = Arc::new(AtomicUsize::new(0));
    let g = generated.clone();
    let generate: Generate = Arc::new(move |_, _, _, _| {
        g.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("report".to_string()) })
    });
    let exhausted: cost::SharedBudget = Arc::new(Mutex::new(cost::RunBudget { spent: 50, limit: 50, cost_spent: 0.0, cost_limit: 0.0, started_at: None, time_limit_ms: 0, ledger: None }));
    let result = run(&f, &files, generate, Arc::new(|_| verdict(Severity::Fail, 0, "fail")), Some(exhausted), None, None).await;
    assert!(!result.ok);
    assert_eq!(generated.load(Ordering::SeqCst), 0);
    assert_match(&result.reason, "token budget reached");
}

#[tokio::test]
async fn a_candidate_whose_hooks_throw_is_recorded_as_failed_and_rolled_back() {
    let f = fixture();
    let files = Arc::new(files::WorkspaceFiles::new(&f.cwd));
    let calls = Arc::new(Mutex::new(vec![]));
    let (fw, c) = (files.clone(), calls.clone());
    let generate: Generate = Arc::new(move |_, index, _, _| {
        c.lock().unwrap().push(format!("generate-{index}"));
        if index == 0 {
            let _ = fw.write("half.ts", "export const half = 1;\n");
            return Box::pin(async { Err(anyhow!("provider exploded")) });
        }
        let r = fw.write("full.ts", "export const full = 1;\n");
        Box::pin(async move { r.map(|_| "report 1".to_string()) })
    });
    let result = run(&f, &files, generate, Arc::new(|_| verdict(Severity::Pass, 0, "pass")), None, None, None).await;
    assert!(result.ok);
    assert_eq!(*calls.lock().unwrap(), vec!["generate-0", "generate-1"]);
    assert!(!result.candidates[0].passed);
    assert_match(&result.candidates[0].reason, "provider exploded");
    assert_eq!(result.winner.as_ref().unwrap().index, 1);
    assert!(!f.exists("half.ts"));
    assert_eq!(f.read("full.ts"), "export const full = 1;\n");
}
