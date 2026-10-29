//! Inline reasoning (`<think>…</think>`) projection and reasoning replay for thinking endpoints.

use crate::model::{content_parts, part_type, role_of};
use serde_json::{json, Value};

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

#[derive(Clone, Debug, PartialEq)]
pub enum AnswerPart {
    Text(String),
    Reasoning(String),
}

fn has_provider_reasoning(message: &Value) -> bool {
    role_of(message) == "assistant"
        && message.get("content").map(Value::is_array).unwrap_or(false)
        && content_parts(message).iter().any(|p| part_type(p) == "reasoning" && p.get("text").and_then(Value::as_str).map(|t| !t.is_empty()).unwrap_or(false))
}

/// Thinking-mode OpenAI-compatible endpoints reject a request unless every replayed assistant
/// message carries `reasoning_content`. Once the history proves the endpoint thinks, steps without
/// reasoning get an explicit empty `reasoning_content`. Returns a request-only view.
pub fn with_reasoning_replay(messages: &[Value], provider: &str, effort: Option<&str>) -> Vec<Value> {
    if provider != "openai-compatible" || (effort.is_none() && !messages.iter().any(has_provider_reasoning)) {
        return messages.to_vec();
    }
    messages
        .iter()
        .map(|m| {
            if role_of(m) != "assistant" || has_provider_reasoning(m) {
                return m.clone();
            }
            let compat = m.get("providerOptions").and_then(|p| p.get("openaiCompatible")).and_then(Value::as_object).cloned().unwrap_or_default();
            if compat.contains_key("reasoning_content") {
                return m.clone();
            }
            let mut out = m.clone();
            let obj = out.as_object_mut().unwrap();
            let mut options = obj.get("providerOptions").and_then(Value::as_object).cloned().unwrap_or_default();
            let mut compat = compat;
            compat.insert("reasoning_content".into(), json!(""));
            options.insert("openaiCompatible".into(), Value::Object(compat));
            obj.insert("providerOptions".into(), Value::Object(options));
            out
        })
        .collect()
}

#[derive(PartialEq)]
enum Mode {
    Prefix,
    Reasoning,
    Answer,
}

/// Presentation projection only: splits a leading `<think>` block from the visible answer.
pub struct AnswerProjection {
    off: bool,
    mode: Mode,
    pending: String,
    whitespace: String,
    depth: u32,
    ended: bool,
    pub saw_reasoning: bool,
}

fn starts_with_ci(full: &str, prefix: &str) -> bool {
    full.to_lowercase().starts_with(&prefix.to_lowercase())
}

impl AnswerProjection {
    pub fn new(policy: &str) -> Self {
        AnswerProjection { off: policy == "off", mode: Mode::Prefix, pending: String::new(), whitespace: String::new(), depth: 0, ended: false, saw_reasoning: false }
    }

    fn append(parts: &mut Vec<AnswerPart>, reasoning: bool, value: &str) {
        if value.is_empty() {
            return;
        }
        match (parts.last_mut(), reasoning) {
            (Some(AnswerPart::Text(t)), false) => t.push_str(value),
            (Some(AnswerPart::Reasoning(t)), true) => t.push_str(value),
            _ => parts.push(if reasoning { AnswerPart::Reasoning(value.into()) } else { AnswerPart::Text(value.into()) }),
        }
    }

