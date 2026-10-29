//! Child processes: credential-free environment, process-group kill, timeouts and bounded output.

use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 64 * 1024;
pub const DEFAULT_HARD_OUTPUT_LIMIT_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 120_000;
pub const MAX_COMMAND_TIMEOUT_MS: u64 = 600_000;

#[derive(Clone, Debug)]
pub struct CommandResult {
    pub code: i32,
    pub output: String,
    pub truncated: bool,
}

/// Why a command did not produce a normal result.
#[derive(Debug)]
pub enum CommandError {
    NotFound(String),
    Cancelled,
    TimedOut(u64),
    OutputLimit(usize),
    Spawn(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::NotFound(message) => write!(f, "{message}"),
            CommandError::Cancelled => write!(f, "Command cancelled."),
            CommandError::TimedOut(ms) => write!(f, "Command timed out after {ms} ms."),
            CommandError::OutputLimit(limit) => write!(f, "Command exceeded the {} KiB output limit.", (*limit as f64 / 1024.0).round() as u64),
            CommandError::Spawn(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CommandError {}

fn secret_names() -> &'static Mutex<HashSet<String>> {
    static NAMES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    NAMES.get_or_init(|| Mutex::new(["GOON_API_KEY".to_string(), "GOON_EMBEDDER_API_KEY".to_string()].into_iter().collect()))
}

/// Environment variables holding goon's own credentials are removed from every child process.
pub fn protect_secret_env(name: &str) {
    if !name.is_empty() {
        secret_names().lock().unwrap().insert(name.to_string());
    }
}

/// The current environment minus provider credentials (`childEnv`).
pub fn child_env(base: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = match base {
        Some(base) => base.clone(),
        None => std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect(),
    };
    for name in secret_names().lock().unwrap().iter() {
        env.remove(name);
    }
    env
}

/// `bash` on Windows resolves to Git Bash when it is installed.
pub fn resolve_executable(command: &str) -> String {
    if command != "bash" || !cfg!(windows) {
        return command.to_string();
    }
    let mut candidates: Vec<String> = vec![];
    if let Ok(v) = std::env::var("GOON_BASH") {
        candidates.push(v);
    }
    candidates.push(r"C:\Program Files\Git\bin\bash.exe".into());
    candidates.push(r"C:\Program Files (x86)\Git\bin\bash.exe".into());
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(Path::new(&local).join("Programs").join("Git").join("bin").join("bash.exe").to_string_lossy().to_string());
    }
    for candidate in candidates {
        if !candidate.is_empty() && Path::new(&candidate).exists() {
            return candidate;
        }
    }
    command.to_string()
}

pub struct RunOptions<'a> {
    pub cwd: &'a Path,
    pub cancel: &'a CancellationToken,
    pub timeout_ms: u64,
    pub env: Option<BTreeMap<String, String>>,
    pub output_limit_bytes: Option<usize>,
    pub hard_output_bytes: Option<usize>,
}

/// Head/tail capture buffer: keeps the first and last `half` bytes of a stream.
struct Capture {
    first: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    bytes: usize,
    half: usize,
}

impl Capture {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len();
        if self.first.len() < self.half {
            let take = (self.half - self.first.len()).min(chunk.len());
            self.first.extend_from_slice(&chunk[..take]);
        }
        for byte in chunk {
            if self.tail.len() == self.half {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
        }
    }

    fn render(&self, output_limit: usize) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.bytes <= self.half {
            return String::from_utf8_lossy(&self.first).to_string();
        }
        if self.bytes <= output_limit {
            let missing = self.bytes - self.first.len();
            let mut joined = self.first.clone();
            joined.extend_from_slice(&tail[tail.len().saturating_sub(missing)..]);
            return String::from_utf8_lossy(&joined).to_string();
        }
        // Cut at UTF-8 boundaries so a multi-byte character is never split into U+FFFD noise.
        let head = trim_utf8_end(&self.first);
        let tail = trim_utf8_start(&tail);
        format!("{}\n...[output truncated]...\n{}", String::from_utf8_lossy(head), String::from_utf8_lossy(tail))
    }
}

