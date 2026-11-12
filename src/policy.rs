//! Execution policy: network default-deny and shared process limits.
//!
//! The policy is a *gate*, not an OS sandbox. Network classification is conservative and
//! fail-closed: `deny` blocks anything that looks like it can open a socket; `allowlist` allows only
//! exact host matches and denies when a host cannot be determined.

use crate::config::Config;
use crate::mcp::McpServerConfig;
use crate::process::{DEFAULT_COMMAND_TIMEOUT_MS, DEFAULT_HARD_OUTPUT_LIMIT_BYTES, DEFAULT_OUTPUT_LIMIT_BYTES, MAX_COMMAND_TIMEOUT_MS};
use crate::risk::{base_name, is_generic_runner, peel_wrappers, scan_shell, tokenize};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const DEFAULT_MCP_MAX_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionLimits {
    pub default_tool_timeout_ms: u64,
    pub max_tool_timeout_ms: u64,
    pub max_output_bytes: usize,
    pub hard_output_bytes: usize,
    pub mcp_max_output_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkMode {
    Deny,
    Allow,
    Allowlist,
}

impl NetworkMode {
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkMode::Deny => "deny",
            NetworkMode::Allow => "allow",
            NetworkMode::Allowlist => "allowlist",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExecutionPolicy {
    pub mode: NetworkMode,
    pub hosts: BTreeSet<String>,
    pub limits: ExecutionLimits,
    allow_all: bool,
    unrestricted: Arc<AtomicBool>,
}

impl PartialEq for ExecutionPolicy {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode && self.hosts == other.hosts && self.limits == other.limits && self.allow_all == other.allow_all && self.is_unrestricted() == other.is_unrestricted()
    }
}

impl ExecutionPolicy {
    pub fn allow_net_for_server(&self, server: Option<&McpServerConfig>) -> bool {
        self.allow_all || self.is_unrestricted() || server.map(|s| s.allow_net).unwrap_or(false)
    }

    pub fn set_unrestricted(&self, value: bool) {
        self.unrestricted.store(value, Ordering::Relaxed);
    }

    pub fn is_unrestricted(&self) -> bool {
        self.unrestricted.load(Ordering::Relaxed)
    }

    pub fn unrestricted_handle(&self) -> Arc<AtomicBool> {
        self.unrestricted.clone()
    }

    pub fn effective_mode(&self) -> NetworkMode {
        if self.is_unrestricted() {
            NetworkMode::Allow
        } else {
            self.mode
        }
    }

    pub fn with_mode(mode: NetworkMode, hosts: &[&str]) -> Self {
        ExecutionPolicy { mode, hosts: hosts.iter().map(|h| h.to_string()).collect(), limits: default_limits(), allow_all: mode == NetworkMode::Allow, unrestricted: Arc::new(AtomicBool::new(false)) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NetworkAssessment {
    pub network: bool,
    pub hosts: Vec<String>,
    pub unknown: bool,
}

pub const NO_NETWORK: NetworkAssessment = NetworkAssessment { network: false, hosts: Vec::new(), unknown: false };

fn unknown_network() -> NetworkAssessment {
    NetworkAssessment { network: true, hosts: vec![], unknown: true }
}

const NETWORK_CLIENTS: &[&str] = &["curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat", "netcat", "ftp", "telnet", "socat", "tnftp", "lftp"];
const OPAQUE_NETWORK_COMMANDS: &[&str] = &[
    "fetch",
    "aria2c",
    "axel",
    "http",
    "https",
    "httpie",
    "xh",
    "xhs",
    "curlie",
    "websocat",
    "wscat",
    "grpcurl",
    "ab",
    "hey",
    "wrk",
    "ssh-keyscan",
    "rclone",
    "s3cmd",
    "ngrok",
    "cloudflared",
    "tailscale",
    "gh",
    "glab",
    "aws",
    "gcloud",
    "gsutil",
    "az",
    "kubectl",
    "helm",
    "docker",
    "podman",
    "nerdctl",
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
const PACKAGE_MANAGERS: &[&str] = &["npm", "pnpm", "yarn", "bun", "pip", "pip3", "uv", "poetry", "cargo", "go", "gem", "composer", "apt", "apt-get", "pkg", "brew", "dnf", "pacman", "apk", "yum"];
const PACKAGE_NETWORK_SUBCOMMANDS: &[&str] = &[
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
const NETWORK_PACKAGE_RUNNERS: &[&str] = &["npx", "pnpx", "bunx"];
const NETWORK_GIT_SUBCOMMANDS: &[&str] = &["push", "pull", "fetch", "clone", "ls-remote", "submodule", "request-pull", "archive"];
const CURL_VALUE_FLAGS: &[&str] = &[
    "-o",
    "--output",
    "-H",
    "--header",
    "-d",
    "--data",
    "--data-raw",
    "--data-binary",
    "--data-urlencode",
    "-X",
    "--request",
    "-u",
    "--user",
    "-A",
    "--user-agent",
    "-e",
    "--referer",
    "-b",
    "--cookie",
    "-c",
    "--cookie-jar",
    "-F",
    "--form",
    "-T",
    "--upload-file",
    "-x",
    "--proxy",
    "-m",
    "--max-time",
    "--connect-timeout",
    "-r",
    "--range",
    "-w",
    "--write-out",
    "-K",
    "--config",
    "-E",
    "--cert",
    "--key",
    "--cacert",
    "--capath",
    "--resolve",
    "--connect-to",
    "--retry",
    "--retry-delay",
    "--retry-max-time",
    "--url",
    "--unix-socket",
    "--interface",
    "--limit-rate",
    "--max-filesize",
    "--speed-limit",
    "--speed-time",
    "--max-redirs",
    "--local-port",
    "--dns-servers",
    "--expect100-timeout",
    "--happy-eyeballs-timeout-ms",
    "--keepalive-time",
];
const WGET_VALUE_FLAGS: &[&str] = &[
    "-O",
    "--output-document",
    "-o",
    "--output-file",
    "-P",
    "--directory-prefix",
    "-U",
    "--user-agent",
    "-T",
    "--timeout",
    "-t",
    "--tries",
    "-w",
    "--wait",
    "-e",
    "--execute",
    "-a",
    "--append-output",
    "-i",
    "--input-file",
    "-B",
    "--base",
    "-D",
    "--domains",
    "-H",
    "--header",
    "-Y",
    "--proxy",
    "-Q",
    "--quota",
    "-l",
    "--level",
    "-A",
    "--accept",
    "-R",
    "--reject",
    "--post-data",
    "--post-file",
    "--method",
    "--body-data",
    "--body-file",
    "--config",
];
const SSH_VALUE_FLAGS: &[&str] = &["-p", "-i", "-l", "-o", "-F", "-J", "-L", "-R", "-D", "-W", "-b", "-c", "-E", "-I", "-m", "-O", "-Q", "-S", "-w", "-e", "-B"];
const COPY_VALUE_FLAGS: &[&str] = &["-e", "--rsh", "-i", "--identity-file", "-P", "-p", "-o", "-F", "-S", "-l"];
const NETCAT_VALUE_FLAGS: &[&str] = &["-p", "-s", "-w", "-i", "-x", "-X", "-c", "-q", "-T", "-O", "-m", "-I", "-e"];
const FTP_VALUE_FLAGS: &[&str] = &["-p", "-P", "-u", "-w", "-o", "-N", "-t", "-r", "-g"];
const GIT_VALUE_FLAGS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--super-prefix"];
const GIT_SUB_VALUE_FLAGS: &[&str] = &[
    "-c",
    "--config",
    "-u",
    "--upload-pack",
    "--receive-pack",
    "--depth",
    "--branch",
    "-b",
    "--origin",
    "-o",
    "--reference",
    "--separate-git-dir",
    "--jobs",
    "-j",
    "--filter",
    "--shallow-since",
    "--shallow-exclude",
    "--repo",
    "--server-option",
];
const PACKAGE_VALUE_FLAGS: &[&str] = &[
    "--prefix",
    "--registry",
    "--cache",
    "--userconfig",
    "--globalconfig",
    "--cwd",
    "--dir",
    "--workspace",
    "-w",
    "--filter",
    "--loglevel",
    "--target",
    "--index-url",
    "-i",
    "--extra-index-url",
    "--python",
    "--root",
    "--manifest-path",
    "--features",
    "-F",
    "--package",
    "-p",
    "--toolchain",
    "--config",
    "--repository",
    "--source",
    "--build-dir",
];
const CURL_REDIRECT_FLAGS: &[&str] = &[
    "-x",
    "--proxy",
    "--proxy1.0",
    "--preproxy",
    "--socks4",
    "--socks4a",
    "--socks5",
    "--socks5-hostname",
    "--resolve",
    "--connect-to",
    "--unix-socket",
    "--abstract-unix-socket",
    "--doh-url",
    "--ipfs-gateway",
    "-K",
    "--config",
];
const CURL_SHORT_VALUE_CHARS: &[char] = &['o', 'H', 'd', 'X', 'u', 'A', 'e', 'b', 'c', 'F', 'T', 'm', 'r', 'w', 'E', 'C', 'D', 'P', 'Q', 'Y', 'y', 'z'];
const WGET_REDIRECT_FLAGS: &[&str] = &["-e", "--execute", "-i", "--input-file", "-B", "--base", "--config"];
const WGET_SHORT_VALUE_CHARS: &[char] = &['O', 'P', 'o', 'U', 'T', 'w', 't', 'Q', 'a', 'D', 'H', 'Y', 'l', 'A', 'R'];
const SSH_REDIRECT_FLAGS: &[&str] = &["-o", "-J", "-L", "-R", "-D", "-W", "-F", "-w"];
const SSH_SHORT_VALUE_CHARS: &[char] = &['p', 'i', 'l', 'b', 'c', 'E', 'e', 'I', 'm', 'O', 'Q', 'S', 'B'];
const COPY_REDIRECT_FLAGS: &[&str] = &["-o", "-F", "-J", "-S"];
const COPY_SHORT_VALUE_CHARS: &[char] = &['P', 'i', 'c', 'l', 'b', 'D', 'R', 's'];
const RSYNC_REDIRECT_FLAGS: &[&str] = &["-e", "--rsh", "--rsync-path"];
const NETCAT_REDIRECT_FLAGS: &[&str] = &["-x", "--proxy", "-e", "--exec", "-c", "--sh-exec"];
const NETCAT_SHORT_VALUE_CHARS: &[char] = &['p', 's', 'w', 'i', 'q', 'X', 'T', 'O', 'm', 'I'];

fn is_flag(token: &str) -> bool {
    token.starts_with('-') && token != "-"
}

fn has_flag(tokens: &[String], flags: &[&str]) -> bool {
    tokens.iter().take_while(|t| *t != "--").any(|token| {
        if !is_flag(token) {
            return false;
        }
        let name = match token.find('=') {
            Some(eq) if eq > 0 => &token[..eq],
            _ => token.as_str(),
        };
        flags.contains(&name)
    })
}

fn has_short_flag_in_cluster(tokens: &[String], redirect_chars: &[char], value_chars: &[char]) -> bool {
    for token in tokens {
        if token == "--" {
            break;
        }
        if !token.starts_with('-') || token.starts_with("--") || token.len() <= 1 {
            continue;
        }
        for ch in token[1..].chars() {
            if redirect_chars.contains(&ch) {
                return true;
            }
            if value_chars.contains(&ch) {
                break;
            }
        }
    }
    false
}

fn git_has_redirect(rest: &[String]) -> bool {
    for token in rest {
        if !is_flag(token) {
            continue;
        }
        if token == "-c" || token.starts_with("-c") {
            return true;
        }
        let lower = token.to_lowercase();
        if lower.starts_with("--config") || lower.starts_with("--exec-path") || lower.starts_with("--upload-pack") || lower.starts_with("--receive-pack") {
            return true;
        }
    }
    false
}

/// Lowercase a host and strip a port; IPv6 brackets stay; wildcards are never accepted.
pub fn normalize_network_host(raw: &str) -> Option<String> {
    let mut host = raw.trim().to_lowercase();
    if host.is_empty() {
        return None;
    }
    if host.contains("://") {
        host = url::Url::parse(&host).ok()?.host_str()?.to_string();
    }
    if host.starts_with('[') {
        if let Some(end) = host.find(']') {
            if end > 0 {
                host = host[..=end].to_string();
            }
        }
    } else if host.matches(':').count() >= 2 {
        host = format!("[{host}]");
    } else if let Some(colon) = host.rfind(':') {
        if colon > 0 && !host[colon + 1..].is_empty() && host[colon + 1..].chars().all(|c| c.is_ascii_digit()) {
            host.truncate(colon);
        }
    }
    let host = host.trim_end_matches('.').to_string();
    if host.is_empty() || host.contains('*') {
        return None;
    }
    Some(host)
}

/// Parse `--allow-net-hosts` / config `allowNetHosts` values (comma lists, arrays, URLs).
pub fn parse_network_host_list<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut hosts: Vec<String> = vec![];
    for value in values {
        for part in value.split(',') {
            if let Some(host) = normalize_network_host(part) {
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
        }
    }
    hosts
}

pub fn default_limits() -> ExecutionLimits {
    ExecutionLimits {
        default_tool_timeout_ms: DEFAULT_COMMAND_TIMEOUT_MS,
        max_tool_timeout_ms: MAX_COMMAND_TIMEOUT_MS,
        max_output_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
        hard_output_bytes: DEFAULT_HARD_OUTPUT_LIMIT_BYTES,
        mcp_max_output_bytes: DEFAULT_MCP_MAX_OUTPUT_BYTES,
    }
}

pub fn resolve_execution_policy(config: &Config, flag_allow_net: bool, flag_hosts: &[String]) -> ExecutionPolicy {
    let flag_hosts = parse_network_host_list(flag_hosts.iter().map(String::as_str));
    let config_hosts = parse_network_host_list(config.allow_net_hosts.iter().map(String::as_str));
    let hosts: BTreeSet<String> = config_hosts.into_iter().chain(flag_hosts).collect();
    let allow_all = flag_allow_net || config.allow_net;
    let mode = if allow_all {
        NetworkMode::Allow
    } else if !hosts.is_empty() {
        NetworkMode::Allowlist
    } else {
        NetworkMode::Deny
    };
    ExecutionPolicy { mode, hosts, limits: default_limits(), allow_all, unrestricted: Arc::new(AtomicBool::new(false)) }
}

fn positionals(tokens: &[String], value_flags: &[&str]) -> Vec<String> {
    let mut out = vec![];
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "--" {
            out.extend(tokens[i + 1..].iter().cloned());
            break;
        }
        if !is_flag(token) {
            out.push(token.clone());
            i += 1;
            continue;
        }
        if value_flags.contains(&token.as_str()) && i + 1 < tokens.len() {
            i += 1;
        }
        i += 1;
    }
    out
}

fn flag_values(tokens: &[String], names: &[&str]) -> Vec<String> {
    let mut out = vec![];
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "--" {
            break;
        }
        if let Some(eq) = t.find('=') {
            if eq > 0 && names.contains(&&t[..eq]) {
                out.push(t[eq + 1..].to_string());
            }
        } else if names.contains(&t.as_str()) {
            out.push(tokens.get(i + 1).cloned().unwrap_or_default());
            i += 1;
        }
        i += 1;
    }
    out
}

fn trim_path(value: &str) -> &str {
    let cut = value.find(['/', '\\']);
    match cut {
        Some(c) if c > 0 => &value[..c],
        _ => value,
    }
}

/// Extract a host from a URL, scp/rsync remote (`user@host:path`), or a bare host[:port].
fn host_from_token(token: &str, allow_bare_host: bool) -> Option<String> {
    let value = token.trim();
    if value.is_empty() || is_flag(value) {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        return None;
    }
    if value.contains("://") {
        return url::Url::parse(value).ok().and_then(|u| u.host_str().map(str::to_string)).and_then(|h| normalize_network_host(&h));
    }
    let at = value.rfind('@');
    let remote = match at {
        Some(i) => &value[i + 1..],
        None => value,
    };
    let mut host = None;
    if remote.starts_with('[') {
        if let Some(end) = remote.find(']') {
            if end > 0 {
                host = normalize_network_host(&remote[..=end]);
            }
        }
    } else if let Some(colon) = remote.find(':') {
        if colon > 0 {
            host = normalize_network_host(&remote[..colon]);
        }
    }
    if let Some(h) = host {
        if h.chars().count() > 1 {
            return Some(h);
        }
    }
    if !allow_bare_host {
        return None;
    }
    normalize_network_host(trim_path(if at.is_some() { remote } else { value }))
}

fn add_host(hosts: &mut Vec<String>, host: Option<String>) {
    if let Some(h) = host {
        if !hosts.contains(&h) {
            hosts.push(h);
        }
    }
}

fn git_subcommand(rest: &[String]) -> (Option<String>, Vec<String>) {
    let mut index = 0;
    while index < rest.len() {
        let token = &rest[index];
        if token == "--" {
            return (rest.get(index + 1).map(|s| s.to_lowercase()), rest.iter().skip(index + 2).cloned().collect());
        }
        if is_flag(token) {
            if GIT_VALUE_FLAGS.contains(&token.as_str()) && !token.contains('=') {
                index += 1;
            }
            index += 1;
            continue;
        }
        return (Some(token.to_lowercase()), rest[index + 1..].to_vec());
    }
    (None, vec![])
}

fn package_subcommand(rest: &[String]) -> Option<String> {
    let mut i = 0;
    while i < rest.len() {
        let token = &rest[i];
        if token == "--" {
            return rest.get(i + 1).map(|s| s.to_lowercase());
        }
        if is_flag(token) {
            if PACKAGE_VALUE_FLAGS.contains(&token.as_str()) && i + 1 < rest.len() {
                i += 1;
            }
            i += 1;
            continue;
        }
        return Some(token.to_lowercase());
    }
    None
}

const SAFE_PYTHON_MODULES: &[&str] = &[
    "pytest",
    "unittest",
    "mypy",
    "ruff",
    "black",
    "flake8",
    "isort",
    "pylint",
    "pyflakes",
    "coverage",
    "doctest",
    "py_compile",
    "compileall",
    "json.tool",
    "ast",
    "dis",
    "timeit",
    "tokenize",
    "tabnanny",
    "venv",
    "zipfile",
    "tarfile",
    "base64",
    "calendar",
];

fn authority_has_unverified_expansion(token: &str) -> bool {
    let rest = match token.find("://") {
        Some(idx) => &token[idx + 3..],
        None => token,
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    authority.chars().any(|c| matches!(c, '$' | '{' | '}' | ' ' | '\t'))
}

fn assess_scripting_runtime(cmd: &str, rest: &[String]) -> Option<NetworkAssessment> {
    let pre_dd: Vec<&String> = rest.iter().take_while(|t| *t != "--").collect();
    let post_dd_nonempty = rest.iter().skip_while(|t| *t != "--").nth(1).is_some();
    if cmd == "node" || cmd == "nodejs" {
        if crate::risk::node_has_inline_or_loader(rest) || pre_dd.iter().any(|t| t.as_str() == "-") {
            return Some(unknown_network());
        }
        if pre_dd.iter().any(|t| matches!(t.as_str(), "--version" | "-v" | "--help" | "-h")) {
            return Some(NO_NETWORK);
        }
        if pre_dd.iter().any(|t| t.as_str() == "--test" || t.starts_with("--test=") || t.as_str() == "--check" || t.as_str() == "-c") {
            return Some(NO_NETWORK);
        }
        let pos = positionals(rest, &["-C", "--conditions", "--env-file", "--env-file-if-exists", "--watch-path", "--test-name-pattern", "--test-reporter", "--test-reporter-destination"]);
        return Some(if !pos.is_empty() || post_dd_nonempty { NO_NETWORK } else { unknown_network() });
    }
    if cmd == "deno" {
        let Some(sub) = pre_dd.iter().find(|t| !is_flag(t)).map(|s| s.to_ascii_lowercase()) else {
            return Some(if pre_dd.iter().any(|t| matches!(t.as_str(), "-V" | "--version" | "-h" | "--help")) { NO_NETWORK } else { unknown_network() });
        };
        if matches!(sub.as_str(), "fmt" | "lint" | "check" | "doc" | "types" | "completions" | "help" | "--version" | "-V") {
            return Some(NO_NETWORK);
        }
        return Some(unknown_network());
    }
    if cmd == "bun" {
        if pre_dd.iter().any(|t| t.as_str() == "-e" || t.as_str() == "--eval" || t.starts_with("--eval=") || (!t.starts_with("--") && t.starts_with("-e")) || t.as_str() == "-") {
            return Some(unknown_network());
        }
        if pre_dd.iter().any(|t| matches!(t.as_str(), "-v" | "--version" | "-h" | "--help")) {
            return Some(NO_NETWORK);
        }
        if let Some(first) = pre_dd.iter().find(|t| !is_flag(t)).map(|s| s.to_ascii_lowercase()) {
            if matches!(first.as_str(), "test" | "build" | "pm") {
                return Some(NO_NETWORK);
            }
            if matches!(first.as_str(), "x" | "install" | "i" | "add" | "update" | "upgrade" | "outdated" | "publish" | "init" | "create" | "repl") {
                return Some(unknown_network());
            }
            if first == "run" {
                let run_idx = rest.iter().position(|t| t.eq_ignore_ascii_case("run")).unwrap_or(0);
                let target = rest[run_idx + 1..].iter().find(|t| !is_flag(t));
                return Some(if target.is_some() { NO_NETWORK } else { unknown_network() });
            }
            return Some(NO_NETWORK);
        }
        return Some(unknown_network());
    }
    if crate::risk::is_python_cmd(cmd) {
        let mut i = 0;
        let mut has_script = post_dd_nonempty;
        while i < pre_dd.len() {
            let t = pre_dd[i];
            if t == "-c" || (!t.starts_with("--") && (t.starts_with("-c") || (t.starts_with('-') && t[1..].chars().take_while(|c| c.is_ascii_alphabetic()).any(|c| c == 'c')))) || t == "-" {
                return Some(unknown_network());
            }
            if t == "-m" || (!t.starts_with("--") && t.starts_with("-m") && t.len() > 2) {
                let module = if t == "-m" { pre_dd.get(i + 1).map(|s| s.to_ascii_lowercase()).unwrap_or_default() } else { t[2..].to_ascii_lowercase() };
                return Some(if SAFE_PYTHON_MODULES.contains(&module.as_str()) { NO_NETWORK } else { unknown_network() });
            }
            if t == "-W" || t == "-X" || t == "--check-hash-based-pycs" {
                i += 2;
                continue;
            }
            if !is_flag(t) {
                has_script = true;
            }
            i += 1;
        }
        if pre_dd.iter().any(|t| matches!(t.as_str(), "-V" | "--version" | "-h" | "--help")) {
            return Some(NO_NETWORK);
        }
        return Some(if has_script { NO_NETWORK } else { unknown_network() });
    }
    if matches!(cmd, "perl" | "ruby" | "php" | "lua" | "luajit" | "rscript" | "tclsh" | "osascript") {
        let inline_or_server = match cmd {
            "perl" => pre_dd.iter().any(|t| {
                t.as_str() == "-e"
                    || t.as_str() == "-E"
                    || (!t.starts_with("--")
                        && (t.starts_with("-e") || t.starts_with("-E") || (t.starts_with('-') && t[1..].chars().take_while(|c| c.is_ascii_alphabetic()).any(|c| c == 'e' || c == 'E'))))
            }),
            "ruby" => pre_dd.iter().any(|t| {
                t.as_str() == "-e"
                    || t.as_str() == "-r"
                    || (!t.starts_with("--") && (t.starts_with("-e") || t.starts_with("-r") || (t.starts_with('-') && t[1..].chars().take_while(|c| c.is_ascii_alphabetic()).any(|c| c == 'e'))))
            }),
            "php" => pre_dd.iter().any(|t| matches!(t.as_str(), "-r" | "-B" | "-R" | "-F" | "-E" | "-S" | "--server" | "-a") || (!t.starts_with("--") && (t.starts_with("-r") || t.starts_with("-S")))),
            "lua" | "luajit" => pre_dd.iter().any(|t| matches!(t.as_str(), "-e" | "-l" | "-i") || (!t.starts_with("--") && (t.starts_with("-e") || t.starts_with("-l")))),
            "rscript" | "osascript" => pre_dd.iter().any(|t| t.as_str() == "-e" || (!t.starts_with("--") && t.starts_with("-e"))),
            "tclsh" => pre_dd.iter().any(|t| t.as_str() == "-c" || (!t.starts_with("--") && t.starts_with("-c"))),
            _ => false,
        };
        if inline_or_server || pre_dd.iter().any(|t| t.as_str() == "-") {
            return Some(unknown_network());
        }
        if pre_dd.iter().any(|t| matches!(t.as_str(), "-v" | "-V" | "--version" | "-h" | "--help" | "-c" | "-l")) {
            return Some(NO_NETWORK);
        }
        let has_script = post_dd_nonempty || pre_dd.iter().any(|t| !is_flag(t));
        return Some(if has_script { NO_NETWORK } else { unknown_network() });
    }
    None
}

fn analyze_segment(tokens: &[String], depth: usize) -> NetworkAssessment {
    let (peeled, _) = peel_wrappers(tokens);
    if peeled.is_empty() {
        return NO_NETWORK;
    }
    let cmd = base_name(&peeled[0]);
    let rest: Vec<String> = peeled[1..].to_vec();
    if cmd == "busybox" || cmd == "toybox" {
        if rest.is_empty() {
            return NO_NETWORK;
        }
        if depth >= 5 {
            return unknown_network();
        }
        return analyze_segment(&rest, depth + 1);
    }
    if is_generic_runner(&cmd) {
        if depth >= 5 {
            return unknown_network();
        }
        for i in 0..rest.len() {
            let token = &rest[i];
            let nested = if token.chars().any(char::is_whitespace) { assess_nested(token, depth + 1) } else { NO_NETWORK };
            let suffix = if token.starts_with('-') { NO_NETWORK } else { analyze_segment(&rest[i..], depth + 1) };
            if nested.network || suffix.network {
                return unknown_network();
            }
        }
        return NO_NETWORK;
    }
    if cmd == "eval" {
        if depth >= 4 {
            return unknown_network();
        }
        let joined = rest.join(" ");
        return if assess_nested(&joined, depth + 1).network { unknown_network() } else { NO_NETWORK };
    }
    if cmd == "pwsh" || cmd == "powershell" {
        return unknown_network();
    }
    if ["bash", "sh", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "ash", "cmd"].contains(&cmd.as_str()) {
        if crate::risk::is_shell_inline_exec(&cmd, &rest) {
            if depth >= 4 {
                return unknown_network();
            }
            for token in &rest {
                if !token.starts_with('-') && !(cmd == "cmd" && (token.to_ascii_lowercase().starts_with("/c") || token.to_ascii_lowercase().starts_with("/k"))) {
                    if assess_nested(token, depth + 1).network {
                        return unknown_network();
                    }
                } else if token.starts_with("-c") && !token.starts_with("--") && token.len() > 2 {
                    if assess_nested(&token[2..], depth + 1).network {
                        return unknown_network();
                    }
                } else if cmd == "fish" {
                    if let Some(script) = token.strip_prefix("--command=") {
                        if assess_nested(script, depth + 1).network {
                            return unknown_network();
                        }
                    }
                }
            }
        }
        return NO_NETWORK;
    }
    if OPAQUE_NETWORK_COMMANDS.contains(&cmd.as_str()) {
        return unknown_network();
    }
    if cmd == "openssl" {
        if let Some(sub) = rest.iter().find(|t| !is_flag(t)) {
            if matches!(sub.to_ascii_lowercase().as_str(), "s_client" | "s_server" | "ocsp" | "s_time") {
                return unknown_network();
            }
        }
        return NO_NETWORK;
    }
    if let Some(runtime_assessment) = assess_scripting_runtime(&cmd, &rest) {
        return runtime_assessment;
    }
    if NETWORK_PACKAGE_RUNNERS.contains(&cmd.as_str()) {
        return unknown_network();
    }
    if PACKAGE_MANAGERS.contains(&cmd.as_str()) {
        let Some(sub) = package_subcommand(&rest) else { return NO_NETWORK };
        if !PACKAGE_NETWORK_SUBCOMMANDS.contains(&sub.as_str()) {
            return NO_NETWORK;
        }
        return unknown_network();
    }
    if cmd == "git" {
        let (sub, args) = git_subcommand(&rest);
        let Some(sub) = sub else { return NO_NETWORK };
        if sub == "remote" {
            let pos = positionals(&args, &["-t", "-m"]);
            let rsub = pos.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
            let has_fetch = args.iter().take_while(|t| *t != "--").any(|t| t == "-f" || t == "--fetch");
            let has_no_query = args.iter().take_while(|t| *t != "--").any(|t| t == "-n");
            if rsub == "add" && has_fetch {
                let mut hosts = vec![];
                if let Some(url_tok) = pos.get(2) {
                    add_host(&mut hosts, host_from_token(url_tok, false));
                }
                let unknown = hosts.is_empty() || git_has_redirect(&rest);
                return NetworkAssessment { network: true, hosts, unknown };
            }
            if rsub == "update" || rsub == "prune" || (rsub == "show" && !has_no_query) {
                return unknown_network();
            }
            return NO_NETWORK;
        }
        if !NETWORK_GIT_SUBCOMMANDS.contains(&sub.as_str()) {
            return NO_NETWORK;
        }
        if sub == "submodule" {
            return unknown_network();
        }
        let mut hosts = vec![];
        let pos = positionals(&args, GIT_SUB_VALUE_FLAGS);
        if sub == "archive" {
            for token in flag_values(&args, &["--remote"]) {
                add_host(&mut hosts, host_from_token(&token, false));
            }
        } else if sub == "request-pull" {
            if let Some(url_tok) = pos.get(1) {
                add_host(&mut hosts, host_from_token(url_tok, false));
            }
        } else {
            if let Some(remote_tok) = pos.first() {
                add_host(&mut hosts, host_from_token(remote_tok, false));
            }
            for repo_tok in flag_values(&args, &["--repo"]) {
                add_host(&mut hosts, host_from_token(&repo_tok, false));
            }
        }
        let multi_or_upload = args.iter().take_while(|t| *t != "--").any(|t| {
            t == "--all"
                || t == "--multiple"
                || t.starts_with("--upload-pack")
                || t.starts_with("--receive-pack")
                || ((sub == "clone" || sub == "fetch" || sub == "ls-remote") && !t.starts_with("--") && t.starts_with("-u"))
        });
        let unknown = hosts.is_empty() || git_has_redirect(&rest) || multi_or_upload;
        return NetworkAssessment { network: true, hosts, unknown };
    }
    if NETWORK_CLIENTS.contains(&cmd.as_str()) {
        let mut hosts = vec![];
        if cmd == "curl" || cmd == "wget" {
            let (value_flags, redirect_flags) = if cmd == "curl" { (CURL_VALUE_FLAGS, CURL_REDIRECT_FLAGS) } else { (WGET_VALUE_FLAGS, WGET_REDIRECT_FLAGS) };
            let mut unverified_target = false;
            let extra_urls = if cmd == "curl" { flag_values(&rest, &["--url"]) } else { vec![] };
            for token in positionals(&rest, value_flags).into_iter().chain(extra_urls) {
                if authority_has_unverified_expansion(&token) {
                    unverified_target = true;
                    continue;
                }
                match host_from_token(&token, true) {
                    Some(h) => add_host(&mut hosts, Some(h)),
                    None => unverified_target = true,
                }
            }
            let short_cluster_redirect =
                if cmd == "curl" { has_short_flag_in_cluster(&rest, &['x', 'K'], CURL_SHORT_VALUE_CHARS) } else { has_short_flag_in_cluster(&rest, &['e', 'i', 'B'], WGET_SHORT_VALUE_CHARS) };
            let redirect = has_flag(&rest, redirect_flags) || short_cluster_redirect;
            let unknown = hosts.is_empty() || redirect || unverified_target;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        if cmd == "ssh" {
            let args = positionals(&rest, SSH_VALUE_FLAGS);
            if let Some(target) = args.first() {
                if !authority_has_unverified_expansion(target) {
                    add_host(&mut hosts, host_from_token(target, true));
                }
            }
            let redirect = has_flag(&rest, SSH_REDIRECT_FLAGS) || has_short_flag_in_cluster(&rest, &['o', 'J', 'L', 'R', 'D', 'W', 'F', 'w'], SSH_SHORT_VALUE_CHARS);
            let unknown = hosts.is_empty() || redirect;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        if cmd == "scp" || cmd == "sftp" || cmd == "rsync" {
            let mut unverified_target = false;
            for token in positionals(&rest, COPY_VALUE_FLAGS) {
                if authority_has_unverified_expansion(&token) && (cmd == "sftp" || token.contains(':')) {
                    unverified_target = true;
                    continue;
                }
                add_host(&mut hosts, host_from_token(&token, cmd == "sftp"));
            }
            let redirect = if cmd == "rsync" {
                has_flag(&rest, RSYNC_REDIRECT_FLAGS) || has_short_flag_in_cluster(&rest, &['e'], &[])
            } else {
                has_flag(&rest, COPY_REDIRECT_FLAGS) || has_short_flag_in_cluster(&rest, &['o', 'F', 'J', 'S'], COPY_SHORT_VALUE_CHARS)
            };
            if hosts.is_empty() && !redirect && !unverified_target {
                return NO_NETWORK;
            }
            let unknown = hosts.is_empty() || redirect || unverified_target;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        if cmd == "socat" {
            let mut redirect = false;
            let address_re = regex::Regex::new(r"^(?:TCP|TCP4|TCP6|UDP|UDP4|UDP6|OPENSSL|SSL|SOCKS4|SOCKS4A|SOCKS5)(?:4|5|6)?:([^:]+):").unwrap();
            for token in &rest {
                if is_flag(token) {
                    continue;
                }
                let lower = token.to_lowercase();
                if lower.starts_with("exec:") || lower.starts_with("system:") || lower.starts_with("program:") {
                    redirect = true;
                    continue;
                }
                let upper = token.to_uppercase();
                if let Some(c) = address_re.captures(&upper) {
                    if authority_has_unverified_expansion(&c[1]) {
                        redirect = true;
                    } else {
                        add_host(&mut hosts, normalize_network_host(&c[1]));
                    }
                }
            }
            let unknown = hosts.is_empty() || redirect;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        let netcat = cmd == "nc" || cmd == "ncat" || cmd == "netcat";
        let args = positionals(&rest, if netcat { NETCAT_VALUE_FLAGS } else { FTP_VALUE_FLAGS });
        if let Some(target) = args.first() {
            if !authority_has_unverified_expansion(target) {
                add_host(&mut hosts, host_from_token(target, true));
            }
        }
        let redirect = netcat && (has_flag(&rest, NETCAT_REDIRECT_FLAGS) || has_short_flag_in_cluster(&rest, &['x', 'e', 'c'], NETCAT_SHORT_VALUE_CHARS));
        let unknown = hosts.is_empty() || redirect;
        return NetworkAssessment { network: true, hosts, unknown };
    }
    NO_NETWORK
}

/// Network behaviour of a whole input line; wrappers are peeled and every segment inspected.
pub fn assess_command_network(input: &str) -> NetworkAssessment {
    assess_nested(input, 0)
}

fn assess_nested(input: &str, depth: usize) -> NetworkAssessment {
    if input.trim().is_empty() {
        return NO_NETWORK;
    }
    let scan = scan_shell(input);
    let mut hosts: Vec<String> = vec![];
    let mut network = scan.critical_escape;
    let mut unknown = scan.critical_escape;
    for segment in &scan.segments {
        let tokens = tokenize(segment);
        if crate::risk::has_dev_net_redirection(segment, &tokens) {
            network = true;
            unknown = true;
        }
        if tokens.is_empty() {
            continue;
        }
        let result = analyze_segment(&tokens, depth);
        if !result.network {
            continue;
        }
        network = true;
        unknown |= result.unknown;
        for host in result.hosts {
            add_host(&mut hosts, Some(host));
        }
    }
    if network {
        NetworkAssessment { network, hosts, unknown }
    } else {
        NO_NETWORK
    }
}

pub fn extract_network_host(input: &str) -> Option<String> {
    assess_command_network(input).hosts.into_iter().next()
}

/// True when the execution policy permits this input line. Non-network operations always pass.
pub fn is_network_allowed_for_command(policy: &ExecutionPolicy, input: &str) -> bool {
    if policy.is_unrestricted() {
        return true;
    }
    let assessment = assess_command_network(input);
    if !assessment.network {
        return true;
    }
    match policy.mode {
        NetworkMode::Allow => true,
        NetworkMode::Deny => false,
        NetworkMode::Allowlist => !assessment.unknown && !assessment.hosts.is_empty() && assessment.hosts.iter().all(|h| policy.hosts.contains(h)),
    }
}

/// User-facing denial text. Does not echo the input (it may contain credentials).
pub fn network_denied_message(input: &str) -> String {
    let assessment = assess_command_network(input);
    if assessment.unknown {
        return "Permission denied: network access is disabled and the target host cannot be verified. Use --allow-net to permit all network access.".into();
    }
    if !assessment.hosts.is_empty() {
        return "Permission denied: network access is blocked by the execution policy for this host. Use --allow-net or --allow-net-hosts=HOST to permit it.".into();
    }
    "Permission denied: network access is blocked by the execution policy. Use --allow-net or --allow-net-hosts=HOST to permit it.".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny() -> ExecutionPolicy {
        ExecutionPolicy::with_mode(NetworkMode::Deny, &[])
    }

    #[test]
    fn deny_mode_blocks_network() {
        for cmd in [
            "curl https://evil.test",
            "git push",
            "npm install",
            "npx x",
            "watch curl evil.test",
            "flock /tmp/l curl x",
            "su -c \"wget x\"",
            "trap \"curl x\" EXIT",
            "strace -f git push",
            "busybox wget x",
            "ssh host",
            "scp a host:b",
        ] {
            assert!(!is_network_allowed_for_command(&deny(), cmd), "{cmd}");
        }
        for cmd in ["ls", "npm test", "git status", "watch ls", "scp a b"] {
            assert!(is_network_allowed_for_command(&deny(), cmd), "{cmd}");
        }
    }

    #[test]
    fn allowlist_requires_exact_hosts() {
        let policy = ExecutionPolicy::with_mode(NetworkMode::Allowlist, &["api.example.com"]);
        assert!(is_network_allowed_for_command(&policy, "curl https://api.example.com/v1"));
        assert!(!is_network_allowed_for_command(&policy, "curl https://evil.example.com"));
        assert!(!is_network_allowed_for_command(&policy, "curl -x http://proxy https://api.example.com"));
        assert!(!is_network_allowed_for_command(&policy, "git push origin main"));
    }

    #[test]
    fn host_normalization() {
        assert_eq!(normalize_network_host("API.Example.com:443"), Some("api.example.com".into()));
        assert_eq!(normalize_network_host("x.test."), Some("x.test".into()));
        assert_eq!(normalize_network_host("https://x.test/path"), Some("x.test".into()));
        assert_eq!(normalize_network_host("*.x.test"), None);
        assert_eq!(normalize_network_host("::1"), Some("[::1]".into()));
    }
}
