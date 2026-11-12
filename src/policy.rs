//! Execution policy: network default-deny and shared process limits.
//!
//! The policy is a *gate*, not an OS sandbox. Network classification is conservative and
//! fail-closed: `deny` blocks anything that looks like it can open a socket; `allowlist` allows only
//! exact host matches and denies when a host cannot be determined.

use crate::config::Config;
use crate::mcp::McpServerConfig;
use crate::process::{DEFAULT_COMMAND_TIMEOUT_MS, DEFAULT_HARD_OUTPUT_LIMIT_BYTES, DEFAULT_OUTPUT_LIMIT_BYTES, MAX_COMMAND_TIMEOUT_MS};
use crate::risk::{base_name, has_inline_code_flag, is_dangerous_env_var, is_generic_runner, parse_python_args, peel_wrappers_with_env, scan_shell, tokenize_argv, PythonInvocation};
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
    "search",
    "login",
    "owner",
    "yank",
];
const NETWORK_PACKAGE_RUNNERS: &[&str] = &["npx", "pnpx", "bunx", "uvx", "pipx"];
const NETWORK_GIT_SUBCOMMANDS: &[&str] = &["push", "pull", "fetch", "clone", "ls-remote", "submodule", "request-pull", "archive", "lfs", "send-email", "imap-send", "svn", "p4"];
const CURL_VALUE_FLAGS: &[&str] = &[
    "-o",
    "--output",
    "--output-dir",
    "-H",
    "--header",
    "--proxy-header",
    "-d",
    "--data",
    "--data-raw",
    "--data-binary",
    "--data-ascii",
    "--data-urlencode",
    "--json",
    "-X",
    "--request",
    "-u",
    "--user",
    "--proxy-user",
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
    "--form-string",
    "-T",
    "--upload-file",
    "-x",
    "--proxy",
    "--proxy1.0",
    "--preproxy",
    "--socks4",
    "--socks4a",
    "--socks5",
    "--socks5-hostname",
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
    "--pinnedpubkey",
    "--resolve",
    "--connect-to",
    "--retry",
    "--retry-delay",
    "--retry-max-time",
    "--url",
    "--url-query",
    "--unix-socket",
    "--abstract-unix-socket",
    "--doh-url",
    "--ipfs-gateway",
    "--proto",
    "--proto-redir",
    "--proto-default",
    "--interface",
    "--limit-rate",
    "--max-filesize",
    "--speed-limit",
    "--speed-time",
    "--max-redirs",
    "--local-port",
    "--dns-servers",
    "--dns-interface",
    "--dns-ipv4-addr",
    "--dns-ipv6-addr",
    "--expect100-timeout",
    "--happy-eyeballs-timeout-ms",
    "--keepalive-time",
    "-D",
    "--dump-header",
    "-C",
    "--continue-at",
    "-Q",
    "--quote",
    "-Y",
    "-y",
    "-z",
    "--time-cond",
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
    "--dns-timeout",
    "--connect-timeout",
    "--read-timeout",
    "-t",
    "--tries",
    "-w",
    "--wait",
    "--waitretry",
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
    "--exclude-domains",
    "-H",
    "--header",
    "--referer",
    "--load-cookies",
    "--save-cookies",
    "--certificate",
    "--private-key",
    "--ca-certificate",
    "--ca-directory",
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
const SCP_SFTP_VALUE_FLAGS: &[&str] = &["-i", "-P", "-o", "-F", "-J", "-S", "-l", "-c", "-b", "-D", "-R", "-s"];
const SCP_SFTP_SHORT_VALUE_CHARS: &[char] = &['P', 'i', 'o', 'F', 'J', 'S', 'c', 'l', 'b', 'D', 'R', 's'];
const RSYNC_VALUE_FLAGS: &[&str] = &[
    "-e",
    "--rsh",
    "--rsync-path",
    "--port",
    "--address",
    "--config",
    "--password-file",
    "--exclude",
    "--include",
    "--exclude-from",
    "--include-from",
    "--files-from",
    "-f",
    "--filter",
    "--log-file",
    "--temp-dir",
    "-T",
    "--compare-dest",
    "--copy-dest",
    "--link-dest",
    "--backup-dir",
    "--suffix",
    "--bwlimit",
    "--max-size",
    "--min-size",
    "--timeout",
    "--contimeout",
    "-M",
    "--remote-option",
];
const RSYNC_SHORT_VALUE_CHARS: &[char] = &['e', 'f', 'T', 'M'];
const NETCAT_VALUE_FLAGS: &[&str] = &[
    "-p",
    "-s",
    "-w",
    "-i",
    "-x",
    "--proxy",
    "--proxy-type",
    "--proxy-auth",
    "--proxy-dns",
    "-X",
    "-c",
    "--sh-exec",
    "--lua-exec",
    "-e",
    "--exec",
    "-q",
    "-T",
    "-O",
    "-m",
    "-I",
    "-o",
    "--output",
    "--hex-dump",
    "--ssl-cert",
    "--ssl-key",
    "--ssl-trustfile",
    "--ssl-ciphers",
    "--ssl-servername",
    "--ssl-alpn",
];
const FTP_VALUE_FLAGS: &[&str] = &["-p", "-P", "-u", "-w", "-o", "-N", "-t", "-r", "-g", "-s", "-l", "-e", "-c", "-f"];
const FTP_SHORT_VALUE_CHARS: &[char] = &['p', 'P', 'u', 'w', 'o', 'N', 't', 'r', 'g', 's', 'l', 'e', 'c', 'f'];
const GIT_VALUE_FLAGS: &[&str] = &["-C", "-c", "--git-dir", "--work-tree", "--namespace", "--super-prefix", "--config-env", "--exec-path"];
const GIT_SHORT_VALUE_CHARS: &[char] = &['C', 'c'];
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
    "--reference-if-able",
    "--separate-git-dir",
    "--jobs",
    "-j",
    "--filter",
    "--shallow-since",
    "--shallow-exclude",
    "--repo",
    "--server-option",
    "--negotiation-tip",
    "--recurse-submodules-default",
    "--template",
    "--remote",
    "--format",
];
const GIT_SUB_SHORT_VALUE_CHARS: &[char] = &['c', 'u', 'b', 'o', 'j'];
const PACKAGE_VALUE_FLAGS: &[&str] = &[
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
const PACKAGE_SHORT_VALUE_CHARS: &[char] = &['w', 'i', 'F', 'p', 'C'];
const CURL_REDIRECT_FLAGS: &[&str] = &[
    "-L",
    "--location",
    "--location-trusted",
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
    "--next",
    "-:",
];
const CURL_SHORT_VALUE_CHARS: &[char] = &['o', 'H', 'd', 'X', 'u', 'A', 'e', 'b', 'c', 'F', 'T', 'x', 'm', 'r', 'w', 'K', 'E', 'C', 'D', 'P', 'Q', 'Y', 'y', 'z'];
const WGET_REDIRECT_FLAGS: &[&str] = &["-e", "--execute", "-i", "--input-file", "-B", "--base", "--config", "-r", "--recursive", "-m", "--mirror", "-p", "--page-requisites"];
const WGET_SHORT_VALUE_CHARS: &[char] = &['O', 'P', 'o', 'U', 'T', 'w', 't', 'Q', 'a', 'e', 'i', 'B', 'D', 'H', 'Y', 'l', 'A', 'R'];
const SSH_REDIRECT_FLAGS: &[&str] = &["-o", "-J", "-L", "-R", "-D", "-W", "-F", "-w"];
const SSH_SHORT_VALUE_CHARS: &[char] = &['p', 'i', 'l', 'o', 'F', 'J', 'L', 'R', 'D', 'W', 'w', 'b', 'c', 'E', 'e', 'I', 'm', 'O', 'Q', 'S', 'B'];
const COPY_REDIRECT_FLAGS: &[&str] = &["-o", "-F", "-J", "-S", "-3"];
const RSYNC_REDIRECT_FLAGS: &[&str] = &["-e", "--rsh", "--rsync-path", "--config", "--files-from", "-M", "--remote-option"];
const NETCAT_REDIRECT_FLAGS: &[&str] = &["-x", "--proxy", "--proxy-type", "--proxy-dns", "-e", "--exec", "-c", "--sh-exec", "--lua-exec", "--ssl-servername", "-L", "-R"];
const NETCAT_SHORT_VALUE_CHARS: &[char] = &['p', 's', 'w', 'i', 'x', 'X', 'c', 'e', 'q', 'T', 'O', 'm', 'I', 'o'];

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

fn positionals(tokens: &[String], value_flags: &[&str], short_value_chars: &[char]) -> Vec<String> {
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
        if value_flags.contains(&token.as_str()) {
            if i + 1 < tokens.len() {
                i += 1;
            }
            i += 1;
            continue;
        }
        if !token.starts_with("--") && token.len() > 2 && !short_value_chars.is_empty() {
            for (byte_idx, ch) in token[1..].char_indices() {
                if !ch.is_ascii_alphanumeric() {
                    break;
                }
                if short_value_chars.contains(&ch) {
                    if 1 + byte_idx + ch.len_utf8() == token.len() && i + 1 < tokens.len() {
                        i += 1;
                    }
                    break;
                }
            }
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
    let cut = value.find(['/', '\\', '?', '#']);
    match cut {
        Some(c) if c > 0 => &value[..c],
        _ => value,
    }
}

fn has_valid_url_scheme(value: &str) -> Option<usize> {
    let idx = value.find("://")?;
    if idx == 0 {
        return None;
    }
    let scheme = &value[..idx];
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(idx)
}

fn is_strict_host_syntax(host: &str) -> bool {
    if host.is_empty() || host.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return false;
    }
    if host.starts_with('[') {
        return host.ends_with(']') && host.len() > 2 && !host[1..host.len() - 1].contains(['[', ']', '\\', '@', '/', '?', '#']);
    }
    !host.chars().any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
}

/// Extract a host from a URL, scp/rsync remote (`user@host:path`), or a bare host[:port].
fn host_from_token(token: &str, allow_bare_host: bool) -> Option<String> {
    let value = token.trim();
    if value.is_empty() || is_flag(value) || value.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
        return None;
    }
    if let Some(rest) = value.strip_prefix("//") {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let auth = &rest[..end];
        if auth.contains('\\') || auth.contains('@') {
            return None;
        }
        let parsed = url::Url::parse(&format!("http:{value}")).ok()?;
        let h = normalize_network_host(parsed.host_str()?)?;
        return is_strict_host_syntax(&h).then_some(h);
    }
    if let Some(scheme_idx) = has_valid_url_scheme(value) {
        let scheme = value[..scheme_idx].to_ascii_lowercase();
        if matches!(scheme.as_str(), "file" | "unix" | "fd" | "data" | "javascript" | "about" | "blob") {
            return None;
        }
        let after_scheme = &value[scheme_idx + 3..];
        let auth_end = after_scheme.find(['/', '?', '#']).unwrap_or(after_scheme.len());
        let raw_auth = &after_scheme[..auth_end];
        if raw_auth.is_empty() || raw_auth.contains('\\') {
            return None;
        }
        if raw_auth.matches('@').count() > 1 {
            return None;
        }
        let hostport = match raw_auth.rfind('@') {
            Some(at) => &raw_auth[at + 1..],
            None => raw_auth,
        };
        if hostport.is_empty() || hostport.contains('%') {
            return None;
        }
        let parsed = url::Url::parse(value).ok()?;
        let h = normalize_network_host(parsed.host_str()?)?;
        return is_strict_host_syntax(&h).then_some(h);
    }
    let at = value.rfind('@');
    let remote = match at {
        Some(i) => {
            if value[..i].contains(['/', '\\', ':']) {
                return None;
            }
            &value[i + 1..]
        }
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
        if colon > 0 && !remote[..colon].contains(['/', '\\']) {
            host = normalize_network_host(&remote[..colon]);
        }
    }
    if let Some(h) = host {
        if h.chars().count() > 1 && is_strict_host_syntax(&h) {
            return Some(h);
        }
        return None;
    }
    if !allow_bare_host {
        return None;
    }
    let candidate = trim_path(if at.is_some() { remote } else { value });
    let h = normalize_network_host(candidate)?;
    is_strict_host_syntax(&h).then_some(h)
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
            } else if !token.starts_with("--") && token.len() > 2 {
                for (byte_idx, ch) in token[1..].char_indices() {
                    if !ch.is_ascii_alphanumeric() {
                        break;
                    }
                    if GIT_SHORT_VALUE_CHARS.contains(&ch) {
                        if 1 + byte_idx + ch.len_utf8() == token.len() && index + 1 < rest.len() {
                            index += 1;
                        }
                        break;
                    }
                }
            }
            index += 1;
            continue;
        }
        return (Some(token.to_lowercase()), rest[index + 1..].to_vec());
    }
    (None, vec![])
}

