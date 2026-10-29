use crate::common::*;
use goon::config::{VerifyConfig, VerifyPolicy};
use goon::verify::*;

fn policy(tests: bool, lint: bool, typecheck: bool, warnings: bool) -> VerifyPolicy {
    VerifyPolicy { require_tests: tests, require_lint: lint, require_typecheck: typecheck, allow_warnings: warnings }
}

fn loose() -> VerifyPolicy {
    policy(false, false, false, true)
}

fn strict_all() -> VerifyPolicy {
    policy(true, true, true, true)
}

fn cfg(commands: &[&str], timeout_ms: u64) -> VerifyConfig {
    VerifyConfig { enabled: true, commands: commands.iter().map(|c| c.to_string()).collect(), timeout_ms, max_repair_rounds: 1, policy: loose() }
}

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn detect_projects_maps_manifests_to_kinds_and_stops_at_the_workspace_boundary() {
    let f = fixture();
    f.write("package.json", "{}");
    f.write("pyproject.toml", "");
    let projects = detect_projects(&f.cwd, &strings(&["src/a.ts", "lib/b.py"]));
    assert_eq!(projects.iter().map(|p| p.kind).collect::<Vec<_>>(), vec!["node", "python"]);
    assert_eq!(projects[0].files, vec!["src/a.ts"]);
    assert_eq!(projects[1].files, vec!["lib/b.py"]);
    std::fs::write(f.root.join("Cargo.toml"), "").unwrap();
    let child = f.cwd.join("nested");
    std::fs::create_dir_all(&child).unwrap();
    let unknown = detect_projects(&child, &strings(&["x.rs"]));
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].kind, "unknown");
    assert_eq!(unknown[0].files, vec!["x.rs"]);
    f.write("go.mod", "");
    assert!(detect_projects(&f.cwd, &strings(&["main.go"])).iter().any(|p| p.kind == "go"));
}

#[test]
fn scope_files_filters_deduplicates_sorts_and_skips_generated_directories() {
    let f = fixture();
    f.write("a.ts", "");
    f.write("node_modules/x/b.ts", "");
    assert_eq!(scope_files(&strings(&["./b.ts", "a.ts", "a.ts", "node_modules/x/b.ts", ".git/x", "dist/y", "missing.ts"]), &f.cwd), vec!["a.ts"]);
}

#[test]
fn resolve_local_tool_uses_node_modules_and_node() {
    let f = fixture();
    f.write("node_modules/typescript/bin/tsc", "");
    let (command, args) = resolve_local_tool(&f.cwd, "tsc").expect("tsc");
    assert_match(&command, "node(\\.exe)?$");
    assert_match(&args[0], r"typescript[\\/]+bin[\\/]+tsc$");
    assert!(resolve_local_tool(&f.cwd, "prettier").is_none());
}

#[test]
fn plan_verification_defaults_to_syntax_only() {
    let f = fixture();
    f.write("package.json", "{\"scripts\":{\"test\":\"node --test\"}}");
    let plan = plan_verification(&f.cwd, &strings(&["src/a.ts"]), &cfg(&[], 1000), &loose());
    assert_eq!(plan.stages, vec!["syntax"]);
    assert!(plan.commands.is_empty());
    assert_eq!(plan.projects[0].kind, "node");
}

