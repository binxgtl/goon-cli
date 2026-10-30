//! Command-line entry point: argument parsing, one-shot subcommands and the session app.

pub mod app;
pub mod args;
pub mod commands;

use crate::config::{self, Config};
use crate::eventlog::{find_seq_gap, read_event_log, session_id_from_event_log_path};
use crate::jsutil::{self, stringify, stringify_pretty};
use crate::redact::{collect_secrets, redact_object};
use crate::term::terminal_text;
use anyhow::{anyhow, bail, Result};
use args::{parse_args, Parsed, MAIN_OPTIONS, MCP_ADD_OPTIONS};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

/// Process-wide output state shared by the entry point and the session app.
pub struct Ctx {
    pub api_key: Mutex<Option<String>>,
    pub json: AtomicBool,
    pub exit_code: AtomicI32,
    pub exit_set: AtomicBool,
}

pub fn ctx() -> &'static Ctx {
    static CTX: OnceLock<Ctx> = OnceLock::new();
    CTX.get_or_init(|| Ctx { api_key: Mutex::new(None), json: AtomicBool::new(false), exit_code: AtomicI32::new(0), exit_set: AtomicBool::new(false) })
}

pub fn set_exit(code: i32) {
    ctx().exit_code.store(code, Ordering::SeqCst);
    ctx().exit_set.store(true, Ordering::SeqCst);
}

pub fn exit_code() -> i32 {
    ctx().exit_code.load(Ordering::SeqCst)
}

pub fn api_key() -> Option<String> {
    ctx().api_key.lock().unwrap().clone()
}

/// `--json` stdout carries the same payloads the event log stores and gets the same redaction.
pub fn write_json_line(value: &Value) {
    let mut secrets = collect_secrets();
    if let Some(k) = api_key().filter(|k| !k.is_empty()) {
        secrets.push(k);
    }
    let line = stringify(&redact_object(value, &secrets));
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

pub fn stdout_write(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

pub fn stderr_write(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}

/// Plain-mode `display`: terminal-safe text on stderr.
pub fn display_clean(text: &str) {
    stderr_write(&terminal_text(text));
}

pub fn safe_error(error: &anyhow::Error) -> String {
    crate::errors::safe_anyhow(error, api_key().as_deref())
}

// ---------------------------------------------------------------------------------------------
// Small helpers shared with the app
// ---------------------------------------------------------------------------------------------

pub fn welcome_banner(model: &str, provider: &str, base_url: &str, effort: Option<&str>, cwd: &Path, session_id: &str, mode: &str) -> String {
    let host = regex::Regex::new(r"^https?://").unwrap().replace(base_url, "").trim_end_matches('/').to_string();
    let cwd_text = cwd.to_string_lossy().to_string();
    let workspace = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && cwd_text.starts_with(&format!("{home}/")) => format!("~{}", &cwd_text[home.len()..]),
        _ => cwd_text,
    };
    [
        format!("◆ goon v{}", crate::version::VERSION),
        format!("  model {model}   effort {}   mode {}", effort.unwrap_or("default"), if mode == "auto" { "auto-approve" } else { mode }),
        format!("  gateway {host} ({provider})"),
        format!("  workspace {workspace}   session {}", jsutil::take_chars(session_id, 8)),
        "  /help commands   @ files   $ skills   ! shell   shift+tab mode".to_string(),
        String::new(),
    ]
    .join("\n")
}

/// Current git branch (or short detached SHA) without spawning git; `None` outside a repository.
pub fn read_git_branch(cwd: &Path) -> Option<String> {
    let mut dir = std::path::absolute(cwd).ok()?;
    for _ in 0..64 {
        let dot_git = dir.join(".git");
        if dot_git.exists() {
            let mut git_dir = dot_git.clone();
            if dot_git.is_file() {
                let text = std::fs::read_to_string(&dot_git).ok()?;
                let pointer = crate::re!(r"(?m)^gitdir:\s*(.+)$").captures(&text)?.get(1)?.as_str().trim().to_string();
                git_dir = crate::files::resolve_lexical(&dir, &pointer);
            }
            let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?.trim().to_string();
            if let Some(c) = crate::re!(r"^ref:\s*refs/heads/(.+)$").captures(&head) {
                return Some(c[1].to_string());
            }
            return crate::re!(r"(?i)^[0-9a-f]{7,}$").is_match(&head).then(|| head[..7].to_string());
        }
        let parent = dir.parent()?.to_path_buf();
        if parent == dir {
            return None;
        }
        dir = parent;
    }
    None
}

pub fn folder_label(cwd: &str) -> String {
    let clean = cwd.trim_end_matches(['/', '\\']);
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && clean == home {
            return "~".into();
        }
    }
    clean.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(clean).to_string()
}