fn trim_utf8_end(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    let mut back = 0;
    while end > 0 && back < 4 {
        let b = bytes[end - 1];
        if b & 0b1100_0000 == 0b1000_0000 {
            end -= 1;
            back += 1;
            continue;
        }
        if b >= 0b1100_0000 {
            let needed = if b >= 0b1111_0000 { 4 } else if b >= 0b1110_0000 { 3 } else { 2 };
            if back + 1 < needed {
                return &bytes[..end - 1];
            }
        }
        break;
    }
    bytes
}

fn trim_utf8_start(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && start < 4 && bytes[start] & 0b1100_0000 == 0b1000_0000 {
        start += 1;
    }
    &bytes[start..]
}

#[cfg(unix)]
fn kill_group(pid: u32, signal: i32) {
    unsafe {
        if libc::kill(-(pid as i32), signal) != 0 {
            libc::kill(pid as i32, signal);
        }
    }
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/pid", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Runs a command with a timeout, cancellation and bounded head/tail output capture. The child gets
/// its own process group (POSIX) so cancellation reaches grandchildren too.
pub async fn run_command(command: &str, args: &[String], options: RunOptions<'_>) -> std::result::Result<CommandResult, CommandError> {
    if options.cancel.is_cancelled() {
        return Err(CommandError::Cancelled);
    }
    let output_limit = options.output_limit_bytes.unwrap_or(DEFAULT_OUTPUT_LIMIT_BYTES).max(1024);
    let hard_limit = options.hard_output_bytes.unwrap_or(DEFAULT_HARD_OUTPUT_LIMIT_BYTES).max(output_limit);
    let half = output_limit / 2;
    let resolved = resolve_executable(command);
    let mut cmd = tokio::process::Command::new(&resolved);
    cmd.args(args)
        .current_dir(options.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .envs(child_env(options.env.as_ref()))
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(CommandError::NotFound(format!("spawn {command} ENOENT")));
        }
        Err(error) => return Err(CommandError::Spawn(error.to_string())),
    };
    let pid = child.id().unwrap_or(0);
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let tx2 = tx.clone();
    let out_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let err_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx2.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut capture = Capture { first: vec![], tail: std::collections::VecDeque::new(), bytes: 0, half };
    let deadline = tokio::time::sleep(Duration::from_millis(options.timeout_ms));
    tokio::pin!(deadline);
    let mut terminal: Option<CommandError> = None;
    let mut streams_open = true;
    let status = loop {
        tokio::select! {
            chunk = rx.recv(), if streams_open => {
                match chunk {
                    Some(chunk) => {
                        capture.push(&chunk);
                        if capture.bytes > hard_limit && terminal.is_none() {
                            terminal = Some(CommandError::OutputLimit(hard_limit));
                            break None;
                        }
                    }
                    None => streams_open = false,
                }
            }
            status = child.wait(), if !streams_open => {
                break status.ok();
            }
            _ = &mut deadline => {
                terminal = Some(CommandError::TimedOut(options.timeout_ms));
                break None;
            }
            _ = options.cancel.cancelled() => {
                terminal = Some(CommandError::Cancelled);
                break None;
            }
        }
    };
    if let Some(error) = terminal {
        #[cfg(unix)]
        {
            if pid > 0 {
                kill_group(pid, libc::SIGTERM);
                let waited = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
                if waited.is_err() {
                    kill_group(pid, libc::SIGKILL);
                    let _ = child.wait().await;
                }
                // Grandchildren holding the pipes open must not outlive the cancelled command.
                kill_group(pid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            if pid > 0 {
                kill_tree(pid);
            }
            let _ = child.kill().await;
        }
        out_task.abort();
        err_task.abort();
        return Err(error);
    }
    let _ = out_task.await;
    let _ = err_task.await;
    while let Ok(chunk) = rx.try_recv() {
        capture.push(&chunk);
    }
    let code = status.and_then(|s| s.code()).unwrap_or(1);
    Ok(CommandResult { code, output: capture.render(output_limit), truncated: capture.bytes > output_limit })
}

/// True when a process with this PID exists (`process.kill(pid, 0)` without ESRCH).
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }
}

/// Finds an executable on PATH (`which`).
pub fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into()).split(';').map(|s| s.to_lowercase()).collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn command_error_to_anyhow(error: CommandError) -> anyhow::Error {
    anyhow!(error.to_string())
}

pub type CmdResult<T> = Result<T>;