#[test]
fn plan_verification_honours_require_flags_and_package_scripts() {
    let f = fixture();
    f.write("package.json", &json!({ "scripts": { "lint": "oxlint src", "typecheck": "tsc --noEmit", "build": "tsc", "test": "node --test" } }).to_string());
    f.write("tsconfig.json", "{}");
    f.write("node_modules/typescript/bin/tsc", "");
    f.write("src/a.ts", "export const x = 1;\n");
    let plan = plan_verification(&f.cwd, &strings(&["src/a.ts"]), &cfg(&[], 1000), &strict_all());
    assert_eq!(plan.stages, vec!["syntax", "lint", "typecheck", "build", "test"]);
    assert_eq!(plan.commands.iter().map(|c| c.display.as_str()).collect::<Vec<_>>(), vec!["npm run lint", "npm run typecheck", "npm run build", "npm run test"]);
    let dir = fixture();
    dir.write("package.json", "{}");
    dir.write("tsconfig.json", "{}");
    dir.write("node_modules/typescript/bin/tsc", "");
    dir.write("src/a.ts", "export const x = 1;\n");
    let plan2 = plan_verification(&dir.cwd, &strings(&["src/a.ts"]), &cfg(&[], 1000), &policy(false, true, true, true));
    assert!(plan2.commands.iter().any(|c| c.display == "tsc --noEmit -p tsconfig.json"));
}

#[test]
fn configured_commands_replace_the_matrix() {
    let f = fixture();
    let plan = plan_verification(&f.cwd, &strings(&["src/a.ts"]), &cfg(&["!echo custom", "node --version"], 1000), &strict_all());
    assert_eq!(plan.commands.iter().map(|c| c.display.as_str()).collect::<Vec<_>>(), vec!["!echo custom", "node --version"]);
    assert_eq!(plan.commands[0].command, "bash");
    assert_eq!(plan.commands[0].args, vec!["-c", "echo custom"]);
    assert_eq!(parse_configured_command("!echo custom").unwrap().stage, "test");
}

fn result(code: i32, output: &str) -> RunnerResult {
    RunnerResult { code, output: output.into(), timed_out: false, not_found: false }
}

#[test]
fn parse_command_result_maps_exit_codes_timeouts_missing_tools_and_missing_modules() {
    let p = strict_all();
    let strict = policy(true, true, true, false);
    assert_eq!(parse_command_result("lint", &result(0, ""), &p).severity, Severity::Pass);
    let failed = parse_command_result("lint", &result(1, "src/a.ts:3:5: Unexpected token"), &p);
    assert_eq!(failed.severity, Severity::Fail);
    assert_eq!((failed.issues[0].path.as_str(), failed.issues[0].line, failed.issues[0].column), ("src/a.ts", Some(3), Some(5)));
    let paren = parse_command_result("typecheck", &result(1, "src/a.ts(3,5): error TS2322: boom"), &p);
    assert_eq!((paren.issues[0].line, paren.issues[0].column), (Some(3), Some(5)));
    let timeout = RunnerResult { timed_out: true, ..result(1, "") };
    assert_eq!(parse_command_result("test", &timeout, &p).reason.as_deref(), Some("timeout"));
    let missing = RunnerResult { not_found: true, ..result(127, "sh: 1: eslint: not found") };
    assert_eq!(parse_command_result("lint", &missing, &p).severity, Severity::Skip);
    assert_eq!(parse_command_result("lint", &result(1, "sh: 1: eslint: not found"), &p).severity, Severity::Skip);
    assert_eq!(parse_command_result("lint", &result(1, "bash: line 1: foo: command not found"), &p).severity, Severity::Skip);
    assert_eq!(parse_command_result("test", &result(1, "Error: Cannot find module './app'"), &p).severity, Severity::Fail);
    assert_eq!(parse_command_result("test", &result(1, "ENOENT: no such file or directory, open 'fixture.json'"), &p).severity, Severity::Fail);
    for output in ["AssertionError: not found", "Error: expected element: not found", "AssertionError [ERR_ASSERTION]: not found"] {
        assert_eq!(parse_command_result("test", &result(1, output), &p).severity, Severity::Fail, "{output}");
    }
    assert_eq!(parse_command_result("lint", &result(0, "src/a.ts:3: warning: unused"), &p).severity, Severity::Warn);
    assert_eq!(parse_command_result("lint", &result(0, "src/a.ts:3: warning: unused"), &strict).severity, Severity::Fail);
    assert_eq!(parse_command_result("format", &result(0, "src/a.go\n"), &p).severity, Severity::Warn);
}

