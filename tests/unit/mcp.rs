use crate::common::*;
use goon::mcp::{mcp_tool_name, parse_codex_mcp_servers, parse_mcp_config, McpConfig};
use goon::mcp_client::McpManager;
use goon::policy::{ExecutionPolicy, NetworkMode};
use goon::risk::RiskClass;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp-server.mjs");

fn stdio_server(name: &str, mode: &str, extra: Value) -> McpConfig {
    let mut server = json!({ "transport": "stdio", "command": "node", "args": [FIXTURE, mode] });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        server[k] = v;
    }
    parse_mcp_config(&json!({ "servers": { name: server } })).unwrap()
}

fn stdio(extra: Value) -> McpConfig {
    stdio_server("local", "--stdio", extra)
}

fn annotations(extra: Value) -> McpConfig {
    stdio_server("ann", "--annotations", extra)
}

fn deny() -> Arc<ExecutionPolicy> {
    Arc::new(ExecutionPolicy::with_mode(NetworkMode::Deny, &[]))
}

fn names(reg: &tools::ToolRegistry) -> Vec<String> {
    reg.schemas().into_iter().map(|s| s.name).collect()
}

/// A Node MCP fixture over HTTP; killed (stdin closed) on drop.
struct HttpFixture {
    child: std::process::Child,
    port: u16,
}

impl HttpFixture {
    fn start(mode: &str, token: &str) -> HttpFixture {
        use std::io::BufRead;
        let mut child =
            std::process::Command::new("node").args([FIXTURE, mode]).env("MCP_FIXTURE_TOKEN", token).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().expect("node");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        HttpFixture { port: line.trim().parse().expect("port"), child }
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn stdio_discovers_pages_validates_arguments_preserves_approvals_and_unregisters_on_close() {
    let f = fixture();
    let (approve, seen) = approver(false);
    let reg = registry(&f.cwd, approve, false);
    let manager = McpManager::new(stdio(json!({ "readOnlyTools": ["echo", "wait", "fail"] })), &f.cwd, None);
    manager.connect(Some(&reg), None).await.unwrap();
    assert_eq!(manager.tools().len(), 4);
    let echo = mcp_tool_name("local", "echo");
    let danger = mcp_tool_name("local", "danger-tool");
    assert_eq!(reg.execute(&call("fixture", &echo, json!({ "text": "hello" })), &token()).await.unwrap().text, "hello");
    assert_match(&reg.execute(&call("fixture", &echo, json!({ "text": 4 })), &token()).await.err().unwrap().to_string(), "Invalid arguments");
    assert!(reg.execute(&call("fixture", &danger, json!({})), &token()).await.unwrap().denied);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_match(&reg.execute(&call("fixture", &mcp_tool_name("local", "fail"), json!({})), &token()).await.err().unwrap().to_string(), "Tool failed");
    let cancel = token();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        c.cancel();
    });
    assert!(reg.execute(&call("fixture", &mcp_tool_name("local", "wait"), json!({})), &cancel).await.is_err());
    manager.close().await;
    assert!(!names(&reg).contains(&echo));
    manager.close().await;
}

#[tokio::test]
async fn read_only_mode_uses_the_user_allowlist_and_stdio_forwards_only_configured_secrets() {
    let f = fixture();
    let _env = env(&[("GOON_API_KEY", Some("must-not-inherit")), ("GOON_TEST_FORWARD", Some("explicit-value"))]);
    let reg = registry(&f.cwd, tools::approve_all(), true);
    let manager = McpManager::new(stdio(json!({ "readOnlyTools": ["echo"], "envFrom": { "FORWARDED": "GOON_TEST_FORWARD" } })), &f.cwd, None);
    manager.connect(Some(&reg), None).await.unwrap();
    assert!(!names(&reg).contains(&mcp_tool_name("local", "danger-tool")));
    let result = reg.execute(&call("fixture", &mcp_tool_name("local", "echo"), json!({ "text": "env" })), &token()).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&result.text).unwrap(), json!({ "forwarded": "explicit-value", "apiKey": null }));
    manager.close().await;
}

