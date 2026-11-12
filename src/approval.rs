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
    "tar",
    "gtar",
    "bsdtar",
    "make",
    "gmake",
    "bmake",
    "nmake",
    "npx",
    "pnpx",
    "bunx",
    "uvx",
    "pipx",
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

fn is_multi_action_subcommand(cmd_name: &str, sub: &str) -> bool {
    matches!((cmd_name, sub), ("git", "remote" | "stash" | "worktree" | "submodule") | ("npm" | "pnpm" | "yarn", "pkg" | "config") | ("uv", "tool" | "pip") | ("pip" | "pip3", "config"))
}

fn is_exact_only_invocation(cmd_name: &str, second: &str, third: &str) -> bool {
    EXACT_ONLY.contains(&cmd_name)
        || (PACKAGE_MANAGERS.contains(&cmd_name) && INSTALL_SUBCOMMANDS.contains(&second))
        || (cmd_name == "uv" && matches!(second, "pip" | "tool") && (INSTALL_SUBCOMMANDS.contains(&third) || third == "run"))
}

fn risky_option_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^(?:--(?:output|out|outfile|out-file|outdir|out-dir|output-dir|write|exec|execdir|ok|okdir|delete|delete-branch|fprint|fprint0|fprintf|fls|exec-batch|ext-diff|upload-pack|receive-pack|exec-path|config-env|config|eval|eval-expression|command|shell|script-shell|pager|editor|global|system|prefix|cwd|dir|directory|target-dir|manifest-path|userconfig|globalconfig|registry|cache|index-url|extra-index-url|find-links|trusted-host|git-dir|work-tree|namespace|super-prefix|repo|hard|force|force-with-lease|init-file|rcfile|require|import|loader|experimental-loader|input-type|inspect|inspect-brk|inspect-port|inspect-publish-uid|prof|prof-process|cpu-prof|heap-prof|trace-event-file-pattern|diagnostic-dir|env-file|env-file-if-exists|pre|pre-glob|preprocessor|hostname-bin|in-place|file|makefile|checkpoint-action|to-command|use-compress-program|rmt-command|rsh-command|info-script|new-volume-script|absolute-names|rootdir|confcutdir|basetemp|override-ini|allow-all|allow-run|allow-write|allow-net|allow-env|allow-ffi|allow-sys)|-(?:exec|execdir|toolexec|ok|okdir|delete|fprint|fprint0|fprintf|fls|outputdir|coverprofile|cpuprofile|memprofile|blockprofile|mutexprofile|trace|modfile|overlay))(?:=|$)",
        )
        .unwrap()
    })
}

fn is_compiler_cmd(cmd_name: &str) -> bool {
    matches!(cmd_name, "gcc" | "cc" | "clang" | "g++" | "c++" | "clang++" | "rustc" | "ld")
}

fn short_cluster_has(flags: &str, needles: &[char]) -> bool {
    flags.chars().take_while(|c| c.is_ascii_alphabetic()).any(|c| needles.contains(&c))
}

fn has_risky_short_flag(cmd_name: &str, second: &str, token: &str) -> bool {
    if !token.starts_with('-') || token.starts_with("--") || token.len() <= 1 {
        return false;
    }
    let head = token.split('=').next().unwrap_or(token);
    let flags = &head[1..];

    // Case-sensitive `-C` (directory change on git/cargo/npm/pnpm/go, but context lines on rg/grep and columns on ls).
    if !matches!(cmd_name, "rg" | "ripgrep" | "grep" | "egrep" | "fgrep" | "ls") && !is_compiler_cmd(cmd_name) && token.starts_with("-C") {
        return true;
    }

    // Case-sensitive `-o` (output file on compilers/go/git/sort/pytest, but --only-matching on grep/rg and format on ls/ps).
    if !matches!(cmd_name, "grep" | "egrep" | "fgrep" | "rg" | "ripgrep" | "ls" | "ps")
        && (token == "-o" || token.starts_with("-o=") || (is_compiler_cmd(cmd_name) && token.starts_with("-o")) || (cmd_name == "sort" && short_cluster_has(flags, &['o'])))
    {
        return true;
    }

    if is_compiler_cmd(cmd_name) {
        return token.starts_with("-fplugin");
    }
    if cmd_name == "go" {
        return false;
    }
    if cmd_name == "gofmt" || cmd_name == "prettier" {
        return short_cluster_has(flags, &['w']);
    }
    if crate::risk::is_python_cmd(cmd_name) {
        return short_cluster_has(flags, &['c', 'm', 'i']);
    }
    if matches!(cmd_name, "node" | "nodejs" | "bun") {
        return matches!(head, "-e" | "-p" | "-r" | "-c") || token.starts_with("-e") || token.starts_with("-r");
    }
    if cmd_name == "deno" {
        return short_cluster_has(flags, &['A', 'e']);
    }
    if matches!(cmd_name, "ruby" | "perl" | "php" | "lua" | "luajit" | "osascript" | "rscript" | "tclsh") {
        return short_cluster_has(flags, &['e', 'r', 'i', 'I', 'M', 'c']);
    }
    if matches!(cmd_name, "sed" | "gsed") {
        return short_cluster_has(flags, &['i', 'e', 'f']);
    }
    if matches!(cmd_name, "awk" | "gawk" | "mawk" | "nawk") {
        return short_cluster_has(flags, &['f', 'e', 'i', 'l']);
    }
    if matches!(cmd_name, "pytest" | "py.test") {
        return matches!(head, "-c" | "-o" | "-p") || token.starts_with("-p");
    }
    if cmd_name == "git" {
        if head == "-c" || (token.starts_with("-c") && token.contains('=')) || head == "-f" || head == "-D" {
            return true;
        }
        if matches!(second, "branch" | "tag" | "clean") && short_cluster_has(flags, &['d', 'D', 'f', 'm', 'M', 'x', 'X']) {
            return true;
        }
        if matches!(second, "clone" | "fetch" | "pull" | "ls-remote") && token.starts_with("-u") {
            return true;
        }
    }
    if matches!(cmd_name, "npm" | "pnpm" | "yarn") && head == "-g" {
        return true;
    }
    false
}

