//! Shared helpers for the library-level tests (ported from the TypeScript `node:test` suite).
#![allow(dead_code, unused_imports)]

pub use anyhow::{anyhow, bail, Result};
pub use futures_util::future::BoxFuture;
pub use goon::*;
pub use serde_json::{json, Map, Value};
pub use std::collections::{BTreeSet, HashMap, HashSet};
pub use std::path::{Path, PathBuf};
pub use std::sync::atomic::{AtomicUsize, Ordering};
pub use std::sync::{Arc, Mutex};
pub use tokio_util::sync::CancellationToken;

use std::sync::OnceLock;

/// A temporary `{root, cwd, config, data}` layout like the JS `fixture(t)`.
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
}

pub fn fixture() -> Fixture {
    let dir = tempfile::Builder::new().prefix("goon-test-").tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let cwd = root.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    Fixture { config: root.join("config"), data: root.join("data"), cwd, root, dir }
}

impl Fixture {
    pub fn write(&self, rel: &str, content: &str) -> PathBuf {
        let file = self.cwd.join(rel);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, content).unwrap();
        file
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.cwd.join(rel)).unwrap()
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.cwd.join(rel).exists()
    }
}

/// Serializes tests that mutate process environment variables (config/data dirs, GOON_*).
pub struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn env_mutex() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn env(vars: &[(&str, Option<&str>)]) -> EnvGuard {
    let lock = env_mutex().lock().unwrap_or_else(|p| p.into_inner());
    let mut saved = vec![];
    for (key, value) in vars {
        saved.push((key.to_string(), std::env::var(key).ok()));
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
    EnvGuard { saved, _lock: lock }
}

/// Points config/data dirs at the fixture and clears GOON_* overrides.
pub fn fixture_env(f: &Fixture) -> EnvGuard {
    let config = f.config.to_string_lossy().to_string();
    let data = f.data.to_string_lossy().to_string();
    let mut vars: Vec<(&str, Option<&str>)> = vec![("XDG_CONFIG_HOME", Some(&config)), ("XDG_DATA_HOME", Some(&data)), ("GOON_HOME", None)];
    let cleared: Vec<String> = std::env::vars().map(|(k, _)| k).filter(|k| k.starts_with("GOON_")).collect();
    for key in &cleared {
        vars.push((key.as_str(), None));
    }
    let guard = env(&vars);
    guard
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
    }
}

pub fn token() -> CancellationToken {
    CancellationToken::new()
}

pub fn call(id: &str, name: &str, input: Value) -> tools::ToolCall {
    tools::ToolCall { tool_call_id: id.into(), tool_name: name.into(), input }
}

pub fn exec(f: impl Fn(Value) -> Result<String> + Send + Sync + 'static) -> tools::ToolExec {
    let f = Arc::new(f);
    Arc::new(move |input, _cancel| {
        let f = f.clone();
        Box::pin(async move { f(input) })
    })
}

pub fn exec_ok(text: &'static str) -> tools::ToolExec {
    exec(move |_| Ok(text.to_string()))
}

/// An approval callback that records every request and answers with `answer`.
pub fn approver(answer: bool) -> (tools::Approve, Arc<Mutex<Vec<tools::ApprovalRequest>>>) {
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    let approve: tools::Approve = Arc::new(move |request| {
        log.lock().unwrap().push(request);
        Box::pin(async move { answer })
    });
    (approve, seen)
}

pub fn registry(cwd: &Path, approve: tools::Approve, read_only: bool) -> Arc<tools::ToolRegistry> {
    tools::ToolRegistry::new(cwd, approve, read_only, None)
}

pub fn register(reg: &tools::ToolRegistry, name: &str, read_only: bool, risk: Option<risk::RiskClass>, trusted: Option<bool>, exec: tools::ToolExec) {
    reg.register_json(name, name, &json!({ "type": "object" }), read_only, risk, None, trusted, exec).unwrap();
}

/// A runtime config for an OpenAI-compatible endpoint, like the objects the JS tests built.
pub fn runtime(cwd: &Path, data: &Path, base_url: &str, extra: Value) -> Arc<config::RuntimeConfig> {
    let mut raw = json!({ "provider": "openai-compatible", "model": "test-model", "baseUrl": base_url, "contextWindow": 128000, "maxOutputTokens": 8192, "timeoutMs": 5000 });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        raw[k] = v;
    }
    let config = config::parse_config(&raw).unwrap();
    let model = config.model.clone().unwrap_or_default();
    Arc::new(config::RuntimeConfig { config, cwd: cwd.to_path_buf(), api_key: "fake".into(), data_dir: data.to_path_buf(), base_url: base_url.into(), model, stub_cursor: None })
}