fn skip_package_flags<'a>(cmd: &str, rest: &'a [String]) -> &'a [String] {
    let mut i = 0;
    while i < rest.len() {
        let token = &rest[i];
        if token == "--" {
            return &rest[i + 1..];
        }
        if cmd == "cargo" && token.starts_with('+') && token.len() > 1 {
            i += 1;
            continue;
        }
        if !is_flag(token) {
            return &rest[i..];
        }
        i += 1;
        if PACKAGE_VALUE_FLAGS.contains(&token.as_str()) {
            if i < rest.len() {
                i += 1;
            }
            continue;
        }
        if !token.starts_with("--") && token.len() > 2 {
            for (byte_idx, ch) in token[1..].char_indices() {
                if !ch.is_ascii_alphanumeric() {
                    break;
                }
                if PACKAGE_SHORT_VALUE_CHARS.contains(&ch) {
                    if 1 + byte_idx + ch.len_utf8() == token.len() && i < rest.len() {
                        i += 1;
                    }
                    break;
                }
            }
        }
    }
    &[]
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
    if has_valid_url_scheme(token).is_some() {
        let before_q = &token[..token.find(['?', '#']).unwrap_or(token.len())];
        if before_q.contains('\\') {
            return true;
        }
    }
    let rest = if let Some(idx) = has_valid_url_scheme(token) {
        &token[idx + 3..]
    } else if let Some(stripped) = token.strip_prefix("//") {
        stripped
    } else {
        token
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.matches('@').count() > 1 {
        return true;
    }
    if authority.chars().any(|c| matches!(c, '$' | '{' | '}' | '*' | '?' | '\\' | '`' | ' ' | '\t' | '\r' | '\n') || c.is_control()) {
        return true;
    }
    let hostport = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    if hostport.contains(['[', ']']) {
        let valid_ipv6 = hostport.starts_with('[')
            && hostport[1..].find(']').is_some_and(|close| {
                let inside = &hostport[1..1 + close];
                let after = &hostport[2 + close..];
                !inside.is_empty() && !inside.contains(['[', ']', '-']) && (after.is_empty() || after.strip_prefix(':').is_some_and(|p| p.is_empty() || p.chars().all(|c| c.is_ascii_digit())))
            });
        if !valid_ipv6 {
            return true;
        }
    }
    false
}