#[tokio::test]
async fn streamable_http_uses_bearer_env_credentials_and_executes_real_protocol_calls() {
    let f = fixture();
    let server = HttpFixture::start("--http", "goon-mcp-test-token");
    let _env = env(&[("GOON_MCP_TEST_TOKEN", Some("goon-mcp-test-token"))]);
    let config = parse_mcp_config(&json!({ "servers": { "remote": { "transport": "http", "url": server.url(), "bearerEnv": "GOON_MCP_TEST_TOKEN", "readOnlyTools": ["echo"] } } })).unwrap();
    let reg = registry(&f.cwd, tools::deny_all(), false);
    let manager = McpManager::new(config, &f.cwd, None);
    manager.connect(Some(&reg), None).await.unwrap();
    assert_eq!(reg.execute(&call("fixture", &mcp_tool_name("remote", "echo"), json!({ "text": "remote works" })), &token()).await.unwrap().text, "remote works");
    manager.close().await;
}

#[tokio::test]
async fn request_failures_leave_no_registered_tools_and_redact_configured_credentials() {
    let f = fixture();
    let server = HttpFixture::start("--http-reject", "mcp-error-secret");
    let _env = env(&[("GOON_MCP_ERROR_TOKEN", Some("mcp-error-secret"))]);
    let reg = registry(&f.cwd, tools::approve_all(), false);
    let config = parse_mcp_config(&json!({ "servers": { "broken": { "transport": "http", "url": server.url(), "bearerEnv": "GOON_MCP_ERROR_TOKEN", "timeoutMs": 200 } } })).unwrap();
    let manager = McpManager::new(config, &f.cwd, None);
    let Err(error) = manager.connect(Some(&reg), None).await else { panic!("the connection must be rejected") };
    let error = error.to_string();
    assert!(!error.contains("mcp-error-secret"), "{error}");
    assert!(manager.tools().is_empty());

    let silent = parse_mcp_config(&json!({ "servers": { "local": { "transport": "stdio", "command": "node", "args": ["-e", "process.stdin.resume()"], "timeoutMs": 100 } } })).unwrap();
    let timeout = McpManager::new(silent, &f.cwd, None);
    let started = std::time::Instant::now();
    assert_match(&timeout.connect(Some(&reg), None).await.err().unwrap().to_string(), "(?i)timed out|timeout");
    assert!(started.elapsed().as_millis() < 5000);
    assert!(!names(&reg).iter().any(|n| n.starts_with("mcp_")));
    timeout.close().await;
}

#[tokio::test]
async fn stdio_handles_a_rea_scale_server_with_mcp_servers_config() {
    let f = fixture();
    let (approve, seen) = approver(false);
    let reg = registry(&f.cwd, approve, false);
    let config = parse_mcp_config(&json!({ "mcpServers": { "rea": { "command": "node", "args": [FIXTURE, "--rea-scale"], "trust": true } } })).unwrap();
    let manager = McpManager::new(config, &f.cwd, None);
    manager.connect(Some(&reg), None).await.unwrap();
    assert_eq!(manager.tools().len(), 207);
    let statuses = manager.statuses();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].status, "ok");
    assert_eq!(statuses[0].tool_count, 207);
    assert!(manager.tools().iter().all(|t| !t.read_only));
    let first = mcp_tool_name("rea", "rea_tool_1");
    let last = mcp_tool_name("rea", "rea_tool_207");
    let all = names(&reg);
    assert!(all.contains(&first) && all.contains(&last));
    let result = reg.execute(&call("fixture", &last, json!({ "query": "hello-rea", "mode": "fast" })), &token()).await.unwrap();
    assert_eq!(result.text, "called:rea_tool_207:hello-rea");
    assert!(seen.lock().unwrap().is_empty());
    manager.close().await;
}