pub fn store(rt: &config::RuntimeConfig) -> Arc<session::SessionStore> {
    Arc::new(session::SessionStore::open(&rt.data_dir.join("sessions"), session::SessionRoute::from_runtime(rt), None, None).unwrap())
}

/// Collects harness events.
pub fn events() -> (harness::Emit, Arc<Mutex<Vec<Value>>>) {
    let log = Arc::new(Mutex::new(vec![]));
    let sink = log.clone();
    (Arc::new(move |e| sink.lock().unwrap().push(e)), log)
}

// ---------------------------------------------------------------------------------------------
// Mock OpenAI-compatible provider (the JS `provider(t, handler)` + `sse(...)` helpers)
// ---------------------------------------------------------------------------------------------

pub struct MockResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// Delay before the response is written (to exercise timeouts / cancellation).
    pub delay_ms: u64,
}

#[derive(Clone, Default)]
pub struct ToolCallSpec {
    pub id: String,
    pub name: String,
    pub input: Value,
}

pub fn tc(id: &str, name: &str, input: Value) -> ToolCallSpec {
    ToolCallSpec { id: id.into(), name: name.into(), input }
}

#[derive(Default)]
pub struct Sse<'a> {
    pub text: &'a str,
    pub calls: Vec<ToolCallSpec>,
    pub reasoning: &'a str,
    pub finish_reason: Option<&'a str>,
}