fn is_stdin_or_fd_path(path: &str) -> bool {
    path == "-" || path == "/dev/stdin" || path.starts_with("/dev/fd/") || path.starts_with("/proc/")
}

fn assess_scripting_runtime(cmd: &str, rest: &[String]) -> Option<NetworkAssessment> {
    let pre_dd: Vec<&String> = rest.iter().take_while(|t| *t != "--").collect();
    let post_dd: Vec<&String> = rest.iter().skip_while(|t| *t != "--").skip(1).collect();
    let post_dd_nonempty = !post_dd.is_empty();
    if cmd == "node" || cmd == "nodejs" {
        if crate::risk::node_has_inline_or_loader(rest) || pre_dd.iter().any(|t| is_stdin_or_fd_path(t)) || post_dd.first().is_some_and(|t| is_stdin_or_fd_path(t)) {
            return Some(unknown_network());
        }
        if pre_dd.iter().any(|t| matches!(t.as_str(), "--version" | "-v" | "--help" | "-h")) {
            return Some(NO_NETWORK);
        }
        if pre_dd.iter().any(|t| t.as_str() == "--test" || t.starts_with("--test=") || t.as_str() == "--check" || t.as_str() == "-c") {
            return Some(NO_NETWORK);
        }
        let pos = positionals(rest, &["-C", "--conditions", "--env-file", "--env-file-if-exists", "--watch-path", "--test-name-pattern", "--test-reporter", "--test-reporter-destination"], &['C']);
        if pos.first().is_some_and(|p| is_stdin_or_fd_path(p)) {
            return Some(unknown_network());
        }
        return Some(if !pos.is_empty() || post_dd_nonempty { NO_NETWORK } else { unknown_network() });
    }
    if cmd == "deno" {
        let Some(sub) = pre_dd.iter().find(|t| !is_flag(t)).map(|s| s.to_ascii_lowercase()) else {
            return Some(if pre_dd.iter().any(|t| matches!(t.as_str(), "-V" | "--version" | "-h" | "--help")) { NO_NETWORK } else { unknown_network() });
        };
        if matches!(sub.as_str(), "fmt" | "lint" | "check" | "doc" | "types" | "completions" | "help" | "--version" | "-V") {
            if rest.iter().any(|t| t.starts_with("http://") || t.starts_with("https://")) {
                return Some(unknown_network());
            }
            return Some(NO_NETWORK);
        }
        return Some(unknown_network());
    }
    if cmd == "bun" {
        if has_inline_code_flag("bun", rest) || pre_dd.iter().any(|t| is_stdin_or_fd_path(t)) || post_dd.first().is_some_and(|t| is_stdin_or_fd_path(t)) {
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
                if target.is_some_and(|t| is_stdin_or_fd_path(t) || t.starts_with("http://") || t.starts_with("https://")) {
                    return Some(unknown_network());
                }
                return Some(if target.is_some() { NO_NETWORK } else { unknown_network() });
            }
            if first.starts_with("http://") || first.starts_with("https://") {
                return Some(unknown_network());
            }
            return Some(NO_NETWORK);
        }
        return Some(unknown_network());
    }
    if crate::risk::is_python_cmd(cmd) {
        return Some(match parse_python_args(rest) {
            PythonInvocation::InlineCode | PythonInvocation::Stdin => unknown_network(),
            PythonInvocation::Module(module) => {
                if SAFE_PYTHON_MODULES.contains(&module.as_str()) {
                    NO_NETWORK
                } else {
                    unknown_network()
                }
            }
            PythonInvocation::ScriptOrOther { has_script, help_or_version } => {
                if help_or_version || has_script {
                    NO_NETWORK
                } else {
                    unknown_network()
                }
            }
        });
    }
    if matches!(cmd, "perl" | "ruby" | "php" | "lua" | "luajit" | "rscript" | "tclsh" | "osascript") {
        let interactive_lua = matches!(cmd, "lua" | "luajit")
            && pre_dd.iter().any(|t| t.as_str() == "-i" || (t.starts_with('-') && !t.starts_with("--") && t.len() > 1 && t[1..].chars().take_while(|c| c.is_ascii_alphabetic()).any(|c| c == 'i')));
        if has_inline_code_flag(cmd, rest) || interactive_lua || pre_dd.iter().any(|t| is_stdin_or_fd_path(t)) || post_dd.first().is_some_and(|t| is_stdin_or_fd_path(t)) {
            return Some(unknown_network());
        }
        if pre_dd.iter().any(|t| matches!(t.as_str(), "-v" | "-V" | "--version" | "-h" | "--help" | "-c")) {
            return Some(NO_NETWORK);
        }
        let value_flags: &[&str] = match cmd {
            "perl" => &["-I", "-x", "-C"],
            "ruby" => &["-I", "-C", "-E", "-K"],
            "php" => &["-c", "-f", "-z"],
            _ => &[],
        };
        let short_vals: &[char] = match cmd {
            "perl" => &['I', 'x', 'C'],
            "ruby" => &['I', 'C', 'E', 'K'],
            "php" => &['c', 'f', 'z'],
            _ => &[],
        };
        let pos = positionals(rest, value_flags, short_vals);
        if pos.first().is_some_and(|p| is_stdin_or_fd_path(p)) {
            return Some(unknown_network());
        }
        return Some(if !pos.is_empty() || post_dd_nonempty { NO_NETWORK } else { unknown_network() });
    }
    None
}

