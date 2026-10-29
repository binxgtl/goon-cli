//! Risk classification for tools and shell commands.
//!
//! Invariants:
//! - `critical` is denied before any approval callback is consulted.
//! - `high` is never auto-approved, never remembered by the session tool allowlist and never added
//!   to the exact/prefix bash allowlists.
//! - Classification is heuristic. Approval is not an OS sandbox.

use crate::mcp::McpServerConfig;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RiskClass {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskClass {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskClass::Low => "low",
            RiskClass::Medium => "medium",
            RiskClass::High => "high",
            RiskClass::Critical => "critical",
        }
    }

    pub fn parse(text: &str) -> Option<RiskClass> {
        match text {
            "low" => Some(RiskClass::Low),
            "medium" => Some(RiskClass::Medium),
            "high" => Some(RiskClass::High),
            "critical" => Some(RiskClass::Critical),
            _ => None,
        }
    }
}

impl std::fmt::Display for RiskClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn risk_level(risk: RiskClass) -> u8 {
    risk as u8
}

fn max_risk(a: RiskClass, b: RiskClass) -> RiskClass {
    a.max(b)
}

const WRAPPERS: &[&str] = &["sudo", "doas", "env", "time", "nice", "nohup", "stdbuf", "timeout", "command", "builtin", "xargs", "setsid", "exec"];
const HIGH_FLOOR_WRAPPERS: &[&str] = &["sudo", "doas", "xargs"];

/// Programs that run another program given somewhere in their arguments. Their option grammars
/// differ too much to peel reliably, so every argument suffix is inspected.
const GENERIC_RUNNERS: &[&str] = &[
    "watch", "ionice", "strace", "ltrace", "chroot", "flock", "unbuffer", "chrt", "taskset", "numactl", "nsenter",
    "unshare", "firejail", "proot", "fakeroot", "fakechroot", "caffeinate", "systemd-run", "runuser", "su", "tsu",
    "script", "parallel", "entr", "cpulimit", "faketime", "torsocks", "proxychains", "proxychains4", "catchsegv",
    "valgrind", "gdb", "expect",
];
const MULTICALL_BINARIES: &[&str] = &["busybox", "toybox"];

fn inline_code_flags(cmd: &str) -> Option<&'static [&'static str]> {
    Some(match cmd {
        "perl" => &["-e", "-E"],
        "ruby" => &["-e"],
        "php" => &["-r"],
        "lua" | "luajit" => &["-e"],
        "osascript" => &["-e"],
        "rscript" => &["-e"],
        "tclsh" => &["-c"],
        "deno" => &["eval"],
        "bun" => &["-e", "--eval"],
        _ => return None,
    })
}

/// True for programs that run another command named anywhere in their arguments.
pub fn is_generic_runner(cmd: &str) -> bool {
    GENERIC_RUNNERS.contains(&cmd) || cmd == "trap"
}

const SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "pwsh", "powershell", "powershell.exe", "cmd", "cmd.exe"];
const NETWORK_COMMANDS: &[&str] = &["curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat", "netcat", "ftp", "telnet", "socat", "ssh-keygen", "tnftp", "lftp"];
const DESTRUCTIVE_COMMANDS: &[&str] = &["dd", "shred", "wipefs", "fdisk", "parted", "mkswap"];
const PROCESS_CONTROL_COMMANDS: &[&str] = &["kill", "pkill", "killall", "taskkill"];
const PRIVILEGED_COMMANDS: &[&str] = &["docker", "podman", "kubectl", "systemctl", "service", "launchctl", "mount", "umount"];
const FILE_WRITE_COMMANDS: &[&str] = &["cp", "mv", "ln", "chmod", "chown", "chgrp", "truncate", "install", "mkdir", "touch", "rmdir", "unlink", "tee", "patch", "unzip", "zip", "gzip", "gunzip", "tar", "cpio", "rsync"];
const SAFE_READ_COMMANDS: &[&str] = &[
    "cat", "ls", "grep", "rg", "head", "tail", "wc", "sort", "uniq", "cut", "tr", "column", "diff", "cmp", "comm", "pwd",
    "echo", "printf", "which", "type", "file", "stat", "du", "df", "tree", "date", "basename", "dirname", "realpath",
    "readlink", "test", "true", "false", "printenv", "id", "whoami", "uname", "hostname", "ps", "top", "free", "nproc",
    "sleep", "jq", "yq", "nl", "od", "xxd", "md5sum", "sha1sum", "sha256sum", "cd", "export", "set",
];
pub const SAFE_SCRIPT_NAMES: &[&str] = &["test", "tests", "build", "lint", "typecheck", "check", "test:unit", "test:ci", "ci"];
const PACKAGE_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun", "pip", "pip3", "uv", "poetry", "cargo", "go", "gem", "composer", "apt", "apt-get", "pkg", "brew", "dnf", "pacman", "apk", "yum", "npx", "pnpx", "bunx"];
const INSTALL_SUBCOMMANDS: &[&str] = &["install", "i", "add", "ci", "update", "upgrade", "up", "get", "remove", "rm", "uninstall", "un", "sync", "publish", "audit", "view", "info", "outdated", "fetch", "download", "exec", "x", "dlx"];
const SAFE_GIT_SUBCOMMANDS: &[&str] = &["status", "diff", "log", "show", "blame", "shortlog", "describe", "rev-parse", "ls-files", "ls-tree", "grep", "whatchanged", "reflog", "cat-file", "hash-object", "version", "help"];
const NETWORK_GIT_SUBCOMMANDS: &[&str] = &["push", "pull", "fetch", "clone", "ls-remote", "submodule", "request-pull", "archive"];
const WRITE_GIT_SUBCOMMANDS: &[&str] = &["add", "commit", "mv", "rm", "restore", "reset", "checkout", "switch", "stash", "merge", "rebase", "cherry-pick", "revert", "tag", "apply", "am", "clean", "config", "init", "gc", "prune", "notes", "worktree"];
const DANGEROUS_RM_TARGETS: &[&str] = &[
    "/", "/*", "~", "~/", "~/*", "$HOME", "${HOME}", "$HOME/", "${HOME}/", "$HOME/*", "${HOME}/*", "/home", "/home/",
    "/home/*", "/root", "/root/", "/root/*", "/Users", "/Users/", "/Users/*", "..", "../", "../..", "../../..", ".", "./",
    "*", "./*", "~/.", "$HOME/.", "${HOME}/.",
];

#[derive(Debug, Clone, PartialEq)]
pub struct ShellScan {
    pub segments: Vec<String>,
    /// True when the command contains code execution the parser cannot classify safely.
    pub critical_escape: bool,
    /// True when any unquoted redirection is present.
    pub redirection: bool,
}

fn is_word_boundary(ch: Option<char>) -> bool {
    matches!(ch, None | Some(' ') | Some('\t') | Some('\n') | Some('\r') | Some(';') | Some('&') | Some('|'))
}