pub fn format_relative_time(iso: &str) -> String {
    let Some(ts) = jsutil::parse_iso_millis(iso) else {
        return jsutil::take_chars(iso, 16).replacen('T', " ", 1);
    };
    let diff = ((jsutil::now_millis() - ts) / 1000).max(0);
    if diff < 60 {
        return "just now".into();
    }
    let mins = diff / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hrs = mins / 60;
    if hrs < 24 {
        return format!("{hrs}h ago");
    }
    let days = hrs / 24;
    if days < 30 {
        return format!("{days}d ago");
    }
    jsutil::take_chars(iso, 10).to_string()
}

/// Text of assistant outputs, newest last, with leading `<think>` blocks removed.
pub fn extract_copyable_from_messages(messages: &[Value], selector: &str) -> Option<(String, String)> {
    let think = regex::Regex::new(r"(?is)^<think>.*?</think>\s*").unwrap();
    let mut outputs: Vec<String> = vec![];
    for m in messages {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let text = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        let cleaned = think.replace(&text, "").trim().to_string();
        if !cleaned.is_empty() {
            outputs.push(cleaned);
        }
    }
    if outputs.is_empty() {
        return None;
    }
    let arg = selector.trim().to_lowercase();
    if arg == "code" || arg == "block" || arg == "snippet" {
        let fence = regex::Regex::new(r"```[^\r\n]*\r?\n((?s:.*?))```").unwrap();
        for out in outputs.iter().rev() {
            if let Some(last) = fence.captures_iter(out).last() {
                let block = last[1].trim_end().to_string();
                if !block.is_empty() {
                    return Some((block, "last code block".into()));
                }
            }
        }
    }
    if let Ok(n) = arg.parse::<usize>() {
        if n >= 1 && n <= outputs.len() && arg.chars().all(|c| c.is_ascii_digit()) {
            let picked = outputs[outputs.len() - n].clone();
            return Some((picked, if n == 1 { "last response".into() } else { format!("response #{n} from latest") }));
        }
    }
    Some((outputs.last().unwrap().clone(), "last response".into()))
}

/// Clipboard helpers in preference order (Termux, Wayland, X11, macOS, Windows).
pub fn clipboard_candidates() -> Vec<(String, Vec<String>)> {
    match jsutil::platform() {
        "win32" => vec![(
            "powershell.exe".into(),
            vec!["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())".into()],
        )],
        "darwin" => vec![("pbcopy".into(), vec![])],
        _ => {
            let mut out = vec![];
            if std::env::var("TERMUX_VERSION").is_ok() || std::env::var("PREFIX").map(|p| p.contains("com.termux")).unwrap_or(false) {
                out.push(("termux-clipboard-set".into(), vec![]));
            }
            if std::env::var("WAYLAND_DISPLAY").map(|v| !v.is_empty()).unwrap_or(false) {
                out.push(("wl-copy".into(), vec![]));
            }
            out.push(("xclip".into(), vec!["-selection".into(), "clipboard".into()]));
            out.push(("xsel".into(), vec!["--clipboard".into(), "--input".into()]));
            out
        }
    }
}

pub async fn copy_text_to_clipboard(text: &str) -> bool {
    use base64::Engine;
    let mut copied = false;
    if crate::term::stdout_is_tty() && text.chars().count() <= 100_000 {
        // OSC 52 works over SSH and in most terminals, even without a clipboard helper.
        stdout_write(&format!("\x1b]52;c;{}\x07", base64::engine::general_purpose::STANDARD.encode(text.as_bytes())));
        copied = true;
    }
    for (bin, args) in clipboard_candidates() {
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&args)
            .env_clear()
            .envs(crate::process::child_env(None))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let Ok(mut child) = cmd.spawn() else { continue };
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(text.as_bytes()).await;
        }
        if let Ok(Ok(status)) = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await {
            if status.success() {
                return true;
            }
        }
    }
    copied
}

