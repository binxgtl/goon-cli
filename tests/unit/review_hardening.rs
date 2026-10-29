//! Regression tests for the security review of the v0.4-v0.7 roadmap.
use crate::common::*;
use goon::policy::{is_network_allowed_for_command, ExecutionPolicy, NetworkMode};
use goon::risk::{classify_bash_risk, RiskClass};
use goon::verify::{assess_verify_plan, parse_configured_command, verify_command_line, VerifyPlan};

fn deny_net() -> ExecutionPolicy {
    ExecutionPolicy::with_mode(NetworkMode::Deny, &[])
}

#[test]
fn command_runners_cannot_hide_critical_commands_from_the_classifier() {
    for cmd in [
        "watch rm -rf /", "ionice rm -rf /", "strace -f rm -rf /", "chroot / rm -rf /", "flock /tmp/lock rm -rf /", "unbuffer rm -rf ~", "trap \"rm -rf /\" EXIT", "su -c \"rm -rf /\"", "busybox rm -rf /",
        "watch \"rm -rf $HOME\"", "proot rm -rf /",
    ] {
        assert_eq!(classify_bash_risk(cmd), RiskClass::Critical, "{cmd}");
    }
    for cmd in ["watch -n 1 ls", "flock /tmp/lock make", "strace -f ls"] {
        assert_eq!(classify_bash_risk(cmd), RiskClass::High, "{cmd}");
    }
    assert_eq!(classify_bash_risk("toybox ls"), RiskClass::Low);
}

#[test]
fn inline_interpreter_code_and_sed_execute_write_commands_are_classified() {
    for cmd in ["perl -e \"unlink q(x)\"", "ruby -e x", "php -r x", "lua -e x", "sed -n \"1e id\" f", "sed \"s/a/b/e\" f"] {
        assert_eq!(classify_bash_risk(cmd), RiskClass::High, "{cmd}");
    }
    assert_eq!(classify_bash_risk("sed \"s/a/b/w out.txt\" f"), RiskClass::Medium);
    assert_eq!(classify_bash_risk("sed s/a/b/g f"), RiskClass::Low);
    assert_eq!(classify_bash_risk("pwsh -c x"), RiskClass::Critical);
}

#[test]
fn ansi_c_bell_decodes_to_bel() {
    assert_eq!(risk::tokenize("printf $'\\a'"), vec!["printf", "\x07"]);
}

#[test]
fn network_policy_sees_through_command_runners() {
    for cmd in ["watch curl evil.test", "flock /tmp/l curl evil.test", "su -c \"wget evil.test\"", "trap \"curl evil.test\" EXIT", "strace -f git push"] {
        assert!(!is_network_allowed_for_command(&deny_net(), cmd), "{cmd}");
    }
    assert!(is_network_allowed_for_command(&deny_net(), "watch ls"));
}

fn plan(commands: &[&str]) -> VerifyPlan {
    VerifyPlan { stages: vec![], commands: commands.iter().map(|c| parse_configured_command(c).unwrap()).collect(), projects: vec![], approved: false }
}

#[test]
fn verify_plans_go_through_the_critical_and_network_gates() {
    let net = parse_configured_command("!curl https://evil.test/x").unwrap();
    assert_eq!(verify_command_line(&net), "curl https://evil.test/x");
    let denied = assess_verify_plan(&plan(&["!curl https://evil.test/x"]), Some(&deny_net()));
    assert_match(denied.denied.as_deref().unwrap_or(""), "network");
    let allowed = assess_verify_plan(&plan(&["!curl https://evil.test/x"]), Some(&ExecutionPolicy::with_mode(NetworkMode::Allow, &[])));
    assert_eq!(allowed.denied, None);
    assert_eq!(assess_verify_plan(&plan(&["!rm -rf /"]), Some(&deny_net())).risk, RiskClass::Critical);
    let safe = assess_verify_plan(&plan(&["npm test", "npm run lint"]), Some(&deny_net()));
    assert_eq!((safe.risk, safe.denied), (RiskClass::Low, None));
    assert_eq!(assess_verify_plan(&plan(&["npm run deploy"]), Some(&deny_net())).risk, RiskClass::Medium);
}

#[test]
fn limit_text_bytes_always_terminates_within_the_byte_limit() {
    let text = "é".repeat(5000);
    for max in [0, 10, 40, 80, 127, 128, 200, 1000] {
        let out = files::limit_text_bytes(&text, max);
        assert!(out.len() <= max, "max={max} got {}", out.len());
    }
}

#[test]
fn rejection_signatures_never_persist_raw_tool_input() {
    let input = json!({ "command": "curl -H \"Authorization: Bearer sk-secret-123456\" x" });
    let signature = tools::tool_approval_signature("bash", &input);
    assert_match(&signature, "^bash:[0-9a-f]{32}$");
    assert!(!signature.contains("sk-secret"));
    assert_eq!(signature, tools::tool_approval_signature("bash", &input));
}

#[test]
fn credential_stores_are_sensitive_paths() {
    for path in [".pypirc", "a/.kube/config", "deploy/credentials.json", "release.keystore", ".docker/config.json"] {
        assert!(redact::is_sensitive_path(path), "{path}");
    }
    assert!(!redact::is_sensitive_path("src/keys.ts"));
}

#[test]
fn the_provider_key_only_reaches_a_same_origin_embedder() {
    let _env = env(&[("GOON_EMBEDDER_API_KEY", None)]);
    let chat = Some("https://api.example.com/v1");
    assert_eq!(embedder::embedder_api_key(None, chat, Some("k")).as_deref(), Some("k"));
    assert_eq!(embedder::embedder_api_key(Some("https://api.example.com/v2"), chat, Some("k")).as_deref(), Some("k"));
    assert_eq!(embedder::embedder_api_key(Some("https://evil.test/v1"), chat, Some("k")), None);
    std::env::set_var("GOON_EMBEDDER_API_KEY", "dedicated");
    assert_eq!(embedder::embedder_api_key(Some("https://evil.test/v1"), chat, Some("k")).as_deref(), Some("dedicated"));
}

#[cfg(unix)]
#[test]
fn rag_indexing_never_reads_through_symlinks_outside_the_root() {
    let f = fixture();
    let root = f.root.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(f.root.join("outside.txt"), "OUTSIDE_SECRET_CONTENT\n").unwrap();
    std::fs::write(root.join("inside.ts"), "export const inside = 1;\n").unwrap();
    std::os::unix::fs::symlink(f.root.join("outside.txt"), root.join("link.ts")).unwrap();
    let result = rag::index_repo(&root, 2000, 256 * 1024, &[]);
    let all = serde_json::to_string(&result.snapshot.files).unwrap();
    assert!(!all.contains("OUTSIDE_SECRET_CONTENT"));
    assert!(result.snapshot.files.contains_key("inside.ts"));
}