#[test]
fn parse_command_result_normalizes_windows_paths() {
    let r = parse_command_result("lint", &result(1, "src\\a.ts:3: boom"), &strict_all());
    assert_eq!(r.issues[0].path, "src/a.ts");
}

struct Scripted {
    result: RunnerResult,
    calls: AtomicUsize,
    env: Mutex<Option<std::collections::BTreeMap<String, String>>>,
}

impl Scripted {
    fn new(result: RunnerResult) -> Arc<Scripted> {
        Arc::new(Scripted { result, calls: AtomicUsize::new(0), env: Mutex::new(None) })
    }
}

impl VerifyRunner for Scripted {
    fn run(&self, _command: String, _args: Vec<String>, opts: RunnerOptions) -> BoxFuture<'static, RunnerResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.env.lock().unwrap() = opts.env.clone();
        let r = self.result.clone();
        Box::pin(async move { r })
    }
}

async fn verify(f: &Fixture, changed: &[&str], config: &VerifyConfig, runner: &dyn VerifyRunner, approve: Option<bool>) -> Result<VerifyVerdict> {
    let changed = strings(changed);
    let mut plan = plan_verification(&f.cwd, &changed, config, &config.policy);
    let approve_fn = approve.map(|answer| move |_: &VerifyPlan| -> BoxFuture<'static, bool> { Box::pin(async move { answer }) });
    let approve_ref: Option<&(dyn Fn(&VerifyPlan) -> BoxFuture<'static, bool> + Send + Sync)> = approve_fn.as_ref().map(|a| a as &(dyn Fn(&VerifyPlan) -> BoxFuture<'static, bool> + Send + Sync));
    run_verification(&f.cwd, &changed, config, runner, &token(), &mut plan, approve_ref).await
}

#[tokio::test]
async fn run_verification_runs_the_syntax_stage_without_a_runner() {
    let f = fixture();
    f.write("bad.ts", "export const x = {\n");
    let runner = Scripted::new(result(0, ""));
    let verdict = verify(&f, &["bad.ts"], &cfg(&[], 1000), &*runner, None).await.unwrap();
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    assert!(!verdict.pass);
    assert_eq!(verdict.stages[0].stage, "syntax");
    assert_eq!(verdict.stages[0].severity, Severity::Fail);
    f.write("good.ts", "export const x = 1;\n");
    assert!(verify(&f, &["good.ts"], &cfg(&[], 1000), &*runner, None).await.unwrap().pass);
}

