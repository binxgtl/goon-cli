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
    "watch",
    "ionice",
    "strace",
    "ltrace",
    "chroot",
    "flock",
    "unbuffer",
    "chrt",
    "taskset",
    "numactl",
    "nsenter",
    "unshare",
    "firejail",
    "proot",
    "fakeroot",
    "fakechroot",
    "caffeinate",
    "systemd-run",
    "runuser",
    "su",
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
];
const MULTICALL_BINARIES: &[&str] = &["busybox", "toybox"];

pub(crate) fn has_inline_code_flag(cmd: &str, rest: &[String]) -> bool {
    let args: Vec<&String> = rest.iter().take_while(|t| *t != "--").collect();
    let has_short =
        |chars: &[char]| args.iter().any(|t| t.starts_with('-') && !t.starts_with("--") && t.len() > 1 && t[1..].chars().take_while(|c| c.is_ascii_alphabetic()).any(|c| chars.contains(&c)));
    match cmd {
        "perl" => has_short(&['e', 'E', 'M', 'm']),
        "ruby" => has_short(&['e', 'r']),
        "php" => args.iter().any(|t| {
            matches!(t.as_str(), "-r" | "-B" | "-R" | "-F" | "-E" | "-S" | "--server" | "-a" | "-d") || (!t.starts_with("--") && (t.starts_with("-r") || t.starts_with("-S") || t.starts_with("-d")))
        }),
        "lua" | "luajit" => has_short(&['e', 'l']),
        "osascript" | "rscript" => args.iter().any(|t| t.as_str() == "-e" || (!t.starts_with("--") && t.starts_with("-e"))),
        "tclsh" => args.iter().any(|t| t.as_str() == "-c" || (!t.starts_with("--") && t.starts_with("-c"))),
        "deno" => args.iter().any(|t| matches!(t.as_str(), "eval" | "install" | "add" | "publish" | "upgrade" | "compile" | "serve" | "jupyter")),
        "bun" => args.iter().any(|t| {
            matches!(t.as_str(), "-e" | "--eval" | "-r" | "--preload" | "--inspect" | "--inspect-brk" | "--inspect-wait")
                || t.starts_with("--eval=")
                || t.starts_with("--preload=")
                || t.starts_with("--inspect=")
                || t.starts_with("--inspect-")
                || (!t.starts_with("--") && (t.starts_with("-e") || t.starts_with("-r")))
        }),
        _ => false,
    }
}

/// True for programs that run another command named anywhere in their arguments.
pub fn is_generic_runner(cmd: &str) -> bool {
    GENERIC_RUNNERS.contains(&cmd) || cmd == "trap"
}

const POSIX_SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "ash"];
const SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "ash", "pwsh", "powershell", "powershell.exe", "cmd", "cmd.exe"];
const NETWORK_COMMANDS: &[&str] = &[
    "curl",
    "curlie",
    "wget",
    "fetch",
    "aria2c",
    "axel",
    "http",
    "https",
    "httpie",
    "xh",
    "xhs",
    "websocat",
    "wscat",
    "grpcurl",
    "ab",
    "hey",
    "wrk",
    "ssh",
    "ssh-keygen",
    "ssh-keyscan",
    "scp",
    "sftp",
    "rsync",
    "rclone",
    "s3cmd",
    "nc",
    "ncat",
    "netcat",
    "ftp",
    "tnftp",
    "lftp",
    "telnet",
    "socat",
    "ngrok",
    "cloudflared",
    "tailscale",
    "gh",
    "glab",
    "aws",
    "gcloud",
    "gsutil",
    "az",
    "dig",
    "nslookup",
    "host",
    "drill",
    "resolvectl",
    "whois",
    "ping",
    "ping6",
    "fping",
    "traceroute",
    "traceroute6",
    "tracepath",
    "mtr",
    "nmap",
];
const DESTRUCTIVE_COMMANDS: &[&str] = &["dd", "shred", "wipefs", "fdisk", "parted", "mkswap"];
const PROCESS_CONTROL_COMMANDS: &[&str] = &["kill", "pkill", "killall", "taskkill"];
const PRIVILEGED_COMMANDS: &[&str] =
    &["docker", "podman", "nerdctl", "kubectl", "helm", "systemctl", "service", "launchctl", "mount", "umount", "iptables", "ufw", "nft", "useradd", "usermod", "userdel", "passwd"];
const FILE_WRITE_COMMANDS: &[&str] =
    &["cp", "mv", "ln", "chmod", "chown", "chgrp", "truncate", "install", "mkdir", "touch", "rmdir", "unlink", "tee", "patch", "unzip", "zip", "gzip", "gunzip", "tar", "cpio", "rsync"];
const SAFE_READ_COMMANDS: &[&str] = &[
    "cat",
    "ls",
    "grep",
    "rg",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "cut",
    "tr",
    "column",
    "diff",
    "cmp",
    "comm",
    "pwd",
    "echo",
    "printf",
    "which",
    "type",
    "file",
    "stat",
    "du",
    "df",
    "tree",
    "date",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "test",
    "true",
    "false",
    "printenv",
    "id",
    "whoami",
    "uname",
    "hostname",
    "ps",
    "top",
    "free",
    "nproc",
    "sleep",
    "jq",
    "yq",
    "nl",
    "od",
    "xxd",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "cd",
    "export",
    "set",
];
pub const SAFE_SCRIPT_NAMES: &[&str] = &["test", "tests", "build", "lint", "typecheck", "check", "test:unit", "test:ci", "ci"];
const PACKAGE_MANAGERS: &[&str] = &[
    "npm", "pnpm", "yarn", "bun", "pip", "pip3", "uv", "poetry", "cargo", "go", "gem", "composer", "apt", "apt-get", "pkg", "brew", "dnf", "pacman", "apk", "yum", "npx", "pnpx", "bunx", "uvx", "pipx",
];
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
    "search",
    "login",
    "owner",
    "yank",
];
const SAFE_GIT_SUBCOMMANDS: &[&str] =
    &["status", "diff", "log", "show", "blame", "shortlog", "describe", "rev-parse", "ls-files", "ls-tree", "grep", "whatchanged", "reflog", "cat-file", "hash-object", "version", "help"];
const NETWORK_GIT_SUBCOMMANDS: &[&str] = &["push", "pull", "fetch", "clone", "ls-remote", "submodule", "request-pull", "archive", "lfs", "send-email", "imap-send", "svn", "p4"];
const WRITE_GIT_SUBCOMMANDS: &[&str] = &[
    "add",
    "commit",
    "mv",
    "rm",
    "restore",
    "reset",
    "checkout",
    "switch",
    "stash",
    "merge",
    "rebase",
    "cherry-pick",
    "revert",
    "tag",
    "apply",
    "am",
    "clean",
    "config",
    "init",
    "gc",
    "prune",
    "notes",
    "worktree",
];
const DANGEROUS_RM_TARGETS: &[&str] = &[
    "/",
    "/*",
    "~",
    "~/",
    "~/*",
    "$HOME",
    "${HOME}",
    "$HOME/",
    "${HOME}/",
    "$HOME/*",
    "${HOME}/*",
    "/home",
    "/home/",
    "/home/*",
    "/root",
    "/root/",
    "/root/*",
    "/Users",
    "/Users/",
    "/Users/*",
    "..",
    "../",
    "../..",
    "../../..",
    ".",
    "./",
    "*",
    "./*",
    "~/.",
    "$HOME/.",
    "${HOME}/.",
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
    matches!(ch, None | Some(' ') | Some('\t') | Some('\n') | Some(';') | Some('&') | Some('|'))
}

const MAX_SCAN_DEPTH: usize = 32;

fn consume_backslash(chars: &[char], i: usize, current: &mut String) -> usize {
    if chars.get(i + 1) == Some(&'\n') {
        return i + 2;
    }
    if chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n') {
        return i + 3;
    }
    current.push('\\');
    if i + 1 < chars.len() {
        current.push(chars[i + 1]);
        i + 2
    } else {
        i + 1
    }
}

