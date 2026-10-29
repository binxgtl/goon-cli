//! OpenAI-compatible Chat Completions (`POST {base}/chat/completions`, streaming).

use super::{generate_id, part_channel, post_sse, session_headers};
use crate::model::*;
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};

pub struct OpenAICompatModel {
    pub model_id: String,
    pub base_url: String,
    pub api_key: String,
    pub session_id: Option<String>,
}

fn openai_metadata(value: &Value) -> Map<String, Value> {
    provider_options(value, "openaiCompatible").cloned().unwrap_or_default()
}

/// `convertToOpenAICompatibleChatMessages` on `ModelMessage` input.
pub fn convert_messages(system: Option<&str>, messages: &[Value]) -> Vec<Value> {
    let mut out = vec![];
    if let Some(system) = system {
        out.push(json!({ "role": "system", "content": system }));
    }
    for message in messages {
        let metadata = openai_metadata(message);
        match role_of(message) {
            "system" => {
                let mut m = Map::new();
                m.insert("role".into(), json!("system"));
                m.insert("content".into(), message.get("content").cloned().unwrap_or(json!("")));
                m.extend(metadata);
                out.push(Value::Object(m));
            }
            "user" => {
                let parts = content_parts(message);
                if parts.len() == 1 && part_type(&parts[0]) == "text" {
                    let mut m = Map::new();
                    m.insert("role".into(), json!("user"));
                    m.insert("content".into(), parts[0].get("text").cloned().unwrap_or(json!("")));
                    m.extend(openai_metadata(&parts[0]));
                    out.push(Value::Object(m));
                    continue;
                }
                let wire: Vec<Value> = parts
                    .iter()
                    .filter_map(|part| {
                        let mut p = Map::new();
                        match part_type(part) {
                            "text" => {
                                p.insert("type".into(), json!("text"));
                                p.insert("text".into(), part.get("text").cloned().unwrap_or(json!("")));
                            }
                            "image" | "file" => {
                                let (media, data) = image_payload(part)?;
                                p.insert("type".into(), json!("image_url"));
                                p.insert("image_url".into(), json!({ "url": format!("data:{media};base64,{data}") }));
                            }
                            _ => return None,
                        }
                        p.extend(openai_metadata(part));
                        Some(Value::Object(p))
                    })
                    .collect();
                let mut m = Map::new();
                m.insert("role".into(), json!("user"));
                m.insert("content".into(), Value::Array(wire));
                m.extend(metadata);
                out.push(Value::Object(m));
            }
            "assistant" => {
                let mut text = String::new();
                let mut reasoning = String::new();
                let mut tool_calls: Vec<Value> = vec![];
                for part in content_parts(message) {
                    match part_type(&part) {
                        "text" => text.push_str(part.get("text").and_then(Value::as_str).unwrap_or("")),
                        "reasoning" => reasoning.push_str(part.get("text").and_then(Value::as_str).unwrap_or("")),
                        "tool-call" => {
                            let input = part.get("input").cloned().unwrap_or(json!({}));
                            let arguments = match &input {
                                Value::String(s) => s.clone(),
                                other => crate::jsutil::stringify(other),
                            };
                            let mut call = Map::new();
                            call.insert("id".into(), part.get("toolCallId").cloned().unwrap_or(json!("")));
                            call.insert("type".into(), json!("function"));
                            call.insert("function".into(), json!({ "name": part.get("toolName").cloned().unwrap_or(json!("")), "arguments": arguments }));
                            call.extend(openai_metadata(&part));
                            tool_calls.push(Value::Object(call));
                        }
                        _ => {}
                    }
                }
                let mut m = Map::new();
                m.insert("role".into(), json!("assistant"));
                m.insert("content".into(), if !tool_calls.is_empty() && text.is_empty() { Value::Null } else { json!(text) });
                if !reasoning.is_empty() {
                    m.insert("reasoning_content".into(), json!(reasoning));
                }
                if !tool_calls.is_empty() {
                    m.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                m.extend(metadata);
                out.push(Value::Object(m));
            }
            "tool" => {
                for part in content_parts(message) {
                    if part_type(&part) != "tool-result" {
                        continue;
                    }
                    let output = part.get("output").cloned().unwrap_or(json!({}));
                    let mut m = Map::new();
                    m.insert("role".into(), json!("tool"));
                    m.insert("tool_call_id".into(), part.get("toolCallId").cloned().unwrap_or(json!("")));
                    m.insert("content".into(), json!(tool_output_text(&output)));
                    m.extend(openai_metadata(&part));
                    out.push(Value::Object(m));
                }
            }
            _ => {}
        }
    }
    out
}

pub fn build_body(model_id: &str, request: &ModelRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model_id));
    body.insert("max_tokens".into(), json!(request.max_output_tokens));
    if let Some(effort) = &request.effort {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    body.insert("messages".into(), Value::Array(convert_messages(request.system.as_deref(), &request.messages)));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.parameters } }))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), json!("auto"));
    }
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));
    Value::Object(body)
}