/// Streaming chat completion chunks, shaped exactly like the JS `sse()` helper.
pub fn sse(spec: Sse) -> MockResponse {
    let mut body = String::new();
    let chunk = |body: &mut String, delta: Value| {
        body.push_str(&format!("data: {}\n\n", json!({ "id": "chatcmpl-test", "object": "chat.completion.chunk", "created": 1, "model": "test-model", "choices": [{ "index": 0, "delta": delta, "finish_reason": null }] })));
    };
    chunk(&mut body, json!({ "role": "assistant" }));
    if !spec.reasoning.is_empty() {
        chunk(&mut body, json!({ "reasoning_content": spec.reasoning }));
    }
    for (index, call) in spec.calls.iter().enumerate() {
        let args = jsutil::stringify(&call.input);
        let split = args.char_indices().nth(3).map(|(i, _)| i).unwrap_or(args.len());
        chunk(&mut body, json!({ "tool_calls": [{ "index": index, "id": call.id, "type": "function", "function": { "name": call.name, "arguments": &args[..split] } }] }));
        chunk(&mut body, json!({ "tool_calls": [{ "index": index, "function": { "arguments": &args[split..] } }] }));
    }
    if !spec.text.is_empty() {
        let split = spec.text.char_indices().nth(2).map(|(i, _)| i).unwrap_or(spec.text.len());
        for part in [&spec.text[..split], &spec.text[split..]] {
            chunk(&mut body, json!({ "content": part }));
        }
    }
    let finish = spec.finish_reason.unwrap_or(if spec.calls.is_empty() { "stop" } else { "tool_calls" });
    body.push_str(&format!(
        "data: {}\n\n",
        json!({ "id": "chatcmpl-test", "object": "chat.completion.chunk", "choices": [{ "index": 0, "delta": {}, "finish_reason": finish }], "usage": { "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110 } })
    ));
    body.push_str("data: [DONE]\n\n");
    MockResponse { status: 200, content_type: "text/event-stream", headers: vec![], body, delay_ms: 0 }
}

pub fn text(t: &str) -> MockResponse {
    sse(Sse { text: t, ..Default::default() })
}

pub fn calls(list: Vec<ToolCallSpec>) -> MockResponse {
    sse(Sse { calls: list, ..Default::default() })
}

pub fn json_response(status: u16, body: Value) -> MockResponse {
    MockResponse { status, content_type: "application/json", headers: vec![], body: body.to_string(), delay_ms: 0 }
}

pub type Handler = Arc<dyn Fn(&Value, usize) -> MockResponse + Send + Sync>;

pub struct MockProvider {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
    shutdown: CancellationToken,
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl MockProvider {
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

/// Starts a loopback HTTP server answering every POST with `handler(body, nth)` (1-based).
pub async fn provider(handler: impl Fn(&Value, usize) -> MockResponse + Send + Sync + 'static) -> MockProvider {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(vec![]));
    let shutdown = CancellationToken::new();
    let handler: Handler = Arc::new(handler);
    let reqs = requests.clone();
    let stop = shutdown.clone();
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = stop.cancelled() => break,
                a = listener.accept() => a,
            };
            let Ok((mut socket, _)) = accepted else { continue };
            let handler = handler.clone();
            let reqs = reqs.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    // Read one request (headers + content-length body); keep-alive capable.
                    let mut buf = Vec::new();
                    let header_end = loop {
                        let mut chunk = [0u8; 8192];
                        let n = tokio::select! {
                            _ = stop.cancelled() => return,
                            n = socket.read(&mut chunk) => n.unwrap_or(0),
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    while buf.len() < header_end + length {
                        let mut chunk = [0u8; 8192];
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let body = String::from_utf8_lossy(&buf[header_end..header_end + length]).to_string();
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let nth = {
                        let mut r = reqs.lock().unwrap();
                        r.push(parsed.clone());
                        r.len()
                    };
                    let response = handler(&parsed, nth);
                    if response.delay_ms > 0 {
                        tokio::select! {
                            _ = stop.cancelled() => return,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(response.delay_ms)) => {}
                        }
                    }
                    let mut out = format!("HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\n", response.status, response.content_type, response.body.len());
                    for (k, v) in &response.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    out.push_str(&response.body);
                    if socket.write_all(out.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    MockProvider { url: format!("http://127.0.0.1:{port}/v1"), requests, shutdown }
}

/// The text of every `role: tool` message the provider received in request `body`.
pub fn tool_results(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .map(|m| m.iter().filter(|m| m["role"] == "tool").map(|m| m["content"].as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default()
}

pub fn assert_match(text: &str, pattern: &str) {
    let re = regex::Regex::new(pattern).unwrap();
    assert!(re.is_match(text), "expected {text:?} to match /{pattern}/");
}

// ---------------------------------------------------------------------------------------------
// Scripted in-process model (the JS `makeFakeModel(turns)` helper)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct Turn {
    pub text: String,
    pub calls: Vec<ToolCallSpec>,
    pub reasoning: String,
}

pub fn say(text: &str) -> Turn {
    Turn { text: text.into(), ..Default::default() }
}

pub fn invoke(list: Vec<ToolCallSpec>) -> Turn {
    Turn { calls: list, ..Default::default() }
}

pub struct FakeModel {
    pub turns: Mutex<std::collections::VecDeque<Turn>>,
    pub requests: Mutex<Vec<model::ModelRequest>>,
}

impl FakeModel {
    pub fn new(turns: Vec<Turn>) -> Arc<FakeModel> {
        Arc::new(FakeModel { turns: Mutex::new(turns.into()), requests: Mutex::new(vec![]) })
    }
    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl model::LanguageModel for FakeModel {
    fn model_id(&self) -> &str {
        "test-model"
    }
    fn provider(&self) -> &str {
        "test"
    }
    fn stream(&self, request: model::ModelRequest) -> BoxFuture<'static, Result<model::PartStream, model::ApiError>> {
        let cancel = request.cancel.clone();
        self.requests.lock().unwrap().push(request);
        let turn = self.turns.lock().unwrap().pop_front().unwrap_or_else(|| say("[stub] script exhausted"));
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(model::ApiError::aborted());
            }
            use model::StreamPart as P;
            let mut parts = vec![];
            if !turn.reasoning.is_empty() {
                parts.push(P::ReasoningDelta { id: "r1".into(), delta: turn.reasoning.clone(), provider_metadata: None });
                parts.push(P::ReasoningEnd { id: "r1".into(), provider_metadata: None });
            }
            if !turn.text.is_empty() {
                parts.push(P::TextDelta { id: "t1".into(), delta: turn.text.clone(), provider_metadata: None });
                parts.push(P::TextEnd { id: "t1".into(), provider_metadata: None });
            }
            for (i, call) in turn.calls.iter().enumerate() {
                let id = if call.id.is_empty() { format!("call-{i}") } else { call.id.clone() };
                let input = jsutil::stringify(&call.input);
                parts.push(P::ToolInputStart { id: id.clone(), tool_name: call.name.clone() });
                parts.push(P::ToolInputDelta { id: id.clone(), delta: input.clone() });
                parts.push(P::ToolCall { id, tool_name: call.name.clone(), input, provider_metadata: None });
            }
            let finish_reason = if turn.calls.is_empty() { model::FinishReason::Stop } else { model::FinishReason::ToolCalls };
            parts.push(P::Finish { usage: model::Usage { input_tokens: Some(10), output_tokens: Some(5), ..Default::default() }, finish_reason });
            Ok(Box::pin(futures_util::stream::iter(parts)) as model::PartStream)
        })
    }
}

/// A verify runner that always succeeds.
pub struct PassRunner;

impl verify::VerifyRunner for PassRunner {
    fn run(&self, _command: String, _args: Vec<String>, _opts: verify::RunnerOptions) -> BoxFuture<'static, verify::RunnerResult> {
        Box::pin(async { verify::RunnerResult { code: 0, output: String::new(), timed_out: false, not_found: false } })
    }
}