fn has_risky_extra_arg(cmd_name: &str, second: &str, token: &str) -> bool {
    if risky_option_re().is_match(token) || has_risky_short_flag(cmd_name, second, token) {
        return true;
    }
    if is_compiler_cmd(cmd_name) && token.starts_with('@') {
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

fn has_non_ascii_whitespace(s: &str) -> bool {
    s.chars().any(|c| c.is_whitespace() && !c.is_ascii_whitespace())
}

fn command_tokens(input: &Value) -> Option<Vec<String>> {
    let raw = command_of(input)?;
    if has_non_ascii_whitespace(raw) {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() || shell_meta_re().is_match(trimmed) || trimmed.contains('"') || trimmed.contains('\'') {
        return None;
    }
    Some(trimmed.split_ascii_whitespace().map(str::to_string).collect())
}

fn value_leaves_workspace(value: &str) -> bool {
    static ABS: OnceLock<Regex> = OnceLock::new();
    static PARENT: OnceLock<Regex> = OnceLock::new();
    ABS.get_or_init(|| Regex::new(r"^(?:/|~|[A-Za-z]:[\\/]|\\\\)").unwrap()).is_match(value) || PARENT.get_or_init(|| Regex::new(r"(?:^|[\\/])\.\.(?:[\\/]|$)").unwrap()).is_match(value)
}

fn leaves_workspace(token: &str) -> bool {
    if value_leaves_workspace(token) {
        return true;
    }
    if let Some((_, rhs)) = token.split_once('=') {
        return value_leaves_workspace(rhs);
    }
    if token.starts_with('-') && !token.starts_with("--") && token.len() > 2 {
        let rest = &token[1..];
        for (idx, ch) in rest.char_indices() {
            if idx > 0 && value_leaves_workspace(&rest[idx..]) {
                return true;
            }
            if !ch.is_ascii_alphanumeric() {
                break;
            }
        }
    }
    false
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
    let third = tokens.get(2).map(String::as_str).unwrap_or("");
    if is_exact_only_invocation(&name, second, third) {
        return Some(tokens.join(" "));
    }
    let take = if tokens.len() > 2 && (LAUNCHER_SUBCOMMANDS.contains(&second) || is_multi_action_subcommand(&name, second)) {
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

pub fn can_auto_approve(mode: &str, risk: RiskClass) -> bool {
    match mode {
        "auto" => matches!(risk, RiskClass::Low | RiskClass::Medium),
        "yolo" => risk != RiskClass::Critical,
        _ => false,
    }
}

pub fn bash_approval_exact(input: &Value) -> Option<String> {
    let raw = command_of(input)?;
    if has_non_ascii_whitespace(raw) {
        return None;
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() || shell_meta_re().is_match(trimmed) || trimmed.contains('"') || trimmed.contains('\'') {
        return None;
    }
    Some(trimmed.split_ascii_whitespace().collect::<Vec<_>>().join(" "))
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
    let third = tokens.get(2).map(String::as_str).unwrap_or("");
    let exact_only = is_exact_only_invocation(&name, second, third);
    let raw_risk = crate::risk::classify_bash_risk(raw);
    for prefix in prefixes {
        let wanted: Vec<&str> = prefix.split(' ').collect();
        if wanted.is_empty() || wanted.len() > tokens.len() || !wanted.iter().enumerate().all(|(i, t)| tokens[i] == *t) {
            continue;
        }
        if wanted.len() == 1 && needs_subcommand_or_script(&name) {
            continue;
        }
        let extra = &tokens[wanted.len()..];
        if exact_only && !extra.is_empty() {
            continue;
        }
        if is_multi_action_subcommand(&name, second) && wanted.len() < 3 && !extra.is_empty() {
            continue;
        }
        if extra.iter().any(|t| leaves_workspace(t) || has_risky_extra_arg(&name, second, t)) {
            continue;
        }
        if !extra.is_empty() && (raw_risk >= RiskClass::High || raw_risk > crate::risk::classify_bash_risk(prefix)) {
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
        assert!(can_auto_approve("auto", RiskClass::Low));
        assert!(can_auto_approve("auto", RiskClass::Medium));
        assert!(!can_auto_approve("auto", RiskClass::High));
        assert!(!can_auto_approve("auto", RiskClass::Critical));
        assert!(can_auto_approve("yolo", RiskClass::High));
        assert!(!can_auto_approve("yolo", RiskClass::Critical));
        assert!(!can_auto_approve("ask", RiskClass::Low));
    }
}
