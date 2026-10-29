//! Anthropic Messages API (`POST {base}/messages`, streaming).

use super::{part_channel, post_json, post_sse, session_headers, without_stream};
use crate::model::*;
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};

pub struct AnthropicModel {
    pub model_id: String,
    pub base_url: String,
    pub api_key: String,
    pub session_id: Option<String>,
}

fn reasoning_block(part: &Value) -> Option<Value> {
    let meta = provider_options(part, "anthropic")?;
    if let Some(signature) = meta.get("signature").and_then(Value::as_str) {
        return Some(json!({ "type": "thinking", "thinking": part.get("text").cloned().unwrap_or(json!("")), "signature": signature }));
    }
    if let Some(data) = meta.get("redactedData").and_then(Value::as_str) {
        return Some(json!({ "type": "redacted_thinking", "data": data }));
    }
    None
}

/// Tool-use blocks must follow the text of their segment (thinking blocks start new segments).
fn move_tool_use_to_end(content: Vec<Value>) -> Vec<Value> {
    let mut result = vec![];
    let mut segment: Vec<Value> = vec![];
    let flush = |segment: &mut Vec<Value>, result: &mut Vec<Value>| {
        let (tools, other): (Vec<Value>, Vec<Value>) = segment.drain(..).partition(|p| part_type(p) == "tool_use");
        result.extend(other);
        result.extend(tools);
    };
    for part in content {
        let kind = part_type(&part).to_string();
        if kind == "thinking" || kind == "redacted_thinking" {
            flush(&mut segment, &mut result);
            result.push(part);
        } else {
            segment.push(part);
        }
    }
    flush(&mut segment, &mut result);
    result
}

/// `convertToAnthropicMessagesPrompt`: user and tool messages merge into one user turn;
/// consecutive assistant messages merge into one assistant turn.
pub fn convert_messages(messages: &[Value]) -> Vec<Value> {
    #[derive(PartialEq)]
    enum Block {
        User,
        Assistant,
    }
    let mut groups: Vec<(Block, Vec<&Value>)> = vec![];
    for message in messages {
        let kind = match role_of(message) {
            "assistant" => Block::Assistant,
            "user" | "tool" => Block::User,
            _ => continue,
        };
        match groups.last_mut() {
            Some((k, items)) if *k == kind => items.push(message),
            _ => groups.push((kind, vec![message])),
        }
    }
    let group_count = groups.len();
    let mut out = vec![];
    for (gi, (kind, items)) in groups.into_iter().enumerate() {
        let mut content: Vec<Value> = vec![];
        match kind {
            Block::User => {
                for message in items {
                    if role_of(message) == "tool" {
                        for part in content_parts(message) {
                            if part_type(&part) != "tool-result" {
                                continue;
                            }
                            let output = part.get("output").cloned().unwrap_or(json!({}));
                            let mut block = Map::new();
                            block.insert("type".into(), json!("tool_result"));
                            block.insert("tool_use_id".into(), part.get("toolCallId").cloned().unwrap_or(json!("")));
                            block.insert("content".into(), json!(tool_output_text(&output)));
                            if matches!(output.get("type").and_then(Value::as_str), Some("error-text") | Some("error-json")) {
                                block.insert("is_error".into(), json!(true));
                            }
                            content.push(Value::Object(block));
                        }
                        continue;
                    }
                    for part in content_parts(message) {
                        match part_type(&part) {
                            "text" => content.push(json!({ "type": "text", "text": part.get("text").cloned().unwrap_or(json!("")) })),
                            "image" | "file" => {
                                if let Some((media, data)) = image_payload(&part) {
                                    content.push(json!({ "type": "image", "source": { "type": "base64", "media_type": media, "data": data } }));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                out.push(json!({ "role": "user", "content": content }));
            }
            Block::Assistant => {
                let count = items.len();
                for (mi, message) in items.into_iter().enumerate() {
                    let parts: Vec<Value> = content_parts(message).into_iter().filter(|p| !(part_type(p) == "text" && p.get("text").and_then(Value::as_str) == Some(""))).collect();
                    let part_count = parts.len();
                    for (pi, part) in parts.into_iter().enumerate() {
                        match part_type(&part) {
                            "text" => {
                                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                                // Anthropic rejects trailing whitespace in a final (prefilled) assistant turn.
                                let last = gi == group_count - 1 && mi == count - 1 && pi == part_count - 1;
                                content.push(json!({ "type": "text", "text": if last { text.trim() } else { text } }));
                            }
                            "reasoning" => {
                                if let Some(block) = reasoning_block(&part) {
                                    content.push(block);
                                }
                            }
                            "tool-call" => {
                                let input = match part.get("input") {
                                    Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!({})),
                                    Some(v) => v.clone(),
                                    None => json!({}),
                                };
                                content.push(json!({ "type": "tool_use", "id": part.get("toolCallId").cloned().unwrap_or(json!("")), "name": part.get("toolName").cloned().unwrap_or(json!("")), "input": input }));
                            }
                            _ => {}
                        }
                    }
                }
                out.push(json!({ "role": "assistant", "content": move_tool_use_to_end(content) }));
            }
        }
    }
    out
}

pub fn build_body(model_id: &str, request: &ModelRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model_id));
    body.insert("max_tokens".into(), json!(request.max_output_tokens));
    if let Some(effort) = &request.effort {
        body.insert("output_config".into(), json!({ "effort": effort }));
    }
    if let Some(system) = &request.system {
        body.insert("system".into(), json!([{ "type": "text", "text": system }]));
    }
    body.insert("messages".into(), Value::Array(convert_messages(&request.messages)));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters, "eager_input_streaming": true }))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), json!({ "type": "auto" }));
    }
    body.insert("stream".into(), json!(true));
    Value::Object(body)
}

