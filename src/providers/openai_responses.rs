//! OpenAI Responses API (`POST {base}/responses`, streaming).

use super::{part_channel, post_json, post_sse, session_headers, without_stream};
use crate::model::*;
use futures_util::future::BoxFuture;
use serde_json::{json, Map, Value};

pub struct OpenAIResponsesModel {
    pub model_id: String,
    pub base_url: String,
    pub api_key: String,
    pub session_id: Option<String>,
}

/// Reasoning models take system prompts as `developer` messages.
pub fn is_reasoning_model(model: &str) -> bool {
    let m = model.to_lowercase();
    let m = m.rsplit('/').next().unwrap_or(&m).to_string();
    (m.starts_with('o') && m.chars().nth(1).map(|c| c.is_ascii_digit()).unwrap_or(false)) || m.starts_with("gpt-5") || m.starts_with("codex")
}

fn openai_options(value: &Value) -> Map<String, Value> {
    provider_options(value, "openai").cloned().unwrap_or_default()
}

/// `convertToOpenAIResponsesInput` (store = true).
pub fn convert_input(model_id: &str, system: Option<&str>, messages: &[Value]) -> Vec<Value> {
    let reasoning_model = is_reasoning_model(model_id);
    let mut input = vec![];
    if let Some(system) = system {
        input.push(json!({ "role": if reasoning_model { "developer" } else { "system" }, "content": system }));
    }
    for message in messages {
        match role_of(message) {
            "system" => input.push(json!({ "role": if reasoning_model { "developer" } else { "system" }, "content": message.get("content").cloned().unwrap_or(json!("")) })),
            "user" => {
                let content: Vec<Value> = content_parts(message)
                    .iter()
                    .filter_map(|part| match part_type(part) {
                        "text" => Some(json!({ "type": "input_text", "text": part.get("text").cloned().unwrap_or(json!("")) })),
                        "image" | "file" => image_payload(part).map(|(media, data)| json!({ "type": "input_image", "image_url": format!("data:{media};base64,{data}") })),
                        _ => None,
                    })
                    .collect();
                input.push(json!({ "role": "user", "content": content }));
            }
            "assistant" => {
                let mut seen_reasoning: Vec<String> = vec![];
                let mut preceding_reasoning = false;
                for part in content_parts(message) {
                    let opts = openai_options(&part);
                    match part_type(&part) {
                        "text" => {
                            let id = opts.get("itemId").and_then(Value::as_str);
                            let can_use_id = id.is_some() && (!reasoning_model || preceding_reasoning);
                            if can_use_id {
                                input.push(json!({ "type": "item_reference", "id": id }));
                            } else {
                                input.push(json!({ "role": "assistant", "content": [{ "type": "output_text", "text": part.get("text").cloned().unwrap_or(json!("")) }] }));
                            }
                        }
                        "reasoning" => {
                            preceding_reasoning = true;
                            match opts.get("itemId").and_then(Value::as_str) {
                                Some(id) => {
                                    if !seen_reasoning.iter().any(|s| s == id) {
                                        seen_reasoning.push(id.to_string());
                                        input.push(json!({ "type": "item_reference", "id": id }));
                                    }
                                }
                                None => {
                                    if let Some(enc) = opts.get("reasoningEncryptedContent").and_then(Value::as_str) {
                                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                                        let summary = if text.is_empty() { json!([]) } else { json!([{ "type": "summary_text", "text": text }]) };
                                        input.push(json!({ "type": "reasoning", "encrypted_content": enc, "summary": summary }));
                                    }
                                }
                            }
                        }
                        "tool-call" => {
                            let arguments = match part.get("input") {
                                Some(Value::String(s)) => s.clone(),
                                Some(v) => crate::jsutil::stringify(v),
                                None => "{}".into(),
                            };
                            input.push(json!({ "type": "function_call", "call_id": part.get("toolCallId").cloned().unwrap_or(json!("")), "name": part.get("toolName").cloned().unwrap_or(json!("")), "arguments": arguments }));
                        }
                        _ => {}
                    }
                }
            }
            "tool" => {
                for part in content_parts(message) {
                    if part_type(&part) != "tool-result" {
                        continue;
                    }
                    let output = part.get("output").cloned().unwrap_or(json!({}));
                    input.push(json!({ "type": "function_call_output", "call_id": part.get("toolCallId").cloned().unwrap_or(json!("")), "output": tool_output_text(&output) }));
                }
            }
            _ => {}
        }
    }
    input
}

/// OpenAI accepts `minimal|low|medium|high|xhigh`; goon's `max` maps to the strongest level.
pub fn normalize_effort(effort: &str) -> String {
    if effort == "max" {
        "xhigh".into()
    } else {
        effort.into()
    }
}

pub fn build_body(model_id: &str, request: &ModelRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model_id));
    body.insert("input".into(), Value::Array(convert_input(model_id, request.system.as_deref(), &request.messages)));
    body.insert("max_output_tokens".into(), json!(request.max_output_tokens));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request.tools.iter().map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.parameters })).collect();
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), json!("auto"));
    }
    body.insert("stream".into(), json!(true));
    if let Some(effort) = &request.effort {
        body.insert("reasoning".into(), json!({ "effort": normalize_effort(effort) }));
    }
    Value::Object(body)
}

