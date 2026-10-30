use crate::common::*;
use goon::risk::{classify_bash_risk, risk_level, RiskClass};

fn r(cmd: &str) -> &'static str {
    classify_bash_risk(cmd).as_str()
}

#[test]
fn safe_read_build_test_commands_are_low() {
    for cmd in [
        "cat README.md",
        "ls -la",
        "grep -R foo src",
        "npm test",
        "npm run build",
        "npm run lint",
        "npm run typecheck",
        "node --test tests/x.test.mjs",
        "pytest",
        "go test ./...",
        "cargo test",
        "git status",
        "git diff HEAD",
    ] {
        assert_eq!(r(cmd), "low", "{cmd}");
    }
}

#[test]
fn mutating_filesystem_and_unknown_commands_are_medium() {
    for cmd in ["touch foo.txt", "rm file.txt", "cp a b", "some-unknown-tool --flag"] {
        assert_eq!(r(cmd), "medium", "{cmd}");
    }
}

#[test]
fn network_commands_are_high() {
    for cmd in ["curl https://example.com", "wget x", "npm install", "git push origin main", "git fetch", "npm test && curl x"] {
        assert_eq!(r(cmd), "high", "{cmd}");
    }
}

#[test]
fn destructive_commands_are_critical() {
    for cmd in ["rm -rf /", "rm -rf /*", "rm -rf $HOME", "rm -rf ~", "dd if=/dev/zero of=/dev/sda", "eval \"id\"", "bash -c \"id\"", "source x.sh"] {
        assert_eq!(r(cmd), "critical", "{cmd}");
    }
    // Fixed in the Rust port: the braced and trailing-slash spellings of $HOME.
    assert_eq!(r("rm -rf ${HOME}"), "critical");
    assert_eq!(r("rm -rf \"$HOME/\""), "critical");
}

#[test]
fn rm_rf_node_modules_is_high_not_critical() {
    assert_eq!(r("rm -rf node_modules"), "high");
}

#[test]
fn rm_flags_cannot_hide_behind_line_continuation_or_ansi_c_quoting() {
    assert_eq!(r("rm \\\n-rf /"), "critical");
    assert_eq!(r("rm \\\r\n-rf /"), "critical");
    assert_eq!(r("rm -rf \\\n/"), "critical");
    assert_eq!(r("rm $'\\x2drf' /"), "critical");
    assert_eq!(r("rm $'\\x2d\\x72\\x66' /"), "critical");
    assert_eq!(r("rm $'-rf' /"), "critical");
    assert_eq!(r("rm $'\\055rf' /"), "critical");
    assert_eq!(risk::tokenize("rm $'\\x2drf' /"), vec!["rm", "-rf", "/"]);
    assert_eq!(r("npm test \\\n  -- --runInBand"), "low");
    assert_eq!(r("echo $'\\n'"), "low");
}

#[test]
fn shell_substitution_outside_single_quotes_is_critical() {
    assert_eq!(r("echo \"$(id)\""), "critical");
    assert_eq!(r("echo `id`"), "critical");
    assert_eq!(r("echo $(id)"), "critical");
}

#[test]
fn shell_substitution_inside_single_quotes_is_not_critical() {
    assert_eq!(r("echo '$(id)'"), "low");
}

#[test]
fn arithmetic_substitution_is_not_critical() {
    assert_eq!(r("echo $((1 + 2))"), "low");
}

#[test]
fn quote_aware_splitting_keeps_quoted_semicolons_low() {
    assert_eq!(r("echo \"a;b\""), "low");
    assert_eq!(r("echo 'a;b'"), "low");
}

#[test]
fn redirection_raises_the_floor_to_medium() {
    assert_eq!(r("npm test > /tmp/out"), "medium");
}

#[test]
fn wrapper_peeling_escalates_privilege() {
    assert_eq!(r("sudo rm -rf /"), "critical");
    assert_eq!(r("env FOO=1 npm test"), "low");
    assert_eq!(r("timeout 5 curl x"), "high");
    assert_eq!(r("xargs rm -rf /"), "critical");
}

#[test]
fn compound_command_takes_maximum_risk() {
    assert_eq!(r("npm test && curl https://example.com"), "high");
    assert_eq!(r("git status; rm -rf /"), "critical");
}