/// API keys sent over plain HTTP are readable on the network path (except loopback).
pub fn insecure_base_url_warning(base_url: Option<&str>) -> Option<String> {
    let url = url::Url::parse(base_url?).ok()?;
    let host = url.host_str().unwrap_or("");
    let loopback = ["localhost", "127.0.0.1", "[::1]", "::1"].contains(&host) || host.ends_with(".localhost");
    if url.scheme() == "http" && !loopback {
        let host_port = match url.port() {
            Some(p) => format!("{host}:{p}"),
            None => host.to_string(),
        };
        return Some(format!("Warning: {host_port} uses plain http; your API key is sent unencrypted. Use https:// unless this is a trusted local network."));
    }
    None
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "arm" => "arm",
        other => other,
    }
}

async fn executable_ok(name: &str, cwd: &Path) -> bool {
    let cancel = tokio_util::sync::CancellationToken::new();
    let options = crate::process::RunOptions { cwd, cancel: &cancel, timeout_ms: 5000, env: None, output_limit_bytes: None, hard_output_bytes: None };
    matches!(crate::process::run_command(name, &["--version".to_string()], options).await, Ok(r) if r.code == 0)
}

fn doctor_warnings(config: &Config, base_url: &str, model_ids: &[String]) -> Vec<String> {
    let mut warnings = vec![];
    if let Some(w) = insecure_base_url_warning(Some(base_url)) {
        warnings.push(w);
    }
    if let Some(model) = &config.model {
        if !model_ids.is_empty() && !model_ids.contains(model) {
            warnings.push(format!("Model \"{model}\" is not listed by {base_url}/models."));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = config::credentials_path();
        if let Ok(meta) = std::fs::metadata(&path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                warnings.push(format!("{} is readable by other users (mode {:o}); run: chmod 600 \"{}\"", path.display(), mode, path.display()));
            }
        }
    }
    warnings
}

// ---------------------------------------------------------------------------------------------
// replay
// ---------------------------------------------------------------------------------------------

fn resolve_event_log_target(target: &str) -> Result<PathBuf> {
    let clean = target.trim();
    if clean.is_empty() {
        bail!("Usage: goon replay <session-id|prefix|event-log-path>");
    }
    let candidate = Path::new(clean);
    if candidate.exists() {
        if !candidate.is_file() {
            bail!("Not a file: {clean}");
        }
        return Ok(std::path::absolute(candidate)?);
    }
    let root = config::data_dir().join("sessions");
    let names: Vec<String> = std::fs::read_dir(&root).map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
    let mut names = names;
    names.sort();
    let logs: Vec<(String, PathBuf)> = names.iter().filter_map(|n| n.strip_suffix(".events.jsonl").map(|id| (id.to_string(), root.join(n)))).collect();
    let lower = clean.to_lowercase();
    let exact: Vec<&(String, PathBuf)> = logs.iter().filter(|(id, _)| id.to_lowercase() == lower).collect();
    let matches: Vec<&(String, PathBuf)> = if !exact.is_empty() { exact } else { logs.iter().filter(|(id, _)| id.to_lowercase().starts_with(&lower)).collect() };
    if matches.len() > 1 {
        bail!("Session prefix \"{clean}\" is ambiguous: {}", matches.iter().map(|(id, _)| jsutil::take_chars(id, 8)).collect::<Vec<_>>().join(", "));
    }
    if let Some((_, path)) = matches.first() {
        return Ok(path.clone());
    }
    let session_exists = names.iter().any(|n| n.strip_suffix(".json").map(|id| id.to_lowercase().starts_with(&lower)).unwrap_or(false));
    if session_exists {
        bail!("No event log found for session \"{clean}\"; it may predate --event-log.");
    }
    bail!("No event log found for \"{clean}\".")
}