impl LanguageModel for OpenAIResponsesModel {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn provider(&self) -> &str {
        "openai-responses"
    }

    fn generate(&self, request: ModelRequest) -> BoxFuture<'static, Result<GenerateResponse, ApiError>> {
        let url = join_url(&self.base_url, "responses");
        let body = without_stream(build_body(&self.model_id, &request));
        let mut headers = vec![("authorization".to_string(), format!("Bearer {}", self.api_key))];
        headers.extend(session_headers(self.session_id.as_deref()));
        Box::pin(async move {
            let response = post_json(&url, &headers, &body, request.cancel.clone(), request.timeout).await?;
            if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
                return Err(ApiError::new(error.get("message").and_then(Value::as_str).unwrap_or("Responses API error").to_string()));
            }
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut has_calls = false;
            for item in response.get("output").and_then(Value::as_array).cloned().unwrap_or_default() {
                match part_type(&item) {
                    "message" => {
                        for c in item.get("content").and_then(Value::as_array).cloned().unwrap_or_default() {
                            if part_type(&c) == "output_text" {
                                text.push_str(c.get("text").and_then(Value::as_str).unwrap_or(""));
                            }
                        }
                    }
                    "reasoning" => {
                        for c in item.get("summary").and_then(Value::as_array).cloned().unwrap_or_default() {
                            reasoning.push_str(c.get("text").and_then(Value::as_str).unwrap_or(""));
                        }
                    }
                    "function_call" => has_calls = true,
                    _ => {}
                }
            }
            let u = response.get("usage").cloned().unwrap_or(json!({}));
            let usage = Usage {
                input_tokens: u.get("input_tokens").and_then(Value::as_u64),
                output_tokens: u.get("output_tokens").and_then(Value::as_u64),
                cache_read_tokens: u.get("input_tokens_details").and_then(|d| d.get("cached_tokens")).and_then(Value::as_u64),
                cache_write_tokens: None,
                reasoning_tokens: u.get("output_tokens_details").and_then(|d| d.get("reasoning_tokens")).and_then(Value::as_u64),
            };
            let finish_reason = match response.get("incomplete_details").and_then(|d| d.get("reason")).and_then(Value::as_str) {
                Some("max_output_tokens") => FinishReason::Length,
                Some("content_filter") => FinishReason::ContentFilter,
                Some(_) => FinishReason::Other,
                None if has_calls => FinishReason::ToolCalls,
                None => FinishReason::Stop,
            };
            Ok(GenerateResponse { text, reasoning, finish_reason, usage })
        })
    }

    fn stream(&self, request: ModelRequest) -> BoxFuture<'static, Result<PartStream, ApiError>> {
        let url = join_url(&self.base_url, "responses");
        let body = build_body(&self.model_id, &request);
        let mut headers = vec![("authorization".to_string(), format!("Bearer {}", self.api_key))];
        headers.extend(session_headers(self.session_id.as_deref()));
        Box::pin(async move {
            let mut rx = post_sse(&url, &headers, &body, request.cancel.clone(), request.timeout).await?;
            let (tx, stream) = part_channel();
            tokio::spawn(async move {
                // item id -> (call id, name, arguments)
                let mut calls: std::collections::HashMap<String, (String, String, String)> = std::collections::HashMap::new();
                let mut has_calls = false;
                let mut finish: Option<FinishReason> = None;
                let mut usage = Usage::default();
                while let Some(event) = rx.recv().await {
                    let event = match event {
                        Ok(e) => e,
                        Err(error) => {
                            let _ = tx.send(StreamPart::Error(error)).await;
                            return;
                        }
                    };
                    let Ok(data) = serde_json::from_str::<Value>(&event.data) else { continue };
                    let kind = data.get("type").and_then(Value::as_str).unwrap_or("").to_string();
                    let mut parts = vec![];
                    match kind.as_str() {
                        "response.output_item.added" => {
                            let item = data.get("item").cloned().unwrap_or(json!({}));
                            if part_type(&item) == "function_call" {
                                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("").to_string();
                                let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                                parts.push(StreamPart::ToolInputStart { id: call_id.clone(), tool_name: name.clone() });
                                calls.insert(item_id, (call_id, name, String::new()));
                            }
                        }
                        "response.output_text.delta" => {
                            let item_id = data.get("item_id").and_then(Value::as_str).unwrap_or("").to_string();
                            let delta = data.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
                            parts.push(StreamPart::TextDelta { id: item_id.clone(), delta, provider_metadata: Some(json!({ "openai": { "itemId": item_id } })) });
                        }
                        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                            let item_id = data.get("item_id").and_then(Value::as_str).unwrap_or("").to_string();
                            let delta = data.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
                            parts.push(StreamPart::ReasoningDelta { id: item_id.clone(), delta, provider_metadata: Some(json!({ "openai": { "itemId": item_id } })) });
                        }
                        "response.function_call_arguments.delta" => {
                            let item_id = data.get("item_id").and_then(Value::as_str).unwrap_or("").to_string();
                            let delta = data.get("delta").and_then(Value::as_str).unwrap_or("").to_string();
                            if let Some((call_id, _, args)) = calls.get_mut(&item_id) {
                                args.push_str(&delta);
                                parts.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta });
                            }
                        }
                        "response.output_item.done" => {
                            let item = data.get("item").cloned().unwrap_or(json!({}));
                            match part_type(&item) {
                                "function_call" => {
                                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                                    let (call_id, name, args) = calls.remove(&item_id).unwrap_or_else(|| {
                                        (item.get("call_id").and_then(Value::as_str).unwrap_or("").to_string(), item.get("name").and_then(Value::as_str).unwrap_or("").to_string(), String::new())
                                    });
                                    let arguments = item.get("arguments").and_then(Value::as_str).map(str::to_string).unwrap_or(args);
                                    has_calls = true;
                                    parts.push(StreamPart::ToolCall { id: call_id, tool_name: name, input: arguments, provider_metadata: Some(json!({ "openai": { "itemId": item_id } })) });
                                }
                                "reasoning" => {
                                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                                    let mut meta = json!({ "openai": { "itemId": item_id } });
                                    if let Some(enc) = item.get("encrypted_content").and_then(Value::as_str) {
                                        meta["openai"]["reasoningEncryptedContent"] = json!(enc);
                                    }
                                    parts.push(StreamPart::ReasoningEnd { id: item_id, provider_metadata: Some(meta) });
                                }
                                "message" => {
                                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                                    parts.push(StreamPart::TextEnd { id: item_id.clone(), provider_metadata: Some(json!({ "openai": { "itemId": item_id } })) });
                                }
                                _ => {}
                            }
                        }
                        "response.completed" | "response.incomplete" | "response.failed" => {
                            let response = data.get("response").cloned().unwrap_or(json!({}));
                            if let Some(u) = response.get("usage") {
                                let input = u.get("input_tokens").and_then(Value::as_u64);
                                let output = u.get("output_tokens").and_then(Value::as_u64);
                                usage = Usage {
                                    input_tokens: input,
                                    output_tokens: output,
                                    cache_read_tokens: u.get("input_tokens_details").and_then(|d| d.get("cached_tokens")).and_then(Value::as_u64),
                                    cache_write_tokens: None,
                                    reasoning_tokens: u.get("output_tokens_details").and_then(|d| d.get("reasoning_tokens")).and_then(Value::as_u64),
                                };
                            }
                            let incomplete = response.get("incomplete_details").and_then(|d| d.get("reason")).and_then(Value::as_str);
                            finish = Some(match (kind.as_str(), incomplete) {
                                (_, Some("max_output_tokens")) => FinishReason::Length,
                                (_, Some("content_filter")) => FinishReason::ContentFilter,
                                ("response.failed", _) => FinishReason::Error,
                                _ if has_calls => FinishReason::ToolCalls,
                                ("response.completed", _) => FinishReason::Stop,
                                _ => FinishReason::Other,
                            });
                            if kind == "response.failed" {
                                let message = response.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("Response failed").to_string();
                                parts.push(StreamPart::Error(ApiError::new(message)));
                            }
                        }
                        "error" => {
                            let message = data
                                .get("message")
                                .and_then(Value::as_str)
                                .or_else(|| data.get("error").and_then(|e| e.get("message")).and_then(Value::as_str))
                                .unwrap_or("Responses stream error")
                                .to_string();
                            finish = Some(FinishReason::Error);
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
                let _ = tx.send(StreamPart::Finish { usage, finish_reason: finish.unwrap_or(FinishReason::Unknown) }).await;
            });
            Ok(stream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_with_item_references() {
        let messages = vec![
            json!({"role": "user", "content": "read"}),
            json!({"role": "assistant", "content": [
                {"type": "reasoning", "text": "think", "providerOptions": {"openai": {"itemId": "rs_1", "reasoningEncryptedContent": "enc"}}},
                {"type": "tool-call", "toolCallId": "call_1", "toolName": "read_file", "input": {"path": "a.txt"}, "providerOptions": {"openai": {"itemId": "fc_1"}}}
            ]}),
            json!({"role": "tool", "content": [{"type": "tool-result", "toolCallId": "call_1", "toolName": "read_file", "output": {"type": "text", "value": "ok"}}]}),
        ];
        let input = convert_input("test-model", Some("sys"), &messages);
        assert_eq!(input[1], json!({"role": "user", "content": [{"type": "input_text", "text": "read"}]}));
        assert_eq!(input[2], json!({"type": "item_reference", "id": "rs_1"}));
        assert_eq!(input[3], json!({"type": "function_call", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"a.txt\"}"}));
        assert_eq!(input[4], json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"}));
        assert_eq!(normalize_effort("max"), "xhigh");
    }
}