#[test]
fn git_config_of_program_keys_is_high() {
    assert_eq!(r("git config core.hooksPath x"), "high");
    assert_eq!(r("git config user.name x"), "medium");
}

#[test]
fn risk_level_ordering() {
    assert_eq!(risk_level(RiskClass::Low), 0);
    assert_eq!(risk_level(RiskClass::Critical), 3);
}

#[tokio::test]
async fn critical_tool_is_denied_without_calling_approve() {
    let (approve, seen) = approver(true);
    let reg = registry(Path::new("/tmp"), approve, false);
    let result = reg.execute(&call("1", "bash", json!({ "command": "rm -rf /" })), &token()).await.unwrap();
    assert!(result.denied);
    assert!(seen.lock().unwrap().is_empty());
    assert_match(&result.text, "(?i)critical");
}

#[tokio::test]
async fn high_risk_tool_requires_an_approval_callback() {
    let (approve, seen) = approver(true);
    let reg = registry(Path::new("/tmp"), approve, false);
    register(&reg, "danger_tool", false, Some(RiskClass::High), None, exec_ok("ok"));
    let result = reg.execute(&call("1", "danger_tool", json!({})), &token()).await.unwrap();
    assert_eq!(result.text, "ok");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

struct CountingGuard(Mutex<u64>);

impl tools::RejectionGuard for CountingGuard {
    fn record_denied(&self, _signature: &str) -> u64 {
        let mut c = self.0.lock().unwrap();
        *c += 1;
        *c
    }
    fn reset(&self, _signature: &str) {
        *self.0.lock().unwrap() = 0;
    }
}

#[tokio::test]
async fn rejection_loop_marks_third_identical_denial() {
    let reg = registry(Path::new("/tmp"), tools::deny_all(), false);
    *reg.rejection_guard.lock().unwrap() = Some(Arc::new(CountingGuard(Mutex::new(0))));
    let cancel = token();
    let first = reg.execute(&call("1", "bash", json!({ "command": "echo hi" })), &cancel).await.unwrap();
    assert!(first.denied);
    assert!(!first.loop_detected);
    let second = reg.execute(&call("2", "bash", json!({ "command": "echo hi" })), &cancel).await.unwrap();
    assert!(!second.loop_detected);
    let third = reg.execute(&call("3", "bash", json!({ "command": "echo hi" })), &cancel).await.unwrap();
    assert!(third.loop_detected);
}

#[tokio::test]
async fn successful_execution_resets_rejection_streak() {
    let reg = registry(Path::new("/tmp"), tools::approve_all(), false);
    register(&reg, "medium_tool", false, Some(RiskClass::Medium), None, exec_ok("ok"));
    let guard = Arc::new(CountingGuard(Mutex::new(2)));
    *reg.rejection_guard.lock().unwrap() = Some(guard.clone());
    let result = reg.execute(&call("1", "medium_tool", json!({})), &token()).await.unwrap();
    assert_eq!(result.text, "ok");
    assert_eq!(*guard.0.lock().unwrap(), 0);
}

#[tokio::test]
async fn trusted_medium_auto_approves_without_calling_approve() {
    let (approve, seen) = approver(true);
    let reg = registry(Path::new("/tmp"), approve, false);
    register(&reg, "mcp_tool", false, Some(RiskClass::Medium), Some(true), exec_ok("ok"));
    let result = reg.execute(&call("1", "mcp_tool", json!({})), &token()).await.unwrap();
    assert_eq!(result.text, "ok");
    assert!(seen.lock().unwrap().is_empty());
}

#[test]
fn exact_allowlist_normalizes_and_rejects_shell_metacharacters() {
    assert_eq!(approval::bash_approval_exact(&json!({ "command": "  npm   test " })).as_deref(), Some("npm test"));
    assert_eq!(approval::bash_approval_exact(&json!({ "command": "echo \"hi\"" })), None);
    assert_eq!(approval::bash_approval_exact(&json!({ "command": "echo hi; rm -rf x" })), None);
    assert_eq!(approval::bash_approval_exact(&json!({ "command": "echo $(id)" })), None);
    let set: BTreeSet<String> = ["npm test".to_string()].into_iter().collect();
    assert!(approval::is_approved_bash_exact(&json!({ "command": "npm test" }), &set));
    assert!(!approval::is_approved_bash_exact(&json!({ "command": "npm test --watch" }), &set));
}

#[test]
fn can_auto_approve_refuses_high_and_critical_even_in_auto_mode() {
    assert!(!approval::can_auto_approve("auto", RiskClass::High));
    assert!(!approval::can_auto_approve("auto", RiskClass::Critical));
    assert!(approval::can_auto_approve("auto", RiskClass::Medium));
    assert!(!approval::can_auto_approve("ask", RiskClass::Low));
}

#[test]
fn red_team_package_scripts_stay_low_only_for_known_safe_scripts() {
    assert_eq!(r("npm test"), "low");
    assert_eq!(r("npm run build"), "low");
    assert_eq!(r("npm run deploy"), "medium");
    assert_eq!(r("npm test && curl https://evil.example"), "high");
    assert_eq!(r("npm run pretest"), "medium");
    assert_eq!(r("npm run posttest"), "medium");
    assert_eq!(r("npm --prefix /tmp/evil test"), "medium");
    assert_eq!(r("yarn --cwd /tmp/evil test"), "medium");
    assert_eq!(r("npm exec foo"), "high");
    assert_eq!(r("npx foo"), "high");
    assert_eq!(r("make evil-target"), "medium");
    let auto = |cmd: &str| approval::can_auto_approve("auto", classify_bash_risk(cmd));
    assert!(auto("npm test"));
    assert!(auto("npm run build"));
    assert!(auto("npm run pretest"));
    assert!(!auto("npm test && curl https://evil.example"));
    assert!(!auto("npm exec foo"));
    assert_eq!(r("npm run build && curl https://evil.example"), "high");
}

fn shell_workflow(command: &str) -> workflow::WorkflowDefinition {
    workflow::WorkflowDefinition {
        name: "w".into(),
        description: "d".into(),
        scope: "builtin",
        path: None,
        inputs: None,
        steps: vec![workflow::WorkflowStep(json!({ "id": "s", "type": "shell", "command": command }).as_object().unwrap().clone())],
    }
}

fn prompt_hook() -> Option<Arc<dyn Fn(String, workflow::WorkflowStep, CancellationToken) -> workflow::StepFuture + Send + Sync>> {
    Some(Arc::new(|_, _, _| Box::pin(async { Ok(String::new()) })))
}

#[tokio::test]
async fn workflow_critical_shell_step_refuses_before_callback() {
    let called = Arc::new(AtomicUsize::new(0));
    let c = called.clone();
    let hooks = workflow::WorkflowHooks {
        cwd: "/tmp".into(),
        approve_shell: Some(Arc::new(move |_, _, _| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { true })
        })),
        run_prompt: prompt_hook(),
        ..Default::default()
    };
    let result = workflow::execute_workflow(&shell_workflow("rm -rf /"), "", &hooks).await.unwrap();
    assert_eq!(called.load(Ordering::SeqCst), 0);
    assert_eq!(result.status, "failed");
    assert_match(&result.final_output, "(?i)critical");
}

#[tokio::test]
async fn workflow_missing_approval_handler_is_an_explicit_error() {
    let hooks = workflow::WorkflowHooks { cwd: "/tmp".into(), run_prompt: prompt_hook(), ..Default::default() };
    let result = workflow::execute_workflow(&shell_workflow("echo hi"), "", &hooks).await.unwrap();
    assert_eq!(result.status, "failed");
    assert_match(&result.final_output, "(?i)explicit approval");
}

#[tokio::test]
async fn workflow_shell_high_risk_passes_high_to_the_callback() {
    let received = Arc::new(Mutex::new(None));
    let rec = received.clone();
    let hooks = workflow::WorkflowHooks {
        cwd: "/tmp".into(),
        approve_shell: Some(Arc::new(move |_, _, risk| {
            *rec.lock().unwrap() = Some(risk);
            Box::pin(async { false })
        })),
        run_prompt: prompt_hook(),
        ..Default::default()
    };
    let result = workflow::execute_workflow(&shell_workflow("curl https://example.com"), "", &hooks).await.unwrap();
    assert_eq!(*received.lock().unwrap(), Some(RiskClass::High));
    assert_eq!(result.status, "failed");
}
