//! HTTP providers (OpenAI-compatible chat, OpenAI Responses, Anthropic Messages) and the offline
//! stub. Each provider converts `ModelMessage` JSON to its wire format and maps the SSE stream to
//! normalized [`StreamPart`]s.

pub mod anthropic;
pub mod openai_compat;
pub mod openai_responses;
pub mod stub;

use crate::model::{ApiError, StreamPart};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!("goon-cli/{}", crate::version::VERSION))
            .build()
            .expect("http client")
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental `text/event-stream` parser (WHATWG semantics for the fields providers use).
#[derive(Default)]
pub struct SseParser {
    buffer: String,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        let mut out = vec![];
        while let Some(pos) = self.buffer.find('\n') {
            let mut line = self.buffer[..pos].to_string();
            self.buffer.drain(..=pos);
            if line.ends_with('\r') {
                line.pop();
            }
            self.line(&line, &mut out);
        }
        out
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = vec![];
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.line(line.trim_end_matches('\r'), &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event = None;
            return;
        }
        out.push(SseEvent { event: self.event.take(), data: self.data.join("\n") });
        self.data.clear();
    }
}

fn header_map(response: &reqwest::Response) -> BTreeMap<String, String> {
    response
        .headers()
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_lowercase(), v.to_str().ok()?.to_string())))
        .collect()
}

fn status_text(status: reqwest::StatusCode) -> String {
    status.canonical_reason().unwrap_or("Error").to_string()
}

/// Turns an error response body into an `ApiError` (provider `error.message` when present).
fn error_from_body(status: reqwest::StatusCode, headers: BTreeMap<String, String>, body: &str) -> ApiError {
    let message = if body.trim().is_empty() {
        status_text(status)
    } else {
        match serde_json::from_str::<Value>(body) {
            Ok(parsed) => parsed
                .get("error")
                .and_then(|e| e.get("message").and_then(Value::as_str).map(str::to_string).or_else(|| e.as_str().map(str::to_string)))
                .or_else(|| parsed.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| status_text(status)),
            Err(_) => status_text(status),
        }
    };
    ApiError::http(status.as_u16(), message, headers)
}

/// POSTs a JSON body and streams parsed SSE events. Cancellation and the request deadline both end
/// the stream with an aborted error.
pub async fn post_sse(
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    cancel: CancellationToken,
    timeout: Duration,
) -> Result<mpsc::Receiver<Result<SseEvent, ApiError>>, ApiError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut request = http_client().post(url).header("content-type", "application/json").header("accept", "text/event-stream");
    for (k, v) in headers {
        request = request.header(k.as_str(), v.as_str());
    }
    let payload = crate::jsutil::stringify(body);
    let send = request.body(payload).send();
    let response = tokio::select! {
        r = send => r.map_err(|e| ApiError::network(network_message(&e)))?,
        _ = cancel.cancelled() => return Err(ApiError::aborted()),
        _ = tokio::time::sleep_until(deadline) => return Err(ApiError::aborted()),
    };
    let status = response.status();
    let response_headers = header_map(&response);
    if !status.is_success() {
        let text = tokio::select! {
            t = response.text() => t.unwrap_or_default(),
            _ = cancel.cancelled() => return Err(ApiError::aborted()),
            _ = tokio::time::sleep_until(deadline) => return Err(ApiError::aborted()),
        };
        return Err(error_from_body(status, response_headers, &text));
    }
    let (tx, rx) = mpsc::channel::<Result<SseEvent, ApiError>>(256);
    tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        let mut parser = SseParser::default();
        let mut pending: Vec<u8> = vec![];
        loop {
            let next = tokio::select! {
                n = stream.next() => n,
                _ = cancel.cancelled() => { let _ = tx.send(Err(ApiError::aborted())).await; return; }
                _ = tokio::time::sleep_until(deadline) => { let _ = tx.send(Err(ApiError::aborted())).await; return; }
            };
            match next {
                Some(Ok(bytes)) => {
                    pending.extend_from_slice(&bytes);
                    // Only decode complete UTF-8 sequences; keep a split character for the next chunk.
                    let valid = match std::str::from_utf8(&pending) {
                        Ok(_) => pending.len(),
                        Err(e) => e.valid_up_to(),
                    };
                    let text = String::from_utf8_lossy(&pending[..valid]).to_string();
                    pending.drain(..valid);
                    for event in parser.push(&text) {
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                }
                Some(Err(error)) => {
                    let _ = tx.send(Err(ApiError::network(network_message(&error)))).await;
                    return;
                }
                None => {
                    if !pending.is_empty() {
                        let text = String::from_utf8_lossy(&pending).to_string();
                        for event in parser.push(&text) {
                            let _ = tx.send(Ok(event)).await;
                        }
                    }
                    for event in parser.finish() {
                        let _ = tx.send(Ok(event)).await;
                    }
                    return;
                }
            }
        }
    });
    Ok(rx)
}