fn scan_dollar_brace(chars: &[char], mut i: usize, current: &mut String, in_double: bool, critical_escape: &mut bool, depth: usize) -> usize {
    if depth > MAX_SCAN_DEPTH {
        *critical_escape = true;
        current.push(chars[i]);
        return i + 1;
    }
    let body_start = i + 2;
    if matches!(chars.get(body_start), Some(c) if c.is_whitespace() || *c == '|' || *c == ';') {
        *critical_escape = true;
    }
    current.push('$');
    current.push('{');
    i += 2;
    let mut sq = false;
    let mut ansi_sq = false;
    let mut dq = false;
    let mut in_double_unmatched_sq = false;
    while i < chars.len() {
        let ch = chars[i];
        if ansi_sq {
            current.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                i += 1;
                current.push(chars[i]);
            } else if ch == '\'' {
                ansi_sq = false;
            }
            i += 1;
            continue;
        }
        if sq {
            if ch == '\'' {
                current.push(ch);
                sq = false;
                i += 1;
                continue;
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '\\' {
            i = consume_backslash(chars, i, current);
            continue;
        }
        if dq {
            if ch == '"' {
                current.push(ch);
                dq = false;
                i += 1;
                continue;
            }
            if ch == '`' {
                *critical_escape = true;
            } else if ch == '$' {
                if chars.get(i + 1) == Some(&'{') {
                    i = scan_dollar_brace(chars, i, current, true, critical_escape, depth + 1);
                    continue;
                }
                if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') {
                    i = scan_arith(chars, i, current, true, critical_escape, depth + 1);
                    continue;
                }
                if chars.get(i + 1) == Some(&'(') {
                    *critical_escape = true;
                }
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '}' {
            if in_double && in_double_unmatched_sq {
                *critical_escape = true;
            }
            current.push(ch);
            return i + 1;
        }
        if ch == '@' && i > body_start && chars.get(i + 1) == Some(&'P') && chars.get(i + 2) == Some(&'}') {
            *critical_escape = true;
        }
        if ch == '`' {
            *critical_escape = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if !in_double && (ch == '<' || ch == '>') && chars.get(i + 1) == Some(&'(') {
            *critical_escape = true;
        }
        if ch == '\'' {
            if in_double {
                in_double_unmatched_sq = !in_double_unmatched_sq;
            } else {
                sq = true;
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' {
            if in_double && in_double_unmatched_sq {
                *critical_escape = true;
            }
            dq = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '$' {
            if !in_double && chars.get(i + 1) == Some(&'\'') {
                ansi_sq = true;
                current.push('$');
                current.push('\'');
                i += 2;
                continue;
            }
            if chars.get(i + 1) == Some(&'"') {
                dq = true;
                current.push('$');
                current.push('"');
                i += 2;
                continue;
            }
            if chars.get(i + 1) == Some(&'{') {
                i = scan_dollar_brace(chars, i, current, in_double, critical_escape, depth + 1);
                continue;
            }
            if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') {
                i = scan_arith(chars, i, current, in_double, critical_escape, depth + 1);
                continue;
            }
            if chars.get(i + 1) == Some(&'(') {
                *critical_escape = true;
            }
        }
        current.push(ch);
        i += 1;
    }
    if in_double_unmatched_sq {
        *critical_escape = true;
    }
    i
}

fn scan_arith(chars: &[char], mut i: usize, current: &mut String, in_double: bool, critical_escape: &mut bool, depth: usize) -> usize {
    if depth > MAX_SCAN_DEPTH {
        *critical_escape = true;
        current.push(chars[i]);
        return i + 1;
    }
    current.push('$');
    current.push('(');
    current.push('(');
    i += 3;
    let mut inner_depth = 1i32;
    let mut sq = false;
    let mut ansi_sq = false;
    let mut dq = false;
    let mut in_double_unmatched_sq = false;
    while i < chars.len() {
        let ch = chars[i];
        if ansi_sq {
            current.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                i += 1;
                current.push(chars[i]);
            } else if ch == '\'' {
                ansi_sq = false;
            }
            i += 1;
            continue;
        }
        if sq {
            current.push(ch);
            if ch == '\'' {
                sq = false;
            }
            i += 1;
            continue;
        }
        if ch == '\\' {
            i = consume_backslash(chars, i, current);
            continue;
        }
        if dq {
            if ch == '"' {
                current.push(ch);
                dq = false;
                i += 1;
                continue;
            }
            if ch == '`' {
                *critical_escape = true;
            } else if ch == '$' {
                if chars.get(i + 1) == Some(&'{') {
                    i = scan_dollar_brace(chars, i, current, true, critical_escape, depth + 1);
                    continue;
                }
                if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') {
                    i = scan_arith(chars, i, current, true, critical_escape, depth + 1);
                    continue;
                }
                if chars.get(i + 1) == Some(&'(') {
                    *critical_escape = true;
                }
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '\'' {
            if in_double {
                in_double_unmatched_sq = !in_double_unmatched_sq;
            } else {
                sq = true;
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' {
            if in_double {
                *critical_escape = true;
            }
            dq = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '`' {
            *critical_escape = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '$' {
            if !in_double && chars.get(i + 1) == Some(&'\'') {
                ansi_sq = true;
                current.push('$');
                current.push('\'');
                i += 2;
                continue;
            }
            if chars.get(i + 1) == Some(&'"') {
                if in_double {
                    *critical_escape = true;
                }
                dq = true;
                current.push('$');
                current.push('"');
                i += 2;
                continue;
            }
            if chars.get(i + 1) == Some(&'{') {
                i = scan_dollar_brace(chars, i, current, in_double, critical_escape, depth + 1);
                continue;
            }
            if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') {
                i = scan_arith(chars, i, current, in_double, critical_escape, depth + 1);
                continue;
            }
            if chars.get(i + 1) == Some(&'(') {
                *critical_escape = true;
            }
        }
        if ch == '(' {
            inner_depth += 1;
            current.push('(');
            i += 1;
            continue;
        }
        if ch == ')' {
            inner_depth -= 1;
            if inner_depth == 0 {
                if in_double && in_double_unmatched_sq {
                    *critical_escape = true;
                }
                if chars.get(i + 1) == Some(&')') {
                    current.push(')');
                    current.push(')');
                    return i + 2;
                }
                *critical_escape = true;
                current.push(')');
                return i + 1;
            }
            current.push(')');
            i += 1;
            continue;
        }
        current.push(ch);
        i += 1;
    }
    *critical_escape = true;
    i
}

struct PendingHeredoc {
    delimiter: String,
    quoted: bool,
    strip_tabs: bool,
}

fn parse_heredoc_delimiter(chars: &[char], mut i: usize, current: &mut String) -> (Option<PendingHeredoc>, usize) {
    let strip_tabs = chars.get(i) == Some(&'-');
    if strip_tabs {
        current.push('-');
        i += 1;
    }
    while i < chars.len() {
        if chars[i] == ' ' || chars[i] == '\t' {
            current.push(chars[i]);
            i += 1;
        } else if chars[i] == '\\' && chars.get(i + 1) == Some(&'\n') {
            i += 2;
        } else {
            break;
        }
    }
    let mut delim = String::new();
    let mut quoted = false;
    let mut has_delim = false;
    let (mut sq, mut dq, mut ansi) = (false, false, false);
    while i < chars.len() {
        let ch = chars[i];
        if ansi {
            current.push(ch);
            if ch == '\'' {
                ansi = false;
                i += 1;
                continue;
            }
            if ch == '\\' {
                let (decoded, consumed) = decode_ansi_escape(chars, i + 1);
                for k in 0..consumed {
                    current.push(chars[i + 1 + k]);
                }
                if let Some(c) = decoded {
                    delim.push(c);
                }
                i += 1 + consumed;
                continue;
            }
            delim.push(ch);
            i += 1;
            continue;
        }
        if sq {
            current.push(ch);
            if ch == '\'' {
                sq = false;
            } else {
                delim.push(ch);
            }
            i += 1;
            continue;
        }
        if dq {
            current.push(ch);
            if ch == '"' {
                dq = false;
            } else if ch == '\\' && chars.get(i + 1) == Some(&'\n') {
                i += 2;
                continue;
            } else if ch == '\\' && matches!(chars.get(i + 1), Some('$' | '`' | '"' | '\\')) {
                i += 1;
                current.push(chars[i]);
                delim.push(chars[i]);
            } else {
                delim.push(ch);
            }
            i += 1;
            continue;
        }
        if matches!(ch, ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>') || (ch == '\r' && chars.get(i + 1) == Some(&'\n')) {
            break;
        }
        if ch == '\\' {
            if chars.get(i + 1) == Some(&'\n') {
                i += 2;
                continue;
            }
            quoted = true;
            has_delim = true;
            current.push('\\');
            if i + 1 < chars.len() {
                i += 1;
                current.push(chars[i]);
                delim.push(chars[i]);
            }
            i += 1;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            quoted = true;
            has_delim = true;
            ansi = true;
            current.push('$');
            current.push('\'');
            i += 2;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'"') {
            quoted = true;
            has_delim = true;
            dq = true;
            current.push('$');
            current.push('"');
            i += 2;
            continue;
        }
        if ch == '\'' {
            quoted = true;
            has_delim = true;
            sq = true;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' {
            quoted = true;
            has_delim = true;
            dq = true;
            current.push(ch);
            i += 1;
            continue;
        }
        has_delim = true;
        current.push(ch);
        delim.push(ch);
        i += 1;
    }
    let doc = has_delim.then_some(PendingHeredoc { delimiter: delim, quoted, strip_tabs });
    (doc, i)
}

fn consume_heredoc_body(chars: &[char], mut i: usize, doc: &PendingHeredoc, critical_escape: &mut bool) -> usize {
    while i < chars.len() {
        let mut line_end = i;
        let mut folded = String::new();
        let mut had_continuation = false;
        if doc.quoted {
            while line_end < chars.len() && chars[line_end] != '\n' {
                folded.push(chars[line_end]);
                line_end += 1;
            }
        } else {
            while line_end < chars.len() && chars[line_end] != '\n' {
                if chars[line_end] == '\\' && chars.get(line_end + 1) == Some(&'\n') {
                    let mut bs = 0usize;
                    let mut p = line_end;
                    while p > i && chars[p - 1] == '\\' {
                        bs += 1;
                        p -= 1;
                    }
                    if bs.is_multiple_of(2) {
                        had_continuation = true;
                        line_end += 2;
                        continue;
                    }
                }
                folded.push(chars[line_end]);
                line_end += 1;
            }
        }
        let folded_no_cr = folded.strip_suffix('\r').unwrap_or(folded.as_str());
        let check = if doc.strip_tabs { folded_no_cr.trim_start_matches('\t') } else { folded_no_cr };
        if check == doc.delimiter {
            if had_continuation {
                // `EO\<newline>F` closes `<<EOF` in bash but not in dash (`/bin/sh`).
                *critical_escape = true;
            }
            return if line_end < chars.len() { line_end + 1 } else { line_end };
        }
        if !doc.quoted {
            let body_chars: Vec<char> = folded.chars().collect();
            let mut k = 0;
            let mut sink = String::new();
            while k < body_chars.len() {
                let bc = body_chars[k];
                if bc == '\\' {
                    k += if k + 1 < body_chars.len() { 2 } else { 1 };
                    continue;
                }
                if bc == '`' {
                    *critical_escape = true;
                    k += 1;
                    continue;
                }
                if bc == '$' {
                    if body_chars.get(k + 1) == Some(&'{') {
                        k = scan_dollar_brace(&body_chars, k, &mut sink, true, critical_escape, 0);
                        continue;
                    }
                    if body_chars.get(k + 1) == Some(&'(') && body_chars.get(k + 2) == Some(&'(') {
                        k = scan_arith(&body_chars, k, &mut sink, true, critical_escape, 0);
                        continue;
                    }
                    if body_chars.get(k + 1) == Some(&'(') {
                        *critical_escape = true;
                    }
                }
                k += 1;
            }
        }
        i = if line_end < chars.len() { line_end + 1 } else { line_end };
    }
    i
}

/// Quote-aware scan: split compound commands into segments while ignoring separators inside
/// single/double quotes. `$(...)`, backticks and process substitution are critical unless they are
/// inside single quotes (literal text).
pub fn scan_shell(command: &str) -> ShellScan {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = vec![];
    let mut current = String::new();
    let mut ansi_c = false;
    let mut single = false;
    let mut double = false;
    let mut critical_escape = false;
    let mut redirection = false;
    let mut at_word_start = true;
    let mut pending_heredocs: Vec<PendingHeredoc> = vec![];
    let push = |current: &mut String, segments: &mut Vec<String>| {
        let trimmed = current.trim_matches(|c| matches!(c, ' ' | '\t' | '\n'));
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
        current.clear();
    };
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ansi_c {
            current.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                i += 1;
                current.push(chars[i]);
            } else if ch == '\'' {
                ansi_c = false;
            }
            i += 1;
            continue;
        }
        if single {
            current.push(ch);
            if ch == '\'' {
                single = false;
            }
            i += 1;
            continue;
        }
        if ch == '\\' && !double {
            let is_line_cont = chars.get(i + 1) == Some(&'\n') || (chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n'));
            i = consume_backslash(&chars, i, &mut current);
            if !is_line_cont {
                at_word_start = false;
            }
            continue;
        }
        if double {
            if ch == '\\' {
                i = consume_backslash(&chars, i, &mut current);
                continue;
            }
            if ch == '"' {
                current.push(ch);
                double = false;
                i += 1;
                continue;
            }
            if ch == '`' {
                critical_escape = true;
                current.push(ch);
                i += 1;
                continue;
            }
            if ch == '$' {
                if chars.get(i + 1) == Some(&'{') {
                    i = scan_dollar_brace(&chars, i, &mut current, true, &mut critical_escape, 0);
                    continue;
                }
                if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') {
                    i = scan_arith(&chars, i, &mut current, true, &mut critical_escape, 0);
                    continue;
                }
                if chars.get(i + 1) == Some(&'(') {
                    critical_escape = true;
                }
            }
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '#' && at_word_start {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == ' ' || ch == '\t' {
            current.push(ch);
            at_word_start = true;
            i += 1;
            continue;
        }
        if ch == '\r' {
            if chars.get(i + 1) == Some(&'\n') {
                i += 1;
            } else {
                current.push('\r');
                at_word_start = false;
                i += 1;
            }
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            ansi_c = true;
            at_word_start = false;
            current.push('$');
            current.push('\'');
            i += 2;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'"') {
            double = true;
            at_word_start = false;
            current.push('$');
            current.push('"');
            i += 2;
            continue;
        }
        if ch == '\'' {
            single = true;
            at_word_start = false;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' {
            double = true;
            at_word_start = false;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '`' {
            critical_escape = true;
            at_word_start = false;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') {
            at_word_start = false;
            i = scan_arith(&chars, i, &mut current, false, &mut critical_escape, 0);
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'{') {
            at_word_start = false;
            i = scan_dollar_brace(&chars, i, &mut current, false, &mut critical_escape, 0);
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'(') {
            critical_escape = true;
            at_word_start = false;
            current.push(ch);
            i += 1;
            continue;
        }
        if (ch == '<' || ch == '>') && chars.get(i + 1) == Some(&'(') {
            critical_escape = true;
            at_word_start = false;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '<' && chars.get(i + 1) == Some(&'<') {
            redirection = true;
            if chars.get(i + 2) == Some(&'<') {
                current.push_str("<<<");
                at_word_start = true;
                i += 3;
                continue;
            }
            current.push_str("<<");
            let (doc, next_i) = parse_heredoc_delimiter(&chars, i + 2, &mut current);
            if let Some(d) = doc {
                pending_heredocs.push(d);
            }
            at_word_start = true;
            i = next_i;
            continue;
        }
        if ch == '&' && chars.get(i + 1) == Some(&'>') {
            redirection = true;
            current.push('&');
            current.push('>');
            i += 2;
            if chars.get(i) == Some(&'>') {
                current.push('>');
                i += 1;
            }
            at_word_start = true;
            continue;
        }
        if ch == '>' || ch == '<' {
            redirection = true;
            current.push(ch);
            if let Some(&nxt) = chars.get(i + 1) {
                if nxt == '&' || nxt == '>' || (ch == '>' && nxt == '|') {
                    current.push(nxt);
                    i += 2;
                    at_word_start = true;
                    continue;
                }
            }
            at_word_start = true;
            i += 1;
            continue;
        }
        if ch == '{' || ch == '}' {
            let prev = if i == 0 { None } else { Some(chars[i - 1]) };
            let next = chars.get(i + 1).copied();
            let standalone = is_word_boundary(prev) && (is_word_boundary(next) || ch == '}');
            if standalone {
                push(&mut current, &mut segments);
                at_word_start = true;
                i += 1;
                continue;
            }
            at_word_start = false;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '\n' {
            push(&mut current, &mut segments);
            at_word_start = true;
            i += 1;
            if !pending_heredocs.is_empty() {
                let docs = std::mem::take(&mut pending_heredocs);
                for doc in docs {
                    i = consume_heredoc_body(&chars, i, &doc, &mut critical_escape);
                }
            }
            continue;
        }
        if ch == ';' || ch == '(' || ch == ')' {
            push(&mut current, &mut segments);
            at_word_start = true;
            i += 1;
            continue;
        }
        if ch == '&' || ch == '|' {
            push(&mut current, &mut segments);
            at_word_start = true;
            if chars.get(i + 1) == Some(&ch) {
                i += 1;
            }
            i += 1;
            continue;
        }
        at_word_start = false;
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

fn consume_redirection_word(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() {
        if chars[i] == ' ' || chars[i] == '\t' {
            i += 1;
        } else if chars[i] == '\\' && chars.get(i + 1) == Some(&'\n') {
            i += 2;
        } else {
            break;
        }
    }
    let (mut sq, mut dq, mut ansi) = (false, false, false);
    let mut brace_depth = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ansi {
            if ch == '\\' && i + 1 < chars.len() {
                i += 2;
                continue;
            }
            if ch == '\'' {
                ansi = false;
            }
            i += 1;
            continue;
        }
        if sq {
            if ch == '\'' {
                sq = false;
            }
            i += 1;
            continue;
        }
        if ch == '\\' {
            i += if i + 1 < chars.len() { 2 } else { 1 };
            continue;
        }
        if dq {
            if ch == '"' {
                dq = false;
            }
            i += 1;
            continue;
        }
        if brace_depth == 0 && matches!(ch, ' ' | '\t' | '\n' | '<' | '>' | ';' | '&' | '|' | '(' | ')') {
            break;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            ansi = true;
            i += 2;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'"') {
            dq = true;
            i += 2;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'{') {
            brace_depth += 1;
            i += 2;
            continue;
        }
        if ch == '}' && brace_depth > 0 {
            brace_depth -= 1;
            i += 1;
            continue;
        }
        if ch == '\'' {
            sq = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            dq = true;
            i += 1;
            continue;
        }
        i += 1;
    }
    i
}

fn tokenize_inner(segment: &str, keep_empty: bool, posix_ifs_only: bool, strip_redirs: bool) -> Vec<String> {
    let chars: Vec<char> = segment.chars().collect();
    let mut tokens = vec![];
    let mut current = String::new();
    let mut has_token = false;
    let mut has_quoted_part = false;
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
            } else if ch == '\\' && chars.get(i + 1) == Some(&'\n') {
                i += 2;
                continue;
            } else if ch == '\\' && chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n') {
                i += 3;
                continue;
            } else if ch == '\\' && i + 1 < chars.len() {
                i += 1;
                current.push(chars[i]);
            } else {
                current.push(ch);
            }
            i += 1;
            continue;
        }
        if ch == '#' && !has_token && current.is_empty() {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '\\' && chars.get(i + 1) == Some(&'\n') {
            i += 2;
            continue;
        }
        if ch == '\\' && chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n') {
            i += 3;
            continue;
        }
        if strip_redirs && (ch == '<' || ch == '>' || (ch == '&' && chars.get(i + 1) == Some(&'>'))) {
            if !has_quoted_part && !current.is_empty() && current.chars().all(|c| c.is_ascii_digit()) {
                current.clear();
                has_token = false;
            } else if !current.is_empty() || (keep_empty && has_token) {
                tokens.push(std::mem::take(&mut current));
                has_token = false;
                has_quoted_part = false;
            }
            if ch == '&' {
                i += 2;
                if chars.get(i) == Some(&'>') {
                    i += 1;
                }
            } else if ch == '<' && chars.get(i + 1) == Some(&'<') {
                i += 2;
                if matches!(chars.get(i), Some('<' | '-')) {
                    i += 1;
                }
            } else {
                i += 1;
                if matches!(chars.get(i), Some('>' | '&' | '|')) {
                    i += 1;
                }
            }
            i = consume_redirection_word(&chars, i);
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            ansi_c = true;
            has_token = true;
            has_quoted_part = true;
            i += 2;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'"') {
            double = true;
            has_token = true;
            has_quoted_part = true;
            i += 2;
            continue;
        }
        if ch == '\'' {
            single = true;
            has_token = true;
            has_quoted_part = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            double = true;
            has_token = true;
            has_quoted_part = true;
            i += 1;
            continue;
        }
        if ch == '\\' && i + 1 < chars.len() {
            i += 1;
            current.push(chars[i]);
            has_token = true;
            has_quoted_part = true;
            i += 1;
            continue;
        }
        let is_ws = if posix_ifs_only { matches!(ch, ' ' | '\t' | '\n') } else { ch.is_whitespace() };
        if is_ws {
            if !current.is_empty() || (keep_empty && has_token) {
                tokens.push(std::mem::take(&mut current));
            }
            has_token = false;
            has_quoted_part = false;
            i += 1;
            continue;
        }
        current.push(ch);
        has_token = true;
        i += 1;
    }
    if !current.is_empty() || (keep_empty && has_token) {
        tokens.push(current);
    }
    tokens
}

/// Quote-aware word split. Quotes are removed; backslash escapes are kept as literal chars.
pub fn tokenize(segment: &str) -> Vec<String> {
    tokenize_inner(segment, false, false, false)
}

/// POSIX shell `IFS` (`' \t\n'`) word split that preserves empty quoted words (`""` / `''`) and
/// strips unquoted I/O redirections (`>/dev/null rm -rf /`, `2>&1`).
pub(crate) fn tokenize_argv(segment: &str) -> Vec<String> {
    tokenize_inner(segment, true, true, true)
}

fn tokenize_env_s(raw: &str) -> Vec<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut expanded = String::with_capacity(raw.len());
    let (mut sq, mut dq) = (false, false);
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() && !sq {
            let next = chars[i + 1];
            if next == '_' && !dq {
                expanded.push(' ');
                i += 2;
                continue;
            }
            if next == 'c' && !dq {
                break;
            }
            if next == 't' {
                expanded.push('\t');
                i += 2;
                continue;
            }
            if next == 'n' {
                expanded.push('\n');
                i += 2;
                continue;
            }
            expanded.push('\\');
            expanded.push(next);
            i += 2;
            continue;
        }
        if ch == '\'' && !dq {
            sq = !sq;
        } else if ch == '"' && !sq {
            dq = !dq;
        }
        expanded.push(ch);
        i += 1;
    }
    tokenize_inner(&expanded, true, true, false)
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

fn skip_flags(tokens: &[String], value_flags: &[&str], short_value_chars: &[char]) -> Vec<String> {
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
        if value_flags.contains(&token.as_str()) {
            if index < tokens.len() {
                index += 1;
            }
            continue;
        }
        if !token.starts_with("--") && token.len() > 2 && !short_value_chars.is_empty() {
            for (byte_idx, ch) in token[1..].char_indices() {
                if !ch.is_ascii_alphanumeric() {
                    break;
                }
                if short_value_chars.contains(&ch) {
                    if 1 + byte_idx + ch.len_utf8() == token.len() && index < tokens.len() {
                        index += 1;
                    }
                    break;
                }
            }
        }
    }
    vec![]
}

fn split_env_s(rest: &[String]) -> Option<(String, usize)> {
    let mut i = 0;
    while i < rest.len() {
        let t = &rest[i];
        if t == "--" || (!t.starts_with('-') && !is_env_assignment(t)) || t == "-" {
            return None;
        }
        if is_env_assignment(t) {
            i += 1;
            continue;
        }
        if matches!(t.as_str(), "-u" | "--unset" | "-C" | "--chdir" | "-a" | "--argv0") {
            i += 2;
            continue;
        }
        if t == "-S" || t == "--split-string" {
            return Some((rest.get(i + 1).cloned().unwrap_or_default(), i + 2));
        }
        if let Some(raw) = t.strip_prefix("--split-string=") {
            return Some((raw.to_string(), i + 1));
        }
        if !t.starts_with("--") && t.len() > 1 {
            for (byte_idx, ch) in t[1..].char_indices() {
                if matches!(ch, 'u' | 'C' | 'a') {
                    if byte_idx + ch.len_utf8() >= t.len() - 1 {
                        i += 1;
                    }
                    break;
                }
                if ch == 'S' {
                    let after_s = 1 + byte_idx + ch.len_utf8();
                    if after_s < t.len() {
                        return Some((t[after_s..].to_string(), i + 1));
                    }
                    return Some((rest.get(i + 1).cloned().unwrap_or_default(), i + 2));
                }
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn is_dangerous_env_var(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    matches!(
        u.as_str(),
        "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "LD_AUDIT"
            | "NODE_OPTIONS"
            | "NODE_PATH"
            | "PYTHONSTARTUP"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "PERL5OPT"
            | "PERL5LIB"
            | "PERLLIB"
            | "RUBYOPT"
            | "RUBYLIB"
            | "JAVA_TOOL_OPTIONS"
            | "JDK_JAVA_OPTIONS"
            | "_JAVA_OPTIONS"
            | "RUSTC_WRAPPER"
            | "RUSTC_WORKSPACE_WRAPPER"
            | "CARGO_BUILD_RUSTC_WRAPPER"
            | "BASH_ENV"
            | "ENV"
            | "PROMPT_COMMAND"
            | "SHELLOPTS"
            | "BASHOPTS"
            | "IFS"
            | "GIT_SSH"
            | "GIT_SSH_COMMAND"
            | "GIT_PROXY_COMMAND"
            | "GIT_ASKPASS"
            | "SSH_ASKPASS"
            | "GIT_EXTERNAL_DIFF"
            | "GIT_EDITOR"
            | "GIT_SEQUENCE_EDITOR"
            | "GIT_PAGER"
            | "GIT_EXEC_PATH"
            | "GIT_TEMPLATE_DIR"
            | "CURL_HOME"
            | "WGETRC"
            | "NPM_CONFIG_SCRIPT_SHELL"
    ) || u.starts_with("DYLD_")
        || u.starts_with("GIT_CONFIG_")
        || (u.starts_with("CARGO_TARGET_") && u.ends_with("_RUNNER"))
}

pub(crate) fn peel_wrappers_with_env(input: &[String]) -> (Vec<String>, RiskClass, Vec<String>, bool) {
    let mut tokens = input.to_vec();
    let mut floor = RiskClass::Low;
    let mut env_names = Vec::new();
    let mut used_env_s = false;
    for _ in 0..20 {
        for tok in tokens.iter().take_while(|t| is_env_assignment(t)) {
            if let Some((k, _)) = tok.split_once('=') {
                if is_dangerous_env_var(k) {
                    floor = max_risk(floor, RiskClass::High);
                }
                env_names.push(k.to_string());
            }
        }
        tokens = strip_env_assignments(&tokens);
        if tokens.is_empty() {
            return (tokens, max_risk(floor, RiskClass::Medium), env_names, used_env_s);
        }
        let wrapper = base_name(&tokens[0]);
        if !WRAPPERS.contains(&wrapper.as_str()) {
            return (tokens, floor, env_names, used_env_s);
        }
        if HIGH_FLOOR_WRAPPERS.contains(&wrapper.as_str()) {
            floor = max_risk(floor, RiskClass::High);
        }
        let rest = tokens[1..].to_vec();
        match wrapper.as_str() {
            "env" => {
                if let Some((raw, after)) = split_env_s(&rest) {
                    used_env_s = true;
                    if raw.is_empty() {
                        return (vec![], max_risk(floor, RiskClass::High), env_names, used_env_s);
                    }
                    let mut split_tokens = tokenize_env_s(&raw);
                    if after < rest.len() {
                        split_tokens.extend_from_slice(&rest[after..]);
                    }
                    tokens = split_tokens;
                    floor = max_risk(floor, RiskClass::High);
                    continue;
                }
                tokens = skip_flags(&rest, &["-u", "--unset", "-C", "--chdir", "-a", "--argv0"], &['u', 'C', 'a']);
            }
            "sudo" => {
                tokens = skip_flags(
                    &rest,
                    &[
                        "-u",
                        "--user",
                        "-g",
                        "--group",
                        "-p",
                        "--prompt",
                        "-c",
                        "--class",
                        "-h",
                        "--host",
                        "-r",
                        "--role",
                        "-t",
                        "--type",
                        "-U",
                        "--other-user",
                        "-D",
                        "--chdir",
                        "-R",
                        "--chroot",
                        "-T",
                        "--command-timeout",
                        "-C",
                        "--close-from",
                    ],
                    &['u', 'g', 'p', 'c', 'h', 'r', 't', 'U', 'D', 'R', 'T', 'C'],
                );
            }
            "doas" => tokens = skip_flags(&rest, &["-u", "-C"], &['u', 'C']),
            "timeout" => {
                let after = skip_flags(&rest, &["-s", "--signal", "-k", "--kill-after"], &['s', 'k']);
                tokens = after.into_iter().skip(1).collect();
            }
            "nice" => tokens = skip_flags(&rest, &["-n", "--adjustment"], &['n']),
            "stdbuf" => tokens = skip_flags(&rest, &["-i", "--input", "-o", "--output", "-e", "--error"], &['i', 'o', 'e']),
            "xargs" => {
                tokens = skip_flags(
                    &rest,
                    &[
                        "-n",
                        "--max-args",
                        "-I",
                        "-i",
                        "--replace",
                        "-d",
                        "--delimiter",
                        "-E",
                        "-e",
                        "--eof",
                        "-L",
                        "-l",
                        "--max-lines",
                        "-P",
                        "--max-procs",
                        "-s",
                        "--max-chars",
                        "-a",
                        "--arg-file",
                        "--process-slot-var",
                    ],
                    &['n', 'I', 'd', 'E', 'L', 'P', 's', 'a'],
                );
            }
            "time" => tokens = skip_flags(&rest, &["-f", "--format", "-o", "--output"], &['f', 'o']),
            "exec" => tokens = skip_flags(&rest, &["-a"], &['a']),
            "command" => {
                let pre_dd: Vec<&String> = rest.iter().take_while(|t| *t != "--" && t.starts_with('-') && t.as_str() != "-").collect();
                let lookup_only = !pre_dd.is_empty()
                    && pre_dd.iter().all(|t| !t.starts_with("--") && t[1..].chars().all(|c| matches!(c, 'p' | 'v' | 'V')))
                    && pre_dd.iter().any(|t| t[1..].chars().any(|c| c == 'v' || c == 'V'));
                if lookup_only {
                    return (vec![], floor, env_names, used_env_s);
                }
                tokens = skip_flags(&rest, &[], &[]);
            }
            _ => tokens = skip_flags(&rest, &[], &[]),
        }
    }
    (vec![], max_risk(floor, RiskClass::High), env_names, used_env_s)
}

/// Peels environment assignments and command wrappers, returning the inner argv and a risk floor.
pub fn peel_wrappers(input: &[String]) -> (Vec<String>, RiskClass) {
    let (tokens, floor, _, _) = peel_wrappers_with_env(input);
    (tokens, floor)
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
    if clean.is_empty() {
        return false;
    }
    if DANGEROUS_RM_TARGETS.contains(&clean) {
        return true;
    }
    let slash = clean.replace('\\', "/");
    let lower = slash.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let rest_drive = slash[2..].trim_matches('/');
        if rest_drive.is_empty() || rest_drive == "*" || rest_drive == "." {
            return true;
        }
    }
    let is_abs = slash.starts_with('/');
    let raw_parts: Vec<&str> = slash.split('/').filter(|p| !p.is_empty()).collect();
    if !is_abs {
        if let Some(&first) = raw_parts.first() {
            if matches!(first, "~" | "$HOME" | "${HOME}") {
                let mut tail: Vec<&str> = Vec::new();
                for &part in &raw_parts[1..] {
                    if part == "." {
                        continue;
                    }
                    if part == ".." {
                        if tail.pop().is_none() {
                            return true;
                        }
                    } else {
                        tail.push(part);
                    }
                }
                return tail.is_empty() || (tail.len() == 1 && tail[0] == "*");
            }
        }
    }
    if is_abs {
        let mut stack: Vec<&str> = Vec::new();
        for part in raw_parts {
            if part == "." {
                continue;
            }
            if part == ".." {
                stack.pop();
            } else {
                stack.push(part);
            }
        }
        return stack.is_empty() || stack == ["*"] || matches!(stack.as_slice(), ["home"] | ["home", "*"] | ["root"] | ["root", "*"] | ["Users"] | ["Users", "*"]);
    }
    let mut stack: Vec<&str> = Vec::new();
    for part in raw_parts {
        if part == "." {
            continue;
        }
        if part == ".." {
            if stack.last().is_some_and(|last| *last != "..") {
                stack.pop();
            } else {
                stack.push("..");
            }
        } else {
            stack.push(part);
        }
    }
    stack.is_empty() || stack == ["*"] || stack.first() == Some(&"..")
}

fn is_gnu_long_prefix(token: &str, full: &str) -> bool {
    token.len() >= 3 && token.starts_with("--") && full.starts_with(token)
}

fn classify_rm(rest: &[String]) -> RiskClass {
    let end_opts = rest.iter().position(|t| t == "--").unwrap_or(rest.len());
    let flags: Vec<&String> = rest[..end_opts].iter().filter(|t| t.starts_with('-') && t.as_str() != "-").collect();
    let recursive = flags.iter().any(|t| is_gnu_long_prefix(t, "--recursive") || (!t.starts_with("--") && (t.contains('r') || t.contains('R'))));
    let force = flags.iter().any(|t| is_gnu_long_prefix(t, "--force") || (!t.starts_with("--") && t[1..].contains('f')));
    let no_preserve_root = flags.iter().any(|t| is_gnu_long_prefix(t, "--no-preserve-root"));
    let mut targets: Vec<&String> = rest[..end_opts].iter().filter(|t| !t.starts_with('-') || t.as_str() == "-").collect();
    if end_opts < rest.len() {
        targets.extend(rest[end_opts + 1..].iter());
    }
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

fn is_git_exec_flag(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    lower == "--exec-path" || lower.starts_with("--exec-path=") || lower == "--upload-pack" || lower.starts_with("--upload-pack=") || lower == "--receive-pack" || lower.starts_with("--receive-pack=")
}

fn classify_git(rest: &[String]) -> RiskClass {
    let mut index = 0;
    let mut floor = RiskClass::Low;
    while index < rest.len() {
        let token = &rest[index];
        if token == "--" {
            index += 1;
            break;
        }
        if matches!(token.as_str(), "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--super-prefix") {
            index += 2;
            continue;
        }
        if token == "-c" || token == "--config-env" {
            floor = max_risk(floor, RiskClass::High);
            index += 2;
            continue;
        }
        if token.starts_with("-c") || token.to_ascii_lowercase().starts_with("--config-env=") {
            floor = max_risk(floor, RiskClass::High);
            index += 1;
            continue;
        }
        if is_git_exec_flag(token) {
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
    if sub_args.iter().take_while(|t| *t != "--").any(|t| is_git_exec_flag(t)) {
        return RiskClass::High;
    }
    if (sub == "clone" || sub == "fetch" || sub == "ls-remote") && sub_args.iter().take_while(|t| *t != "--").any(|t| !t.starts_with("--") && t.starts_with("-u")) {
        return RiskClass::High;
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
    if sub == "config"
        && sub_args.iter().any(|t| {
            let lower = t.to_lowercase();
            [
                "core.hookspath",
                "core.pager",
                "core.editor",
                "core.sshcommand",
                "core.fsmonitor",
                "core.gitproxy",
                "core.askpass",
                "alias.",
                "http.",
                "https.",
                "ssh.",
                "credential.",
                "diff.external",
                "difftool.",
                "merge.",
                "mergetool.",
                "sequence.editor",
                "filter.",
                "protocol.",
                "gpg.",
                "fsck.",
                "transfer.",
                "receive.",
                "uploadpack.",
                "remote.",
                "url.",
            ]
            .iter()
            .any(|key| lower.starts_with(key))
        })
    {
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

const PKG_RISK_VALUE_FLAGS: &[&str] = &[
    "--prefix",
    "--registry",
    "--cache",
    "--userconfig",
    "--globalconfig",
    "--cwd",
    "--dir",
    "--directory",
    "--project",
    "--workspace",
    "-w",
    "--filter",
    "--loglevel",
    "--color",
    "--target",
    "--index-url",
    "-i",
    "--extra-index-url",
    "--python",
    "-p",
    "--root",
    "--manifest-path",
    "--features",
    "-F",
    "--package",
    "--toolchain",
    "--config",
    "--config-file",
    "--repository",
    "--source",
    "--build-dir",
    "-C",
];

fn skip_pkg_leading_flags<'a>(cmd: &str, tokens: &'a [String]) -> &'a [String] {
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "--" {
            return &tokens[i + 1..];
        }
        if cmd == "cargo" && t.starts_with('+') && t.len() > 1 {
            i += 1;
            continue;
        }
        if !t.starts_with('-') || t == "-" {
            return &tokens[i..];
        }
        i += 1;
        if PKG_RISK_VALUE_FLAGS.contains(&t.as_str()) {
            if i < tokens.len() {
                i += 1;
            }
            continue;
        }
        if !t.starts_with("--") && t.len() > 2 {
            for (byte_idx, ch) in t[1..].char_indices() {
                if !ch.is_ascii_alphanumeric() {
                    break;
                }
                if matches!(ch, 'w' | 'i' | 'F' | 'p' | 'C') {
                    if 1 + byte_idx + ch.len_utf8() == t.len() && i < tokens.len() {
                        i += 1;
                    }
                    break;
                }
            }
        }
    }
    &[]
}

fn classify_package_manager(cmd: &str, rest: &[String], depth: usize) -> RiskClass {
    if matches!(cmd, "npx" | "pnpx" | "bunx" | "uvx" | "pipx") {
        return RiskClass::High;
    }
    let mut floor = RiskClass::Low;
    let pre_dd: Vec<&String> = rest.iter().take_while(|t| *t != "--").collect();
    for (idx, tok) in pre_dd.iter().enumerate() {
        let lower = tok.to_ascii_lowercase();
        if matches!(lower.as_str(), "--script-shell" | "--onload-script")
            || lower.starts_with("--script-shell=")
            || lower.starts_with("--onload-script=")
            || (cmd == "go"
                && (matches!(lower.as_str(), "-exec" | "--exec" | "-toolexec" | "--toolexec")
                    || lower.starts_with("-exec=")
                    || lower.starts_with("--exec=")
                    || lower.starts_with("-toolexec=")
                    || lower.starts_with("--toolexec=")))
        {
            return RiskClass::High;
        }
        if cmd == "cargo" {
            let cfg_val = if lower == "--config" { pre_dd.get(idx + 1).map(|s| s.to_ascii_lowercase()) } else { lower.strip_prefix("--config=").map(str::to_string) };
            if cfg_val.is_some_and(|v| v.contains("wrapper") || v.contains("runner") || v.contains("linker")) {
                return RiskClass::High;
            }
        }
        if matches!(
            lower.as_str(),
            "--prefix" | "--cwd" | "--dir" | "--directory" | "--project" | "--userconfig" | "--globalconfig" | "--config" | "--config-file" | "--manifest-path" | "-modfile" | "-overlay"
        ) || lower.starts_with("--prefix=")
            || lower.starts_with("--cwd=")
            || lower.starts_with("--dir=")
            || lower.starts_with("--directory=")
            || lower.starts_with("--project=")
            || lower.starts_with("--userconfig=")
            || lower.starts_with("--globalconfig=")
            || lower.starts_with("--config=")
            || lower.starts_with("--config-file=")
            || lower.starts_with("--manifest-path=")
            || lower.starts_with("-modfile=")
            || lower.starts_with("-overlay=")
            || (!tok.starts_with("--") && tok.starts_with("-C"))
        {
            floor = max_risk(floor, RiskClass::Medium);
        }
    }

    let tail = skip_pkg_leading_flags(cmd, rest);
    let sub = tail.first().map(|s| s.to_lowercase()).unwrap_or_default();
    let after_sub = if tail.is_empty() { &[][..] } else { &tail[1..] };
    if sub.is_empty() {
        return max_risk(floor, RiskClass::Medium);
    }
    if INSTALL_SUBCOMMANDS.contains(&sub.as_str()) {
        return RiskClass::High;
    }
    if (cmd == "poetry" && sub == "run") || (cmd == "uv" && sub == "run") {
        let mut idx = 0;
        while idx < after_sub.len() {
            let t = &after_sub[idx];
            if t == "--" {
                idx += 1;
                break;
            }
            if !t.starts_with('-') || t == "-" {
                break;
            }
            let lower = t.to_ascii_lowercase();
            if lower.starts_with("--with")
                || lower == "--refresh"
                || lower == "--upgrade"
                || t == "-U"
                || lower.starts_with("--index")
                || lower.starts_with("--default-index")
                || lower.starts_with("--extra-index")
            {
                floor = max_risk(floor, RiskClass::High);
            }
            idx += 1;
            if matches!(
                lower.as_str(),
                "--with"
                    | "--with-editable"
                    | "--with-requirements"
                    | "--python"
                    | "-p"
                    | "--index"
                    | "--default-index"
                    | "--index-url"
                    | "-i"
                    | "--extra-index-url"
                    | "--find-links"
                    | "-f"
                    | "--package"
                    | "--group"
                    | "--extra"
                    | "--env-file"
                    | "--cache-dir"
                    | "--directory"
                    | "-c"
                    | "--project"
                    | "--config-file"
            ) && idx < after_sub.len()
            {
                idx += 1;
            }
        }
        let inner = &after_sub[idx..];
        if inner.is_empty() {
            return max_risk(floor, RiskClass::Medium);
        }
        if inner[0].starts_with("http://") || inner[0].starts_with("https://") || depth > 4 {
            return RiskClass::High;
        }
        let (peeled, inner_floor) = peel_wrappers(inner);
        if peeled.is_empty() {
            return max_risk(floor, inner_floor);
        }
        return max_risk(floor, max_risk(inner_floor, classify_base(&base_name(&peeled[0]), &peeled[1..], depth + 1)));
    }
    if cmd == "uv" {
        if sub == "tool" {
            let next = after_sub.iter().find(|t| !t.starts_with('-')).map(|s| s.to_ascii_lowercase());
            return if matches!(next.as_deref(), Some("list" | "dir")) { max_risk(floor, RiskClass::Low) } else { RiskClass::High };
        }
        if sub == "pip" {
            let next = after_sub.iter().find(|t| !t.starts_with('-')).map(|s| s.to_ascii_lowercase());
            return if matches!(next.as_deref(), Some("list" | "show" | "freeze" | "check" | "tree")) { max_risk(floor, RiskClass::Low) } else { RiskClass::High };
        }
        if sub == "self" || (sub == "python" && after_sub.iter().any(|t| t == "install" || t == "upgrade")) {
            return RiskClass::High;
        }
    }
    if cmd == "go" {
        if sub == "generate" {
            return RiskClass::High;
        }
        if sub == "run" {
            let remote = after_sub.iter().take_while(|t| *t != "--").filter(|t| !t.starts_with('-')).any(|a| {
                a.contains('@')
                    || (a.contains('/')
                        && !a.starts_with("./")
                        && !a.starts_with("../")
                        && !a.starts_with('/')
                        && a.split('/').next().is_some_and(|first| first.contains('.') && !first.ends_with(".go")))
            });
            return if remote { RiskClass::High } else { max_risk(floor, RiskClass::Medium) };
        }
    }
    if sub == "config" {
        let non_flags: Vec<&String> = after_sub.iter().take_while(|t| *t != "--").filter(|t| !t.starts_with('-')).collect();
        let action = non_flags.first().map(|s| s.to_ascii_lowercase());
        if matches!(action.as_deref(), Some("get" | "list" | "ls") | None) {
            return max_risk(floor, RiskClass::Low);
        }
        if action.as_deref() == Some("set")
            && after_sub.iter().any(|a| {
                let l = a.to_ascii_lowercase();
                l.contains("script-shell") || l.contains("onload-script") || l.contains("proxy") || l.contains("registry")
            })
        {
            return RiskClass::High;
        }
        return max_risk(floor, RiskClass::Medium);
    }
    if sub == "pkg" {
        let non_flags: Vec<&String> = after_sub.iter().take_while(|t| *t != "--").filter(|t| !t.starts_with('-')).collect();
        let action = non_flags.first().map(|s| s.to_ascii_lowercase());
        if action.as_deref() == Some("get") {
            return max_risk(floor, RiskClass::Low);
        }
        if action.as_deref() == Some("set") && after_sub.iter().any(|a| a.starts_with("scripts.") || a == "scripts") {
            return RiskClass::High;
        }
        return max_risk(floor, RiskClass::Medium);
    }
    if ["ls", "list", "help", "version", "why", "root"].contains(&sub.as_str()) {
        return max_risk(floor, RiskClass::Low);
    }
    if sub == "run" || sub == "run-script" || sub == "workspace" {
        let rest_after = skip_pkg_leading_flags(cmd, after_sub);
        let script = rest_after.first().map(|s| s.to_lowercase()).unwrap_or_default();
        if script.is_empty() {
            return max_risk(floor, RiskClass::Medium);
        }
        return if SAFE_SCRIPT_NAMES.contains(&script.as_str()) { max_risk(floor, RiskClass::Low) } else { max_risk(floor, RiskClass::Medium) };
    }
    if ["test", "t", "exec"].contains(&sub.as_str()) {
        return if sub == "exec" { RiskClass::High } else { max_risk(floor, RiskClass::Low) };
    }
    if ["pip", "pip3", "uv", "poetry", "gem", "composer"].contains(&cmd) && ["list", "show", "freeze", "check", "--version", "version"].contains(&sub.as_str()) {
        return max_risk(floor, RiskClass::Low);
    }
    max_risk(floor, RiskClass::Medium)
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

pub(crate) fn is_shell_inline_exec(cmd: &str, rest: &[String]) -> bool {
    if POSIX_SHELL_INTERPRETERS.contains(&cmd) {
        return rest.iter().take_while(|t| *t != "--").any(|t| {
            (t.starts_with('-') && !t.starts_with("--") && t.len() > 1 && t[1..].chars().take_while(|c| c.is_ascii_alphabetic()).any(|c| c == 'c' || (cmd == "fish" && c == 'C')))
                || (cmd == "fish" && (t == "--command" || t == "--init-command" || t.starts_with("--command=") || t.starts_with("--init-command=")))
        });
    }
    if cmd == "pwsh" || cmd == "powershell" {
        return rest.iter().any(|t| {
            let lower = t.to_ascii_lowercase();
            lower == "-c"
                || lower == "/c"
                || lower == "-ec"
                || lower == "/ec"
                || (lower.len() >= 2 && (lower.starts_with("-c") || lower.starts_with("/c")) && "-command".starts_with(&format!("-{}", &lower[1..])))
                || (lower.len() >= 2 && (lower.starts_with("-e") || lower.starts_with("/e")) && "-encodedcommand".starts_with(&format!("-{}", &lower[1..])))
        });
    }
    if cmd == "cmd" {
        return rest.iter().any(|t| {
            let lower = t.to_ascii_lowercase();
            lower.starts_with("/c") || lower.starts_with("/k")
        });
    }
    false
}

pub(crate) fn is_python_cmd(cmd: &str) -> bool {
    matches!(cmd, "python" | "python2" | "python3" | "py" | "pypy" | "pypy3") || (cmd.starts_with("python") && cmd["python".len()..].chars().all(|c| c.is_ascii_digit() || c == '.'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PythonInvocation {
    InlineCode,
    Module(String),
    Stdin,
    ScriptOrOther { has_script: bool, help_or_version: bool },
}

pub(crate) fn parse_python_args(rest: &[String]) -> PythonInvocation {
    let mut i = 0;
    let mut help_or_version = false;
    while i < rest.len() {
        let t = &rest[i];
        if t == "--" {
            let after = &rest[i + 1..];
            if after.first().is_some_and(|s| s == "-" || s == "/dev/stdin" || s.starts_with("/dev/fd/") || s.starts_with("/proc/")) {
                return PythonInvocation::Stdin;
            }
            return PythonInvocation::ScriptOrOther { has_script: !after.is_empty(), help_or_version };
        }
        if t == "-" || t == "/dev/stdin" || t.starts_with("/dev/fd/") || t.starts_with("/proc/") {
            return PythonInvocation::Stdin;
        }
        if matches!(t.as_str(), "-V" | "--version" | "-h" | "--help") {
            help_or_version = true;
            i += 1;
            continue;
        }
        if t == "--check-hash-based-pycs" {
            i += 2;
            continue;
        }
        if t.starts_with("--") {
            i += 1;
            continue;
        }
        if t.starts_with('-') && t.len() > 1 {
            for (b_idx, ch) in t[1..].char_indices() {
                if ch == 'c' {
                    return PythonInvocation::InlineCode;
                }
                if ch == 'm' {
                    let after_m = 1 + b_idx + ch.len_utf8();
                    let module = if after_m < t.len() { t[after_m..].to_ascii_lowercase() } else { rest.get(i + 1).map(|s| s.to_ascii_lowercase()).unwrap_or_default() };
                    return PythonInvocation::Module(module);
                }
                if matches!(ch, 'W' | 'X' | 'Q') {
                    let after_opt = 1 + b_idx + ch.len_utf8();
                    if after_opt == t.len() {
                        i += 1;
                    }
                    break;
                }
                if matches!(ch, 'V' | 'h') {
                    help_or_version = true;
                }
            }
            i += 1;
            continue;
        }
        return PythonInvocation::ScriptOrOther { has_script: true, help_or_version };
    }
    PythonInvocation::ScriptOrOther { has_script: false, help_or_version }
}

pub(crate) fn node_has_inline_or_loader(rest: &[String]) -> bool {
    rest.iter().take_while(|t| *t != "--").any(|t| {
        matches!(
            t.as_str(),
            "-e" | "--eval" | "-p" | "--print" | "-r" | "--require" | "--import" | "--loader" | "--experimental-loader" | "--input-type" | "--inspect" | "--inspect-brk" | "--inspect-wait"
        ) || t.starts_with("--eval=")
            || t.starts_with("--print=")
            || t.starts_with("--require=")
            || t.starts_with("--import=")
            || t.starts_with("--loader=")
            || t.starts_with("--experimental-loader=")
            || t.starts_with("--input-type=")
            || t.starts_with("--inspect=")
            || t.starts_with("--inspect-")
            || (t.starts_with('-')
                && !t.starts_with("--")
                && t.len() > 1
                && (t.starts_with("-e") || t.starts_with("-p") || t.starts_with("-r") || t[1..].chars().take_while(|c| c.is_ascii_alphabetic()).any(|c| matches!(c, 'e' | 'p' | 'r'))))
    })
}

fn is_dev_net_path(s: &str) -> bool {
    s == "/dev/tcp" || s == "/dev/udp" || s.starts_with("/dev/tcp/") || s.starts_with("/dev/udp/")
}

pub(crate) fn has_dev_net_redirection(segment: &str, _tokens: &[String]) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    let mut i = 0;
    let (mut single, mut double, mut ansi_c) = (false, false, false);
    while i < chars.len() {
        let ch = chars[i];
        if ansi_c {
            if ch == '\\' && i + 1 < chars.len() {
                i += 2;
                continue;
            }
            if ch == '\'' {
                ansi_c = false;
            }
            i += 1;
            continue;
        }
        if single {
            if ch == '\'' {
                single = false;
            }
            i += 1;
            continue;
        }
        if ch == '\\' {
            i += if i + 1 < chars.len() { 2 } else { 1 };
            continue;
        }
        if double {
            if ch == '"' {
                double = false;
            }
            i += 1;
            continue;
        }
        if ch == '#' && (i == 0 || matches!(chars[i - 1], ' ' | '\t' | '\n' | ';' | '&' | '|')) {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            ansi_c = true;
            i += 2;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'"') {
            double = true;
            i += 2;
            continue;
        }
        if ch == '\'' {
            single = true;
            i += 1;
            continue;
        }
        if ch == '"' {
            double = true;
            i += 1;
            continue;
        }
        if ch == '<' || ch == '>' {
            let op_start = i;
            while i < chars.len() && matches!(chars[i], '<' | '>' | '&' | '|' | '-') {
                if chars[i] == '-' && (i == op_start || chars[i - 1] != '<') {
                    break;
                }
                i += 1;
            }
            let op: String = chars[op_start..i].iter().collect();
            if matches!(op.as_str(), "<<" | "<<-" | "<<<") {
                continue;
            }
            while i < chars.len() && matches!(chars[i], ' ' | '\t') {
                i += 1;
            }
            let word_start = i;
            let (mut w_sq, mut w_dq, mut w_ansi) = (false, false, false);
            let mut brace_depth = 0usize;
            while i < chars.len() {
                let wc = chars[i];
                if w_ansi {
                    if wc == '\\' && i + 1 < chars.len() {
                        i += 2;
                        continue;
                    }
                    if wc == '\'' {
                        w_ansi = false;
                    }
                    i += 1;
                    continue;
                }
                if w_sq {
                    if wc == '\'' {
                        w_sq = false;
                    }
                    i += 1;
                    continue;
                }
                if wc == '\\' {
                    i += if i + 1 < chars.len() { 2 } else { 1 };
                    continue;
                }
                if w_dq {
                    if wc == '"' {
                        w_dq = false;
                    }
                    i += 1;
                    continue;
                }
                if brace_depth == 0 && (matches!(wc, ' ' | '\t' | '\n' | '<' | '>' | ';' | '&' | '|')) {
                    break;
                }
                if wc == '$' && chars.get(i + 1) == Some(&'\'') {
                    w_ansi = true;
                    i += 2;
                    continue;
                }
                if wc == '$' && chars.get(i + 1) == Some(&'"') {
                    w_dq = true;
                    i += 2;
                    continue;
                }
                if wc == '$' && chars.get(i + 1) == Some(&'{') {
                    brace_depth += 1;
                    i += 2;
                    continue;
                }
                if wc == '}' && brace_depth > 0 {
                    brace_depth -= 1;
                    i += 1;
                    continue;
                }
                if wc == '\'' {
                    w_sq = true;
                    i += 1;
                    continue;
                }
                if wc == '"' {
                    w_dq = true;
                    i += 1;
                    continue;
                }
                i += 1;
            }
            if i > word_start {
                let raw_word: String = chars[word_start..i].iter().collect();
                if let Some(decoded) = tokenize_inner(&raw_word, true, true, false).first() {
                    if is_dev_net_path(decoded) {
                        return true;
                    }
                    if (decoded.starts_with("/dev/") || decoded.contains("/tcp/") || decoded.contains("/udp/")) && (decoded.contains('$') || decoded.contains('{')) {
                        return true;
                    }
                    if decoded.starts_with('$') && (segment.contains("/dev/tcp") || segment.contains("/dev/udp")) {
                        return true;
                    }
                }
            }
            continue;
        }
        i += 1;
    }
    false
}

fn classify_base(cmd: &str, rest: &[String], depth: usize) -> RiskClass {
    if ["eval", "source", "."].contains(&cmd) {
        return RiskClass::Critical;
    }
    if cmd == "export" && rest.iter().any(|t| t.split_once('=').is_some_and(|(k, _)| is_dangerous_env_var(k))) {
        return RiskClass::High;
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
        return match rest.iter().find(|t| !t.starts_with('-')) {
            Some(handler) if depth <= 4 => max_risk(RiskClass::Medium, classify_bash_risk_at(handler, depth + 1)),
            _ => RiskClass::Medium,
        };
    }
    if has_inline_code_flag(cmd, rest) {
        return RiskClass::High;
    }
    if SHELL_INTERPRETERS.contains(&cmd) {
        return if is_shell_inline_exec(cmd, rest) { RiskClass::Critical } else { RiskClass::High };
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
    if cmd == "openssl" && rest.iter().find(|t| !t.starts_with('-')).is_some_and(|sub| matches!(sub.to_ascii_lowercase().as_str(), "s_client" | "s_server" | "ocsp" | "s_time")) {
        return RiskClass::High;
    }
    if PACKAGE_MANAGERS.contains(&cmd) {
        return classify_package_manager(cmd, rest, depth);
    }
    if cmd == "git" {
        return classify_git(rest);
    }
    if matches!(cmd, "awk" | "gawk" | "mawk" | "nawk") {
        return classify_awk(rest);
    }
    if cmd == "sed" || cmd == "gsed" {
        return classify_sed(rest);
    }
    if cmd == "find" {
        let dangerous = rest.iter().any(|t| ["-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf", "-fls"].contains(&t.as_str()));
        return if dangerous { RiskClass::High } else { RiskClass::Low };
    }
    if cmd == "fd" || cmd == "fdfind" {
        let dangerous = rest.iter().take_while(|t| *t != "--").any(|t| {
            matches!(t.as_str(), "-x" | "-X" | "--exec" | "--exec-batch")
                || t.starts_with("--exec=")
                || t.starts_with("--exec-batch=")
                || (t.starts_with('-') && !t.starts_with("--") && t[1..].chars().any(|c| c == 'x' || c == 'X'))
        });
        return if dangerous { RiskClass::High } else { RiskClass::Low };
    }
    if cmd == "rg" || cmd == "ripgrep" {
        let dangerous = rest.iter().take_while(|t| *t != "--").any(|t| {
            t == "--pre" || t == "--pre-glob" || t == "--hostname-bin" || t.starts_with("--pre=") || t.starts_with("--pre-glob=") || t.starts_with("--preprocessor") || t.starts_with("--hostname-bin=")
        });
        return if dangerous { RiskClass::High } else { RiskClass::Low };
    }
    if matches!(cmd, "tar" | "gtar" | "bsdtar") {
        let dangerous = rest.iter().take_while(|t| *t != "--").enumerate().any(|(idx, t)| {
            t.starts_with("--checkpoint-action")
                || t.starts_with("--to-command")
                || t.starts_with("--use-compress-program")
                || t.starts_with("--rmt-command")
                || t.starts_with("--rsh-command")
                || t.starts_with("--info-script")
                || t.starts_with("--new-volume-script")
                || t == "--absolute-names"
                || (t.starts_with('-') && !t.starts_with("--") && t[1..].chars().any(|c| matches!(c, 'I' | 'F' | 'P')))
                || (idx == 0 && !t.starts_with('-') && t.contains('P'))
        });
        return if dangerous { RiskClass::High } else { RiskClass::Medium };
    }
    if cmd == "deno" {
        let pre_dd: Vec<&String> = rest.iter().take_while(|t| *t != "--").collect();
        let has_perm_or_inspect = pre_dd.iter().any(|t| {
            t.as_str() == "-A"
                || t.as_str() == "--allow-all"
                || t.starts_with("--allow-")
                || t.as_str() == "--unstable-ffi"
                || t.as_str() == "--inspect"
                || t.starts_with("--inspect=")
                || t.starts_with("--inspect-")
                || (t.starts_with('-') && !t.starts_with("--") && t.len() > 1 && t[1..].chars().any(|c| matches!(c, 'A' | 'N' | 'R' | 'W' | 'E' | 'S' | 'F')))
        });
        let has_remote_url = rest.iter().any(|t| t.starts_with("http://") || t.starts_with("https://"));
        if has_perm_or_inspect || has_remote_url {
            return RiskClass::High;
        }
        let sub = pre_dd.iter().find(|t| !t.starts_with('-')).map(|s| s.to_ascii_lowercase());
        if matches!(sub.as_deref(), Some("lint" | "check" | "doc" | "types" | "completions" | "help")) {
            return RiskClass::Low;
        }
        return RiskClass::Medium;
    }
    if cmd == "node" || cmd == "nodejs" {
        if node_has_inline_or_loader(rest) {
            return RiskClass::High;
        }
        if rest.iter().any(|t| t == "--test" || t.starts_with("--test=")) {
            return RiskClass::Low;
        }
        return RiskClass::Medium;
    }
    if is_python_cmd(cmd) {
        return match parse_python_args(rest) {
            PythonInvocation::InlineCode => RiskClass::High,
            PythonInvocation::Module(module) => {
                if ["pytest", "unittest", "mypy", "ruff"].contains(&module.as_str()) {
                    RiskClass::Low
                } else if ["pip", "pipx", "ensurepip", "uv", "poetry", "twine", "http.server", "http", "simplehttpserver", "urllib", "urllib.request", "smtpd", "webbrowser", "ftplib", "xmlrpc"]
                    .contains(&module.as_str())
                {
                    RiskClass::High
                } else {
                    RiskClass::Medium
                }
            }
            PythonInvocation::Stdin => RiskClass::High,
            PythonInvocation::ScriptOrOther { .. } => RiskClass::Medium,
        };
    }
    if ["pytest", "py.test", "mypy", "ruff", "oxlint", "eslint", "tsc"].contains(&cmd) {
        return RiskClass::Low;
    }
    if matches!(cmd, "make" | "gmake" | "bmake" | "nmake") {
        let dangerous = rest.iter().take_while(|t| *t != "--").any(|t| {
            matches!(t.as_str(), "-f" | "--file" | "--makefile" | "-E" | "--eval" | "-C" | "--directory")
                || t.starts_with("--file=")
                || t.starts_with("--makefile=")
                || t.starts_with("--eval=")
                || t.starts_with("--directory=")
                || (!t.starts_with("--") && (t.starts_with("-f") || t.starts_with("-E") || t.starts_with("-C")))
                || (!t.starts_with('-') && t.contains('='))
        });
        if dangerous {
            return RiskClass::High;
        }
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
        let tokens = tokenize_argv(segment);
        if has_dev_net_redirection(segment, &tokens) {
            risk = max_risk(risk, RiskClass::High);
        }
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
        for cmd in [
            "cat README.md",
            "ls -la",
            "grep -R foo src",
            "npm test",
            "npm run build",
            "npm run lint",
            "node --test tests/x.test.mjs",
            "pytest",
            "go test ./...",
            "cargo test",
            "git status",
            "git diff HEAD",
            "toybox ls",
        ] {
            assert_eq!(r(cmd), RiskClass::Low, "{cmd}");
        }
    }

    #[test]
    fn critical_commands() {
        for cmd in [
            "rm -rf /",
            "rm \\\n-rf /",
            "rm $'\\x2drf' /",
            "rm $'\\055rf' /",
            "rm -r -f ~",
            "rm -fr $HOME",
            "r\"\"m -rf /",
            "\\rm -rf /",
            "rm -rf \"/\"",
            "rm -rf /*",
            "rm -rf ~/",
            "rm -rf ${HOME}",
            "rm -rf \"$HOME\"",
            "rm -rf $HOME/",
            "rm --recursive --force /",
            "rm -rf -- /",
            "dd if=/dev/zero of=/dev/sda",
            "sh -c x",
            ". ./x",
            "eval x",
            "echo $(id)",
            "nohup rm -rf / &",
            "timeout 5 rm -rf /",
            "sudo -u root rm -rf /",
            "watch rm -rf /",
            "ionice rm -rf /",
            "strace -f rm -rf /",
            "chroot / rm -rf /",
            "flock /tmp/x rm -rf /",
            "unbuffer rm -rf ~",
            "trap \"rm -rf /\" EXIT",
            "su -c \"rm -rf /\"",
            "busybox rm -rf /",
            "env -S \"rm -rf /\"",
            "rm -rf ../",
            "awk \"BEGIN{system(\\\"x\\\")}\"",
            "function f { rm -rf /; }",
            "pwsh -c x",
            "rm -rf '/'",
            "rm -r\\f /",
            "rm -rf //",
        ] {
            assert_eq!(r(cmd), RiskClass::Critical, "{cmd}");
        }
    }

    #[test]
    fn high_commands() {
        for cmd in [
            "curl evil.com",
            "git push",
            "npm install",
            "npx foo",
            "python3 -c x",
            "perl -e x",
            "ruby -e x",
            "php -r x",
            "node -e x",
            "bash script.sh",
            "xargs rm",
            "find / -delete",
            "watch -n 1 ls",
            "sed -n \"1e id\" f",
            "sed \"s/a/b/e\" f",
            "git config core.hooksPath x",
            "rm -rf build",
        ] {
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