fn replay_command(target: &str, envelopes_out: bool, rebuild: bool) -> Result<()> {
    let path = resolve_event_log_target(target)?;
    let result = read_event_log(&path)?;
    if result.truncated {
        stderr_write(&format!("goon: replay: ignored a torn final line in {}\n", path.display()));
    }
    let mut previous: Option<&crate::eventlog::EventEnvelope> = None;
    let mut out = String::new();
    for envelope in &result.envelopes {
        if find_seq_gap(previous, envelope) {
            stderr_write(&format!("goon: replay: sequence gap after {} (next {})\n", previous.unwrap().seq, envelope.seq));
        }
        previous = Some(envelope);
        out.push_str(&stringify(&if envelopes_out { envelope.to_value() } else { envelope.event.clone() }));
        out.push('\n');
    }
    stdout_write(&out);
    if !rebuild {
        return Ok(());
    }
    let session_body = result.envelopes.iter().find(|e| e.kind == "session").map(|e| e.event.clone()).filter(Value::is_object).unwrap_or(json!({}));
    let raw_id = session_body.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| session_id_from_event_log_path(&path));
    let id: String = raw_id.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    let users: Vec<Value> = result.envelopes.iter().filter(|e| e.kind == "user").filter_map(|e| e.event.get("text").and_then(Value::as_str)).map(|t| json!(t)).collect();
    let mut rebuilt = Map::new();
    rebuilt.insert("version".into(), json!(1));
    rebuilt.insert("id".into(), json!(id));
    rebuilt.insert("cwd".into(), json!(session_body.get("cwd").and_then(Value::as_str).unwrap_or("")));
    rebuilt.insert("user".into(), Value::Array(users));
    if let Some(done) = result.envelopes.iter().rev().find(|e| e.kind == "done") {
        rebuilt.insert("answer".into(), json!(done.event.get("text").and_then(Value::as_str).unwrap_or("")));
        rebuilt.insert("denied".into(), json!(done.event.get("denied") == Some(&json!(true))));
    }
    if let Some(error) = result.envelopes.iter().rev().find(|e| e.kind == "error") {
        rebuilt.insert("error".into(), json!(error.event.get("message").and_then(Value::as_str).unwrap_or("unknown error")));
    }
    config::atomic_json(&path.parent().unwrap_or(Path::new(".")).join(format!("{id}.replayed.json")), &Value::Object(rebuilt), true)
}

// ---------------------------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------------------------

fn config_overrides(values: &Parsed) -> Map<String, Value> {
    let mut overrides = Map::new();
    let numeric = ["contextWindow", "maxOutputTokens", "runTokenBudget", "timeoutMs", "maxCostUsd", "maxTimeMs"];
    for (flag, key) in [
        ("provider", "provider"),
        ("base-url", "baseUrl"),
        ("model", "model"),
        ("effort", "effort"),
        ("key-env", "keyEnv"),
        ("context-window", "contextWindow"),
        ("max-output-tokens", "maxOutputTokens"),
        ("run-token-budget", "runTokenBudget"),
        ("timeout-ms", "timeoutMs"),
        ("max-cost-usd", "maxCostUsd"),
        ("max-time-ms", "maxTimeMs"),
        ("inline-reasoning", "inlineReasoning"),
    ] {
        if let Some(v) = values.str(flag) {
            overrides.insert(key.into(), if numeric.contains(&key) { config::number_value(config::js_number(&v)) } else { json!(v) });
        }
    }
    if values.has("show-reasoning") {
        overrides.insert("showReasoning".into(), json!(values.flag("show-reasoning")));
    }
    if values.flag("event-log") {
        overrides.insert("eventLog".into(), json!(true));
    }
    if values.flag("no-event-log") {
        overrides.insert("eventLog".into(), json!(false));
    }
    if values.flag("stub") {
        overrides.insert("stub".into(), json!(true));
    }
    if let Some(f) = values.str("stub-fixture") {
        overrides.insert("stubFixture".into(), json!(f));
    }
    let mut verify = Map::new();
    if values.flag("no-verify") {
        verify.insert("enabled".into(), json!(false));
    }
    let cmds = values.list("verify-cmd");
    if !cmds.is_empty() {
        verify.insert("commands".into(), json!(cmds));
    }
    if !verify.is_empty() {
        overrides.insert("verify".into(), Value::Object(verify));
    }
    overrides
}

