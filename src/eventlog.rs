//! Append-only JSONL event log (`{data}/sessions/{id}.events.jsonl`) with a `.seq` sidecar.

use crate::redact::{collect_secrets, is_sensitive_path, redact_object, redact_secrets};
use anyhow::{anyhow, bail, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const EVENT_LOG_VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq)]
pub struct EventEnvelope {
    pub seq: u64,
    pub at: String,
    pub session_id: String,
    pub kind: String,
    pub event: Value,
}

impl EventEnvelope {
    pub fn to_value(&self) -> Value {
        json!({ "v": EVENT_LOG_VERSION, "seq": self.seq, "at": self.at, "sessionId": self.session_id, "type": self.kind, "event": self.event })
    }
}

pub fn event_log_path(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("sessions").join(format!("{session_id}.events.jsonl"))
}

pub fn event_log_sidecar_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.seq", path.display()))
}

pub fn session_id_from_event_log_path(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Some(stem) = name.strip_suffix(".events.jsonl") {
        return stem.to_string();
    }
    match name.rfind('.') {
        Some(i) if i > 0 => name[..i].to_string(),
        _ => name,
    }
}

fn safe_seq(value: Option<&Value>) -> Option<u64> {
    let n = value?.as_f64()?;
    (n.fract() == 0.0 && n > 0.0 && n <= 9_007_199_254_740_991.0).then_some(n as u64)
}

pub fn normalize_event_envelope(value: &Value, session_id: Option<&str>, seq: Option<u64>) -> Result<EventEnvelope> {
    let raw = value.as_object().ok_or_else(|| anyhow!("Invalid event envelope: expected an object."))?;
    if raw.get("v").and_then(Value::as_f64) != Some(1.0) {
        let shown = match raw.get("v") {
            None => "undefined".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(v) => crate::jsutil::stringify(v),
        };
        bail!("Unsupported event log version {shown}; expected {EVENT_LOG_VERSION}.");
    }
    let seq = safe_seq(raw.get("seq")).or(seq).ok_or_else(|| anyhow!("Invalid event envelope: missing or invalid seq."))?;
    let session_id = raw
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or(session_id.map(str::to_string))
        .ok_or_else(|| anyhow!("Invalid event envelope: missing sessionId."))?;
    let kind = raw.get("type").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("Invalid event envelope: missing type."))?;
    let event = raw.get("event").cloned().ok_or_else(|| anyhow!("Invalid event envelope: missing event body."))?;
    let at = raw.get("at").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into());
    Ok(EventEnvelope { seq, at, session_id, kind: kind.into(), event })
}

/// Holds back a tail of streamed text so a secret split across deltas is redacted as one unit.
pub struct StreamRedactor {
    carry: String,
    known: Vec<String>,
    tail_size: usize,
}

impl StreamRedactor {
    pub fn new(secrets: &[String]) -> Self {
        let mut known: Vec<String> = vec![];
        for s in secrets {
            if !s.is_empty() && !known.contains(s) {
                known.push(s.clone());
            }
        }
        let longest = known.iter().map(|s| s.chars().count()).max().unwrap_or(0);
        StreamRedactor { carry: String::new(), known, tail_size: 256usize.max(longest.saturating_sub(1)) }
    }

    pub fn push(&mut self, chunk: &str) -> String {
        if chunk.is_empty() {
            return String::new();
        }
        self.carry.push_str(chunk);
        let len = self.carry.chars().count();
        if len <= self.tail_size {
            return String::new();
        }
        let cut = len - self.tail_size;
        let idx = self.carry.char_indices().nth(cut).map(|(i, _)| i).unwrap_or(self.carry.len());
        let safe = self.carry[..idx].to_string();
        self.carry.drain(..idx);
        redact_secrets(&safe, &self.known)
    }

    pub fn flush_all(&mut self) -> String {
        let tail = std::mem::take(&mut self.carry);
        redact_secrets(&tail, &self.known)
    }
}

fn last_seq_from_text(text: &str, started_mid_file: bool) -> Option<u64> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    lines.pop();
    if started_mid_file && !lines.is_empty() {
        lines.remove(0);
    }
    for line in lines.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            if let Some(seq) = safe_seq(parsed.get("seq")) {
                return Some(seq);
            }
        }
    }
    None
}