/// POSTs a JSON body and parses the JSON response (non-streaming `generate`).
pub async fn post_json(url: &str, headers: &[(String, String)], body: &Value, cancel: CancellationToken, timeout: Duration) -> Result<Value, ApiError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut request = http_client().post(url).header("content-type", "application/json");
    for (k, v) in headers {
        request = request.header(k.as_str(), v.as_str());
    }
    let send = request.body(crate::jsutil::stringify(body)).send();
    let response = tokio::select! {
        r = send => r.map_err(|e| ApiError::network(network_message(&e)))?,
        _ = cancel.cancelled() => return Err(ApiError::aborted()),
        _ = tokio::time::sleep_until(deadline) => return Err(ApiError::aborted()),
    };
    let status = response.status();
    let response_headers = header_map(&response);
    let text = tokio::select! {
        t = response.text() => t.map_err(|e| ApiError::network(network_message(&e)))?,
        _ = cancel.cancelled() => return Err(ApiError::aborted()),
        _ = tokio::time::sleep_until(deadline) => return Err(ApiError::aborted()),
    };
    if !status.is_success() {
        return Err(error_from_body(status, response_headers, &text));
    }
    serde_json::from_str(&text).map_err(|e| ApiError::new(format!("Invalid JSON response: {e}")))
}

/// Request body for a non-streaming call: the streaming body without its stream fields.
pub fn without_stream(mut body: Value) -> Value {
    if let Some(o) = body.as_object_mut() {
        o.remove("stream");
        o.remove("stream_options");
    }
    body
}

pub fn network_message(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        message = format!("{message}: {inner}");
        source = inner.source();
    }
    if error.is_connect() {
        return format!("Cannot connect to API: {message}");
    }
    message
}

/// Bridges a provider state machine running in a task to a `PartStream`.
pub fn part_channel() -> (mpsc::Sender<StreamPart>, crate::model::PartStream) {
    let (tx, rx) = mpsc::channel::<StreamPart>(256);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|part| (part, rx)) }).boxed();
    (tx, stream)
}

/// Common request headers (session affinity + request id).
pub fn session_headers(session_id: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![("x-request-id".to_string(), uuid::Uuid::new_v4().to_string())];
    if let Some(id) = session_id {
        headers.push(("x-session-id".into(), id.into()));
        headers.push(("x-opencode-session".into(), id.into()));
    }
    headers
}

pub fn generate_id() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    raw[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_handles_split_chunks() {
        let mut parser = SseParser::default();
        assert!(parser.push("data: {\"a\"").is_empty());
        let events = parser.push(":1}\n\nevent: x\ndata: y\r\n\r\n");
        assert_eq!(events, vec![SseEvent { event: None, data: "{\"a\":1}".into() }, SseEvent { event: Some("x".into()), data: "y".into() }]);
        assert_eq!(parser.push("data: tail"), vec![]);
        assert_eq!(parser.finish(), vec![SseEvent { event: None, data: "tail".into() }]);
    }
}