async fn mcp_add(argv: &[String]) -> Result<()> {
    let rest = &argv[2..];
    let dash = rest.iter().position(|a| a == "--");
    let (flag_part, cmd_part) = match dash {
        Some(i) => (&rest[..i], rest[i + 1..].to_vec()),
        None => (rest, vec![]),
    };
    let parsed = parse_args(flag_part, MCP_ADD_OPTIONS, false)?;
    let Some(name) = parsed.positionals.first().cloned() else {
        bail!("Usage: goon mcp add <name> (--url <URL> [--bearer-env ENV] | -- <command> [args...])");
    };
    let mut env_map = Map::new();
    for pair in parsed.list("env") {
        if let Some(eq) = pair.find('=').filter(|e| *e > 0) {
            env_map.insert(pair[..eq].trim().to_string(), json!(pair[eq + 1..]));
        }
    }
    let url = parsed.str("url");
    let command_tokens = if !cmd_part.is_empty() { cmd_part } else { parsed.positionals[1..].to_vec() };
    if url.is_none() && command_tokens.is_empty() {
        bail!("Specify either --url <URL> or -- <command> [args...] when adding an MCP server.");
    }
    let bearer = parsed.str("bearer-env").or_else(|| parsed.str("bearer-token-env-var"));
    let timeout = parsed.str("timeout-ms").map(|t| config::js_number(&t));
    let trust = matches!(parsed.values.get("trust"), Some(args::OptValue::Bool(true)));
    let mut raw = Map::new();
    if let Some(url) = &url {
        raw.insert("transport".into(), json!("http"));
        raw.insert("url".into(), json!(url));
        if let Some(b) = &bearer {
            raw.insert("bearerEnv".into(), json!(b));
        }
    } else {
        raw.insert("transport".into(), json!("stdio"));
        raw.insert("command".into(), json!(command_tokens[0]));
        raw.insert("args".into(), json!(command_tokens[1..]));
        if !env_map.is_empty() {
            raw.insert("env".into(), Value::Object(env_map));
        }
    }
    if trust {
        raw.insert("trust".into(), json!(true));
    }
    if let Some(t) = timeout.filter(|t| *t != 0.0 && !t.is_nan()) {
        raw.insert("timeoutMs".into(), config::number_value(t));
    }
    let entry = crate::mcp::parse_server(&Value::Object(raw))?;
    let mut mcp_config = crate::mcp::load_mcp_config()?;
    let transport = entry.transport_name();
    mcp_config.servers.insert(name.clone(), entry);
    crate::mcp::save_mcp_config(&mcp_config)?;
    stdout_write(&format!("Added MCP server \"{name}\" ({transport}).\n"));
    Ok(())
}

/// Installs SIGINT/SIGTERM handlers for the lifetime of the returned guard.
pub struct SignalGuard {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn on_signals(handler: std::sync::Arc<dyn Fn(&'static str) + Send + Sync>) -> SignalGuard {
    let task = tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut int), Ok(mut term)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else { return };
            let mut hup = signal(SignalKind::hangup()).ok();
            loop {
                tokio::select! {
                    _ = int.recv() => handler("SIGINT"),
                    _ = term.recv() => handler("SIGTERM"),
                    _ = async { match hup.as_mut() { Some(h) => { h.recv().await; } None => std::future::pending::<()>().await } } => handler("SIGTERM"),
                }
            }
        }
        #[cfg(not(unix))]
        {
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    return;
                }
                handler("SIGINT");
            }
        }
    });
    SignalGuard { task }
}

/// Signals relayed by the npm wrapper over a loopback socket (`GOON_CONTROL_PORT`); Windows has
/// no SIGTERM, and a relayed SIGINT must not double up with the terminal's own.
pub fn on_control_channel(handler: std::sync::Arc<dyn Fn(&'static str) + Send + Sync>) -> Option<SignalGuard> {
    let port: u16 = std::env::var("GOON_CONTROL_PORT").ok()?.parse().ok()?;
    let task = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let Ok(stream) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else { return };
        let mut lines = tokio::io::BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match line.trim() {
                "SIGINT" => handler("SIGINT"),
                "SIGTERM" => handler("SIGTERM"),
                _ => {}
            }
        }
    });
    Some(SignalGuard { task })
}

pub async fn main(argv: Vec<String>) -> i32 {
    if let Err(error) = main_inner(argv).await {
        let message = terminal_text(&safe_error(&error));
        if ctx().json.load(Ordering::SeqCst) {
            write_json_line(&json!({ "type": "error", "message": message }));
        } else {
            stderr_write(&format!("goon: {message}\n"));
        }
        let code = exit_code();
        if code != 130 && code != 143 {
            set_exit(1);
        }
    }
    exit_code()
}