fn is_network_tainting_env_var(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    if u == "NO_PROXY" {
        return false;
    }
    if is_dangerous_env_var(&u) {
        return true;
    }
    matches!(
        u.as_str(),
        "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "FTP_PROXY"
            | "SOCKS_PROXY"
            | "NETRC"
            | "SSL_CERT_FILE"
            | "SSL_CERT_DIR"
            | "CURL_CA_BUNDLE"
            | "REQUESTS_CA_BUNDLE"
            | "NODE_EXTRA_CA_CERTS"
            | "GOPROXY"
            | "GONOPROXY"
            | "GOPRIVATE"
            | "GOSUMDB"
            | "CARGO_HTTP_PROXY"
            | "CARGO_HTTPS_PROXY"
            | "PIP_INDEX_URL"
            | "PIP_EXTRA_INDEX_URL"
            | "UV_INDEX_URL"
            | "UV_DEFAULT_INDEX"
            | "NPM_CONFIG_REGISTRY"
            | "NPM_CONFIG_PROXY"
            | "NPM_CONFIG_HTTPS_PROXY"
            | "YARN_REGISTRY"
            | "YARN_HTTP_PROXY"
            | "YARN_HTTPS_PROXY"
    ) || u.ends_with("_PROXY")
}

fn extract_shell_inline_scripts(cmd: &str, rest: &[String]) -> (Vec<String>, bool) {
    let mut scripts = Vec::new();
    let mut unparseable = false;
    let mut i = 0;
    while i < rest.len() {
        let t = &rest[i];
        if t == "--" {
            if let Some(s) = rest.get(i + 1) {
                scripts.push(s.clone());
            } else {
                unparseable = true;
            }
            break;
        }
        if cmd == "cmd" {
            let lower = t.to_ascii_lowercase();
            if lower == "/c" || lower == "/k" {
                if i + 1 < rest.len() {
                    scripts.push(rest[i + 1..].join(" "));
                } else {
                    unparseable = true;
                }
                break;
            }
            if lower.starts_with("/c") || lower.starts_with("/k") {
                scripts.push(t[2..].to_string());
                if i + 1 < rest.len() {
                    scripts.push(rest[i + 1..].join(" "));
                }
                break;
            }
            i += 1;
            continue;
        }
        if cmd == "fish" {
            if t == "--command" || t == "--init-command" {
                if let Some(s) = rest.get(i + 1) {
                    scripts.push(s.clone());
                    i += 2;
                    continue;
                }
                unparseable = true;
                break;
            }
            if let Some(s) = t.strip_prefix("--command=").or_else(|| t.strip_prefix("--init-command=")) {
                scripts.push(s.to_string());
                i += 1;
                continue;
            }
        }
        if matches!(t.as_str(), "-o" | "+o" | "--rcfile" | "--init-file") {
            i += 2;
            continue;
        }
        if t.starts_with('-') && !t.starts_with("--") && t.len() > 1 {
            let flags = &t[1..];
            let mut found_c = false;
            for (b_idx, ch) in flags.char_indices() {
                if !ch.is_ascii_alphabetic() {
                    break;
                }
                if ch == 'o' {
                    if b_idx + ch.len_utf8() == flags.len() {
                        i += 1;
                    }
                    break;
                }
                if ch == 'c' || (cmd == "fish" && ch == 'C') {
                    found_c = true;
                    let after = b_idx + ch.len_utf8();
                    if after < flags.len() {
                        let tail = &flags[after..];
                        scripts.push(tail.to_string());
                        if tail.chars().all(|x| x.is_ascii_alphabetic()) {
                            if let Some(next_s) = rest.get(i + 1) {
                                scripts.push(next_s.clone());
                            }
                        }
                    } else {
                        let mut next_idx = i + 1;
                        if rest.get(next_idx).is_some_and(|x| x == "--") {
                            next_idx += 1;
                        }
                        if let Some(next_s) = rest.get(next_idx) {
                            scripts.push(next_s.clone());
                        } else {
                            unparseable = true;
                        }
                    }
                    break;
                }
            }
            if found_c && cmd != "fish" {
                break;
            }
            i += 1;
            continue;
        }
        if !t.starts_with('-') {
            scripts.push(t.clone());
            break;
        }
        i += 1;
    }
    (scripts, unparseable)
}