pub fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "pause_turn" | "end_turn" | "stop_sequence" => FinishReason::Stop,
        "refusal" => FinishReason::ContentFilter,
        "tool_use" => FinishReason::ToolCalls,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        _ => FinishReason::Other,
    }
}

enum OpenBlock {
    Text { id: String },
    Thinking { id: String, signature: Option<String> },
    Redacted { id: String, data: String },
    Tool { id: String, name: String, json: String },
}

impl LanguageModel for AnthropicModel {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn provider(&self) -> &str {
        "anthropic"
    }

    fn generate(&self, request: ModelRequest) -> BoxFuture<'static, Result<GenerateResponse, ApiError>> {
        let url = join_url(&self.base_url, "messages");
        let body = without_stream(build_body(&self.model_id, &request));
        let mut headers = vec![
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("x-api-key".to_string(), self.api_key.clone()),
            ("authorization".to_string(), format!("Bearer {}", self.api_key)),
        ];
        headers.extend(session_headers(self.session_id.as_deref()));
        Box::pin(async move {
            let response = post_json(&url, &headers, &body, request.cancel.clone(), request.timeout).await?;
            if response.get("type").and_then(Value::as_str) == Some("error") {
                let message = response.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("Anthropic API error").to_string();
                return Err(ApiError::new(message));
            }
            let mut text = String::new();
            let mut reasoning = String::new();
            for block in response.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
                match part_type(&block) {
                    "text" => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
                    "thinking" => reasoning.push_str(block.get("thinking").and_then(Value::as_str).unwrap_or("")),
                    _ => {}
                }
            }
            let u = response.get("usage").cloned().unwrap_or(json!({}));
            let cache_read = u.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let cache_write = u.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);
            let usage = Usage {
                input_tokens: u.get("input_tokens").and_then(Value::as_u64).map(|i| i + cache_read + cache_write),
                output_tokens: u.get("output_tokens").and_then(Value::as_u64),
                cache_read_tokens: Some(cache_read),
                cache_write_tokens: Some(cache_write),
                reasoning_tokens: None,
            };
            let finish_reason = response.get("stop_reason").and_then(Value::as_str).map(map_stop_reason).unwrap_or(FinishReason::Unknown);
            Ok(GenerateResponse { text, reasoning, finish_reason, usage })
        })
    }

    fn stream(&self, request: ModelRequest) -> BoxFuture<'static, Result<PartStream, ApiError>> {
        let url = join_url(&self.base_url, "messages");
        let body = build_body(&self.model_id, &request);
        let mut headers = vec![
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("x-api-key".to_string(), self.api_key.clone()),
            ("authorization".to_string(), format!("Bearer {}", self.api_key)),
        ];
        headers.extend(session_headers(self.session_id.as_deref()));
        Box::pin(async move {
            let mut rx = post_sse(&url, &headers, &body, request.cancel.clone(), request.timeout).await?;
            let (tx, stream) = part_channel();
            tokio::spawn(async move {
                let mut blocks: std::collections::HashMap<i64, OpenBlock> = std::collections::HashMap::new();
                let mut input_tokens: Option<u64> = None;
                let mut output_tokens: Option<u64> = None;
                let mut cache_read = 0u64;
                let mut cache_write = 0u64;
                let mut finish = FinishReason::Unknown;
                let mut saw_finish = false;
                while let Some(event) = rx.recv().await {
                    let event = match event {
                        Ok(e) => e,
                        Err(error) => {
                            let _ = tx.send(StreamPart::Error(error)).await;
                            return;
                        }
                    };
                    let Ok(data) = serde_json::from_str::<Value>(&event.data) else { continue };
                    let kind = data.get("type").and_then(Value::as_str).or(event.event.as_deref()).unwrap_or("").to_string();
                    let mut parts = vec![];
                    match kind.as_str() {
                        "message_start" => {
                            if let Some(u) = data.get("message").and_then(|m| m.get("usage")) {
                                input_tokens = u.get("input_tokens").and_then(Value::as_u64);
                                cache_read = u.get("cache_read_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                                cache_write = u.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0);
                                if let Some(o) = u.get("output_tokens").and_then(Value::as_u64) {
                                    output_tokens = Some(o);
                                }
                            }
                        }
                        "content_block_start" => {
                            let index = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                            let block = data.get("content_block").cloned().unwrap_or(json!({}));
                            let id = index.to_string();
                            match part_type(&block) {
                                "text" => {
                                    blocks.insert(index, OpenBlock::Text { id: id.clone() });
                                    if let Some(t) = block.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                                        parts.push(StreamPart::TextDelta { id, delta: t.to_string(), provider_metadata: None });
                                    }
                                }
                                "thinking" => {
                                    blocks.insert(index, OpenBlock::Thinking { id: id.clone(), signature: None });
                                    if let Some(t) = block.get("thinking").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                                        parts.push(StreamPart::ReasoningDelta { id, delta: t.to_string(), provider_metadata: None });
                                    }
                                }
                                "redacted_thinking" => {
                                    blocks.insert(index, OpenBlock::Redacted { id, data: block.get("data").and_then(Value::as_str).unwrap_or("").to_string() });
                                }
                                "tool_use" => {
                                    let call_id = block.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                                    let name = block.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                                    parts.push(StreamPart::ToolInputStart { id: call_id.clone(), tool_name: name.clone() });
                                    blocks.insert(index, OpenBlock::Tool { id: call_id, name, json: String::new() });
                                }
                                _ => {}
                            }
                        }
                        "content_block_delta" => {
                            let index = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                            let delta = data.get("delta").cloned().unwrap_or(json!({}));
                            match (part_type(&delta), blocks.get_mut(&index)) {
                                ("text_delta", Some(OpenBlock::Text { id })) => {
                                    if let Some(t) = delta.get("text").and_then(Value::as_str) {
                                        parts.push(StreamPart::TextDelta { id: id.clone(), delta: t.to_string(), provider_metadata: None });
                                    }
                                }
                                ("thinking_delta", Some(OpenBlock::Thinking { id, .. })) => {
                                    if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                                        parts.push(StreamPart::ReasoningDelta { id: id.clone(), delta: t.to_string(), provider_metadata: None });
                                    }
                                }
                                ("signature_delta", Some(OpenBlock::Thinking { signature, .. })) => {
                                    if let Some(s) = delta.get("signature").and_then(Value::as_str) {
                                        signature.get_or_insert_with(String::new).push_str(s);
                                    }
                                }
                                ("input_json_delta", Some(OpenBlock::Tool { id, json, .. })) => {
                                    if let Some(p) = delta.get("partial_json").and_then(Value::as_str) {
                                        json.push_str(p);
                                        if !p.is_empty() {
                                            parts.push(StreamPart::ToolInputDelta { id: id.clone(), delta: p.to_string() });
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            let index = data.get("index").and_then(Value::as_i64).unwrap_or(0);
                            match blocks.remove(&index) {
                                Some(OpenBlock::Text { id }) => parts.push(StreamPart::TextEnd { id, provider_metadata: None }),
                                Some(OpenBlock::Thinking { id, signature }) => {
                                    let meta = signature.map(|s| json!({ "anthropic": { "signature": s } }));
                                    parts.push(StreamPart::ReasoningEnd { id, provider_metadata: meta });
                                }
                                Some(OpenBlock::Redacted { id, data }) => {
                                    parts.push(StreamPart::ReasoningDelta { id: id.clone(), delta: String::new(), provider_metadata: None });
                                    parts.push(StreamPart::ReasoningEnd { id, provider_metadata: Some(json!({ "anthropic": { "redactedData": data } })) });
                                }
                                Some(OpenBlock::Tool { id, name, json }) => {
                                    let input = if json.trim().is_empty() { "{}".to_string() } else { json };
                                    parts.push(StreamPart::ToolCall { id, tool_name: name, input, provider_metadata: None });
                                }
                                None => {}
                            }
                        }
                        "message_delta" => {
                            if let Some(reason) = data.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                                finish = map_stop_reason(reason);
                                saw_finish = true;
                            }
                            if let Some(u) = data.get("usage") {
                                if let Some(o) = u.get("output_tokens").and_then(Value::as_u64) {
                                    output_tokens = Some(o);
                                }
                                if let Some(i) = u.get("input_tokens").and_then(Value::as_u64) {
                                    input_tokens = Some(i);
                                }
                                if let Some(c) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
                                    cache_read = c;
                                }
                                if let Some(c) = u.get("cache_creation_input_tokens").and_then(Value::as_u64) {
                                    cache_write = c;
                                }
                            }
                        }
                        "error" => {
                            let message = data.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("Anthropic stream error").to_string();
                            finish = FinishReason::Error;
                            saw_finish = true;
                            parts.push(StreamPart::Error(ApiError::new(message)));
                        }
                        _ => {}
                    }
                    for part in parts {
                        if tx.send(part).await.is_err() {
                            return;
                        }
                    }
                }
                if !saw_finish {
                    finish = FinishReason::Unknown;
                }
                let usage = Usage {
                    input_tokens: input_tokens.map(|i| i + cache_read + cache_write),
                    output_tokens,
                    cache_read_tokens: Some(cache_read),
                    cache_write_tokens: Some(cache_write),
                    reasoning_tokens: None,
                };
                let _ = tx.send(StreamPart::Finish { usage, finish_reason: finish }).await;
            });
            Ok(stream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_tool_results_into_user_turns() {
        let messages = vec![
            json!({"role": "user", "content": "read a.txt please"}),
            json!({"role": "assistant", "content": [{"type": "reasoning", "text": "think", "providerOptions": {"anthropic": {"signature": "sig"}}}, {"type": "tool-call", "toolCallId": "toolu_1", "toolName": "read_file", "input": {"path": "a.txt"}}]}),
            json!({"role": "tool", "content": [{"type": "tool-result", "toolCallId": "toolu_1", "toolName": "read_file", "output": {"type": "error-text", "value": "boom"}}]}),
        ];
        let wire = convert_messages(&messages);
        assert_eq!(wire[0], json!({"role": "user", "content": [{"type": "text", "text": "read a.txt please"}]}));
        assert_eq!(wire[1], json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "think", "signature": "sig"}, {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.txt"}}]}));
        assert_eq!(wire[2], json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "boom", "is_error": true}]}));
    }
}