async fn main_inner(argv: Vec<String>) -> Result<()> {
    if argv.first().map(String::as_str) == Some("mcp") && argv.get(1).map(String::as_str) == Some("add") {
        return mcp_add(&argv).await;
    }
    let values = parse_args(&argv, MAIN_OPTIONS, true)?;
    ctx().json.store(values.flag("json"), Ordering::SeqCst);
    if values.flag("help") {
        stdout_write(&crate::help::help());
        return Ok(());
    }
    if values.flag("version") {
        stdout_write(&format!("goon cli {}\n", crate::version::VERSION));
        return Ok(());
    }
    let overrides = config_overrides(&values);
    let config = config::load_config(&overrides)?;
    let policy = crate::policy::resolve_execution_policy(&config, values.flag("allow-net"), &values.list("allow-net-hosts"));
    let positionals = values.positionals.clone();
    let first = positionals.first().map(String::as_str);
    let cwd_flag = values.str("cwd");
    let flag_cwd = || std::path::absolute(cwd_flag.clone().unwrap_or_else(|| ".".into())).unwrap_or_else(|_| PathBuf::from("."));

    if first == Some("mcp") {
        return mcp_command(&positionals, flag_cwd()).await;
    }
    if first == Some("setup") {
        crate::setup::run_setup_wizard(Some(config), Default::default()).await?;
        return Ok(());
    }
    if first == Some("init") {
        if config.model.is_none() && crate::term::stdin_is_tty() && !values.flag("json") {
            crate::setup::run_setup_wizard(Some(config), Default::default()).await?;
            return Ok(());
        }
        if config.model.is_none() {
            bail!("Specify --model and optionally --base-url/--provider.");
        }
        config::save_config(&config)?;
        stdout_write(&format!("Saved {}\nSet {} in your shell, then run goon in your project.\n", config::config_path().display(), config.key_env));
        return Ok(());
    }
    if first == Some("doctor") {
        return doctor_command(&config, flag_cwd()).await;
    }
    if first == Some("sessions") {
        let list = crate::session::SessionStore::list(&config::data_dir().join("sessions"))?;
        if values.flag("json") {
            stdout_write(&format!("{}\n", stringify(&serde_json::to_value(&list)?)));
        } else {
            let lines: Vec<String> = list.iter().map(|s| format!("{}  {}  {}{}  {}", s.id, s.updated_at, s.model, s.title.as_ref().map(|t| format!("  [{t}]")).unwrap_or_default(), s.cwd)).collect();
            stdout_write(&format!("{}\n", lines.join("\n")));
        }
        return Ok(());
    }
    if first == Some("replay") {
        let Some(target) = positionals.get(1) else {
            bail!("Usage: goon replay <session-id|prefix|event-log-path> [--json] [--envelopes] [--rebuild-session]");
        };
        return replay_command(target, values.flag("envelopes"), values.flag("rebuild-session"));
    }
    if first == Some("workflow") || first == Some("workflows") {
        let ws = std::path::absolute(cwd_flag.clone().unwrap_or_else(|| std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| ".".into())))?;
        let sub = positionals.get(1).map(|s| s.to_lowercase()).unwrap_or_else(|| "list".into());
        if let Some(done) = workflow_command(&sub, &positionals, &ws, values.flag("json"))? {
            return done;
        }
    }
    app::run_session(values, config, policy).await
}