fn read_range(file: &Path, start: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut f = File::open(file)?;
    f.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::with_capacity(length as usize);
    f.take(length).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// The sidecar is authoritative when it is not behind the log, otherwise the tail scan wins.
pub fn read_last_seq(path: &Path) -> u64 {
    let scanned = scan_last_seq(path);
    match read_sidecar_seq(path) {
        Some(sidecar) if sidecar >= scanned => sidecar,
        _ => scanned,
    }
}

fn scan_last_seq(path: &Path) -> u64 {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return 0,
    };
    if size == 0 {
        return 0;
    }
    let mut window = (64 * 1024).min(size);
    loop {
        let start = size.saturating_sub(window);
        let text = read_range(path, start, size - start).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
        if let Some(found) = last_seq_from_text(&text, start > 0) {
            return found;
        }
        if start == 0 {
            return 0;
        }
        if window >= (1024 * 1024).min(size) {
            let whole = read_range(path, 0, size).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
            return last_seq_from_text(&whole, false).unwrap_or(0);
        }
        window = (window * 2).min(1024 * 1024).min(size);
    }
}

fn read_sidecar_seq(path: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(event_log_sidecar_path(path)).ok()?;
    let raw = raw.trim();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u64>().ok().filter(|v| *v <= 9_007_199_254_740_991)
}