#[tokio::test]
async fn run_verification_is_fail_closed_on_approval() {
    let f = fixture();
    let config = cfg(&["node --version"], 1000);
    let error = verify(&f, &[], &config, &CommandRunner, None).await.unwrap_err();
    assert!(error.downcast_ref::<VerificationApprovalError>().is_some());
    let runner = Scripted::new(result(0, ""));
    let denied = verify(&f, &[], &config, &*runner, Some(false)).await.unwrap();
    assert_eq!(denied.severity, Severity::Skip);
    assert!(denied.pass);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    assert!(verify(&f, &[], &config, &*runner, Some(true)).await.unwrap().pass);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn run_verification_executes_commands_and_reports_failures() {
    let f = fixture();
    let config = cfg(&["node --version"], 1000);
    assert!(verify(&f, &[], &config, &*Scripted::new(result(0, "v22.0.0")), Some(true)).await.unwrap().pass);
    let fail = verify(&f, &[], &config, &*Scripted::new(result(1, "src/a.ts:3: message")), Some(true)).await.unwrap();
    assert!(!fail.pass);
    assert_eq!((fail.errors[0].path.as_str(), fail.errors[0].line), ("src/a.ts", Some(3)));
    let timeout = verify(&f, &[], &config, &*Scripted::new(RunnerResult { timed_out: true, ..result(124, "") }), Some(true)).await.unwrap();
    assert!(!timeout.pass);
    assert_eq!(timeout.stages.iter().find(|s| s.severity == Severity::Fail).and_then(|s| s.reason.clone()).as_deref(), Some("timeout"));
    let skipped = verify(&f, &[], &config, &*Scripted::new(RunnerResult { not_found: true, ..result(127, "") }), Some(true)).await.unwrap();
    assert_eq!(skipped.severity, Severity::Skip);
    assert!(skipped.pass);
}

#[tokio::test]
async fn run_verification_passes_offline_go_environment_to_the_runner() {
    let f = fixture();
    let mut plan = VerifyPlan { stages: strings(&["test"]), commands: vec![VerifyCommand { stage: "test".into(), command: "go".into(), args: strings(&["test", "./..."]), display: "go test ./...".into() }], projects: vec![], approved: true };
    let runner = Scripted::new(result(0, ""));
    let verdict = run_verification(&f.cwd, &[], &cfg(&[], 1000), &*runner, &token(), &mut plan, None).await.unwrap();
    assert!(verdict.pass);
    let env = runner.env.lock().unwrap().clone().unwrap();
    assert_eq!(env["GOPROXY"], "off");
    assert_eq!(env["GOFLAGS"], "-mod=readonly");
}

fn opts(timeout_ms: u64) -> RunnerOptions {
    RunnerOptions { cwd: std::env::current_dir().unwrap(), timeout_ms, cancel: token(), env: None }
}

#[tokio::test]
async fn command_runner_maps_success_missing_tool_and_timeout() {
    let ok = CommandRunner.run("node".into(), strings(&["-e", "console.log('ok')"]), opts(5000)).await;
    assert_eq!(ok.code, 0);
    assert_match(&ok.output, "ok");
    assert!(CommandRunner.run("definitely-not-a-real-tool-xyz".into(), vec![], opts(2000)).await.not_found);
    assert!(CommandRunner.run("node".into(), strings(&["-e", "setInterval(()=>{},1000)"]), opts(200)).await.timed_out);
}

#[test]
fn redact_verdict_and_redact_text_remove_secrets_and_credential_patterns() {
    let issue = |m: &str| VerifyIssue { stage: "test".into(), path: String::new(), line: None, column: None, message: m.into() };
    let verdict = VerifyVerdict {
        pass: false,
        severity: Severity::Fail,
        stages: vec![StageResult { stage: "test".into(), severity: Severity::Fail, duration_ms: 1, issues: vec![issue("token=secret-token bearer abc.def")], output: Some("Authorization: Bearer abc.def".into()), reason: None }],
        errors: vec![issue("token=secret-token")],
        warnings: vec![],
        reason: "secret-token failed".into(),
    };
    let redacted = redact_verdict(&verdict, &["secret-token"]);
    assert!(!format!("{redacted:?}").contains("secret-token"));
    assert_match(redacted.stages[0].output.as_deref().unwrap(), r"\[redacted\]");
    assert_match(&redact_text("{\"token\":\"hunter2\"}", &[]), r"\[redacted\]");
}

#[test]
fn format_verify_errors_and_warnings_produce_readable_text() {
    let verdict = VerifyVerdict {
        pass: false,
        severity: Severity::Fail,
        stages: vec![],
        errors: vec![VerifyIssue { stage: "lint".into(), path: "src/a.ts".into(), line: Some(3), column: Some(5), message: "boom".into() }],
        warnings: vec![VerifyIssue { stage: "lint".into(), path: "src/b.ts".into(), line: Some(1), column: None, message: "warn".into() }],
        reason: "lint failed".into(),
    };
    assert_match(&format_verify_errors(&verdict), "lint failed");
    assert_match(&format_verify_errors(&verdict), r"src/a\.ts:3:5: boom");
    assert_match(&format_verify_warnings(&verdict), r"src/b\.ts:1: warn");
}
