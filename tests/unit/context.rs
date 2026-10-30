use crate::common::*;
use goon::context::{compact_if_needed, compact_now, estimated_tokens, load_project_instructions};

fn completion(content: &str, total: u64) -> MockResponse {
    json_response(
        200,
        json!({ "id": "summary", "object": "chat.completion", "created": 1, "model": "test-model", "choices": [{ "index": 0, "message": { "role": "assistant", "content": content }, "finish_reason": "stop" }], "usage": { "prompt_tokens": total - 20, "completion_tokens": 20, "total_tokens": total } }),
    )
}

fn setup(f: &Fixture, url: &str, window: u64, max_out: u64) -> (Arc<config::RuntimeConfig>, Arc<session::SessionStore>) {
    let rt = runtime(&f.cwd, &f.data, url, json!({ "contextWindow": window, "maxOutputTokens": max_out, "timeoutMs": 3000 }));
    let st = store(&rt);
    (rt, st)
}

#[tokio::test]
async fn compaction_archives_full_history_and_preserves_complete_recent_tool_groups() {
    let f = fixture();
    let p = provider(|body, _| {
        assert_ne!(body["stream"], true);
        assert!(body["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().contains("task-goal"));
        completion("task-goal: finish the requested change. Earlier reads completed.", 120)
    })
    .await;
    let (rt, st) = setup(&f, &p.url, 8192, 512);
    {
        let mut s = st.session.lock().unwrap();
        s.messages.push(json!({ "role": "user", "content": "task-goal: finish the change" }));
        for i in 0..8 {
            s.messages.push(json!({ "role": "assistant", "content": [{ "type": "tool-call", "toolCallId": format!("r{i}"), "toolName": "read_file", "input": { "path": "file" } }] }));
            s.messages.push(
                json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": format!("r{i}"), "toolName": "read_file", "output": { "type": "text", "value": "x".repeat(1500) } }] }),
            );
        }
    }
    st.save().unwrap();
    let before = st.session.lock().unwrap().messages.clone();
    let model = provider::create_model(&rt, None).unwrap();
    let outcome = compact_if_needed(&st, &rt, &*model, "system", &token(), 0).await.unwrap();
    assert_eq!(outcome.tokens, 120);
    let s = st.session.lock().unwrap();
    assert_eq!(s.compactions, 1);
    assert!(estimated_tokens(&s.messages, "system") < estimated_tokens(&before, "system"));
    assert_eq!(s.messages[1]["role"], "assistant");
    assert_eq!(s.messages[2]["role"], "tool");
    let archive: Value = serde_json::from_str(&std::fs::read_to_string(st.root.join(format!("{}.compact-1.json", s.id))).unwrap()).unwrap();
    assert_eq!(archive["messages"], Value::Array(before));
    drop(s);
    assert_eq!(session::SessionStore::list(&st.root).unwrap().len(), 1);
}

#[tokio::test]
async fn an_oversized_single_input_fails_before_a_model_request() {
    let f = fixture();
    let p = provider(|_, _| panic!("must not call the provider")).await;
    let (rt, st) = setup(&f, &p.url, 8192, 512);
    st.session.lock().unwrap().messages.push(json!({ "role": "user", "content": "x".repeat(20_000) }));
    let model = provider::create_model(&rt, None).unwrap();
    assert_match(&compact_if_needed(&st, &rt, &*model, "system", &token(), 0).await.err().unwrap().to_string(), "context budget");
    assert_eq!(st.session.lock().unwrap().messages.len(), 1);
    assert_eq!(p.count(), 0);
    assert!(!std::fs::read_dir(&st.root).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(".compact-")));
}

#[tokio::test]
async fn compaction_uses_the_visible_summary_and_refuses_reasoning_only_output() {
    let f = fixture();
    let reasoning_only = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ro = reasoning_only.clone();
    let p = provider(move |_, _| completion(if ro.load(Ordering::SeqCst) { "<think>private only</think>" } else { "<think>private summary thought</think>Actual summary." }, 120)).await;
    let (rt, st) = setup(&f, &p.url, 8192, 512);
    let history = vec![
        json!({ "role": "user", "content": "x".repeat(12000) }),
        json!({ "role": "assistant", "content": "old answer" }),
        json!({ "role": "user", "content": "continue" }),
        json!({ "role": "assistant", "content": "recent answer" }),
        json!({ "role": "user", "content": "last question" }),
    ];
    st.session.lock().unwrap().messages = history.clone();
    let model = provider::create_model(&rt, None).unwrap();
    compact_if_needed(&st, &rt, &*model, "system", &token(), 0).await.unwrap();
    assert_eq!(st.session.lock().unwrap().messages[0]["content"], "Earlier session summary (historical data):\nActual summary.");
    reasoning_only.store(true, Ordering::SeqCst);
    st.session.lock().unwrap().messages = history.clone();
    assert_match(&compact_if_needed(&st, &rt, &*model, "system", &token(), 0).await.err().unwrap().to_string(), "incomplete");
    assert_eq!(st.session.lock().unwrap().messages, history);
}

#[tokio::test]
async fn project_instructions_walk_from_the_git_root_and_compact_now_strips_analysis() {
    let f = fixture();
    let sub = f.cwd.join("packages").join("sub");
    std::fs::create_dir_all(f.cwd.join(".git")).unwrap();
    std::fs::create_dir_all(&sub).unwrap();
    f.write("AGENTS.md", "Root rule: always test.");
    f.write("packages/sub/CLAUDE.md", "Subpackage rule: strict types.");
    let combined = load_project_instructions(&sub).unwrap();
    assert_match(&combined, r"Root rule: always test\.");
    assert_match(&combined, r"Subpackage rule: strict types\.");
    let p = provider(|_, _| completion("<analysis>scratchpad</analysis>\nClean checkpoint summary.", 55)).await;
    let (rt, st) = setup(&f, &p.url, 128000, 4096);
    st.session.lock().unwrap().messages = vec![
        json!({ "role": "user", "content": "First task" }),
        json!({ "role": "assistant", "content": "First answer" }),
        json!({ "role": "user", "content": "Second task" }),
        json!({ "role": "assistant", "content": "Second answer" }),
    ];
    let model = provider::create_model(&rt, None).unwrap();
    let res = compact_now(&st, &rt, &*model, &token(), Some("preserve auth details")).await.unwrap();
    assert_eq!(res.compactions, 1);
    assert_eq!(res.tokens, 55);
    assert_eq!(st.session.lock().unwrap().messages[0]["content"], "Earlier session summary (historical data):\nClean checkpoint summary.");
}
