use crate::common::*;
use goon::context::{expand_file_mentions, MentionOptions};
use goon::provider::{discover_models, format_model_badge, resolve_model_capabilities, DiscoveredModel};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"].as_array().map(|t| t.iter().filter_map(|t| t["function"]["name"].as_str().map(str::to_string)).collect()).unwrap_or_default()
}

#[tokio::test]
async fn discover_models_queries_the_models_endpoint_with_the_bearer_key() {
    let p = provider(|_, _| json_response(200, json!({ "data": [{ "id": "live-model-alpha", "context_window": 200000, "max_output_tokens": 16384 }, { "id": "live-model-beta", "context_length": 65536 }] }))).await;
    let discovered = discover_models(&p.url, Some("disc-key"), "openai-compatible", None).await;
    assert_eq!(p.path(0), "/v1/models");
    assert_eq!(p.header(0, "authorization").as_deref(), Some("Bearer disc-key"));
    assert_eq!(
        discovered,
        vec![
            DiscoveredModel { id: "live-model-alpha".into(), context_window: Some(200000), max_output_tokens: Some(16384), ..Default::default() },
            DiscoveredModel { id: "live-model-beta".into(), context_window: Some(65536), ..Default::default() },
        ]
    );
}

#[tokio::test]
async fn plan_mode_blocks_mutations_until_exit_plan_mode_and_revert_restores_messages_and_files() {
    let f = fixture();
    f.write("app.ts", "export const v = 1;\n");
    let p = provider(|body, n| {
        let names = tool_names(body);
        match n {
            1 => {
                assert!(names.contains(&"exit_plan_mode".to_string()));
                assert!(!names.contains(&"write_file".to_string()) && !names.contains(&"edit_file".to_string()));
                assert_match(&body["messages"].to_string(), r"referenced_file[\s\S]*app\.ts");
                calls(vec![tc("plan1", "exit_plan_mode", json!({ "plan": "# Upgrade Plan\n1. Update app.ts\n2. Add extra.ts" }))])
            }
            2 => {
                assert!(names.contains(&"write_file".to_string()) && names.contains(&"edit_file".to_string()));
                calls(vec![
                    tc("todo1", "todo_write", json!({ "todos": [{ "content": "Update app.ts to v=2", "status": "in_progress", "priority": "high" }, { "content": "Create extra.ts", "status": "pending", "priority": "medium" }] })),
                    tc("edit1", "edit_file", json!({ "path": "app.ts", "oldText": "export const v = 1;", "newText": "export const v = 2;" })),
                    tc("write1", "write_file", json!({ "path": "extra.ts", "content": "export const extra = true;\n" })),
                ])
            }
            3 => text("Turn 1 complete."),
            4 => calls(vec![tc("edit2", "edit_file", json!({ "path": "app.ts", "oldText": "export const v = 2;", "newText": "export const v = 3;" }))]),
            _ => text("Turn 2 complete."),
        }
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "contextWindow": 8192, "maxOutputTokens": 512, "runTokenBudget": 100000 }));
    let st = store(&rt);
    let tools = registry(&f.cwd, tools::approve_all(), false);
    tools.set_plan_mode(true);
    let (emit, log) = events();
    let h = harness::Harness::new(rt, st.clone(), tools.clone(), emit, None, Default::default()).unwrap();
    assert_eq!(h.run("Update @app.ts and add extra.ts", &token(), None).await.unwrap(), "Turn 1 complete.");
    assert!(!tools.plan_mode());
    {
        let events = log.lock().unwrap();
        assert!(events.iter().any(|e| e["type"] == "plan_exit"));
        assert!(events.iter().any(|e| e["type"] == "todo" && e["todos"].as_array().map(|t| t.len()) == Some(2)));
    }
    assert_eq!(f.read("app.ts"), "export const v = 2;\n");
    assert_eq!(f.read("extra.ts"), "export const extra = true;\n");
    let after_turn1 = st.session.lock().unwrap().messages.len();
    assert!(after_turn1 > 0);
    assert_eq!(h.run("Bump app.ts to v=3", &token(), None).await.unwrap(), "Turn 2 complete.");
    assert_eq!(f.read("app.ts"), "export const v = 3;\n");
    assert!(st.session.lock().unwrap().messages.len() > after_turn1);
    assert_eq!(st.session.lock().unwrap().checkpoints.as_ref().unwrap().len(), 2);
    let rev2 = h.revert(Some(1), "both", false).unwrap();
    assert_eq!(rev2.turns_reverted, 1);
    assert_eq!(rev2.restored_files, vec!["app.ts"]);
    assert_eq!(st.session.lock().unwrap().messages.len(), after_turn1);
    assert_eq!(f.read("app.ts"), "export const v = 2;\n");
    assert_eq!(f.read("extra.ts"), "export const extra = true;\n");
    let rev1 = h.revert(Some(1), "both", false).unwrap();
    assert_eq!(rev1.turns_reverted, 1);
    assert_eq!(rev1.restored_files, vec!["app.ts"]);
    assert_eq!(rev1.deleted_files, vec!["extra.ts"]);
    assert!(st.session.lock().unwrap().messages.is_empty());
    assert_eq!(f.read("app.ts"), "export const v = 1;\n");
    assert!(!f.exists("extra.ts"));
}

