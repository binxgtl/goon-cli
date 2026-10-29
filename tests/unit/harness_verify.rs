use crate::common::*;
use goon::verify::{RunnerOptions, RunnerResult, VerificationFailed, VerifyRunner};

/// Fails while `bad.ts` contains BROKEN (echoing the API key so redaction is exercised).
struct BrokenFileRunner {
    cwd: PathBuf,
    calls: AtomicUsize,
}

impl VerifyRunner for BrokenFileRunner {
    fn run(&self, _command: String, _args: Vec<String>, _opts: RunnerOptions) -> BoxFuture<'static, RunnerResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let broken = std::fs::read_to_string(self.cwd.join("bad.ts")).map(|c| c.contains("BROKEN")).unwrap_or(false);
        Box::pin(async move {
            if broken {
                RunnerResult { code: 1, output: "bad.ts(1,1): error TS2322: Type string is not assignable to number (test-secret-never-echo)".into(), timed_out: false, not_found: false }
            } else {
                RunnerResult { code: 0, output: String::new(), timed_out: false, not_found: false }
            }
        })
    }
}

/// Simulates the user editing the file while verification runs, then fails.
struct UserEditRunner(PathBuf);

impl VerifyRunner for UserEditRunner {
    fn run(&self, _command: String, _args: Vec<String>, _opts: RunnerOptions) -> BoxFuture<'static, RunnerResult> {
        let path = self.0.join("bad.ts");
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        std::fs::write(&path, format!("// user change\n{current}")).unwrap();
        Box::pin(async { RunnerResult { code: 1, output: "still broken".into(), timed_out: false, not_found: false } })
    }
}

struct Setup {
    f: Fixture,
    store: Arc<session::SessionStore>,
    harness: Arc<harness::Harness>,
    events: Arc<Mutex<Vec<Value>>>,
    runner_calls: Option<Arc<BrokenFileRunner>>,
}

fn setup(turns: Vec<Turn>, enabled: bool, approve: tools::Approve, custom_runner: Option<Arc<dyn VerifyRunner>>) -> Setup {
    let f = fixture();
    std::fs::create_dir_all(f.cwd.join("src")).unwrap();
    let rt = runtime_keyed(
        &f.cwd,
        &f.data,
        "http://localhost/v1",
        json!({ "maxOutputTokens": 1024, "timeoutMs": 30000, "inlineReasoning": "off", "verify": { "enabled": enabled, "commands": ["node -e \"process.exit(0)\""], "timeoutMs": 30000, "maxRepairRounds": 2 } }),
        "test-secret-never-echo",
    );
    let store = store(&rt);
    let tools = registry(&f.cwd, approve, false);
    let (emit, events) = events();
    let (runner, runner_calls): (Arc<dyn VerifyRunner>, Option<Arc<BrokenFileRunner>>) = match custom_runner {
        Some(r) => (r, None),
        None => {
            let r = Arc::new(BrokenFileRunner { cwd: f.cwd.clone(), calls: AtomicUsize::new(0) });
            (r.clone(), Some(r))
        }
    };
    let deps = harness::HarnessDeps { verify_enabled: Some(enabled), verify_runner: Some(runner), verify_policy: None, memory: None };
    let harness = harness::Harness::new(rt, store.clone(), tools, emit, Some(FakeModel::new(turns)), deps).unwrap();
    Setup { f, store, harness, events, runner_calls }
}

fn of_type(events: &Arc<Mutex<Vec<Value>>>, kind: &str) -> Vec<Value> {
    events.lock().unwrap().iter().filter(|e| e["type"] == kind).cloned().collect()
}

fn write_broken() -> Turn {
    invoke(vec![tc("w1", "write_file", json!({ "path": "bad.ts", "content": "const x: number = \"BROKEN\";\n" }))])
}

#[tokio::test]
async fn fail_then_repair_pass_emits_start_and_result_and_injects_only_once() {
    let s = setup(
        vec![
            write_broken(),
            invoke(vec![tc("r0", "read_file", json!({ "path": "bad.ts" }))]),
            invoke(vec![tc("e1", "edit_file", json!({ "path": "bad.ts", "oldText": "const x: number = \"BROKEN\";", "newText": "const x: number = 42;" }))]),
            say("Fixed."),
        ],
        true,
        tools::approve_all(),
        None,
    );
    let text = s.harness.run("please fix", &token(), None).await.unwrap();
    assert_eq!(text.trim(), "Fixed.");
    assert_eq!(of_type(&s.events, "verify_start").len(), 2);
    assert_eq!(of_type(&s.events, "verify_result").iter().map(|r| r["status"].as_str().unwrap().to_string()).collect::<Vec<_>>(), vec!["fail", "pass"]);
    assert_eq!(s.runner_calls.as_ref().unwrap().calls.load(Ordering::SeqCst), 2);
    let session = s.store.session.lock().unwrap();
    assert!(session.messages.iter().filter(|m| m["role"] == "user").any(|m| m["content"].as_str().unwrap_or("").contains("Verification failed (attempt 1/2)")));
    let serialized = format!("{}{}", serde_json::to_string(&*s.events.lock().unwrap()).unwrap(), serde_json::to_string(&session.messages).unwrap());
    assert!(!serialized.contains("test-secret-never-echo"));
    assert!(!s.f.read("bad.ts").contains("BROKEN"));
}

