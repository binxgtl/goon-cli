//! Provider-neutral model interface: requests, normalized stream parts, usage and API errors.
//!
//! Messages are kept in the AI SDK `ModelMessage` JSON shape (the format stored in session files),
//! so every provider converts from the same representation.

use futures_util::stream::BoxStream;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub system: Option<String>,
    /// AI SDK `ModelMessage` values (`role` + `content`, optional `providerOptions`).
    pub messages: Vec<Value>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u64,
    /// Reasoning effort for this request (already resolved: configured or auxiliary `low`).
    pub effort: Option<String>,
    pub cancel: CancellationToken,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// `totalTokens` as the AI SDK computes it (input + output; undefined when both are missing).
    pub fn total_tokens(&self) -> Option<u64> {
        if self.input_tokens.is_none() && self.output_tokens.is_none() {
            return None;
        }
        Some(self.input_tokens.unwrap_or(0) + self.output_tokens.unwrap_or(0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    ToolCalls,
    Error,
    Other,
    Unknown,
}

impl FinishReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::Length => "length",
            FinishReason::ContentFilter => "content-filter",
            FinishReason::ToolCalls => "tool-calls",
            FinishReason::Error => "error",
            FinishReason::Other => "other",
            FinishReason::Unknown => "unknown",
        }
    }
}

/// Normalized stream events (the subset of LanguageModelV3 stream parts the harness consumes).
#[derive(Clone, Debug)]
pub enum StreamPart {
    TextDelta {
        id: String,
        delta: String,
        provider_metadata: Option<Value>,
    },
    ReasoningDelta {
        id: String,
        delta: String,
        provider_metadata: Option<Value>,
    },
    /// Attaches provider metadata (signature, encrypted content, item id) to a reasoning block.
    ReasoningEnd {
        id: String,
        provider_metadata: Option<Value>,
    },
    TextEnd {
        id: String,
        provider_metadata: Option<Value>,
    },
    ToolInputStart {
        id: String,
        tool_name: String,
    },
    ToolInputDelta {
        id: String,
        delta: String,
    },
    ToolCall {
        id: String,
        tool_name: String,
        input: String,
        provider_metadata: Option<Value>,
    },
    Finish {
        usage: Usage,
        finish_reason: FinishReason,
    },
    Error(ApiError),
}

/// `APICallError` equivalent.
#[derive(Clone, Debug)]
pub struct ApiError {
    pub message: String,
    pub status: Option<u16>,
    pub retryable: bool,
    pub headers: BTreeMap<String, String>,
    pub aborted: bool,
}

impl ApiError {
    pub fn new(message: impl Into<String>) -> Self {
        ApiError { message: message.into(), status: None, retryable: false, headers: BTreeMap::new(), aborted: false }
    }

    pub fn http(status: u16, message: impl Into<String>, headers: BTreeMap<String, String>) -> Self {
        let retryable = status == 408 || status == 409 || status == 429 || status >= 500;
        ApiError { message: message.into(), status: Some(status), retryable, headers, aborted: false }
    }

    pub fn network(message: impl Into<String>) -> Self {
        ApiError { message: message.into(), status: None, retryable: true, headers: BTreeMap::new(), aborted: false }
    }

