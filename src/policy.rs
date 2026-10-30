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

#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionPolicy {
    pub mode: NetworkMode,
    pub hosts: BTreeSet<String>,
    pub limits: ExecutionLimits,
    allow_all: bool,
}

impl ExecutionPolicy {
    pub fn allow_net_for_server(&self, server: Option<&McpServerConfig>) -> bool {
        self.allow_all || server.map(|s| s.allow_net).unwrap_or(false)
    }

    pub fn with_mode(mode: NetworkMode, hosts: &[&str]) -> Self {
        ExecutionPolicy { mode, hosts: hosts.iter().map(|h| h.to_string()).collect(), limits: default_limits(), allow_all: mode == NetworkMode::Allow }
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
];
const SSH_VALUE_FLAGS: &[&str] = &["-p", "-i", "-l", "-o", "-F", "-J", "-L", "-R", "-D", "-W", "-b", "-c", "-E", "-I", "-m", "-O", "-Q", "-S", "-w", "-e", "-B"];
const COPY_VALUE_FLAGS: &[&str] = &["-e", "--rsh", "-i", "--identity-file", "-P", "-p", "-o", "-F", "-S", "-l"];
const NETCAT_VALUE_FLAGS: &[&str] = &["-p", "-s", "-w", "-i", "-x", "-X", "-c", "-q", "-T", "-O", "-m", "-I", "-e"];
const FTP_VALUE_FLAGS: &[&str] = &["-p", "-P", "-u", "-w", "-o", "-N", "-t", "-r", "-g"];
const GIT_VALUE_FLAGS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--super-prefix"];
const GIT_SUB_VALUE_FLAGS: &[&str] =
    &["--depth", "--branch", "-b", "--origin", "-o", "--config", "--reference", "--separate-git-dir", "--jobs", "-j", "--filter", "--shallow-since", "--shallow-exclude", "--repo", "--server-option"];
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
const CURL_REDIRECT_FLAGS: &[&str] =
    &["-x", "--proxy", "--preproxy", "--socks4", "--socks4a", "--socks5", "--socks5-hostname", "--resolve", "--connect-to", "--unix-socket", "--abstract-unix-socket", "--doh-url", "-K", "--config"];
const WGET_REDIRECT_FLAGS: &[&str] = &["-e", "--execute"];
const SSH_REDIRECT_FLAGS: &[&str] = &["-o", "-J", "-L", "-R", "-D", "-W", "-F", "-w"];
const COPY_REDIRECT_FLAGS: &[&str] = &["-o", "-F", "-J", "-S"];
const RSYNC_REDIRECT_FLAGS: &[&str] = &["-e", "--rsh", "--rsync-path"];
const NETCAT_REDIRECT_FLAGS: &[&str] = &["-x", "--proxy", "-e", "--exec", "-c", "--sh-exec"];

fn is_flag(token: &str) -> bool {
    token.starts_with('-') && token != "-"
}

fn has_flag(tokens: &[String], flags: &[&str]) -> bool {
    tokens.iter().any(|token| {
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

fn has_attached_short_flag(tokens: &[String], short: &str) -> bool {
    tokens.iter().any(|t| t.len() > short.len() && t.starts_with(short))
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
    ExecutionPolicy { mode, hosts, limits: default_limits(), allow_all }
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

fn inline_values(tokens: &[String], names: &[&str]) -> Vec<String> {
    tokens
        .iter()
        .filter_map(|t| {
            let eq = t.find('=')?;
            (eq > 0 && names.contains(&&t[..eq])).then(|| t[eq + 1..].to_string())
        })
        .collect()
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

fn analyze_segment(tokens: &[String], depth: usize) -> NetworkAssessment {
    let (peeled, _) = peel_wrappers(tokens);
    if peeled.is_empty() {
        return NO_NETWORK;
    }
    let cmd = base_name(&peeled[0]);
    let rest: Vec<String> = peeled[1..].to_vec();
    if (cmd == "busybox" || cmd == "toybox") && !rest.is_empty() && depth < 5 {
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
        if !NETWORK_GIT_SUBCOMMANDS.contains(&sub.as_str()) {
            return NO_NETWORK;
        }
        let mut hosts = vec![];
        for token in positionals(&args, GIT_SUB_VALUE_FLAGS).into_iter().chain(inline_values(&args, &["--remote", "--upload-pack", "--receive-pack"])) {
            add_host(&mut hosts, host_from_token(&token, false));
        }
        let unknown = hosts.is_empty() || git_has_redirect(&rest);
        return NetworkAssessment { network: true, hosts, unknown };
    }
    if NETWORK_CLIENTS.contains(&cmd.as_str()) {
        let mut hosts = vec![];
        if cmd == "curl" || cmd == "wget" {
            let (value_flags, redirect_flags) = if cmd == "curl" { (CURL_VALUE_FLAGS, CURL_REDIRECT_FLAGS) } else { (WGET_VALUE_FLAGS, WGET_REDIRECT_FLAGS) };
            for token in positionals(&rest, value_flags).into_iter().chain(inline_values(&rest, &["--url"])) {
                add_host(&mut hosts, host_from_token(&token, true));
            }
            let redirect = has_flag(&rest, redirect_flags) || (cmd == "curl" && has_attached_short_flag(&rest, "-x")) || (cmd == "wget" && has_attached_short_flag(&rest, "-e"));
            let unknown = hosts.is_empty() || redirect;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        if cmd == "ssh" {
            let args = positionals(&rest, SSH_VALUE_FLAGS);
            add_host(&mut hosts, host_from_token(args.first().map(String::as_str).unwrap_or(""), true));
            let redirect = has_flag(&rest, SSH_REDIRECT_FLAGS) || has_attached_short_flag(&rest, "-o");
            let unknown = hosts.is_empty() || redirect;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        if cmd == "scp" || cmd == "sftp" || cmd == "rsync" {
            for token in positionals(&rest, COPY_VALUE_FLAGS) {
                add_host(&mut hosts, host_from_token(&token, cmd == "sftp"));
            }
            let redirect = if cmd == "rsync" {
                has_flag(&rest, RSYNC_REDIRECT_FLAGS) || has_attached_short_flag(&rest, "-e")
            } else {
                has_flag(&rest, COPY_REDIRECT_FLAGS) || has_attached_short_flag(&rest, "-o")
            };
            if hosts.is_empty() && !redirect {
                return NO_NETWORK;
            }
            return NetworkAssessment { network: true, hosts, unknown: redirect };
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
                    add_host(&mut hosts, normalize_network_host(&c[1]));
                }
            }
            let unknown = hosts.is_empty() || redirect;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        let netcat = cmd == "nc" || cmd == "ncat" || cmd == "netcat";
        let args = positionals(&rest, if netcat { NETCAT_VALUE_FLAGS } else { FTP_VALUE_FLAGS });
        add_host(&mut hosts, host_from_token(args.first().map(String::as_str).unwrap_or(""), true));
        let redirect = netcat && (has_flag(&rest, NETCAT_REDIRECT_FLAGS) || has_attached_short_flag(&rest, "-x"));
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
    let mut network = false;
    let mut unknown = false;
    for segment in &scan.segments {
        let tokens = tokenize(segment);
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