fn write_sidecar_seq(path: &Path, seq: u64) {
    let sidecar = event_log_sidecar_path(path);
    let temporary = PathBuf::from(format!("{}.{}.tmp", sidecar.display(), std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&temporary)?;
        f.write_all(format!("{seq}\n").as_bytes())?;
        drop(f);
        std::fs::rename(&temporary, &sidecar)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

fn is_valid_envelope_line(line: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<Value>(line) else { return false };
    let Some(o) = parsed.as_object() else { return false };
    o.get("v").and_then(Value::as_f64) == Some(1.0)
        && safe_seq(o.get("seq")).is_some()
        && o.get("type").map(Value::is_string).unwrap_or(false)
        && o.get("sessionId").map(Value::is_string).unwrap_or(false)
        && o.contains_key("event")
}

/// Drops a half-written final line (or adds the missing newline to a complete one).
fn repair_torn_tail(path: &Path, on_error: &dyn Fn(String)) {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if size == 0 {
        return;
    }
    let result = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new().read(true).write(true).open(path)?;
        let mut last = [0u8; 1];
        f.seek(SeekFrom::Start(size - 1))?;
        let has_newline = f.read(&mut last)? == 1 && last[0] == b'\n';
        let end = if has_newline { size - 1 } else { size };
        let mut start = 0u64;
        let mut window = 64 * 1024u64;
        let mut position = end;
        while position > 0 {
            let from = position.saturating_sub(window);
            let chunk = read_range(path, from, position - from)?;
            if let Some(nl) = chunk.iter().rposition(|b| *b == b'\n') {
                start = from + nl as u64 + 1;
                break;
            }
            position = from;
            if window >= 2 * 1024 * 1024 {
                break;
            }
            window = (window * 2).min(2 * 1024 * 1024);
        }
        if end <= start {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&read_range(path, start, end - start)?).to_string();
        if !is_valid_envelope_line(&line) {
            f.set_len(start)?;
        } else if !has_newline {
            f.seek(SeekFrom::Start(size))?;
            f.write_all(b"\n")?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        on_error(format!("cannot repair {}: {error}", path.display()));
    }
}

const EDIT_TOOLS: [&str; 3] = ["write_file", "edit_file", "apply_patch"];

fn mentions_sensitive_path(value: Option<&Value>) -> bool {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    static KEY: OnceLock<Regex> = OnceLock::new();
    let Some(text) = value.and_then(Value::as_str).filter(|s| !s.is_empty()) else { return false };
    if KEY.get_or_init(|| Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").unwrap()).is_match(text) {
        return true;
    }
    if is_sensitive_path(text) {
        return true;
    }
    for m in TOKEN.get_or_init(|| Regex::new(r#"[^\s"'`,;()\[\]{}<>]+"#).unwrap()).find_iter(text) {
        let token = m.as_str().trim_start_matches(['+', '-', '*']).trim_end_matches(['.', ':']);
        if !token.is_empty() && is_sensitive_path(token) {
            return true;
        }
    }
    false
}

fn touches_sensitive_path(event: &Value) -> bool {
    let mut candidates: Vec<Option<&Value>> = vec![];
    if let Some(meta) = event.get("meta").filter(|m| m.is_object()) {
        candidates.extend([meta.get("summary"), meta.get("diff"), meta.get("outputPreview")]);
    }
    if event.get("type").and_then(Value::as_str) == Some("tool_stream") {
        candidates.push(event.get("preview"));
    }
    candidates.into_iter().any(mentions_sensitive_path)
}

/// Keeps a tool event's path/counts but drops file content when it touched a sensitive path.
pub fn strip_sensitive_content(event: &Value) -> Value {
    match event.get("type").and_then(Value::as_str) {
        Some("tool") => {
            if !touches_sensitive_path(event) {
                return event.clone();
            }
            let meta = event.get("meta").and_then(Value::as_object).cloned().unwrap_or_default();
            let mut safe = Map::new();
            for key in ["kind", "summary", "verb", "added", "removed", "durationMs", "exitCode"] {
                if let Some(v) = meta.get(key) {
                    safe.insert(key.into(), v.clone());
                }
            }
            let mut out = event.clone();
            out["meta"] = Value::Object(safe);
            out
        }
        Some("tool_stream") => {
            let name = event.get("name").and_then(Value::as_str).unwrap_or("");
            if EDIT_TOOLS.contains(&name) || touches_sensitive_path(event) {
                let mut out = event.clone();
                out["preview"] = json!("");
                return out;
            }
            event.clone()
        }
        _ => event.clone(),
    }
}

/// Append-only JSONL writer. A write failure is reported and never stops the run.
pub struct EventLogWriter {
    file_path: PathBuf,
    fd: Option<File>,
    seq: u64,
    session_id: String,
    text_redactor: StreamRedactor,
    reasoning_redactor: StreamRedactor,
    broken: bool,
    secrets: Vec<String>,
    on_error: Box<dyn Fn(String) + Send + Sync>,
}

impl EventLogWriter {
    pub fn open(path: &Path, session_id: Option<&str>, extra_secrets: &[String], on_error: Box<dyn Fn(String) + Send + Sync>) -> Self {
        let mut secrets = collect_secrets();
        secrets.extend(extra_secrets.iter().cloned());
        let mut writer = EventLogWriter {
            file_path: path.to_path_buf(),
            fd: None,
            seq: 0,
            session_id: session_id.map(str::to_string).unwrap_or_else(|| session_id_from_event_log_path(path)),
            text_redactor: StreamRedactor::new(&secrets),
            reasoning_redactor: StreamRedactor::new(&secrets),
            broken: false,
            secrets,
            on_error,
        };
        repair_torn_tail(&writer.file_path, &*writer.on_error);
        writer.seq = read_last_seq(&writer.file_path);
        writer.open_fd();
        writer
    }

    pub fn path(&self) -> &Path {
        &self.file_path
    }

    pub fn last_seq(&self) -> u64 {
        self.seq
    }

    fn open_fd(&mut self) {
        let result = (|| -> std::io::Result<File> {
            if let Some(dir) = self.file_path.parent() {
                crate::config::mkdir_private(dir)?;
            }
            let mut options = OpenOptions::new();
            options.append(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let f = options.open(&self.file_path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
            }
            Ok(f)
        })();
        match result {
            Ok(f) => self.fd = Some(f),
            Err(error) => {
                self.fd = None;
                (self.on_error)(format!("cannot open {}: {error}", self.file_path.display()));
            }
        }
    }

    pub fn append(&mut self, event: &Value) {
        if self.fd.is_none() {
            return;
        }
        if self.broken {
            repair_torn_tail(&self.file_path, &*self.on_error);
            self.broken = false;
            self.seq = read_last_seq(&self.file_path);
        }
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "done" || kind == "error" || kind == "usage" {
            self.flush_streams();
        }
        let stripped = strip_sensitive_content(event);
        if let Some(redacted) = self.redact_stream(stripped) {
            self.write_envelope(redacted);
        }
    }

    fn redact_stream(&mut self, mut event: Value) -> Option<Value> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let Some(text) = event.get("text").and_then(Value::as_str).map(str::to_string) else { return Some(event) };
        let safe = match kind.as_str() {
            "text" => self.text_redactor.push(&text),
            "reasoning" => self.reasoning_redactor.push(&text),
            _ => return Some(event),
        };
        if safe.is_empty() {
            return None;
        }
        event["text"] = Value::String(safe);
        Some(event)
    }

    fn flush_streams(&mut self) {
        if self.fd.is_none() {
            return;
        }
        let text = self.text_redactor.flush_all();
        if !text.is_empty() {
            self.write_envelope(json!({ "type": "text", "text": text }));
        }
        let reasoning = self.reasoning_redactor.flush_all();
        if !reasoning.is_empty() {
            self.write_envelope(json!({ "type": "reasoning", "text": reasoning, "source": "provider" }));
        }
    }

    fn write_envelope(&mut self, event: Value) {
        let Some(fd) = self.fd.as_mut() else { return };
        let safe = redact_object(&event, &self.secrets);
        let seq = self.seq + 1;
        let kind = safe.get("type").and_then(Value::as_str).unwrap_or("unknown").to_string();
        let envelope = EventEnvelope { seq, at: crate::jsutil::now_iso(), session_id: self.session_id.clone(), kind, event: safe };
        let payload = format!("{}\n", crate::jsutil::stringify(&envelope.to_value()));
        match fd.write_all(payload.as_bytes()).and_then(|_| fd.sync_data()) {
            Ok(()) => {
                self.seq = seq;
                write_sidecar_seq(&self.file_path, seq);
            }
            Err(error) => {
                self.broken = true;
                (self.on_error)(format!("cannot append to {}: {error}", self.file_path.display()));
            }
        }
    }

    /// Flushes the withheld stream tail and reopens on the next session's log.
    pub fn switch_session(&mut self, session_id: &str, path: Option<PathBuf>) {
        self.flush_streams();
        self.fd = None;
        let dir = self.file_path.parent().map(Path::to_path_buf).unwrap_or_default();
        self.file_path = path.unwrap_or_else(|| dir.join(format!("{session_id}.events.jsonl")));
        self.session_id = session_id.into();
        self.broken = false;
        repair_torn_tail(&self.file_path, &*self.on_error);
        self.seq = read_last_seq(&self.file_path);
        self.text_redactor = StreamRedactor::new(&self.secrets);
        self.reasoning_redactor = StreamRedactor::new(&self.secrets);
        self.open_fd();
    }

    pub fn close(&mut self) {
        self.flush_streams();
        self.fd = None;
    }
}

pub struct ReadEventLogResult {
    pub envelopes: Vec<EventEnvelope>,
    pub truncated: bool,
}

/// A malformed final line is a torn append (`truncated`); malformed middle lines are corruption.
pub fn read_event_log(path: &Path) -> Result<ReadEventLogResult> {
    let text = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = text.split('\n').collect();
    let last_non_empty = lines.iter().rposition(|l| !l.trim().is_empty());
    let mut envelopes: Vec<EventEnvelope> = vec![];
    let mut truncated = false;
    let session_id = session_id_from_event_log_path(path);
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(error) => {
                if Some(i) == last_non_empty {
                    truncated = true;
                    break;
                }
                bail!("Invalid JSON in event log {} at line {}: {error}", path.display(), i + 1);
            }
        };
        let outcome = normalize_event_envelope(&parsed, Some(&session_id), None).and_then(|envelope| {
            if let Some(previous) = envelopes.last() {
                if envelope.seq <= previous.seq {
                    bail!("seq must increase ({} -> {})", previous.seq, envelope.seq);
                }
            }
            Ok(envelope)
        });
        match outcome {
            Ok(envelope) => envelopes.push(envelope),
            Err(error) => {
                if Some(i) == last_non_empty {
                    truncated = true;
                    break;
                }
                bail!("Invalid event envelope in {} at line {}: {error}", path.display(), i + 1);
            }
        }
    }
    Ok(ReadEventLogResult { envelopes, truncated })
}

pub fn find_seq_gap(previous: Option<&EventEnvelope>, next: &EventEnvelope) -> bool {
    previous.map(|p| next.seq != p.seq + 1).unwrap_or(false)
}

pub fn is_event_log_path(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".events.jsonl") && path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_redacts_and_repairs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions").join("abc.events.jsonl");
        let secret = "sk-test-secret-value-1234567890".to_string();
        let mut w = EventLogWriter::open(&path, None, std::slice::from_ref(&secret), Box::new(|_| {}));
        w.append(&json!({"type": "text", "text": "key sk-test-secret-"}));
        w.append(&json!({"type": "text", "text": "value-1234567890 end"}));
        w.append(&json!({"type": "done", "text": "x"}));
        w.close();
        let log = read_event_log(&path).unwrap();
        assert_eq!(log.envelopes.len(), 2);
        assert_eq!(log.envelopes[0].kind, "text");
        assert!(!log.envelopes[0].event["text"].as_str().unwrap().contains("value-1234567890"));
        assert_eq!(log.envelopes[1].seq, 2);
        // Torn tail is dropped on reopen and sequence continues.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"v\":1,\"seq\":3").unwrap();
        drop(f);
        assert!(read_event_log(&path).unwrap().truncated);
        let mut w = EventLogWriter::open(&path, None, &[], Box::new(|_| {}));
        assert_eq!(w.last_seq(), 2);
        w.append(&json!({"type": "error", "message": "m"}));
        w.close();
        let log = read_event_log(&path).unwrap();
        assert!(!log.truncated);
        assert_eq!(log.envelopes.last().unwrap().seq, 3);
    }

    #[test]
    fn strips_sensitive_tool_content() {
        let event = json!({"type": "tool", "name": "read_file", "meta": {"kind": "read", "summary": "Read .env", "outputPreview": "SECRET=1"}});
        let out = strip_sensitive_content(&event);
        assert!(out["meta"].get("outputPreview").is_none());
        let stream = json!({"type": "tool_stream", "name": "write_file", "preview": "abc"});
        assert_eq!(strip_sensitive_content(&stream)["preview"], json!(""));
    }
}
