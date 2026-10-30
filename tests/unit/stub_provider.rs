use crate::common::*;
use futures_util::StreamExt;
use goon::model::{LanguageModel, ModelRequest, StreamPart};
use goon::providers::stub::{load_stub_fixture, parse_fixture, StubModel};

fn request(text: &str) -> ModelRequest {
    ModelRequest {
        system: None,
        messages: vec![json!({ "role": "user", "content": [{ "type": "text", "text": text }] })],
        tools: vec![],
        max_output_tokens: 1024,
        effort: None,
        cancel: token(),
        timeout: std::time::Duration::from_secs(5),
    }
}

fn model(fixture: Value) -> StubModel {
    StubModel::new(parse_fixture(&fixture).unwrap(), Arc::new(Mutex::new(0)), None)
}

async fn reply(m: &StubModel, text: &str) -> String {
    m.generate(request(text)).await.unwrap().text
}

fn two_turns() -> Value {
    json!({ "v": 1, "turns": [
        { "match": "first", "text": ["A"], "usage": { "inputTokens": 1, "outputTokens": 2, "totalTokens": 3 } },
        { "match": "second", "text": ["B"], "usage": { "inputTokens": 4, "outputTokens": 5, "totalTokens": 9 } },
    ] })
}

#[tokio::test]
async fn total_tokens_only_fixtures_still_move_usage_budgets() {
    let only_total = model(json!({ "v": 1, "turns": [{ "text": ["x"], "usage": { "totalTokens": 100 } }] })).generate(request("x")).await.unwrap();
    assert_eq!((only_total.usage.input_tokens, only_total.usage.output_tokens), (Some(0), Some(100)));
    let with_input = model(json!({ "v": 1, "turns": [{ "text": ["x"], "usage": { "inputTokens": 40, "totalTokens": 100 } }] })).generate(request("x")).await.unwrap();
    assert_eq!((with_input.usage.input_tokens, with_input.usage.output_tokens), (Some(40), Some(60)));
}

#[test]
fn resolve_runtime_keeps_one_cursor_across_config_reloads() {
    let f = fixture();
    let _env = fixture_env(&f);
    let raw = json!({ "stub": true, "stubFixture": f.root.join("fix.json").to_string_lossy(), "keyEnv": "GOON_API_KEY", "model": "stub-model", "baseUrl": "stub://local" });
    let config = config::parse_config(&raw).unwrap();
    let first = config::resolve_runtime(&config, &f.cwd).unwrap();
    let second = config::resolve_runtime(&config, &f.cwd).unwrap();
    assert!(Arc::ptr_eq(first.stub_cursor.as_ref().unwrap(), second.stub_cursor.as_ref().unwrap()));
    assert_eq!(first.model, "stub-model");
    assert_eq!(first.base_url, "stub://local");
}

#[tokio::test]
async fn turn_selection_by_match_auxiliary_and_exhaustion() {
    let m = model(two_turns());
    assert_eq!(reply(&m, "third prompt").await, "[stub] auxiliary response");
    assert_eq!(reply(&m, "first prompt").await, "A");
    assert_eq!(reply(&m, "second prompt").await, "B");
    assert_eq!(reply(&m, "third prompt").await, "[stub] script exhausted");
}

#[tokio::test]
async fn fixture_input_selects_the_first_unused_turn() {
    let m = model(json!({ "v": 1, "input": "hi", "turns": [{ "text": ["A"] }, { "text": ["B"] }] }));
    assert_eq!(reply(&m, "hi there").await, "A");
    assert_eq!(reply(&m, "hi again").await, "B");
}

#[tokio::test]
async fn fresh_cursor_per_model_and_shared_cursor_when_passed() {
    assert_eq!(reply(&model(two_turns()), "first").await, "A");
    assert_eq!(reply(&model(two_turns()), "first").await, "A");
    let cursor = Arc::new(Mutex::new(0));
    let a = StubModel::new(parse_fixture(&two_turns()).unwrap(), cursor.clone(), None);
    let b = StubModel::new(parse_fixture(&two_turns()).unwrap(), cursor, None);
    assert_eq!(reply(&a, "first").await, "A");
    assert_eq!(reply(&b, "first").await, "B");
}