#[tokio::test]
async fn codex_config_import_and_enabled_disabled_tool_filtering() {
    let toml = "\nmodel = \"gpt-5.4\"\n\n[mcp_servers.rea]\ncommand = \"rea\"\nargs = [\"mcp\", \"serve\"]\nstartup_timeout_sec = 45\ntool_timeout_sec = 120\nenabled_tools = [\"rea_tool_1\", \"rea_tool_2\"]\ndisabled_tools = [\"rea_tool_2\"]\ndefault_tools_approval_mode = \"approve\"\n\n[mcp_servers.rea.env]\nREA_MODE = \"production\"\n";
    let parsed = parse_codex_mcp_servers(toml);
    let rea = parsed.get("rea").expect("rea");
    assert_eq!(rea.transport_name(), "stdio");
    let value = rea.to_value();
    assert_eq!(value["command"], "rea");
    assert_eq!(value["args"], json!(["mcp", "serve"]));
    assert_eq!(rea.timeout_ms, 45_000);
    assert_eq!(rea.tool_timeout_ms, Some(120_000));
    assert!(rea.trust);
    assert_eq!(value["env"], json!({ "REA_MODE": "production" }));

    let f = fixture();
    let reg = registry(&f.cwd, tools::approve_all(), false);
    let config = parse_mcp_config(
        &json!({ "mcpServers": { "rea": { "command": "node", "args": [FIXTURE, "--rea-scale"], "enabledTools": ["rea_tool_1", "rea_tool_2", "rea_tool_3"], "disabledTools": ["rea_tool_2"] } } }),
    )
    .unwrap();
    let manager = McpManager::new(config, &f.cwd, None);
    manager.connect(Some(&reg), None).await.unwrap();
    let mut got: Vec<String> = manager.tools().into_iter().map(|t| t.name).collect();
    got.sort();
    let mut want = vec![mcp_tool_name("rea", "rea_tool_1"), mcp_tool_name("rea", "rea_tool_3")];
    want.sort();
    assert_eq!(got, want);
    manager.close().await;
}

#[test]
fn annotations_may_only_escalate_and_read_only_tools_is_the_only_low_risk_override() {
    let base = |extra: Value| stdio(extra).servers["local"].clone();
    let risk = |annotation: Option<Value>, extra: Value| risk::classify_mcp_risk("s", "tool", annotation.as_ref(), &base(extra));
    assert_eq!(risk(Some(json!({ "readOnlyHint": true })), json!({})).risk, RiskClass::Medium);
    assert!(!risk(Some(json!({ "readOnlyHint": true })), json!({})).net);
    assert_eq!(risk(Some(json!({ "destructiveHint": true })), json!({})).risk, RiskClass::High);
    assert_eq!(risk(Some(json!({ "openWorldHint": true })), json!({})).risk, RiskClass::High);
    assert!(risk(Some(json!({ "openWorldHint": true })), json!({})).net);
    assert_eq!(risk(None, json!({})).risk, RiskClass::Medium);
    assert_eq!(risk(None, json!({ "trust": true })).risk, RiskClass::Medium);
    assert_eq!(risk(Some(json!({ "destructiveHint": true })), json!({ "trust": true })).risk, RiskClass::High);
    assert!(risk(Some(json!({ "openWorldHint": true })), json!({ "trust": true })).net);
    assert_eq!(risk(Some(json!({ "readOnlyHint": true })), json!({ "readOnlyTools": ["tool"] })).risk, RiskClass::Low);
    assert_eq!(risk(Some(json!({ "destructiveHint": true })), json!({ "readOnlyTools": ["tool"] })).risk, RiskClass::Low);
    assert!(risk(None, json!({ "net": true })).net);
    let http = parse_mcp_config(&json!({ "servers": { "remote": { "transport": "http", "url": "http://127.0.0.1:1/mcp" } } })).unwrap();
    assert!(!risk::classify_mcp_risk("remote", "tool", Some(&json!({})), &http.servers["remote"]).net);
}

#[tokio::test]
async fn tool_specs_keep_risk_net_trusted_annotations_and_open_world_tools_are_gated_before_approval() {
    let f = fixture();
    let (approve, seen) = approver(true);
    let reg = registry(&f.cwd, approve, false);
    reg.set_policy(Some(deny()));
    let manager = McpManager::new(annotations(json!({})), &f.cwd, Some(deny()));
    manager.connect(Some(&reg), None).await.unwrap();
    let tools = manager.tools();
    assert_eq!(tools.len(), 5);
    let by: HashMap<String, mcp_client::McpToolInfo> = tools.into_iter().map(|t| (t.original_name.clone(), t)).collect();
    assert_eq!(by["plain-tool"].risk, RiskClass::Medium);
    assert!(!by["plain-tool"].net && !by["plain-tool"].trusted && !by["plain-tool"].read_only);
    assert_eq!(by["readonly-tool"].annotations, Some(json!({ "readOnlyHint": true })));
    assert_eq!(by["readonly-tool"].risk, RiskClass::Medium);
    assert_eq!(by["destructive-tool"].risk, RiskClass::High);
    assert_eq!(by["open-tool"].risk, RiskClass::High);
    assert!(by["open-tool"].net);
    assert_eq!(by["destructive-open-tool"].risk, RiskClass::High);
    assert!(by["destructive-open-tool"].net);
    let denied = reg.execute(&call("fixture", &mcp_tool_name("ann", "open-tool"), json!({})), &token()).await.unwrap();
    assert!(denied.denied);
    assert_match(&denied.text, "(?i)network");
    assert!(seen.lock().unwrap().is_empty());
    let plain = reg.execute(&call("fixture", &mcp_tool_name("ann", "plain-tool"), json!({})), &token()).await.unwrap();
    assert_eq!(plain.text, "annotated:plain-tool");
    assert_eq!(seen.lock().unwrap().len(), 1);
    manager.close().await;
}