pub fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        "function_call" | "tool_calls" => FinishReason::ToolCalls,
        _ => FinishReason::Other,
    }
}

pub fn convert_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage.filter(|u| u.is_object()) else { return Usage::default() };
    let prompt = usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion = usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0);
    let cached = usage.get("prompt_tokens_details").and_then(|d| d.get("cached_tokens")).and_then(Value::as_u64).unwrap_or(0);
    let reasoning = usage.get("completion_tokens_details").and_then(|d| d.get("reasoning_tokens")).and_then(Value::as_u64).unwrap_or(0);
    Usage { input_tokens: Some(prompt), output_tokens: Some(completion), cache_read_tokens: Some(cached), cache_write_tokens: None, reasoning_tokens: Some(reasoning) }
}

#[derive(Default)]
struct PendingCall {
    id: Option<String>,
    started: bool,
    stream_id: String,
    name: Option<String>,
    buffered: String,
    arguments: String,
    finished: bool,
}

/// Tracks streamed tool-call fragments by index (falling back to id), emitting input-start when
/// the name is known and the final tool-call at the end of the stream.
#[derive(Default)]
pub struct ToolCallTracker {
    calls: Vec<PendingCall>,
    by_index: std::collections::HashMap<i64, usize>,
    used_ids: std::collections::HashSet<String>,
}

impl ToolCallTracker {
    fn fresh_id(&mut self, wire: Option<&str>) -> String {
        if let Some(w) = wire.filter(|w| !w.trim().is_empty()) {
            if self.used_ids.insert(w.to_string()) {
                return w.to_string();
            }
        }
        loop {
            let id = generate_id();
            if self.used_ids.insert(id.clone()) {
                return id;
            }
        }
    }