#[tokio::test]
async fn custom_auxiliary_and_exhaustion() {
    let m = model(json!({ "v": 1, "input": "go", "turns": [{ "text": ["once"] }, { "text": ["twice"] }], "auxiliary": { "text": ["aux"] } }));
    assert_eq!(reply(&m, "nothing").await, "aux");
    assert_eq!(reply(&m, "go").await, "once");
    assert_eq!(reply(&m, "go").await, "twice");
    assert_eq!(reply(&m, "go").await, "[stub] script exhausted");
}

#[tokio::test]
async fn exhausted_default_auxiliary_text() {
    let m = model(json!({ "v": 1, "turns": [{ "text": ["once"] }] }));
    reply(&m, "x").await;
    assert_eq!(reply(&m, "y").await, "[stub] script exhausted");
}

#[tokio::test]
async fn error_turns_raise_api_errors() {
    let m = model(json!({ "v": 1, "turns": [{ "error": { "message": "boom", "statusCode": 400, "retryable": false } }] }));
    let error = m.generate(request("x")).await.unwrap_err();
    assert_eq!(error.status, Some(400));
    assert!(!error.retryable);
    assert_eq!(error.message, "boom");
}

#[tokio::test]
async fn reasoning_tool_calls_and_usage_are_emitted_in_order() {
    let fixture = json!({ "v": 1, "turns": [{ "reasoning": ["think"], "text": ["answer"], "toolCalls": [{ "id": "call-1", "name": "read_file", "input": { "path": "a.txt" } }], "usage": { "inputTokens": 7, "outputTokens": 9, "totalTokens": 16 } }] });
    let generated = model(fixture.clone()).generate(request("x")).await.unwrap();
    assert_eq!((generated.reasoning.as_str(), generated.text.as_str()), ("think", "answer"));
    assert_eq!(generated.finish_reason, model::FinishReason::ToolCalls);
    assert_eq!((generated.usage.input_tokens, generated.usage.output_tokens), (Some(7), Some(9)));
    let mut stream = model(fixture).stream(request("x")).await.unwrap();
    let mut kinds = vec![];
    while let Some(part) = stream.next().await {
        kinds.push(match part {
            StreamPart::ReasoningDelta { .. } => "reasoning-delta".to_string(),
            StreamPart::ReasoningEnd { .. } => "reasoning-end".into(),
            StreamPart::ToolInputStart { .. } => "tool-input-start".into(),
            StreamPart::ToolInputDelta { .. } => "tool-input-delta".into(),
            StreamPart::ToolCall { id, tool_name, input, .. } => {
                assert_eq!((id.as_str(), tool_name.as_str(), input.as_str()), ("call-1", "read_file", "{\"path\":\"a.txt\"}"));
                "tool-call".into()
            }
            StreamPart::TextDelta { .. } => "text-delta".into(),
            StreamPart::TextEnd { .. } => "text-end".into(),
            StreamPart::Finish { .. } => "finish".into(),
            StreamPart::Error(_) => "error".into(),
        });
    }
    assert_eq!(kinds, vec!["reasoning-delta", "reasoning-end", "tool-input-start", "tool-input-delta", "tool-call", "text-delta", "text-end", "finish"]);
}

#[test]
fn fixture_schema_is_strict_and_files_are_capped_at_one_mib() {
    assert!(parse_fixture(&json!({ "v": 1, "turns": [{ "extra": true }] })).is_err());
    let f = fixture();
    let big = f.root.join("big.json");
    std::fs::write(&big, json!({ "v": 1, "turns": [{ "text": ["x".repeat(1_100_000)] }] }).to_string()).unwrap();
    assert_match(&load_stub_fixture(Some(&big.to_string_lossy())).err().unwrap().to_string(), "exceeds 1 MiB");
}

#[tokio::test]
async fn the_default_fixture_answers_deterministically_without_network() {
    let m = StubModel::new(load_stub_fixture(None).unwrap(), Arc::new(Mutex::new(0)), None);
    assert_eq!(reply(&m, "anything").await, "[stub] deterministic reply");
    let mut stream = m.stream(request("anything")).await.unwrap();
    while let Some(part) = stream.next().await {
        assert!(!matches!(part, StreamPart::Error(_)));
    }
}