#[tokio::test]
async fn trusted_servers_auto_approve_medium_and_low_but_never_high_risk() {
    let f = fixture();
    let (approve, seen) = approver(false);
    let reg = registry(&f.cwd, approve, false);
    reg.set_policy(Some(deny()));
    let manager = McpManager::new(annotations(json!({ "trust": true })), &f.cwd, Some(deny()));
    manager.connect(Some(&reg), None).await.unwrap();
    assert!(manager.tools().iter().all(|t| t.trusted));
    assert_eq!(reg.execute(&call("fixture", &mcp_tool_name("ann", "readonly-tool"), json!({})), &token()).await.unwrap().text, "annotated:readonly-tool");
    assert!(seen.lock().unwrap().is_empty());
    assert!(reg.execute(&call("fixture", &mcp_tool_name("ann", "destructive-tool"), json!({})), &token()).await.unwrap().denied);
    assert_eq!(seen.lock().unwrap().len(), 1);
    manager.close().await;
}

#[tokio::test]
async fn server_allow_net_exempts_its_tools_and_global_allow_net_exempts_every_server() {
    let f = fixture();
    let (approve, seen) = approver(false);
    let reg = registry(&f.cwd, approve, false);
    reg.set_policy(Some(deny()));
    let blocked = McpManager::new(annotations(json!({ "readOnlyTools": ["open-tool"] })), &f.cwd, Some(deny()));
    blocked.connect(Some(&reg), None).await.unwrap();
    assert!(reg.execute(&call("fixture", &mcp_tool_name("ann", "open-tool"), json!({})), &token()).await.unwrap().denied);
    blocked.close().await;

    let server_allowed = McpManager::new(annotations(json!({ "readOnlyTools": ["open-tool"], "allowNet": true })), &f.cwd, Some(deny()));
    server_allowed.connect(Some(&reg), None).await.unwrap();
    assert_eq!(reg.execute(&call("fixture", &mcp_tool_name("ann", "open-tool"), json!({})), &token()).await.unwrap().text, "annotated:open-tool");
    assert!(seen.lock().unwrap().is_empty());
    server_allowed.close().await;

    let allow = Arc::new(ExecutionPolicy::with_mode(NetworkMode::Allow, &[]));
    let global = registry(&f.cwd, tools::deny_all(), false);
    global.set_policy(Some(allow.clone()));
    let globally = McpManager::new(annotations(json!({ "readOnlyTools": ["open-tool"] })), &f.cwd, Some(allow));
    globally.connect(Some(&global), None).await.unwrap();
    assert_eq!(global.execute(&call("fixture", &mcp_tool_name("ann", "open-tool"), json!({})), &token()).await.unwrap().text, "annotated:open-tool");
    globally.close().await;
}

#[tokio::test]
async fn trust_does_not_expose_tools_in_read_only_mode() {
    let f = fixture();
    let reg = registry(&f.cwd, tools::approve_all(), true);
    let manager = McpManager::new(annotations(json!({ "trust": true })), &f.cwd, None);
    manager.connect(Some(&reg), None).await.unwrap();
    assert_eq!(manager.tools().len(), 5);
    assert!(manager.tools().iter().all(|t| !t.read_only));
    assert!(!names(&reg).iter().any(|n| n.starts_with("mcp_")));
    manager.close().await;
}
