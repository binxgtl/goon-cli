//! Session-scoped approval of simple shell command prefixes and exact commands.
//!
//! Approving the prefix "npm test" lets later commands that start with exactly those words run
//! without a prompt. Extra arguments are allowed only while they stay harmless: anything pointing
//! outside the workspace or switching a command into "write a file / run a program" mode asks.

use crate::risk::RiskClass;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Anything that can chain, substitute, redirect or group commands disqualifies a command.
fn shell_meta_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[;&|`$<>(){}\\\n\r]").unwrap())
}

const LAUNCHER_SUBCOMMANDS: &[&str] = &["run", "exec", "x", "dlx", "-m"];
/// Commands that run other commands or rewrite their environment: never approvable by prefix.
const ALWAYS_EXPLICIT: &[&str] = &[
    "env", "sudo", "doas", "su", "xargs", "eval", "exec", "command", "builtin", "source", ".", "nohup", "time", "nice", "timeout",
    "watch", "ssh", "find", "bash", "sh", "zsh", "fish", "dash", "pwsh", "powershell", "flock", "strace", "ltrace", "chroot",
    "ionice", "unbuffer", "setsid", "stdbuf", "busybox", "toybox", "proot", "tsu", "script", "parallel", "trap",
];
/// Interpreters may be approved for a specific script/module, never as a bare single-word prefix.
const INTERPRETERS: &[&str] = &["python", "python3", "node", "deno", "bun", "ruby", "perl", "php", "awk", "sed", "git"];
/// Commands whose arguments decide everything: only the exact same command line is remembered.
const EXACT_ONLY: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "ln", "chmod", "chown", "chgrp", "dd", "truncate", "shred", "mkfs", "curl", "wget", "scp", "rsync",
    "nc", "ncat", "kill", "pkill", "killall", "docker", "podman", "kubectl",
];
const PACKAGE_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun", "pip", "pip3", "uv", "poetry", "cargo", "go", "gem", "composer", "apt", "apt-get", "pkg", "brew", "dnf", "pacman"];
const INSTALL_SUBCOMMANDS: &[&str] = &["install", "i", "add", "ci", "update", "upgrade", "up", "get", "remove", "rm", "uninstall", "un", "sync"];

fn risky_option_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^--?(?:o|output|out|outfile|out-file|outdir|out-dir|output-dir|exec|ext-diff|upload-pack|receive-pack|config|c|eval|e|command|shell|pager|editor|global|prefix|target-dir|init-file|rcfile|require|r|import|loader)(?:=|$)").unwrap()
    })
}

fn command_of(input: &Value) -> Option<&str> {
    input.get("command").and_then(Value::as_str)
}

fn command_tokens(input: &Value) -> Option<Vec<String>> {
    let trimmed = command_of(input)?.trim();
    if trimmed.is_empty() || shell_meta_re().is_match(trimmed) || trimmed.contains('"') || trimmed.contains('\'') {
        return None;
    }
    Some(trimmed.split_whitespace().map(str::to_string).collect())
}

/// An argument that names something outside the workspace (absolute, home-relative or `..`).
fn leaves_workspace(token: &str) -> bool {
    let value = match token.find('=') {
        Some(i) => &token[i + 1..],
        None => token,
    };
    static ABS: OnceLock<Regex> = OnceLock::new();
    static PARENT: OnceLock<Regex> = OnceLock::new();
    ABS.get_or_init(|| Regex::new(r"^(?:/|~|[A-Za-z]:[\\/]|\\\\)").unwrap()).is_match(value)
        || PARENT.get_or_init(|| Regex::new(r"(?:^|[\\/])\.\.(?:[\\/]|$)").unwrap()).is_match(value)
}

fn command_name(token: &str) -> String {
    crate::risk::base_name(token)
}

/// Build the prefix that "always allow" may remember for a bash command.
pub fn bash_approval_prefix(input: &Value) -> Option<String> {
    let tokens = command_tokens(input)?;
    if tokens.is_empty() || tokens[0].contains('=') {
        return None;
    }
    let name = command_name(&tokens[0]);
    if ALWAYS_EXPLICIT.contains(&name.as_str()) {
        return None;
    }
    if tokens.iter().any(|t| leaves_workspace(t)) {
        return None;
    }
    let second = tokens.get(1).map(String::as_str).unwrap_or("");
    if EXACT_ONLY.contains(&name.as_str()) || (PACKAGE_MANAGERS.contains(&name.as_str()) && INSTALL_SUBCOMMANDS.contains(&second)) {
        return Some(tokens.join(" "));
    }
    let take = if tokens.len() > 2 && LAUNCHER_SUBCOMMANDS.contains(&second) {
        3
    } else if tokens.len() > 1 && !second.starts_with('-') {
        2
    } else {
        1
    };
    if take == 1 && INTERPRETERS.contains(&name.as_str()) {
        return None;
    }
    Some(tokens[..take].join(" "))
}

/// Auto mode never approves high or critical risk.
pub fn can_auto_approve(mode: &str, risk: RiskClass) -> bool {
    mode == "auto" && risk != RiskClass::High && risk != RiskClass::Critical
}

/// An exact, session-scoped allowlist entry for one simple command (no quoting/metacharacters).
pub fn bash_approval_exact(input: &Value) -> Option<String> {
    let trimmed = command_of(input)?.trim();
    if trimmed.is_empty() || shell_meta_re().is_match(trimmed) || trimmed.contains('"') || trimmed.contains('\'') {
        return None;
    }
    Some(trimmed.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub fn is_approved_bash_exact(input: &Value, exact: &BTreeSet<String>) -> bool {
    if exact.is_empty() {
        return false;
    }
    bash_approval_exact(input).map(|n| exact.contains(&n)).unwrap_or(false)
}

/// True when the command starts, token by token, with an approved prefix and its extra arguments
/// neither leave the workspace nor use a risky option.
pub fn is_approved_bash_prefix(input: &Value, prefixes: &BTreeSet<String>) -> bool {
    if prefixes.is_empty() {
        return false;
    }
    let Some(tokens) = command_tokens(input) else { return false };
    if tokens.is_empty() || tokens[0].contains('=') {
        return false;
    }
    let name = command_name(&tokens[0]);
    if ALWAYS_EXPLICIT.contains(&name.as_str()) {
        return false;
    }
    let second = tokens.get(1).map(String::as_str).unwrap_or("");
    for prefix in prefixes {
        let wanted: Vec<&str> = prefix.split(' ').collect();
        if wanted.len() > tokens.len() || !wanted.iter().enumerate().all(|(i, t)| tokens[i] == *t) {
            continue;
        }
        let extra = &tokens[wanted.len()..];
        let exact_only = EXACT_ONLY.contains(&name.as_str()) || (PACKAGE_MANAGERS.contains(&name.as_str()) && INSTALL_SUBCOMMANDS.contains(&second));
        if exact_only && !extra.is_empty() {
            continue;
        }
        if extra.iter().any(|t| leaves_workspace(t) || risky_option_re().is_match(t)) {
            continue;
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd(c: &str) -> Value {
        json!({ "command": c })
    }

    #[test]
    fn prefixes() {
        assert_eq!(bash_approval_prefix(&cmd("npm test")), Some("npm test".into()));
        assert_eq!(bash_approval_prefix(&cmd("npm run build --watch")), Some("npm run build".into()));
        assert_eq!(bash_approval_prefix(&cmd("rm -rf dist")), Some("rm -rf dist".into()));
        assert_eq!(bash_approval_prefix(&cmd("python")), None);
        assert_eq!(bash_approval_prefix(&cmd("sudo ls")), None);
        assert_eq!(bash_approval_prefix(&cmd("cat ../x")), None);
        assert_eq!(bash_approval_prefix(&cmd("npm test; curl x")), None);
        let set: BTreeSet<String> = ["npm test".to_string()].into_iter().collect();
        assert!(is_approved_bash_prefix(&cmd("npm test -- --watch"), &set));
        assert!(!is_approved_bash_prefix(&cmd("npm test && curl x"), &set));
        assert!(!is_approved_bash_prefix(&cmd("npm test --prefix=/tmp"), &set));
    }

    #[test]
    fn exact_and_auto() {
        assert_eq!(bash_approval_exact(&cmd("  ls   -la ")), Some("ls -la".into()));
        assert_eq!(bash_approval_exact(&cmd("echo 'x'")), None);
        assert!(can_auto_approve("auto", RiskClass::Medium));
        assert!(!can_auto_approve("auto", RiskClass::High));
        assert!(!can_auto_approve("ask", RiskClass::Low));
    }
}
