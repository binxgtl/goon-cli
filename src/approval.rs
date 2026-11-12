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

const LAUNCHER_SUBCOMMANDS: &[&str] = &["run", "run-script", "workspace", "exec", "x", "dlx", "-m"];
/// Commands that run other commands or rewrite their environment: never approvable by prefix.
const ALWAYS_EXPLICIT: &[&str] = &[
    "env",
    "sudo",
    "doas",
    "su",
    "xargs",
    "eval",
    "exec",
    "command",
    "builtin",
    "source",
    ".",
    "nohup",
    "time",
    "nice",
    "timeout",
    "watch",
    "ssh",
    "find",
    "fd",
    "fdfind",
    "bash",
    "sh",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "mksh",
    "tcsh",
    "csh",
    "ash",
    "pwsh",
    "powershell",
    "cmd",
    "flock",
    "strace",
    "ltrace",
    "chroot",
    "ionice",
    "unbuffer",
    "chrt",
    "taskset",
    "numactl",
    "nsenter",
    "unshare",
    "firejail",
    "fakeroot",
    "fakechroot",
    "caffeinate",
    "systemd-run",
    "runuser",
    "setsid",
    "stdbuf",
    "busybox",
    "toybox",
    "proot",
    "tsu",
    "script",
    "parallel",
    "entr",
    "cpulimit",
    "faketime",
    "torsocks",
    "proxychains",
    "proxychains4",
    "catchsegv",
    "valgrind",
    "gdb",
    "expect",
    "trap",
];
const INTERPRETERS: &[&str] = &[
    "python",
    "python2",
    "python3",
    "py",
    "pypy",
    "pypy3",
    "node",
    "nodejs",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "lua",
    "luajit",
    "osascript",
    "rscript",
    "tclsh",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
    "gsed",
    "git",
    "tar",
    "gtar",
    "bsdtar",
];
const EXACT_ONLY: &[&str] = &[
    "rm",
    "rmdir",
    "mv",
    "cp",
    "ln",
    "chmod",
    "chown",
    "chgrp",
    "dd",
    "truncate",
    "shred",
    "wipefs",
    "fdisk",
    "parted",
    "mkswap",
    "mkfs",
    "tee",
    "install",
    "patch",
    "unzip",
    "zip",
    "cpio",
    "make",
    "gmake",
    "bmake",
    "nmake",
    "npx",
    "pnpx",
    "bunx",
    "curl",
    "wget",
    "scp",
    "sftp",
    "rsync",
    "nc",
    "ncat",
    "netcat",
    "ftp",
    "tnftp",
    "lftp",
    "telnet",
    "socat",
    "kill",
    "pkill",
    "killall",
    "taskkill",
    "docker",
    "podman",
    "nerdctl",
    "kubectl",
    "helm",
    "systemctl",
    "service",
    "launchctl",
    "mount",
    "umount",
];
const PACKAGE_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun", "pip", "pip3", "uv", "poetry", "cargo", "go", "gem", "composer", "apt", "apt-get", "pkg", "brew", "dnf", "pacman", "apk", "yum"];
const INSTALL_SUBCOMMANDS: &[&str] = &[
    "install",
    "i",
    "add",
    "ci",
    "update",
    "upgrade",
    "up",
    "get",
    "remove",
    "rm",
    "uninstall",
    "un",
    "sync",
    "publish",
    "audit",
    "view",
    "info",
    "outdated",
    "fetch",
    "download",
    "exec",
    "x",
    "dlx",
];

fn risky_option_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^--?(?:o|output|out|outfile|out-file|outdir|out-dir|output-dir|exec|execdir|ok|okdir|delete|fprint|fprint0|fprintf|fls|exec-batch|ext-diff|upload-pack|receive-pack|exec-path|config-env|config|c|eval|eval-expression|e|print|p|command|shell|pager|editor|global|prefix|cwd|dir|target-dir|manifest-path|userconfig|globalconfig|registry|cache|script-shell|index-url|extra-index-url|find-links|trusted-host|git-dir|work-tree|namespace|super-prefix|repo|hard|force|init-file|rcfile|require|r|import|loader|experimental-loader|input-type|inspect|inspect-brk|prof|prof-process|cpu-prof|heap-prof|trace-event-file-pattern|diagnostic-dir|env-file|env-file-if-exists|pre|pre-glob|preprocessor|in-place|i|file|makefile|f|directory|checkpoint-action|to-command|use-compress-program|rmt-command|rsh-command|info-script|new-volume-script|absolute-names|extract|get|x|create|list|t|diff|compare|d|append|update|u|catenate|concatenate)(?:=|$)",
        )
        .unwrap()
    })
}