fn workflow_command(sub: &str, positionals: &[String], ws: &Path, json_out: bool) -> Result<Option<Result<()>>> {
    use crate::workflow::{create_workflow_scaffold, discover_workflows, format_workflow_details, remove_custom_workflow};
    match sub {
        "list" => {
            let list = discover_workflows(ws);
            if json_out {
                stdout_write(&format!("{}\n", stringify_pretty(&Value::Array(list.iter().map(|w| w.to_value()).collect()))));
            } else {
                let mut out = String::new();
                for wf in &list {
                    out.push_str(&format!("  {} [{}] ({} steps) — {}\n", pad_end(&wf.name, 20), wf.scope, wf.steps.len(), wf.description));
                }
                stdout_write(&out);
            }
            Ok(Some(Ok(())))
        }
        "show" | "get" => {
            let Some(name) = positionals.get(2) else { bail!("Usage: goon workflow show <name>") };
            let list = discover_workflows(ws);
            let found = list.iter().find(|w| w.name.to_lowercase() == name.to_lowercase()).ok_or_else(|| anyhow!("Workflow \"{name}\" not found."))?;
            stdout_write(&format!("{}\n", if json_out { stringify_pretty(&found.to_value()) } else { format_workflow_details(found) }));
            Ok(Some(Ok(())))
        }
        "init" | "create" => {
            let Some(name) = positionals.get(2) else { bail!("Usage: goon workflow init <name> [description]") };
            let desc = positionals[3..].join(" ").trim().to_string();
            let (path, wf) = create_workflow_scaffold(ws, name, (!desc.is_empty()).then_some(desc.as_str()), false, false)?;
            stdout_write(&format!("Created workflow \"{}\" at {}\n", wf.name, path.display()));
            Ok(Some(Ok(())))
        }
        "rm" | "remove" | "delete" => {
            let Some(name) = positionals.get(2) else { bail!("Usage: goon workflow rm <name>") };
            let removed = remove_custom_workflow(ws, name)?;
            stdout_write(&format!("Removed workflow \"{name}\" ({}).\n", removed.display()));
            Ok(Some(Ok(())))
        }
        _ => Ok(None),
    }
}