/// Quote-aware scan: split compound commands into segments while ignoring separators inside
/// single/double quotes. `$(...)`, backticks and process substitution are critical unless they are
/// inside single quotes (literal text).
pub fn scan_shell(command: &str) -> ShellScan {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = vec![];
    let mut current = String::new();
    let mut single = false;
    let mut double = false;
    let mut critical_escape = false;
    let mut redirection = false;
    let push = |current: &mut String, segments: &mut Vec<String>| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
        current.clear();
    };
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if single {
            current.push(ch);
            if ch == '\'' {
                single = false;
            }
            i += 1;
            continue;
        }
        if ch == '\\' {
            let next = chars.get(i + 1).copied();
            // `\<newline>` is a line continuation: drop both so the newline neither splits the
            // segment nor hides inside a token (`rm \<newline>-rf /` classifies like `rm -rf /`).
            if next == Some('\n') {
                i += 2;
                continue;
            }
            if next == Some('\r') && chars.get(i + 2) == Some(&'\n') {
                i += 3;
                continue;
            }
            current.push(ch);
            if let Some(n) = next {
                current.push(n);
                i += 1;
            }
            i += 1;
            continue;
        }
        if double {
            current.push(ch);
            if ch == '"' {
                double = false;
            } else if ch == '`' {
                critical_escape = true;
            } else if ch == '$' && chars.get(i + 1) == Some(&'(') && chars.get(i + 2) != Some(&'(') {
                critical_escape = true;
            }
            i += 1;
            continue;
        }
        if ch == '\'' {
            single = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' {
            double = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '`' {
            critical_escape = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') {
            // Arithmetic expansion: parentheses are part of the expression, not separators.
            let mut depth = 0i32;
            current.push(ch);
            i += 1;
            while i < chars.len() {
                let inner = chars[i];
                current.push(inner);
                if inner == '(' {
                    depth += 1;
                } else if inner == ')' {
                    depth -= 1;
                    if depth == 0 && chars.get(i + 1) == Some(&')') {
                        i += 1;
                        current.push(chars[i]);
                        break;
                    }
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'{') {
            // Parameter expansion `${NAME}`: braces belong to the word, they never group commands.
            let mut j = i;
            while j < chars.len() {
                current.push(chars[j]);
                if chars[j] == '}' {
                    break;
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'(') {
            critical_escape = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if (ch == '<' || ch == '>') && chars.get(i + 1) == Some(&'(') {
            critical_escape = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '>' || ch == '<' {
            redirection = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '{' || ch == '}' {
            // A brace only groups commands when it stands alone as a word (`{ cmd; }`); inside a word
            // it is brace expansion (`file.{a,b}`) and stays part of the token.
            let prev = if i == 0 { None } else { Some(chars[i - 1]) };
            let next = chars.get(i + 1).copied();
            let standalone = is_word_boundary(prev) && (is_word_boundary(next) || ch == '}');
            if standalone {
                push(&mut current, &mut segments);
                i += 1;
                continue;
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == ';' || ch == '\n' || ch == '\r' || ch == '(' || ch == ')' {
            push(&mut current, &mut segments);
            i += 1;
            continue;
        }
        if ch == '&' || ch == '|' {
            push(&mut current, &mut segments);
            if chars.get(i + 1) == Some(&ch) {
                i += 1;
            }
            i += 1;
            continue;
        }
        current.push(ch);
        i += 1;
    }
    push(&mut current, &mut segments);
    ShellScan { segments, critical_escape, redirection }
}

fn hex_digit(ch: char) -> Option<u32> {
    ch.to_digit(16)
}

fn octal_digit(ch: char) -> Option<u32> {
    ch.to_digit(8)
}

/// Bash ANSI-C quoting (`$'...'`) decodes escapes, so a destructive flag can hide as
/// `$'\x2drf'`. Returns the decoded char and how many source characters it consumed.
fn decode_ansi_escape(segment: &[char], after_backslash: usize) -> (Option<char>, usize) {
    let Some(&lead) = segment.get(after_backslash) else {
        return (Some('\\'), 0);
    };
    if lead == '\'' {
        return (Some('\''), 1);
    }
    if lead == 'x' || lead == 'u' || lead == 'U' {
        let max_digits = match lead {
            'x' => 2,
            'u' => 4,
            _ => 8,
        };
        let mut value: u32 = 0;
        let mut digits = 0;
        let mut index = after_backslash + 1;
        while digits < max_digits && index < segment.len() {
            let Some(digit) = hex_digit(segment[index]) else { break };
            value = value.wrapping_mul(16).wrapping_add(digit);
            digits += 1;
            index += 1;
        }
        if digits > 0 && value <= 0x10ffff {
            return (char::from_u32(value), index - after_backslash);
        }
        return (Some(lead), 1);
    }
    if octal_digit(lead).is_some() {
        let mut value: u32 = 0;
        let mut digits = 0;
        let mut index = after_backslash;
        while digits < 3 && index < segment.len() {
            let Some(digit) = octal_digit(segment[index]) else { break };
            value = value * 8 + digit;
            digits += 1;
            index += 1;
        }
        return (char::from_u32(value), index - after_backslash);
    }
    let mapped = match lead {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'a' => '\u{7}',
        'b' => '\u{8}',
        'e' | 'E' => '\u{1b}',
        'f' => '\u{c}',
        'v' => '\u{b}',
        other => other,
    };
    (Some(mapped), 1)
}

/// Quote-aware word split. Quotes are removed; backslash escapes are kept as literal chars.
pub fn tokenize(segment: &str) -> Vec<String> {
    let chars: Vec<char> = segment.chars().collect();
    let mut tokens = vec![];
    let mut current = String::new();
    let mut has_token = false;
    let mut single = false;
    let mut double = false;
    let mut ansi_c = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ansi_c {
            if ch == '\'' {
                ansi_c = false;
                i += 1;
                continue;
            }
            if ch == '\\' {
                let (decoded, consumed) = decode_ansi_escape(&chars, i + 1);
                if let Some(c) = decoded {
                    current.push(c);
                }
                i += 1 + consumed;
                continue;
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if single {
            if ch == '\'' {
                single = false;
            } else {
                current.push(ch);
            }
            i += 1;
            continue;
        }
        if double {
            if ch == '"' {
                double = false;
            } else if ch == '\\' && i + 1 < chars.len() {
                i += 1;
                current.push(chars[i]);
            } else {
                current.push(ch);
            }
            i += 1;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            ansi_c = true;
            has_token = true;
            i += 2;
            continue;
        }
        if ch == '\'' {
            single = true;
            has_token = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            double = true;
            has_token = true;
            i += 1;
            continue;
        }
        if ch == '\\' && i + 1 < chars.len() {
            i += 1;
            current.push(chars[i]);
            has_token = true;
            i += 1;
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            has_token = false;
            i += 1;
            continue;
        }
        current.push(ch);
        has_token = true;
        i += 1;
    }
    let _ = has_token;
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Executable name without directory or Windows executable suffix (`curl.exe` → `curl`).
pub fn base_name(token: &str) -> String {
    let normalized = token.replace('\\', "/");
    let base = normalized.rsplit('/').next().unwrap_or(&normalized).to_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".com"] {
        if let Some(stripped) = base.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    base
}

fn is_env_assignment(token: &str) -> bool {
    let mut chars = token.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    for c in chars {
        if c == '=' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    false
}

fn strip_env_assignments(tokens: &[String]) -> Vec<String> {
    let index = tokens.iter().take_while(|t| is_env_assignment(t)).count();
    tokens[index..].to_vec()
}

fn skip_flags(tokens: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--" {
            return tokens[index + 1..].to_vec();
        }
        if !token.starts_with('-') || token == "-" {
            return tokens[index..].to_vec();
        }
        index += 1;
        if value_flags.contains(&token.as_str()) && index < tokens.len() {
            index += 1;
        }
    }
    vec![]
}

/// Peels environment assignments and command wrappers, returning the inner argv and a risk floor.
pub fn peel_wrappers(input: &[String]) -> (Vec<String>, RiskClass) {
    let mut tokens = input.to_vec();
    let mut floor = RiskClass::Low;
    for _ in 0..20 {
        tokens = strip_env_assignments(&tokens);
        if tokens.is_empty() {
            return (tokens, max_risk(floor, RiskClass::Medium));
        }
        let wrapper = base_name(&tokens[0]);
        if !WRAPPERS.contains(&wrapper.as_str()) {
            return (tokens, floor);
        }
        if HIGH_FLOOR_WRAPPERS.contains(&wrapper.as_str()) {
            floor = max_risk(floor, RiskClass::High);
        }
        let rest = tokens[1..].to_vec();
        match wrapper.as_str() {
            "env" => {
                // `env -S 'cmd args'` re-splits a string; treat the string as a new command.
                if let Some(split_index) = rest.iter().position(|t| t == "-S" || t == "--split-string" || t.starts_with("--split-string=")) {
                    let raw = if rest[split_index].contains('=') {
                        rest[split_index][rest[split_index].find('=').unwrap() + 1..].to_string()
                    } else {
                        rest.get(split_index + 1).cloned().unwrap_or_default()
                    };
                    if raw.is_empty() {
                        return (vec![], max_risk(floor, RiskClass::High));
                    }
                    tokens = tokenize(&raw);
                    floor = max_risk(floor, RiskClass::High);
                    continue;
                }
                tokens = skip_flags(&rest, &["-u", "--unset", "-C", "--chdir"]);
            }
            "sudo" | "doas" => tokens = skip_flags(&rest, &["-u", "-g", "-p", "-c", "-h", "-r", "-t", "-U", "--user", "--group"]),
            "timeout" => {
                let after = skip_flags(&rest, &["-s", "--signal", "-k", "--kill-after"]);
                tokens = after.into_iter().skip(1).collect();
            }
            "nice" => tokens = skip_flags(&rest, &["-n", "--adjustment"]),
            "stdbuf" => tokens = skip_flags(&rest, &[]),
            "xargs" => tokens = skip_flags(&rest, &["-n", "-I", "-d", "-E", "-L", "-P", "-s", "-a"]),
            "time" => tokens = skip_flags(&rest, &["-f", "--format", "-o", "--output"]),
            "command" | "builtin" => tokens = rest.into_iter().enumerate().filter(|(i, t)| *i > 0 || !t.starts_with('-')).map(|(_, t)| t).collect(),
            _ => tokens = skip_flags(&rest, &[]),
        }
    }
    (vec![], max_risk(floor, RiskClass::High))
}

fn has_flag(tokens: &[String], short: char, long: &str) -> bool {
    tokens.iter().any(|token| {
        if token == long {
            return true;
        }
        if !token.starts_with('-') || token.starts_with("--") {
            return false;
        }
        token[1..].contains(short)
    })
}

fn is_dangerous_path(target: &str) -> bool {
    let clean = target.trim().trim_matches(|c| c == '"' || c == '\'');
    if DANGEROUS_RM_TARGETS.contains(&clean) {
        return true;
    }
    // `//`, `/./`, `//*` and friends still point at the filesystem root; trailing slashes on a
    // home/parent reference (`$HOME//`, `../`) do not change the target.
    let mut normalized = clean.to_string();
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    let normalized = regex_replace(&normalized, r"/\.(/|$)", "$1");
    if normalized == "/" || normalized == "/*" || normalized.is_empty() && clean.starts_with('/') {
        return true;
    }
    let trimmed = normalized.trim_end_matches('/');
    !trimmed.is_empty() && DANGEROUS_RM_TARGETS.contains(&trimmed)
}

fn regex_replace(text: &str, pattern: &str, replacement: &str) -> String {
    let mut current = text.to_string();
    let re = regex::Regex::new(pattern).unwrap();
    loop {
        let next = re.replace_all(&current, replacement).to_string();
        if next == current {
            return next;
        }
        current = next;
    }
}

fn classify_rm(rest: &[String]) -> RiskClass {
    let flags: Vec<&String> = rest.iter().filter(|t| t.starts_with('-')).collect();
    let recursive = flags.iter().any(|t| *t == "--recursive" || (!t.starts_with("--") && (t.contains('r') || t.contains('R'))));
    let force = flags.iter().any(|t| *t == "--force" || (!t.starts_with("--") && t[1..].contains('f')));
    let no_preserve_root = flags.iter().any(|t| *t == "--no-preserve-root");
    let targets: Vec<&String> = rest.iter().filter(|t| !t.starts_with('-')).collect();
    if no_preserve_root || (targets.iter().any(|t| is_dangerous_path(t)) && (recursive || force)) {
        return RiskClass::Critical;
    }
    if targets.iter().any(|t| *t == ".." || t.starts_with("../")) && (recursive || force) {
        return RiskClass::Critical;
    }
    if recursive || force {
        return RiskClass::High;
    }
    RiskClass::Medium
}

fn classify_git(rest: &[String]) -> RiskClass {
    let mut index = 0;
    let mut floor = RiskClass::Low;
    while index < rest.len() {
        let token = &rest[index];
        if token == "-C" || token == "--git-dir" || token == "--work-tree" || token == "-c" {
            floor = max_risk(floor, if token == "-c" { RiskClass::High } else { RiskClass::Low });
            index += 2;
            continue;
        }
        if token == "--exec-path" || token == "--upload-pack" || token == "--receive-pack" {
            return RiskClass::High;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    let sub = rest.get(index).map(|s| s.to_lowercase()).unwrap_or_default();
    let sub_args: Vec<String> = rest.iter().skip(index + 1).cloned().collect();
    if sub.is_empty() {
        return max_risk(floor, RiskClass::High);
    }
    if NETWORK_GIT_SUBCOMMANDS.contains(&sub.as_str()) {
        return RiskClass::High;
    }
    if sub == "reset" && has_flag(&sub_args, 'h', "--hard") {
        return RiskClass::High;
    }
    if sub == "clean" && sub_args.iter().any(|t| t.starts_with('-') && t.chars().any(|c| matches!(c, 'f' | 'd' | 'x'))) {
        return RiskClass::High;
    }
    if sub == "config" && sub_args.iter().any(|t| {
        let lower = t.to_lowercase();
        // Hooks, pagers, editors and ssh/proxy commands make git run arbitrary programs later.
        ["core.hookspath", "core.pager", "core.editor", "core.sshcommand", "core.fsmonitor", "alias.", "http.proxy", "credential.helper", "diff.external", "sequence.editor"]
            .iter()
            .any(|key| lower.starts_with(key))
    }) {
        return RiskClass::High;
    }
    if WRITE_GIT_SUBCOMMANDS.contains(&sub.as_str()) {
        return max_risk(floor, RiskClass::Medium);
    }
    if SAFE_GIT_SUBCOMMANDS.contains(&sub.as_str()) {
        return max_risk(floor, RiskClass::Low);
    }
    max_risk(floor, RiskClass::Medium)
}

fn classify_package_manager(cmd: &str, rest: &[String]) -> RiskClass {
    if cmd == "npx" || cmd == "pnpx" || cmd == "bunx" {
        return RiskClass::High;
    }
    let sub = rest.first().map(|s| s.to_lowercase()).unwrap_or_default();
    if sub.is_empty() {
        return RiskClass::Medium;
    }
    if INSTALL_SUBCOMMANDS.contains(&sub.as_str()) {
        return RiskClass::High;
    }
    if ["ls", "list", "help", "version", "why", "root", "config"].contains(&sub.as_str()) {
        return RiskClass::Low;
    }
    if sub == "run" || sub == "run-script" || sub == "workspace" {
        let script = rest.get(1).map(|s| s.to_lowercase()).unwrap_or_default();
        if script.is_empty() {
            return RiskClass::Medium;
        }
        return if SAFE_SCRIPT_NAMES.contains(&script.as_str()) { RiskClass::Low } else { RiskClass::Medium };
    }
    if ["test", "t", "exec"].contains(&sub.as_str()) {
        return if sub == "exec" { RiskClass::High } else { RiskClass::Low };
    }
    if ["pip", "pip3", "uv", "poetry", "gem", "composer"].contains(&cmd) && ["list", "show", "freeze", "check", "--version", "version"].contains(&sub.as_str()) {
        return RiskClass::Low;
    }
    RiskClass::Medium
}

fn classify_awk(rest: &[String]) -> RiskClass {
    let program = rest.join(" ");
    if regex::Regex::new(r"system\s*\(").unwrap().is_match(&program) {
        return RiskClass::Critical;
    }
    if regex::Regex::new(r#"\|\s*(?:getline|["'])"#).unwrap().is_match(&program) || regex::Regex::new(r"print\s*\|").unwrap().is_match(&program) {
        return RiskClass::High;
    }
    RiskClass::Low
}

fn classify_sed(rest: &[String]) -> RiskClass {
    let in_place = has_flag(rest, 'i', "--in-place");
    let scripts: Vec<&String> = rest.iter().filter(|t| !t.starts_with('-')).collect();
    // GNU sed `e` runs a shell command: bare, after an address (`1e id`, `$e`, `/x/e`) or as an
    // `s///e` flag. `w`/`W` write arbitrary files.
    let exec_bare = regex::Regex::new(r"(?:^|[;\s\d$/!},])e(?:[\s;]|$)").unwrap();
    let exec_flag = regex::Regex::new(r"/[gpIiMm0-9w]*e[gpIiMm0-9]*(?:[;\s]|$)").unwrap();
    if scripts.iter().any(|s| exec_bare.is_match(s) || exec_flag.is_match(s)) {
        return RiskClass::High;
    }
    let write_bare = regex::Regex::new(r"(?:^|[;\s\d$/!},])[wW]\s+\S").unwrap();
    let write_flag = regex::Regex::new(r"/[gpIiMm0-9e]*w\s+\S").unwrap();
    let writes = scripts.iter().any(|s| write_bare.is_match(s) || write_flag.is_match(s));
    if in_place || writes {
        RiskClass::Medium
    } else {
        RiskClass::Low
    }
}

fn classify_generic_runner(rest: &[String], depth: usize) -> RiskClass {
    let mut risk = RiskClass::High;
    if depth > 4 {
        return risk;
    }
    for i in 0..rest.len() {
        let token = &rest[i];
        // A quoted argument such as `watch "rm -rf /"` or `su -c 'rm -rf /'` is a command line.
        if token.chars().any(char::is_whitespace) {
            risk = max_risk(risk, classify_bash_risk_at(token, depth + 1));
        }
        if token.starts_with('-') {
            continue;
        }
        let (tokens, floor) = peel_wrappers(&rest[i..]);
        if tokens.is_empty() {
            continue;
        }
        risk = max_risk(risk, max_risk(floor, classify_base(&base_name(&tokens[0]), &tokens[1..], depth + 1)));
        if risk == RiskClass::Critical {
            return risk;
        }
    }
    risk
}

fn classify_base(cmd: &str, rest: &[String], depth: usize) -> RiskClass {
    if ["eval", "source", "."].contains(&cmd) {
        return RiskClass::Critical;
    }
    if MULTICALL_BINARIES.contains(&cmd) {
        if rest.is_empty() || depth > 4 {
            return RiskClass::Medium;
        }
        let (tokens, floor) = peel_wrappers(rest);
        if tokens.is_empty() {
            return max_risk(RiskClass::Medium, floor);
        }
        return max_risk(floor, classify_base(&base_name(&tokens[0]), &tokens[1..], depth + 1));
    }
    if GENERIC_RUNNERS.contains(&cmd) {
        return classify_generic_runner(rest, depth);
    }
    if cmd == "trap" {
        // `trap 'cmd' SIGNAL` runs the handler later in this shell.
        return match rest.iter().find(|t| !t.starts_with('-')) {
            Some(handler) if depth <= 4 => max_risk(RiskClass::Medium, classify_bash_risk_at(handler, depth + 1)),
            _ => RiskClass::Medium,
        };
    }
    if let Some(flags) = inline_code_flags(cmd) {
        if rest.iter().any(|t| flags.contains(&t.as_str())) {
            return RiskClass::High;
        }
    }
    if SHELL_INTERPRETERS.contains(&cmd) {
        return if rest.iter().any(|t| t == "-c" || t == "/c" || t == "-Command" || t == "-command") { RiskClass::Critical } else { RiskClass::High };
    }
    if cmd == "rm" {
        return classify_rm(rest);
    }
    if DESTRUCTIVE_COMMANDS.contains(&cmd) || cmd.starts_with("mkfs") {
        return RiskClass::Critical;
    }
    if NETWORK_COMMANDS.contains(&cmd) || PROCESS_CONTROL_COMMANDS.contains(&cmd) || PRIVILEGED_COMMANDS.contains(&cmd) {
        return RiskClass::High;
    }
    if PACKAGE_MANAGERS.contains(&cmd) {
        return classify_package_manager(cmd, rest);
    }
    if cmd == "git" {
        return classify_git(rest);
    }
    if cmd == "awk" || cmd == "gawk" || cmd == "mawk" {
        return classify_awk(rest);
    }
    if cmd == "sed" {
        return classify_sed(rest);
    }
    if cmd == "find" {
        let dangerous = rest.iter().any(|t| ["-exec", "-execdir", "-ok", "-okdir", "-delete"].contains(&t.as_str()));
        return if dangerous { RiskClass::High } else { RiskClass::Low };
    }
    if cmd == "node" || cmd == "nodejs" {
        if rest.iter().any(|t| t == "--test" || t.starts_with("--test=")) {
            return RiskClass::Low;
        }
        if rest.iter().any(|t| ["-e", "--eval", "-p", "--print"].contains(&t.as_str())) {
            return RiskClass::High;
        }
        return RiskClass::Medium;
    }
    if ["python", "python3", "pypy", "pypy3"].contains(&cmd) {
        if rest.iter().any(|t| t == "-c") {
            return RiskClass::High;
        }
        if let Some(pos) = rest.iter().position(|t| t == "-m") {
            let module = rest.get(pos + 1).map(|s| s.to_lowercase()).unwrap_or_default();
            if ["pytest", "unittest", "mypy", "ruff"].contains(&module.as_str()) {
                return RiskClass::Low;
            }
            if module == "pip" {
                return RiskClass::High;
            }
            return RiskClass::Medium;
        }
        return RiskClass::Medium;
    }
    if ["pytest", "py.test", "mypy", "ruff", "oxlint", "eslint", "tsc"].contains(&cmd) {
        return RiskClass::Low;
    }
    if cmd == "make" {
        let target = rest.iter().find(|t| !t.starts_with('-')).map(|t| t.to_lowercase());
        return match target {
            Some(t) if SAFE_SCRIPT_NAMES.contains(&t.as_str()) => RiskClass::Low,
            _ => RiskClass::Medium,
        };
    }
    if FILE_WRITE_COMMANDS.contains(&cmd) {
        return RiskClass::Medium;
    }
    if SAFE_READ_COMMANDS.contains(&cmd) {
        return RiskClass::Low;
    }
    RiskClass::Medium
}

/// Classify a shell command line. Compound commands take the maximum risk of their segments;
/// redirection raises the floor to medium; unquoted substitutions are critical.
pub fn classify_bash_risk(command: &str) -> RiskClass {
    classify_bash_risk_at(command, 0)
}

fn classify_bash_risk_at(command: &str, depth: usize) -> RiskClass {
    if command.trim().is_empty() {
        return RiskClass::Medium;
    }
    let scan = scan_shell(command);
    if scan.critical_escape {
        return RiskClass::Critical;
    }
    let mut risk = if scan.redirection { RiskClass::Medium } else { RiskClass::Low };
    for segment in &scan.segments {
        let tokens = tokenize(segment);
        if tokens.is_empty() {
            continue;
        }
        let (inner, floor) = peel_wrappers(&tokens);
        if inner.is_empty() {
            risk = max_risk(risk, floor);
            continue;
        }
        let cmd = base_name(&inner[0]);
        risk = max_risk(risk, max_risk(floor, classify_base(&cmd, &inner[1..], depth)));
    }
    risk
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpRiskAssessment {
    pub risk: RiskClass,
    pub net: bool,
    pub reason: String,
}

/// Conservative MCP tool risk. Server annotations are untrusted and may only escalate; `trust`
/// never makes a tool read-only. The user's `readOnlyTools` allowlist is the only thing that
/// lowers a tool to low. `openWorldHint` (or server `net`) marks network access.
pub fn classify_mcp_risk(server: &str, tool: &str, remote: Option<&Value>, config: &McpServerConfig) -> McpRiskAssessment {
    let annotations = remote.and_then(Value::as_object);
    let flag = |key: &str| annotations.and_then(|a| a.get(key)).and_then(Value::as_bool) == Some(true);
    let mut reasons: Vec<String> = vec![];
    let mut risk = RiskClass::Medium;
    if flag("readOnlyHint") {
        reasons.push("readOnlyHint (medium floor)".into());
    }
    if flag("destructiveHint") {
        risk = max_risk(risk, RiskClass::High);
        reasons.push("destructiveHint".into());
    }
    if flag("openWorldHint") {
        risk = max_risk(risk, RiskClass::High);
        reasons.push("openWorldHint".into());
    }
    if config.read_only_tools.iter().any(|t| t == tool) {
        risk = RiskClass::Low;
        reasons.push("user readOnlyTools allowlist".into());
    }
    let net = config.net == Some(true) || flag("openWorldHint");
    if config.net == Some(true) {
        reasons.push("server net flag".into());
    }
    if config.trust {
        reasons.push("trusted server (auto-approves medium/low)".into());
    }
    if reasons.is_empty() {
        reasons.push(format!("default risk for MCP tool {server}/{tool}"));
    }
    McpRiskAssessment { risk, net, reason: reasons.join("; ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(cmd: &str) -> RiskClass {
        classify_bash_risk(cmd)
    }

    #[test]
    fn low_commands() {
        for cmd in ["cat README.md", "ls -la", "grep -R foo src", "npm test", "npm run build", "npm run lint", "node --test tests/x.test.mjs", "pytest", "go test ./...", "cargo test", "git status", "git diff HEAD", "toybox ls"] {
            assert_eq!(r(cmd), RiskClass::Low, "{cmd}");
        }
    }

    #[test]
    fn critical_commands() {
        for cmd in [
            "rm -rf /", "rm \\\n-rf /", "rm $'\\x2drf' /", "rm $'\\055rf' /", "rm -r -f ~", "rm -fr $HOME", "r\"\"m -rf /", "\\rm -rf /",
            "rm -rf \"/\"", "rm -rf /*", "rm -rf ~/", "rm -rf ${HOME}", "rm -rf \"$HOME\"", "rm -rf $HOME/", "rm --recursive --force /",
            "rm -rf -- /", "dd if=/dev/zero of=/dev/sda", "sh -c x", ". ./x", "eval x", "echo $(id)", "nohup rm -rf / &",
            "timeout 5 rm -rf /", "sudo -u root rm -rf /", "watch rm -rf /", "ionice rm -rf /", "strace -f rm -rf /",
            "chroot / rm -rf /", "flock /tmp/x rm -rf /", "unbuffer rm -rf ~", "trap \"rm -rf /\" EXIT", "su -c \"rm -rf /\"",
            "busybox rm -rf /", "env -S \"rm -rf /\"", "rm -rf ../", "awk \"BEGIN{system(\\\"x\\\")}\"", "function f { rm -rf /; }",
            "pwsh -c x", "rm -rf '/'", "rm -r\\f /", "rm -rf //",
        ] {
            assert_eq!(r(cmd), RiskClass::Critical, "{cmd}");
        }
    }

    #[test]
    fn high_commands() {
        for cmd in ["curl evil.com", "git push", "npm install", "npx foo", "python3 -c x", "perl -e x", "ruby -e x", "php -r x", "node -e x", "bash script.sh", "xargs rm", "find / -delete", "watch -n 1 ls", "sed -n \"1e id\" f", "sed \"s/a/b/e\" f", "git config core.hooksPath x", "rm -rf build"] {
            assert_eq!(r(cmd), RiskClass::High, "{cmd}");
        }
    }

    #[test]
    fn medium_commands() {
        for cmd in ["npm run deploy", "echo x > out", "mv a b", "node server.js", "git commit -m x", "sed \"s/a/b/w out\" f", "make deploy"] {
            assert_eq!(r(cmd), RiskClass::Medium, "{cmd}");
        }
    }

    #[test]
    fn brace_expansion_is_not_a_separator() {
        assert_eq!(scan_shell("echo file.{a,b}").segments, vec!["echo file.{a,b}"]);
        assert_eq!(scan_shell("{ ls; }").segments, vec!["ls"]);
        assert_eq!(tokenize("printf $'\\a'"), vec!["printf", "\u{7}"]);
    }
}