fn shell_reads_stdin_or_interactive(cmd: &str, rest: &[String], stdin_redirected_or_piped: bool) -> bool {
    if cmd == "cmd" {
        return stdin_redirected_or_piped || rest.is_empty();
    }
    let mut positional_count = 0usize;
    let mut explicit_s_or_i = false;
    let mut i = 0;
    while i < rest.len() {
        let t = &rest[i];
        if t == "--" {
            for p in &rest[i + 1..] {
                if is_stdin_or_fd_path(p) {
                    return true;
                }
                positional_count += 1;
            }
            break;
        }
        if matches!(t.as_str(), "-o" | "+o" | "--rcfile" | "--init-file") {
            i += 2;
            continue;
        }
        if is_stdin_or_fd_path(t) {
            return true;
        }
        if t.starts_with('-') && !t.starts_with("--") && t.len() > 1 {
            for ch in t[1..].chars() {
                if matches!(ch, 's' | 'i') {
                    explicit_s_or_i = true;
                }
            }
            i += 1;
            continue;
        }
        if t.starts_with('-') {
            if matches!(t.as_str(), "--version" | "--help") {
                return false;
            }
            i += 1;
            continue;
        }
        positional_count += 1;
        i += 1;
    }
    explicit_s_or_i || (stdin_redirected_or_piped && positional_count == 0)
}

fn segment_has_stdin_redirection(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    let (mut sq, mut dq, mut ansi) = (false, false, false);
    let mut i = 0;
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
        if ch == '#' && (i == 0 || matches!(chars[i - 1], ' ' | '\t' | '\n' | ';' | '&' | '|')) {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
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
        if ch == '<' {
            return true;
        }
        i += 1;
    }
    false
}

fn analyze_segment(tokens: &[String], depth: usize, stdin_piped_or_redirected: bool) -> NetworkAssessment {
    let (peeled, _, env_names, _) = peel_wrappers_with_env(tokens);
    let env_tainted = env_names.iter().any(|k| is_network_tainting_env_var(k));
    let had_xargs = tokens.iter().any(|t| base_name(t) == "xargs");
    if peeled.is_empty() {
        return if env_tainted { unknown_network() } else { NO_NETWORK };
    }
    let raw = analyze_peeled(&peeled, depth, stdin_piped_or_redirected || had_xargs);
    if env_tainted {
        return unknown_network();
    }
    if had_xargs && raw.network {
        return unknown_network();
    }
    raw
}