    pub fn process(&mut self, delta: &Value, out: &mut Vec<StreamPart>) {
        let index = delta.get("index").and_then(Value::as_i64);
        let wire_id = delta.get("id").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string);
        let function = delta.get("function");
        let name = function.and_then(|f| f.get("name")).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string);
        let args = function.and_then(|f| f.get("arguments")).and_then(Value::as_str).map(str::to_string);
        // Resolve the call: by index, else by wire id, else the single unfinished call, else new.
        let mut slot = index.and_then(|i| self.by_index.get(&i).copied());
        if slot.is_none() {
            if let Some(id) = &wire_id {
                slot = self.calls.iter().position(|c| c.id.as_deref() == Some(id.as_str()));
            }
        }
        if slot.is_none() && index.is_none() && name.is_none() {
            let unfinished: Vec<usize> = self.calls.iter().enumerate().filter(|(_, c)| !c.finished).map(|(i, _)| i).collect();
            if unfinished.len() == 1 {
                slot = Some(unfinished[0]);
            }
        }
        // A new name on an index whose call is complete starts another call (some gateways reuse index 0).
        if let (Some(s), Some(_)) = (slot, &name) {
            let call = &self.calls[s];
            if call.started && call.name.is_some() && wire_id.as_ref().map(|w| call.id.as_deref() != Some(w.as_str())).unwrap_or(false) {
                slot = None;
            }
        }
        let s = match slot {
            Some(s) => s,
            None => {
                self.calls.push(PendingCall { id: wire_id.clone(), ..Default::default() });
                let s = self.calls.len() - 1;
                if let Some(i) = index {
                    self.by_index.insert(i, s);
                }
                s
            }
        };
        if self.calls[s].id.is_none() {
            self.calls[s].id = wire_id.clone();
        }
        if let Some(n) = &name {
            if self.calls[s].name.is_none() {
                self.calls[s].name = Some(n.clone());
            }
        }
        if let Some(a) = &args {
            if self.calls[s].started {
                self.calls[s].arguments.push_str(a);
                if !a.is_empty() {
                    out.push(StreamPart::ToolInputDelta { id: self.calls[s].stream_id.clone(), delta: a.clone() });
                }
            } else {
                self.calls[s].buffered.push_str(a);
            }
        }
        if !self.calls[s].started && self.calls[s].name.is_some() {
            let wire = self.calls[s].id.clone();
            let id = self.fresh_id(wire.as_deref());
            let call = &mut self.calls[s];
            call.started = true;
            call.stream_id = id.clone();
            out.push(StreamPart::ToolInputStart { id: id.clone(), tool_name: call.name.clone().unwrap() });
            let buffered = std::mem::take(&mut call.buffered);
            if !buffered.is_empty() {
                call.arguments.push_str(&buffered);
                out.push(StreamPart::ToolInputDelta { id, delta: buffered });
            }
        }
    }

    pub fn flush(&mut self, out: &mut Vec<StreamPart>) {
        for call in self.calls.iter_mut() {
            if call.finished || !call.started {
                continue;
            }
            call.finished = true;
            out.push(StreamPart::ToolCall { id: call.stream_id.clone(), tool_name: call.name.clone().unwrap_or_default(), input: call.arguments.clone(), provider_metadata: None });
        }
    }
}