#[tokio::test]
async fn gateway_compatibility_session_headers_aliases_vision_filtering_and_compaction_effort() {
    use base64::Engine;
    let f = fixture();
    assert_eq!(config::normalize_gateway_url("https://api.yuristore.xyz/"), "https://api.yuristore.xyz/v1");
    assert_eq!(config::normalize_gateway_url("http://127.0.0.1:8000"), "http://127.0.0.1:8000/v1");
    let gateway = provider(|_, _| {
        json_response(200, json!({ "data": [
            { "id": "glm-5.3-flash", "context_window": 200000, "max_output_tokens": 32000, "supports_vision": true, "reasoning_effort": "max" },
            { "id": "deepseek-v4.1-flash", "context_window": 200000, "max_output_tokens": 32000, "supports_vision": true, "reasoning_effort": "max" },
            { "id": "space-bunny", "context_window": 1048576, "max_output_tokens": 65536, "supports_vision": true, "supports_1m_context": true, "reasoning_effort": "max" },
            { "id": "beam", "context_window": 1048576, "max_output_tokens": 65536, "supports_vision": false, "supports_1m_context": true, "reasoning_effort": "max" },
        ] }))
    })
    .await;
    let models = discover_models(&gateway.url, Some("gw-key"), "openai-compatible", None).await;
    assert_eq!(models.len(), 4);
    assert_eq!(format!("{}{}", models[2].id, format_model_badge(&models[2])), "space-bunny (ctx: 1024k · out: 64k · vision · effort: max)");
    assert_eq!(format!("{}{}", models[3].id, format_model_badge(&models[3])), "beam (ctx: 1024k · out: 64k · text-only · effort: max)");
    assert_eq!(resolve_model_capabilities("beam", None), DiscoveredModel { id: "beam".into(), context_window: Some(1048576), max_output_tokens: Some(65536), supports_vision: Some(false), default_effort: Some("max".into()) });

    std::fs::write(f.cwd.join("shot.png"), base64::engine::general_purpose::STANDARD.decode(PNG).unwrap()).unwrap();
    let ws = files::WorkspaceFiles::new(&f.cwd);
    let vision = expand_file_mentions("Check @shot.png please", &ws, &MentionOptions { model: Some("space-bunny"), ..Default::default() });
    assert!(vision.attached_paths.contains(&"shot.png".to_string()));
    assert_eq!(vision.image_parts.len(), 1);
    assert_eq!(vision.image_parts[0]["type"], "image");
    assert_eq!(vision.image_parts[0]["mediaType"], "image/png");
    let beam = expand_file_mentions("Check @shot.png please", &ws, &MentionOptions { model: Some("beam"), ..Default::default() });
    assert!(beam.image_parts.is_empty());
    assert_match(&beam.expanded_prompt, r#"\[Attached image @shot\.png omitted: model "beam" is text-only\]"#);

    f.write("hello.txt", "line one\n");
    let efforts = Arc::new(Mutex::new(vec![]));
    let ef = efforts.clone();
    let p = provider(move |body, n| {
        ef.lock().unwrap().push(body["reasoning_effort"].clone());
        match n {
            1 => calls(vec![tc("c_alias_1", "read", json!({ "filePath": "hello.txt" }))]),
            2 => calls(vec![tc("c_alias_2", "edit", json!({ "file_path": "hello.txt", "old_string": "line one", "new_string": "line two" }))]),
            3 => text("Aliases and session headers verified."),
            _ => json_response(200, json!({ "id": "summary", "object": "chat.completion", "created": 1, "model": "space-bunny", "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Clean checkpoint summary of gateway test." }, "finish_reason": "stop" }], "usage": { "prompt_tokens": 40, "completion_tokens": 15, "total_tokens": 55 } })),
        }
    })
    .await;
    let rt = runtime_keyed(&f.cwd, &f.data, &p.url, json!({ "model": "space-bunny", "effort": "max", "contextWindow": 1048576, "maxOutputTokens": 65536, "runTokenBudget": 1000000, "timeoutMs": 30000 }), "test-secret-never-echo");
    let st = store(&rt);
    let tools = registry(&f.cwd, tools::approve_all(), false);
    let h = harness::Harness::new(rt.clone(), st.clone(), tools, harness::noop_emit(), None, Default::default()).unwrap();
    assert_eq!(h.run("Edit @shot.png and hello.txt", &token(), None).await.unwrap(), "Aliases and session headers verified.");
    assert_eq!(f.read("hello.txt"), "line two\n");
    let id = st.id();
    for n in 0..3 {
        assert_eq!(p.header(n, "x-session-id").as_deref(), Some(id.as_str()));
        assert_eq!(p.header(n, "x-opencode-session").as_deref(), Some(id.as_str()));
    }
    assert_eq!(efforts.lock().unwrap()[..3], [json!("max"), json!("max"), json!("max")]);
    let model = provider::create_model(&rt, Some(&id)).unwrap();
    let compacted = context::compact_now(&st, &rt, &*model, &token(), None).await.unwrap();
    assert_eq!(compacted.compactions, 1);
    assert_eq!(efforts.lock().unwrap()[3], json!("low"));
}