fn analyze_peeled(peeled: &[String], depth: usize, stdin_piped_or_redirected: bool) -> NetworkAssessment {
    let cmd = base_name(&peeled[0]);
    let rest: Vec<String> = peeled[1..].to_vec();
    if cmd == "export" && rest.iter().any(|t| t.split_once('=').is_some_and(|(k, _)| is_network_tainting_env_var(k))) {
        return unknown_network();
    }
    if cmd == "busybox" || cmd == "toybox" {
        if rest.is_empty() {
            return NO_NETWORK;
        }
        if depth >= 5 {
            return unknown_network();
        }
        return analyze_segment(&rest, depth + 1, stdin_piped_or_redirected);
    }
    if is_generic_runner(&cmd) {
        if depth >= 5 {
            return unknown_network();
        }
        for i in 0..rest.len() {
            let token = &rest[i];
            let nested = if token.chars().any(char::is_whitespace) { assess_nested(token, depth + 1) } else { NO_NETWORK };
            let suffix = if token.starts_with('-') { NO_NETWORK } else { analyze_segment(&rest[i..], depth + 1, stdin_piped_or_redirected) };
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
            let (scripts, unparseable) = extract_shell_inline_scripts(&cmd, &rest);
            if unparseable || scripts.is_empty() {
                return unknown_network();
            }
            for script in &scripts {
                if assess_nested(script, depth + 1).network {
                    return unknown_network();
                }
            }
            return NO_NETWORK;
        }
        if shell_reads_stdin_or_interactive(&cmd, &rest, stdin_piped_or_redirected) {
            return unknown_network();
        }
        return NO_NETWORK;
    }
    if OPAQUE_NETWORK_COMMANDS.contains(&cmd.as_str()) {
        return unknown_network();
    }
    if (cmd == "rg" || cmd == "ripgrep")
        && rest.iter().take_while(|t| *t != "--").any(|t| {
            t == "--pre" || t == "--pre-glob" || t == "--hostname-bin" || t.starts_with("--pre=") || t.starts_with("--pre-glob=") || t.starts_with("--preprocessor") || t.starts_with("--hostname-bin=")
        })
    {
        return unknown_network();
    }
    if cmd == "openssl" {
        if rest.iter().find(|t| !is_flag(t)).is_some_and(|sub| matches!(sub.to_ascii_lowercase().as_str(), "s_client" | "s_server" | "ocsp" | "s_time")) {
            return unknown_network();
        }
        return NO_NETWORK;
    }
    if let Some(runtime_assessment) = assess_scripting_runtime(&cmd, &rest) {
        if !runtime_assessment.network
            && stdin_piped_or_redirected
            && (crate::risk::is_python_cmd(&cmd) || matches!(cmd.as_str(), "node" | "nodejs" | "bun" | "perl" | "ruby" | "php" | "lua" | "luajit" | "tclsh"))
        {
            let has_explicit_file = rest.iter().any(|t| !is_flag(t) && t != "--");
            if !has_explicit_file {
                return unknown_network();
            }
        }
        return runtime_assessment;
    }
    if NETWORK_PACKAGE_RUNNERS.contains(&cmd.as_str()) {
        return unknown_network();
    }
    if PACKAGE_MANAGERS.contains(&cmd.as_str()) {
        let pre_dd: Vec<&String> = rest.iter().take_while(|t| *t != "--").collect();
        if cmd == "go"
            && pre_dd.iter().any(|t| {
                let l = t.to_ascii_lowercase();
                matches!(l.as_str(), "-exec" | "--exec" | "-toolexec" | "--toolexec")
                    || l.starts_with("-exec=")
                    || l.starts_with("--exec=")
                    || l.starts_with("-toolexec=")
                    || l.starts_with("--toolexec=")
            })
        {
            return unknown_network();
        }
        if pre_dd.iter().any(|t| {
            let l = t.to_ascii_lowercase();
            matches!(l.as_str(), "--script-shell" | "--onload-script") || l.starts_with("--script-shell=") || l.starts_with("--onload-script=")
        }) {
            return unknown_network();
        }
        let tail = skip_package_flags(&cmd, &rest);
        let Some(sub) = tail.first().map(|s| s.to_lowercase()) else { return NO_NETWORK };
        let after_sub = &tail[1..];
        if PACKAGE_NETWORK_SUBCOMMANDS.contains(&sub.as_str()) {
            return unknown_network();
        }
        if (cmd == "poetry" && sub == "run") || (cmd == "uv" && sub == "run") {
            let mut idx = 0;
            while idx < after_sub.len() {
                let t = &after_sub[idx];
                if t == "--" {
                    idx += 1;
                    break;
                }
                if !is_flag(t) {
                    break;
                }
                let l = t.to_ascii_lowercase();
                if l.starts_with("--with") || l == "--refresh" || l == "--upgrade" || t == "-U" || l.starts_with("--index") || l.starts_with("--default-index") || l.starts_with("--extra-index") {
                    return unknown_network();
                }
                idx += 1;
                if matches!(
                    l.as_str(),
                    "--python" | "-p" | "--find-links" | "-f" | "--package" | "--group" | "--extra" | "--env-file" | "--cache-dir" | "--directory" | "-c" | "--project" | "--config-file"
                ) && idx < after_sub.len()
                {
                    idx += 1;
                }
            }
            let inner = &after_sub[idx..];
            if inner.is_empty() || depth >= 5 {
                return NO_NETWORK;
            }
            if inner[0].starts_with("http://") || inner[0].starts_with("https://") {
                return unknown_network();
            }
            return analyze_segment(inner, depth + 1, stdin_piped_or_redirected);
        }
        if cmd == "uv" {
            if sub == "tool" {
                let next = after_sub.iter().find(|t| !is_flag(t)).map(|s| s.to_ascii_lowercase());
                return if matches!(next.as_deref(), Some("list" | "dir")) { NO_NETWORK } else { unknown_network() };
            }
            if sub == "pip" {
                let next = after_sub.iter().find(|t| !is_flag(t)).map(|s| s.to_ascii_lowercase());
                return if matches!(next.as_deref(), Some("list" | "show" | "freeze" | "check" | "tree")) { NO_NETWORK } else { unknown_network() };
            }
            if sub == "self" || (sub == "python" && after_sub.iter().any(|t| t == "install" || t == "upgrade")) {
                return unknown_network();
            }
        }
        if cmd == "go" && (sub == "generate" || sub == "run") {
            if sub == "generate" {
                return unknown_network();
            }
            let remote = after_sub.iter().take_while(|t| *t != "--").filter(|t| !is_flag(t)).any(|a| {
                a.contains('@')
                    || (a.contains('/')
                        && !a.starts_with("./")
                        && !a.starts_with("../")
                        && !a.starts_with('/')
                        && a.split('/').next().is_some_and(|first| first.contains('.') && !first.ends_with(".go")))
            });
            if remote {
                return unknown_network();
            }
        }
        return NO_NETWORK;
    }
    if cmd == "git" {
        let (sub, args) = git_subcommand(&rest);
        let Some(sub) = sub else { return NO_NETWORK };
        if sub == "remote" {
            let pos = positionals(&args, &["-t", "-m"], &['t', 'm']);
            let rsub = pos.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
            let has_fetch = args.iter().take_while(|t| *t != "--").any(|t| t == "-f" || t == "--fetch");
            let has_no_query = args.iter().take_while(|t| *t != "--").any(|t| t == "-n");
            if rsub == "add" && has_fetch {
                let mut hosts = vec![];
                let mut unverified = false;
                if let Some(url_tok) = pos.get(2) {
                    if authority_has_unverified_expansion(url_tok) {
                        unverified = true;
                    } else {
                        match host_from_token(url_tok, false) {
                            Some(h) => add_host(&mut hosts, Some(h)),
                            None => unverified = true,
                        }
                    }
                }
                let unknown = hosts.is_empty() || unverified || git_has_redirect(&rest);
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
        if matches!(sub.as_str(), "submodule" | "lfs" | "send-email" | "imap-send" | "svn" | "p4") {
            return unknown_network();
        }
        let mut hosts = vec![];
        let mut unverified_target = false;
        let pos = positionals(&args, GIT_SUB_VALUE_FLAGS, GIT_SUB_SHORT_VALUE_CHARS);
        if sub == "archive" {
            let remotes = flag_values(&args, &["--remote"]);
            if remotes.is_empty() {
                return NO_NETWORK;
            }
            for token in remotes {
                if authority_has_unverified_expansion(&token) {
                    unverified_target = true;
                } else {
                    match host_from_token(&token, false) {
                        Some(h) => add_host(&mut hosts, Some(h)),
                        None => unverified_target = true,
                    }
                }
            }
        } else if sub == "request-pull" {
            if let Some(url_tok) = pos.get(1) {
                if authority_has_unverified_expansion(url_tok) {
                    unverified_target = true;
                } else {
                    match host_from_token(url_tok, false) {
                        Some(h) => add_host(&mut hosts, Some(h)),
                        None => unverified_target = true,
                    }
                }
            }
        } else {
            if let Some(remote_tok) = pos.first() {
                if authority_has_unverified_expansion(remote_tok) {
                    unverified_target = true;
                } else {
                    match host_from_token(remote_tok, false) {
                        Some(h) => add_host(&mut hosts, Some(h)),
                        None => unverified_target = true,
                    }
                }
            }
            for repo_tok in flag_values(&args, &["--repo"]) {
                if authority_has_unverified_expansion(&repo_tok) {
                    unverified_target = true;
                } else {
                    match host_from_token(&repo_tok, false) {
                        Some(h) => add_host(&mut hosts, Some(h)),
                        None => unverified_target = true,
                    }
                }
            }
        }
        let multi_or_upload = args.iter().take_while(|t| *t != "--").any(|t| {
            t == "--all"
                || t == "--multiple"
                || t == "--recurse-submodules"
                || t.starts_with("--recurse-submodules=")
                || t == "--recursive"
                || t.starts_with("--upload-pack")
                || t.starts_with("--receive-pack")
                || ((sub == "clone" || sub == "fetch" || sub == "ls-remote") && !t.starts_with("--") && t.starts_with("-u"))
        });
        let unknown = hosts.is_empty() || unverified_target || git_has_redirect(&rest) || multi_or_upload;
        return NetworkAssessment { network: true, hosts, unknown };
    }
    if NETWORK_CLIENTS.contains(&cmd.as_str()) {
        let mut hosts = vec![];
        if cmd == "curl" || cmd == "wget" {
            let (value_flags, redirect_flags, short_vals) =
                if cmd == "curl" { (CURL_VALUE_FLAGS, CURL_REDIRECT_FLAGS, CURL_SHORT_VALUE_CHARS) } else { (WGET_VALUE_FLAGS, WGET_REDIRECT_FLAGS, WGET_SHORT_VALUE_CHARS) };
            let mut unverified_target = false;
            let extra_urls = if cmd == "curl" { flag_values(&rest, &["--url"]) } else { vec![] };
            for token in positionals(&rest, value_flags, short_vals).into_iter().chain(extra_urls) {
                if authority_has_unverified_expansion(&token) {
                    unverified_target = true;
                    continue;
                }
                match host_from_token(&token, true) {
                    Some(h) => add_host(&mut hosts, Some(h)),
                    None => unverified_target = true,
                }
            }
            let short_cluster_redirect = if cmd == "curl" {
                has_short_flag_in_cluster(&rest, &['L', 'x', 'K', ':'], CURL_SHORT_VALUE_CHARS)
            } else {
                has_short_flag_in_cluster(&rest, &['e', 'i', 'B', 'r', 'm', 'p'], WGET_SHORT_VALUE_CHARS)
            };
            let redirect = has_flag(&rest, redirect_flags) || short_cluster_redirect;
            let unknown = hosts.is_empty() || redirect || unverified_target;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        if cmd == "ssh" {
            let args = positionals(&rest, SSH_VALUE_FLAGS, SSH_SHORT_VALUE_CHARS);
            let mut unverified = false;
            if let Some(target) = args.first() {
                if authority_has_unverified_expansion(target) {
                    unverified = true;
                } else {
                    match host_from_token(target, true) {
                        Some(h) => add_host(&mut hosts, Some(h)),
                        None => unverified = true,
                    }
                }
            }
            let has_long_flag = rest.iter().take_while(|t| *t != "--").any(|t| t.starts_with("--"));
            let redirect = has_long_flag || has_flag(&rest, SSH_REDIRECT_FLAGS) || has_short_flag_in_cluster(&rest, &['o', 'J', 'L', 'R', 'D', 'W', 'F', 'w'], SSH_SHORT_VALUE_CHARS);
            let unknown = hosts.is_empty() || unverified || redirect;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        if cmd == "scp" || cmd == "sftp" || cmd == "rsync" {
            let (val_flags, short_vals) = if cmd == "rsync" { (RSYNC_VALUE_FLAGS, RSYNC_SHORT_VALUE_CHARS) } else { (SCP_SFTP_VALUE_FLAGS, SCP_SFTP_SHORT_VALUE_CHARS) };
            let mut unverified_target = false;
            for token in positionals(&rest, val_flags, short_vals) {
                let bytes = token.as_bytes();
                let is_win_drive = bytes.len() >= 3 && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/');
                let looks_remote = cmd == "sftp" || (!is_win_drive && (token.contains(':') || token.starts_with("//")));
                if !looks_remote {
                    continue;
                }
                if authority_has_unverified_expansion(&token) {
                    unverified_target = true;
                    continue;
                }
                match host_from_token(&token, cmd == "sftp") {
                    Some(h) => add_host(&mut hosts, Some(h)),
                    None => unverified_target = true,
                }
            }
            let redirect = if cmd == "rsync" {
                has_flag(&rest, RSYNC_REDIRECT_FLAGS) || has_short_flag_in_cluster(&rest, &['e', 'M'], RSYNC_SHORT_VALUE_CHARS)
            } else {
                has_flag(&rest, COPY_REDIRECT_FLAGS) || has_short_flag_in_cluster(&rest, &['o', 'F', 'J', 'S', '3'], SCP_SFTP_SHORT_VALUE_CHARS)
            };
            if hosts.is_empty() && !redirect && !unverified_target {
                return NO_NETWORK;
            }
            let unknown = hosts.is_empty() || redirect || unverified_target;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        if cmd == "socat" {
            let mut redirect = false;
            let connect_re = regex::Regex::new(r"(?i)^(?:TCP|TCP4|TCP6|UDP|UDP4|UDP6|SCTP|SCTP4|SCTP6|OPENSSL|SSL)(?:-(?:CONNECT|SENDTO|DATAGRAM))?:(\[[^\]]+\]|[^:,!]+):(\d+)(?:,(.*))?$").unwrap();
            let pos = positionals(&rest, &["-lf", "-ly", "-lp", "-T", "-t", "-b", "-w"], &[]);
            for token in &pos {
                for part in token.split("!!") {
                    let trimmed = part.trim();
                    if trimmed.is_empty() || matches!(trimmed.to_ascii_uppercase().as_str(), "-" | "STDIO" | "STDIN" | "STDOUT" | "STDERR") {
                        continue;
                    }
                    if let Some(c) = connect_re.captures(trimmed) {
                        let opts_suspicious = c.get(3).is_some_and(|m| {
                            let o = m.as_str().to_ascii_lowercase();
                            o.contains("proxy") || o.contains("connect") || o.contains("exec") || o.contains("system") || o.contains('$') || o.contains('{')
                        });
                        if opts_suspicious || authority_has_unverified_expansion(&c[1]) {
                            redirect = true;
                        } else {
                            match host_from_token(&c[1], true) {
                                Some(h) => add_host(&mut hosts, Some(h)),
                                None => redirect = true,
                            }
                        }
                    } else {
                        redirect = true;
                    }
                }
            }
            let unknown = hosts.is_empty() || redirect;
            return NetworkAssessment { network: true, hosts, unknown };
        }
        let netcat = cmd == "nc" || cmd == "ncat" || cmd == "netcat";
        if !netcat && (has_flag(&rest, &["-e", "-c", "-f"]) || has_short_flag_in_cluster(&rest, &['e', 'c', 'f'], FTP_SHORT_VALUE_CHARS)) {
            return unknown_network();
        }
        let (val_flags, short_vals) = if netcat { (NETCAT_VALUE_FLAGS, NETCAT_SHORT_VALUE_CHARS) } else { (FTP_VALUE_FLAGS, FTP_SHORT_VALUE_CHARS) };
        let args = positionals(&rest, val_flags, short_vals);
        let mut unverified = false;
        if let Some(target) = args.first() {
            if authority_has_unverified_expansion(target) {
                unverified = true;
            } else {
                match host_from_token(target, true) {
                    Some(h) => add_host(&mut hosts, Some(h)),
                    None => unverified = true,
                }
            }
        }
        let unknown_long_flag = netcat
            && rest.iter().take_while(|t| *t != "--").any(|t| {
                t.starts_with("--")
                    && !matches!(
                        t.as_str(),
                        "--ssl" | "--udp" | "--tcp" | "--sctp" | "--nodns" | "--zero" | "--verbose" | "--send-only" | "--recv-only" | "--no-shutdown" | "--keep-open" | "--crlf" | "--telnet"
                    )
            });
        let redirect = netcat && (unknown_long_flag || has_flag(&rest, NETCAT_REDIRECT_FLAGS) || has_short_flag_in_cluster(&rest, &['x', 'e', 'c', 'L', 'R'], NETCAT_SHORT_VALUE_CHARS));
        let unknown = hosts.is_empty() || unverified || redirect;
        return NetworkAssessment { network: true, hosts, unknown };
    }
    NO_NETWORK
}

/// True when any unquoted single `|` (or `|&`, NOT `||`) appears in `input`, or an unquoted `<<` heredoc is present.
fn command_has_pipe_or_heredoc(input: &str) -> bool {
    let chars: Vec<char> = input.chars().collect();
    let (mut sq, mut dq, mut ansi) = (false, false, false);
    let mut i = 0;
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
        if ch == '#' && (i == 0 || matches!(chars[i - 1], ' ' | '\t' | '\n' | ';' | '&' | '|')) {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
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
        if ch == '|' {
            if chars.get(i + 1) == Some(&'|') {
                i += 2;
                continue;
            }
            return true;
        }
        if ch == '<' && chars.get(i + 1) == Some(&'<') {
            return true;
        }
        i += 1;
    }
    false
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
    let has_pipe_or_heredoc = command_has_pipe_or_heredoc(input);
    let mut hosts: Vec<String> = vec![];
    let mut network = scan.critical_escape;
    let mut unknown = scan.critical_escape;
    for (seg_idx, segment) in scan.segments.iter().enumerate() {
        let tokens = tokenize_argv(segment);
        if crate::risk::has_dev_net_redirection(segment, &tokens) {
            network = true;
            unknown = true;
        }
        if tokens.is_empty() {
            continue;
        }
        let stdin_fed = (has_pipe_or_heredoc && (seg_idx > 0 || segment.contains("<<"))) || segment_has_stdin_redirection(segment);
        let result = analyze_segment(&tokens, depth, stdin_fed);
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