fn content_items(content: Option<&Value>) -> Vec<(bool, String)> {
    match content {
        Some(Value::String(s)) if !s.is_empty() => vec![(false, s.clone())],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                Some("text") => p.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()).map(|t| (false, t.to_string())),
                Some("thinking") => {
                    let text: String = p.get("thinking").and_then(Value::as_array).map(|items| items.iter().filter(|c| c.get("type").and_then(Value::as_str) == Some("text")).filter_map(|c| c.get("text").and_then(Value::as_str)).collect()).unwrap_or_default();
                    (!text.is_empty()).then_some((true, text))
                }
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

impl LanguageModel for OpenAICompatModel {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn provider(&self) -> &str {
        "openai-compatible"
    }

    fn stream(&self, request: ModelRequest) -> BoxFuture<'static, Result<PartStream, ApiError>> {
        let url = join_url(&self.base_url, "chat/completions");
        let body = build_body(&self.model_id, &request);
        let mut headers = vec![("authorization".to_string(), format!("Bearer {}", self.api_key))];
        headers.extend(session_headers(self.session_id.as_deref()));
        Box::pin(async move {
            let mut rx = post_sse(&url, &headers, &body, request.cancel.clone(), request.timeout).await?;
            let (tx, stream) = part_channel();
            tokio::spawn(async move {
                let mut tracker = ToolCallTracker::default();
                let mut finish: Option<FinishReason> = None;
                let mut usage: Option<Value> = None;
                while let Some(event) = rx.recv().await {
                    let event = match event {
                        Ok(e) => e,
                        Err(error) => {
                            let _ = tx.send(StreamPart::Error(error)).await;
                            return;
                        }
                    };
                    if event.data.trim() == "[DONE]" {
                        break;
                    }
                    let Ok(chunk) = serde_json::from_str::<Value>(&event.data) else {
                        let _ = tx.send(StreamPart::Error(ApiError::new(format!("Invalid JSON in stream chunk: {}", crate::jsutil::take_chars(&event.data, 200))))).await;
                        finish = Some(FinishReason::Error);
                        continue;
                    };
                    if let Some(error) = chunk.get("error") {
                        let message = error.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| crate::jsutil::stringify(error));
                        finish = Some(FinishReason::Error);
                        let _ = tx.send(StreamPart::Error(ApiError::new(message))).await;
                        continue;
                    }
                    if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
                        usage = Some(u.clone());
                    }
                    let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|c| c.first()) else { continue };
                    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                        finish = Some(map_finish_reason(reason));
                    }
                    let Some(delta) = choice.get("delta").filter(|d| d.is_object()) else { continue };
                    let mut parts = vec![];
                    let reasoning = delta.get("reasoning_content").and_then(Value::as_str).or_else(|| delta.get("reasoning").and_then(Value::as_str));
                    if let Some(r) = reasoning.filter(|r| !r.is_empty()) {
                        parts.push(StreamPart::ReasoningDelta { id: "reasoning-0".into(), delta: r.to_string(), provider_metadata: None });
                    }
                    for (is_reasoning, text) in content_items(delta.get("content")) {
                        if is_reasoning {
                            parts.push(StreamPart::ReasoningDelta { id: "reasoning-0".into(), delta: text, provider_metadata: None });
                        } else {
                            parts.push(StreamPart::TextDelta { id: "txt-0".into(), delta: text, provider_metadata: None });
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            tracker.process(call, &mut parts);
                        }
                    }
                    for part in parts {
                        if tx.send(part).await.is_err() {
                            return;
                        }
                    }
                }
                let mut parts = vec![];
                tracker.flush(&mut parts);
                for part in parts {
                    let _ = tx.send(part).await;
                }
                let finish_reason = match finish {
                    Some(f) => f,
                    None => {
                        let _ = tx.send(StreamPart::Error(ApiError::new("Response stream ended without a finish reason."))).await;
                        FinishReason::Error
                    }
                };
                let _ = tx.send(StreamPart::Finish { usage: convert_usage(usage.as_ref()), finish_reason }).await;
            });
            Ok(stream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_messages_like_the_ai_sdk() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": [{"type": "reasoning", "text": "think"}, {"type": "text", "text": "Let me read."}, {"type": "tool-call", "toolCallId": "call_1", "toolName": "read_file", "input": {"path": "a.txt", "offset": 1, "lines": 200}}]}),
            json!({"role": "tool", "content": [{"type": "tool-result", "toolCallId": "call_1", "toolName": "read_file", "output": {"type": "text", "value": "ok"}}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "x"}], "providerOptions": {"openaiCompatible": {"reasoning_content": ""}}}),
        ];
        let wire = convert_messages(Some("sys"), &messages);
        assert_eq!(crate::jsutil::stringify(&wire[2]), r#"{"role":"assistant","content":"Let me read.","reasoning_content":"think","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\",\"offset\":1,\"lines\":200}"}}]}"#);
        assert_eq!(wire[3], json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}));
        assert_eq!(wire[4], json!({"role": "assistant", "content": "x", "reasoning_content": ""}));
    }

    #[test]
    fn tracker_buffers_arguments_until_name() {
        let mut tracker = ToolCallTracker::default();
        let mut out = vec![];
        tracker.process(&json!({"index": 0, "function": {"arguments": "{\"a\""}}), &mut out);
        tracker.process(&json!({"index": 0, "id": "c1", "function": {"name": "read_file", "arguments": ":1}"}}), &mut out);
        tracker.flush(&mut out);
        match &out[..] {
            [StreamPart::ToolInputStart { id, tool_name }, StreamPart::ToolInputDelta { delta, .. }, StreamPart::ToolCall { input, .. }] => {
                assert_eq!(id, "c1");
                assert_eq!(tool_name, "read_file");
                assert_eq!(delta, "{\"a\":1}");
                assert_eq!(input, "{\"a\":1}");
            }
            other => panic!("{other:?}"),
        }
    }
}