    pub fn push(&mut self, text: &str) -> Vec<AnswerPart> {
        if self.ended {
            return vec![];
        }
        if self.off {
            return if text.is_empty() { vec![] } else { vec![AnswerPart::Text(text.into())] };
        }
        let mut parts = vec![];
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if self.mode == Mode::Answer {
                let rest: String = chars[i..].iter().collect();
                Self::append(&mut parts, false, &rest);
                break;
            }
            let ch = chars[i];
            i += 1;
            if self.mode == Mode::Prefix {
                if self.pending.is_empty() && ch.is_whitespace() {
                    self.whitespace.push(ch);
                    continue;
                }
                self.pending.push(ch);
                if starts_with_ci(OPEN, &self.pending) {
                    if self.pending.chars().count() == OPEN.len() {
                        self.pending.clear();
                        self.whitespace.clear();
                        self.depth = 1;
                        self.mode = Mode::Reasoning;
                        self.saw_reasoning = true;
                    }
                } else {
                    let value = format!("{}{}", self.whitespace, self.pending);
                    Self::append(&mut parts, false, &value);
                    self.pending.clear();
                    self.whitespace.clear();
                    self.mode = Mode::Answer;
                }
                continue;
            }
            // Hold only a possible marker suffix. Never rescan accumulated reasoning.
            self.pending.push(ch);
            let lower = self.pending.to_lowercase();
            if lower == OPEN {
                self.depth += 1;
                self.pending.clear();
            } else if lower == CLOSE {
                self.pending.clear();
                self.depth -= 1;
                if self.depth == 0 {
                    self.mode = Mode::Prefix;
                }
            } else {
                while !self.pending.is_empty() && !starts_with_ci(OPEN, &self.pending) && !starts_with_ci(CLOSE, &self.pending) {
                    let first = self.pending.chars().next().unwrap();
                    Self::append(&mut parts, true, &first.to_string());
                    self.pending.drain(..first.len_utf8());
                }
            }
        }
        parts
    }

    pub fn finish(&mut self) -> Vec<AnswerPart> {
        if self.ended {
            return vec![];
        }
        self.ended = true;
        // An unclosed think block stays reasoning; a partial opening tag is literal text.
        let text = format!("{}{}", self.whitespace, self.pending);
        self.whitespace.clear();
        self.pending.clear();
        if text.is_empty() {
            return vec![];
        }
        vec![if self.mode == Mode::Reasoning { AnswerPart::Reasoning(text) } else { AnswerPart::Text(text) }]
    }
}

pub fn visible_answer(text: &str, policy: &str) -> String {
    let mut projection = AnswerProjection::new(policy);
    let mut parts = projection.push(text);
    parts.extend(projection.finish());
    parts
        .into_iter()
        .filter_map(|p| match p {
            AnswerPart::Text(t) => Some(t),
            AnswerPart::Reasoning(_) => None,
        })
        .collect()
}

/// A child's report is its last visible assistant answer, not all tool preambles.
pub fn last_assistant_answer(messages: &[Value], policy: &str) -> String {
    for message in messages.iter().rev() {
        if role_of(message) != "assistant" {
            continue;
        }
        let raw: String = match message.get("content") {
            Some(Value::String(s)) => s.clone(),
            _ => content_parts(message).iter().filter(|p| part_type(p) == "text").filter_map(|p| p.get("text").and_then(Value::as_str)).collect(),
        };
        let answer = visible_answer(&raw, policy);
        if !answer.trim().is_empty() {
            return answer;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_leading_think_block_across_chunks() {
        let mut p = AnswerProjection::new("leading-think");
        let mut parts = vec![];
        for chunk in ["  <thi", "nk>private</th", "ink>Answer", " <think>kept</think>"] {
            parts.extend(p.push(chunk));
        }
        parts.extend(p.finish());
        let text: String = parts.iter().filter_map(|x| if let AnswerPart::Text(t) = x { Some(t.as_str()) } else { None }).collect();
        let reasoning: String = parts.iter().filter_map(|x| if let AnswerPart::Reasoning(t) = x { Some(t.as_str()) } else { None }).collect();
        assert_eq!(text, "Answer <think>kept</think>");
        assert_eq!(reasoning, "private");
    }

    #[test]
    fn unclosed_and_partial() {
        assert_eq!(visible_answer("<think>private</thi", "leading-think"), "");
        assert_eq!(visible_answer("<thi", "leading-think"), "<thi");
        assert_eq!(visible_answer("<think>x</think>", "off"), "<think>x</think>");
    }

    #[test]
    fn replay_adds_empty_reasoning_content() {
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "reasoning", "text": "r"}, {"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
        ];
        let out = with_reasoning_replay(&messages, "openai-compatible", None);
        assert_eq!(out[1]["providerOptions"]["openaiCompatible"]["reasoning_content"], json!(""));
        assert!(out[0].get("providerOptions").is_none());
        assert_eq!(with_reasoning_replay(&messages[1..], "openai-compatible", None), messages[1..].to_vec());
    }
}