    pub fn aborted() -> Self {
        ApiError { message: "Model request cancelled or timed out.".into(), status: None, retryable: false, headers: BTreeMap::new(), aborted: true }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

pub type PartStream = BoxStream<'static, StreamPart>;

/// Result of a non-streaming call (`generateText` for compaction, /recap, /btw).
#[derive(Clone, Debug)]
pub struct GenerateResponse {
    pub text: String,
    pub reasoning: String,
    pub finish_reason: FinishReason,
    pub usage: Usage,
}

/// A chat model that streams normalized parts.
pub trait LanguageModel: Send + Sync {
    fn model_id(&self) -> &str;
    fn provider(&self) -> &str;
    fn stream(&self, request: ModelRequest) -> futures_util::future::BoxFuture<'static, Result<PartStream, ApiError>>;

    /// Non-streaming generation. The default collects the stream (used by the offline stub).
    fn generate(&self, request: ModelRequest) -> futures_util::future::BoxFuture<'static, Result<GenerateResponse, ApiError>> {
        let stream = self.stream(request);
        Box::pin(async move {
            use futures_util::StreamExt;
            let mut parts = stream.await?;
            let mut out = GenerateResponse { text: String::new(), reasoning: String::new(), finish_reason: FinishReason::Unknown, usage: Usage::default() };
            while let Some(part) = parts.next().await {
                match part {
                    StreamPart::TextDelta { delta, .. } => out.text.push_str(&delta),
                    StreamPart::ReasoningDelta { delta, .. } => out.reasoning.push_str(&delta),
                    StreamPart::Finish { usage, finish_reason } => {
                        out.usage = usage;
                        out.finish_reason = finish_reason;
                    }
                    StreamPart::Error(error) => return Err(error),
                    _ => {}
                }
            }
            Ok(out)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// ModelMessage helpers
// ---------------------------------------------------------------------------------------------

pub fn user_text(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

pub fn tool_result_part(tool_call_id: &str, tool_name: &str, output_type: &str, value: &str) -> Value {
    json!({
        "type": "tool-result",
        "toolCallId": tool_call_id,
        "toolName": tool_name,
        "output": { "type": output_type, "value": value },
    })
}

pub fn role_of(message: &Value) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("")
}

/// Normalizes `content` to a parts array (`string` becomes one text part).
pub fn content_parts(message: &Value) -> Vec<Value> {
    match message.get("content") {
        Some(Value::String(text)) => vec![json!({ "type": "text", "text": text })],
        Some(Value::Array(parts)) => parts.clone(),
        _ => vec![],
    }
}

pub fn part_type(part: &Value) -> &str {
    part.get("type").and_then(Value::as_str).unwrap_or("")
}

/// The text of a tool result output (`text`/`error-text` value, JSON otherwise).
pub fn tool_output_text(output: &Value) -> String {
    match output.get("type").and_then(Value::as_str) {
        Some("text") | Some("error-text") => output.get("value").and_then(Value::as_str).unwrap_or("").to_string(),
        Some("execution-denied") => output.get("reason").and_then(Value::as_str).unwrap_or("Tool call execution denied.").to_string(),
        _ => crate::jsutil::stringify(output.get("value").unwrap_or(&Value::Null)),
    }
}

pub fn is_error_output(output: &Value) -> bool {
    matches!(output.get("type").and_then(Value::as_str), Some("error-text") | Some("error-json") | Some("execution-denied"))
}

/// Provider options of a message or part for a given namespace.
pub fn provider_options<'a>(value: &'a Value, namespace: &str) -> Option<&'a Map<String, Value>> {
    value.get("providerOptions")?.get(namespace)?.as_object()
}

/// Image part media type and base64 data (`image` parts, or `file` parts with image media).
pub fn image_payload(part: &Value) -> Option<(String, String)> {
    let kind = part_type(part);
    let media = part.get("mediaType").and_then(Value::as_str).unwrap_or("image/jpeg");
    let data = match kind {
        "image" => part.get("image")?,
        "file" if media.starts_with("image/") => part.get("data")?,
        _ => return None,
    };
    let media = if media == "image/*" { "image/jpeg" } else { media };
    let data = data.as_str()?;
    if let Some(rest) = data.strip_prefix("data:") {
        let (meta, payload) = rest.split_once(',')?;
        let mime = meta.split(';').next().unwrap_or(media).to_string();
        return Some((mime, payload.to_string()));
    }
    Some((media.to_string(), data.to_string()))
}

/// Resolve `{base}/{path}` without double slashes.
pub fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}