fn has_risky_short_flag(token: &str) -> bool {
    if !token.starts_with('-') || token.starts_with("--") || token.len() <= 1 {
        return false;
    }
    let head = token.split('=').next().unwrap_or(token);
    head[1..].chars().any(|c| matches!(c, 'o' | 'O' | 'c' | 'C' | 'e' | 'E' | 'f' | 'F' | 'i' | 'I' | 'p' | 'P' | 'r' | 'R' | 'S' | 'u' | 'x' | 'X'))
}

fn has_risky_extra_arg(cmd_name: &str, token: &str) -> bool {
    if risky_option_re().is_match(token) || has_risky_short_flag(token) {
        return true;
    }
    if matches!(cmd_name, "awk" | "gawk" | "mawk" | "nawk" | "make" | "gmake" | "bmake" | "nmake") && !token.starts_with('-') && token.contains('=') {
        return true;
    }
    false
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

fn leaves_workspace(token: &str) -> bool {
    let value = match token.find('=') {
        Some(i) => &token[i + 1..],
        None => token,
    };
    static ABS: OnceLock<Regex> = OnceLock::new();
    static PARENT: OnceLock<Regex> = OnceLock::new();
    ABS.get_or_init(|| Regex::new(r"^(?:/|~|[A-Za-z]:[\\/]|\\\\)").unwrap()).is_match(value) || PARENT.get_or_init(|| Regex::new(r"(?:^|[\\/])\.\.(?:[\\/]|$)").unwrap()).is_match(value)
}

fn command_name(token: &str) -> String {
    crate::risk::base_name(token)
}

fn needs_subcommand_or_script(name: &str) -> bool {
    INTERPRETERS.contains(&name) || PACKAGE_MANAGERS.contains(&name) || crate::risk::is_python_cmd(name)
}

pub fn bash_approval_prefix(input: &Value) -> Option<String> {
    let tokens = command_tokens(input)?;
    if tokens.is_empty() || tokens[0].contains('=') {
        return None;
    }
    let name = command_name(&tokens[0]);
    if ALWAYS_EXPLICIT.contains(&name.as_str()) || name.starts_with("mkfs") {
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
        if tokens[2].starts_with('-') {
            return None;
        }
        3
    } else if tokens.len() > 1 && !second.starts_with('-') {
        2
    } else {
        1
    };
    if take == 1 && needs_subcommand_or_script(&name) {
        return None;
    }
    Some(tokens[..take].join(" "))
}

pub fn can_auto_approve(mode: &str, _risk: RiskClass) -> bool {
    mode == "auto" || mode == "yolo"
}

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

pub fn is_approved_bash_prefix(input: &Value, prefixes: &BTreeSet<String>) -> bool {
    if prefixes.is_empty() {
        return false;
    }
    let Some(raw) = command_of(input) else { return false };
    let Some(tokens) = command_tokens(input) else { return false };
    if tokens.is_empty() || tokens[0].contains('=') {
        return false;
    }
    let name = command_name(&tokens[0]);
    if ALWAYS_EXPLICIT.contains(&name.as_str()) || name.starts_with("mkfs") {
        return false;
    }
    let second = tokens.get(1).map(String::as_str).unwrap_or("");
    for prefix in prefixes {
        let wanted: Vec<&str> = prefix.split(' ').collect();
        if wanted.is_empty() || wanted.len() > tokens.len() || !wanted.iter().enumerate().all(|(i, t)| tokens[i] == *t) {
            continue;
        }
        if wanted.len() == 1 && needs_subcommand_or_script(&name) {
            continue;
        }
        let extra = &tokens[wanted.len()..];
        let exact_only = EXACT_ONLY.contains(&name.as_str()) || (PACKAGE_MANAGERS.contains(&name.as_str()) && INSTALL_SUBCOMMANDS.contains(&second));
        if exact_only && !extra.is_empty() {
            continue;
        }
        if extra.iter().any(|t| leaves_workspace(t) || has_risky_extra_arg(&name, t)) {
            continue;
        }
        if !extra.is_empty() && crate::risk::classify_bash_risk(raw) >= RiskClass::High {
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
        assert!(can_auto_approve("auto", RiskClass::High));
        assert!(can_auto_approve("auto", RiskClass::Critical));
        assert!(can_auto_approve("yolo", RiskClass::Critical));
        assert!(!can_auto_approve("ask", RiskClass::Low));
    }
}