pub fn pad_end(text: &str, width: usize) -> String {
    let len = jsutil::utf16_len(text);
    if len >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

pub fn pad_start(text: &str, width: usize) -> String {
    let len = jsutil::utf16_len(text);
    if len >= width {
        text.to_string()
    } else {
        format!("{}{text}", " ".repeat(width - len))
    }
}

async fn mcp_command(positionals: &[String], cwd: PathBuf) -> Result<()> {
    use crate::mcp::{import_codex_mcp_servers, load_mcp_config, mcp_config_path, save_mcp_config, McpConfig};
    let mut mcp_config = load_mcp_config()?;
    let sub = positionals.get(1).map(String::as_str);
    match sub {
        Some("list") => {
            let servers: Vec<Value> = mcp_config
                .servers
                .iter()
                .map(|(name, s)| json!({ "name": name, "transport": s.transport_name(), "disabled": s.disabled, "trust": s.trust, "readOnlyTools": s.read_only_tools }))
                .collect();
            stdout_write(&format!("{}\n", stringify_pretty(&json!({ "path": mcp_config_path().to_string_lossy(), "servers": servers }))));
            Ok(())
        }
        Some("get") => {
            let target = positionals.get(2).cloned().unwrap_or_default();
            let Some(server) = mcp_config.servers.get(&target) else { bail!("MCP server \"{target}\" not found.") };
            let mut m = Map::new();
            m.insert("name".into(), json!(target));
            if let Value::Object(rest) = server.to_value() {
                m.extend(rest);
            }
            stdout_write(&format!("{}\n", stringify_pretty(&Value::Object(m))));
            Ok(())
        }
        Some("remove") | Some("rm") => {
            let target = positionals.get(2).cloned().unwrap_or_default();
            if mcp_config.servers.shift_remove(&target).is_none() {
                bail!("MCP server \"{target}\" not found.");
            }
            save_mcp_config(&mcp_config)?;
            stdout_write(&format!("Removed MCP server \"{target}\".\n"));
            Ok(())
        }
        Some("import-codex") => {
            let filter: Vec<String> = positionals[2..].to_vec();
            let (imported, path) = import_codex_mcp_servers(None, &filter)?;
            stdout_write(&format!("{}\n", stringify_pretty(&json!({ "imported": imported, "path": path.to_string_lossy() }))));
            Ok(())
        }
        Some("check") => {
            let filtered = match positionals.get(2) {
                Some(name) => McpConfig { servers: mcp_config.servers.into_iter().filter(|(k, _)| k == name).collect() },
                None => mcp_config,
            };
            let manager = crate::mcp_client::McpManager::new(filtered, &cwd, None);
            let cancel = tokio_util::sync::CancellationToken::new();
            let token = cancel.clone();
            let _guard = on_signals(std::sync::Arc::new(move |sig| {
                set_exit(if sig == "SIGTERM" { 143 } else { 130 });
                token.cancel();
            }));
            let result = manager.connect(None, Some(&cancel)).await;
            manager.close().await;
            let tools = result?;
            stdout_write(&format!("{}\n", stringify_pretty(&Value::Array(tools.iter().map(|t| t.to_value()).collect()))));
            Ok(())
        }
        _ => bail!("Usage: goon mcp list | check [name] | get <name> | add <name> ... | remove <name> | import-codex [name...]"),
    }
}

async fn doctor_command(config: &Config, cwd: PathBuf) -> Result<()> {
    let wrapper_node = std::env::var("GOON_WRAPPER_NODE").ok().map(Value::String).unwrap_or(Value::Null);
    if config.stub {
        let doc = json!({
            "version": crate::version::VERSION, "node": wrapper_node, "platform": jsutil::platform(), "arch": node_arch(),
            "config": config::config_path().to_string_lossy(), "provider": "goon-stub", "model": config.model.clone().unwrap_or_else(|| "stub-model".into()),
            "effort": config.effort.clone().unwrap_or_else(|| "default".into()), "baseUrl": "stub://local", "credentialVariable": config.key_env,
            "credentialPresent": false, "bash": false, "ripgrep": false, "searchFallback": true, "data": config::data_dir().to_string_lossy(),
            "networkTested": false, "gatewayReachable": false, "discoveredModels": [], "modelAvailable": true, "stub": true,
            "stubFixture": config.stub_fixture, "warnings": []
        });
        stdout_write(&format!("{}\n", stringify_pretty(&doc)));
        return Ok(());
    }
    let resolved_key = config::resolve_api_key(config);
    let (bash, rg) = tokio::join!(executable_ok("bash", &cwd), executable_ok("rg", &cwd));
    let target_url = config::normalize_gateway_url(config.base_url.as_deref().unwrap_or(config::DEFAULT_GATEWAY_URL));
    let discovered = crate::provider::discover_models(&target_url, resolved_key.as_deref(), &config.provider, None).await;
    let model_ids: Vec<String> = discovered.iter().map(|m| m.id.clone()).collect();
    let model_available = config.model.as_ref().map(|m| model_ids.is_empty() || model_ids.contains(m)).unwrap_or(false);
    let warnings = doctor_warnings(config, &target_url, &model_ids);
    let doc = json!({
        "version": crate::version::VERSION, "node": wrapper_node, "platform": jsutil::platform(), "arch": node_arch(),
        "config": config::config_path().to_string_lossy(), "provider": config.provider, "model": config.model,
        "effort": config.effort.clone().unwrap_or_else(|| "default".into()), "baseUrl": config.base_url.clone().unwrap_or_else(|| "provider default".into()),
        "credentialVariable": config.key_env, "credentialPresent": resolved_key.is_some(), "bash": bash, "ripgrep": rg, "searchFallback": !rg,
        "data": config::data_dir().to_string_lossy(), "networkTested": true, "gatewayReachable": !model_ids.is_empty(),
        "discoveredModels": model_ids, "modelAvailable": model_available, "warnings": warnings
    });
    stdout_write(&format!("{}\n", stringify_pretty(&doc)));
    if !bash || config.model.is_none() || resolved_key.is_none() {
        set_exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_match_ts() {
        assert_eq!(
            insecure_base_url_warning(Some("http://example.com:8080/v1")).unwrap(),
            "Warning: example.com:8080 uses plain http; your API key is sent unencrypted. Use https:// unless this is a trusted local network."
        );
        assert!(insecure_base_url_warning(Some("http://127.0.0.1:1/v1")).is_none());
        let messages = vec![json!({"role": "assistant", "content": "first"}), json!({"role": "assistant", "content": [{"type": "text", "text": "<think>x</think> two\n```js\ncode()\n```"}]})];
        assert_eq!(extract_copyable_from_messages(&messages, "").unwrap().0, "two\n```js\ncode()\n```");
        assert_eq!(extract_copyable_from_messages(&messages, "code").unwrap(), ("code()".to_string(), "last code block".to_string()));
        assert_eq!(extract_copyable_from_messages(&messages, "2").unwrap().1, "response #2 from latest");
        assert_eq!(folder_label("/a/b/"), "b");
    }
}