#[tokio::test]
async fn transient_500s_are_retried_three_times_and_empty_sessions_are_ignored_on_resume() {
    let f = fixture();
    let p = provider(|_, n| if n <= 3 { json_response(500, json!({ "error": { "message": format!("Transient upstream 500 on attempt #{n}") } })) } else { text("Recovered on retry #3 (attempt #4).") }).await;
    let rt = runtime_keyed(&f.cwd, &f.data, &p.url, json!({ "contextWindow": 32000, "maxOutputTokens": 1024, "runTokenBudget": 100000, "timeoutMs": 10000 }), "test-secret-never-echo");
    let root = rt.data_dir.join("sessions");
    let s1 = store(&rt);
    let (emit, log) = events();
    let h = harness::Harness::new(rt.clone(), s1.clone(), registry(&f.cwd, tools::approve_all(), false), emit, None, Default::default()).unwrap();
    assert_eq!(h.run("Test API retry", &token(), None).await.unwrap(), "Recovered on retry #3 (attempt #4).");
    assert_eq!(p.count(), 4);
    let retries: Vec<Value> = log.lock().unwrap().iter().filter(|e| e["type"] == "retry").cloned().collect();
    assert_eq!(retries.iter().map(|r| r["attempt"].as_u64().unwrap()).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(retries[0]["maxRetries"], 3);
    let s1_id = s1.id();
    s1.close().unwrap();
    drop(h);
    std::thread::sleep(std::time::Duration::from_millis(15));
    let route = || session::SessionRoute::from_runtime(&rt);
    let empty = session::SessionStore::open(&root, route(), None, None).unwrap();
    assert!(empty.session.lock().unwrap().messages.is_empty());
    let empty_id = empty.id();
    empty.close().unwrap();
    let latest = session::SessionStore::open(&root, route(), Some("latest"), None).unwrap();
    assert_eq!(latest.id(), s1_id);
    assert_eq!(latest.session.lock().unwrap().messages.len(), 2);
    latest.close().unwrap();
    let one = session::SessionStore::open(&root, route(), Some("1"), None).unwrap();
    assert_eq!(one.id(), s1_id);
    one.close().unwrap();
    let reopened = session::SessionStore::open(&root, route(), Some(&empty_id), None).unwrap();
    reopened.discard_if_empty().unwrap();
    reopened.close().unwrap();
    let remaining = session::SessionStore::list(&root).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, s1_id);

    let _env = fixture_env(&f);
    std::fs::create_dir_all(f.config.join("goon")).unwrap();
    std::fs::write(f.config.join("goon").join("config.json"), json!({ "model": "gpt-5.4", "runTokenBudget": 120000 }).to_string()).unwrap();
    assert_eq!(config::load_config(&Map::new()).unwrap().run_token_budget, 0, "legacy 120000 runTokenBudget migrates to 0 (unlimited)");
}