#[tokio::test]
async fn model_giving_up_triggers_rollback_and_a_verification_failure() {
    let s = setup(vec![write_broken(), say("I give up.")], true, tools::approve_all(), None);
    let error = s.harness.run("please fix", &token(), None).await.unwrap_err();
    assert!(error.downcast_ref::<VerificationFailed>().is_some(), "{error}");
    assert!(!of_type(&s.events, "verify_rollback").is_empty());
    assert!(!s.f.exists("bad.ts"));
    let session = s.store.session.lock().unwrap();
    let last = session.checkpoints.as_ref().and_then(|c| c.last()).expect("checkpoint");
    assert!(last.files.is_empty());
}

#[tokio::test]
async fn approval_denied_skips_without_rollback() {
    let approve: tools::Approve = Arc::new(|req| {
        let ok = req.tool != "verify";
        Box::pin(async move { ok })
    });
    let s = setup(vec![write_broken(), say("Done without fixing.")], true, approve, None);
    assert_eq!(s.harness.run("please fix", &token(), None).await.unwrap().trim(), "Done without fixing.");
    let results = of_type(&s.events, "verify_result");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["status"], "skip");
    assert!(of_type(&s.events, "verify_rollback").is_empty());
    assert_eq!(s.runner_calls.as_ref().unwrap().calls.load(Ordering::SeqCst), 0);
    assert!(s.f.read("bad.ts").contains("BROKEN"));
}

#[tokio::test]
async fn a_disabled_harness_never_calls_the_runner() {
    let s = setup(vec![write_broken(), say("Done.")], false, tools::approve_all(), None);
    assert_eq!(s.harness.run("please fix", &token(), None).await.unwrap().trim(), "Done.");
    assert_eq!(s.runner_calls.as_ref().unwrap().calls.load(Ordering::SeqCst), 0);
    assert!(of_type(&s.events, "verify_start").is_empty());
}

#[tokio::test]
async fn rollback_keeps_user_changed_files_as_conflicts() {
    // The runner needs the fixture path, so this test assembles its harness by hand.
    let s = {
        let f = fixture();
        std::fs::create_dir_all(f.cwd.join("src")).unwrap();
        let rt = runtime(&f.cwd, &f.data, "http://localhost/v1", json!({ "maxOutputTokens": 1024, "inlineReasoning": "off", "verify": { "enabled": true, "commands": ["node -e \"process.exit(0)\""], "maxRepairRounds": 2 } }));
        let store = store(&rt);
        let tools = registry(&f.cwd, tools::approve_all(), false);
        let (emit, events) = events();
        let deps = harness::HarnessDeps { verify_enabled: Some(true), verify_runner: Some(Arc::new(UserEditRunner(f.cwd.clone()))), verify_policy: None, memory: None };
        let harness = harness::Harness::new(rt, store.clone(), tools, emit, Some(FakeModel::new(vec![write_broken(), say("Giving up.")])), deps).unwrap();
        Setup { f, store, harness, events, runner_calls: None }
    };
    assert!(s.harness.run("please fix", &token(), None).await.unwrap_err().downcast_ref::<VerificationFailed>().is_some());
    assert!(s.f.read("bad.ts").contains("// user change"));
    let rollback = of_type(&s.events, "verify_rollback").remove(0);
    assert_eq!(rollback["conflicts"], json!(["bad.ts"]));
    let session = s.store.session.lock().unwrap();
    assert_eq!(session.checkpoints.as_ref().unwrap().last().unwrap().files.len(), 1);
}

#[test]
fn config_verify_env_and_cli_overrides_merge_without_dropping_defaults() {
    let f = fixture();
    let _env = fixture_env(&f);
    std::env::set_var("GOON_VERIFY_MAX_REPAIR_ROUNDS", "4");
    std::env::set_var("GOON_VERIFY_CMDS", "npm test,npm run build");
    std::env::set_var("GOON_VERIFY_REQUIRE_TESTS", "true");
    let mut overrides = Map::new();
    overrides.insert("verify".into(), json!({ "enabled": false }));
    let config = config::load_config(&overrides).unwrap();
    for k in ["GOON_VERIFY_MAX_REPAIR_ROUNDS", "GOON_VERIFY_CMDS", "GOON_VERIFY_REQUIRE_TESTS"] {
        std::env::remove_var(k);
    }
    assert!(!config.verify.enabled);
    assert_eq!(config.verify.max_repair_rounds, 4);
    assert_eq!(config.verify.commands, vec!["npm test", "npm run build"]);
    assert!(config.verify.policy.require_tests);
    assert!(config.verify.policy.allow_warnings);
}

#[test]
fn help_lists_verify_and_cost_flags() {
    let help = help::help();
    for flag in ["--no-verify", "--verify-cmd", "--max-cost-usd", "--max-time-ms"] {
        assert!(help.contains(flag), "{flag} missing from help");
    }
}
