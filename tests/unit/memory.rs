use crate::common::*;
use goon::memory::{MemoryStore, RagSettings};
use goon::rag::{project_id_for, rag_snapshot_path};
use goon::redact::{is_sensitive_path, redact_object, redact_secrets};

const SECRET: &str = "goon-test-secret-abc123";

fn rag() -> RagSettings {
    RagSettings { top_k: None, capacity: None, threshold: None, max_files: None, max_file_bytes: None, embedder: None }
}

async fn open_store(f: &Fixture) -> Arc<MemoryStore> {
    f.write("src/widget.ts", "export function quuxflurb(x: number) {\n  return x * 2;\n}\n");
    reopen(f).await
}

async fn reopen(f: &Fixture) -> Arc<MemoryStore> {
    MemoryStore::open(&f.data, &project_id_for(&f.cwd), Some(&f.cwd), &[SECRET.to_string()], rag()).await.unwrap()
}

fn memory_json(f: &Fixture) -> String {
    std::fs::read_to_string(f.data.join("memory").join(project_id_for(&f.cwd)).join("memory.json")).unwrap()
}

#[tokio::test]
async fn handle_event_routes_every_kind_and_task_completed_writes_a_lesson_and_clears_working() {
    let f = fixture();
    let _env = fixture_env(&f);
    let store = open_store(&f).await;
    store.handle_event(json!({ "type": "task_started", "task": "fix the widget bug" })).await;
    assert_match(&store.working.lock().unwrap().render().unwrap(), "widget bug");
    store.handle_event(json!({ "type": "state_changed", "state": "investigating", "note": "reading widget.ts" })).await;
    store.handle_event(json!({ "type": "tool_result", "tool": "read_file", "summary": "read src/widget.ts" })).await;
    store.handle_event(json!({ "type": "patch_applied", "path": "src/widget.ts", "bytes": 12 })).await;
    store.handle_event(json!({ "type": "verify_result", "status": "pass", "issues": 0 })).await;
    store.handle_event(json!({ "type": "assistant_message", "text": "Fixed the widget multiplication bug." })).await;
    assert!(store.working.lock().unwrap().render().unwrap().contains("verify pass"));
    store.handle_event(json!({ "type": "task_completed", "outcome": "done" })).await;
    assert_eq!(store.exec_size(), 1);
    assert_eq!(store.working.lock().unwrap().render(), None);
    store.refresh_repo_tree("src/widget.ts");
    store.handle_event(json!({ "type": "patch_applied", "path": "other.ts", "bytes": 1 })).await;
    assert_eq!(store.repo_tree(), None);
    store.close().await;
    let raw: Value = serde_json::from_str(&memory_json(&f)).unwrap();
    assert_eq!(raw["version"], 1);
    assert_eq!(raw["exec"].as_array().unwrap().len(), 1);
    assert_match(raw["exec"][0]["text"].as_str().unwrap(), "task: fix the widget bug");
}

#[tokio::test]
async fn concurrent_handle_event_and_persist_never_corrupt_the_store() {
    let f = fixture();
    let _env = fixture_env(&f);
    let store = open_store(&f).await;
    let mut jobs = vec![];
    for i in 0..30 {
        let s = store.clone();
        jobs.push(tokio::spawn(async move { s.handle_event(json!({ "type": "tool_result", "tool": format!("tool{i}"), "summary": format!("step {i}") })).await }));
        if i % 10 == 0 {
            let s = store.clone();
            jobs.push(tokio::spawn(async move { s.persist().await.unwrap() }));
        }
    }
    for job in jobs {
        job.await.unwrap();
    }
    store.close().await;
    let raw: Value = serde_json::from_str(&memory_json(&f)).unwrap();
    assert!(raw["exec"].is_array());
}

#[tokio::test]
async fn exec_rolls_at_fifty_entries() {
    let f = fixture();
    let _env = fixture_env(&f);
    let store = open_store(&f).await;
    for i in 0..60 {
        store.handle_event(json!({ "type": "task_started", "task": format!("lesson {i}") })).await;
        store.handle_event(json!({ "type": "task_completed", "outcome": "done" })).await;
    }
    let all = store.exec_lessons();
    assert_eq!(all.len(), 50);
    assert_match(&all[0].text, "lesson 10 ");
    assert_match(&all[49].text, "lesson 59 ");
    store.close().await;
}

#[tokio::test]
async fn preferences_live_in_preferences_json_separate_from_memories_json() {
    let f = fixture();
    let _env = fixture_env(&f);
    let store = open_store(&f).await;
    let memories = f.config.join("goon").join("memories.json");
    std::fs::create_dir_all(memories.parent().unwrap()).unwrap();
    let kept = json!([{ "id": "1", "text": "keep me", "createdAt": "2026-01-01T00:00:00.000Z" }]);
    std::fs::write(&memories, kept.to_string()).unwrap();
    store.handle_event(json!({ "type": "user_preference", "key": "style", "value": "concise answers" })).await;
    store.handle_event(json!({ "type": "user_preference", "key": "editor", "value": "vim bindings" })).await;
    store.close().await;
    let prefs: Value = serde_json::from_str(&std::fs::read_to_string(&store.preference.file).unwrap()).unwrap();
    assert_eq!(prefs["style"], "concise answers");
    assert_eq!(prefs["editor"], "vim bindings");
    assert_eq!(store.preference.file.parent(), memories.parent());
    let memories_now: Value = serde_json::from_str(&std::fs::read_to_string(&memories).unwrap()).unwrap();
    assert_eq!(memories_now, kept);
    assert_eq!(store.preference.get("style").as_deref(), Some("concise answers"));
    assert!(store.preference.remove("editor").unwrap());
    assert_eq!(store.preference.get("editor"), None);
}

#[tokio::test]
async fn everything_persisted_is_redacted() {
    let f = fixture();
    let _env = fixture_env(&f);
    let store = open_store(&f).await;
    store.handle_event(json!({ "type": "task_started", "task": "curl -H \"Authorization: Bearer eyJhbGciOi.J9\" and token=plainvalue1 with goon-test-secret-abc123" })).await;
    store.handle_event(json!({ "type": "task_completed", "outcome": "done" })).await;
    store.knowledge.add_insight("export API_KEY=goon-test-secret-abc123 in prod").await.unwrap();
    store.persist().await.unwrap();
    store.close().await;
    let raw = memory_json(&f);
    assert!(!raw.contains("goon-test-secret-abc123"));
    assert!(!raw.contains("eyJhbGciOi.J9"));
    assert!(!raw.contains("plainvalue1"));
    assert!(raw.contains("[REDACTED]"));
    assert!(raw.contains("API_KEY=[REDACTED]"));
}

#[tokio::test]
async fn retrieve_returns_working_notes_repo_tree_chunks_and_relevant_preferences_only() {
    let f = fixture();
    let _env = fixture_env(&f);
    let store = open_store(&f).await;
    store.handle_event(json!({ "type": "task_started", "task": "extend quuxflurb behavior" })).await;
    store.handle_event(json!({ "type": "user_preference", "key": "rag-hint", "value": "quuxflurb lives in src/widget.ts" })).await;
    store.handle_event(json!({ "type": "tool_result", "tool": "bash", "summary": format!("export TOKEN={SECRET}") })).await;
    let result = store.ensure_repo_indexed(None).await.unwrap();
    assert_eq!(result.reindexed, "full");
    store.refresh_repo_tree("src/\n  widget.ts (12 lines)");
    let context = store.retrieve("quuxflurb", 4000).await;
    assert!(context.working_notes.as_deref().unwrap().contains("extend quuxflurb behavior"));
    assert!(context.repo_tree.as_deref().unwrap().contains("widget.ts"));
    assert!(!context.rag.is_empty());
    assert_eq!(context.rag[0].path, "src/widget.ts");
    assert!(context.preferences.as_ref().unwrap().iter().any(|p| p.starts_with("rag-hint:")));
    let serialized = format!("{context:?}");
    assert!(!serialized.contains(SECRET));
    assert!(!serialized.contains("outcome:"));
    assert!(store.retrieve("quuxflurb", 0).await.rag.is_empty());
    store.close().await;
}

#[tokio::test]
async fn ensure_repo_indexed_is_idempotent_detects_staleness_and_dedups_in_flight_calls() {
    let f = fixture();
    let _env = fixture_env(&f);
    let store = open_store(&f).await;
    let first = store.ensure_repo_indexed(None).await.unwrap();
    assert_eq!(first.reindexed, "full");
    assert!(first.files >= 1 && first.chunks >= 1);
    assert_eq!(store.ensure_repo_indexed(None).await.unwrap().reindexed, "none");
    // Make the mtime change observable even on coarse-grained filesystems.
    std::thread::sleep(std::time::Duration::from_millis(20));
    f.write("src/widget.ts", "export function quuxflurb(x: number) {\n  return x * 3; // changed\n}\n");
    assert_eq!(store.ensure_repo_indexed(None).await.unwrap().reindexed, "partial");
    let (a, b) = tokio::join!(store.ensure_repo_indexed(None), store.ensure_repo_indexed(None));
    assert!(a.unwrap().files >= 1 && b.unwrap().files >= 1);
    assert!(rag_snapshot_path(&f.data, &project_id_for(&f.cwd)).exists());
    let stats = store.knowledge.stats().await;
    assert!(stats.files >= 1 && stats.chunks >= 1 && stats.indexed_at.is_some());
    store.close().await;
}

#[tokio::test]
async fn insights_are_deduplicated_and_survive_restart() {
    let f = fixture();
    let _env = fixture_env(&f);
    let store = open_store(&f).await;
    store.knowledge.add_insight("The widget entrypoint is src/widget.ts with quuxflurb.").await.unwrap();
    store.knowledge.add_insight("The widget entrypoint is src/widget.ts with quuxflurb.").await.unwrap();
    store.close().await;
    let reopened = reopen(&f).await;
    reopened.ensure_repo_indexed(None).await.unwrap();
    let hits = reopened.retrieve("quuxflurb entrypoint", 2000).await;
    assert!(hits.rag.iter().any(|h| h.path == "memory://insight"));
    reopened.close().await;
    let raw: Value = serde_json::from_str(&memory_json(&f)).unwrap();
    assert_eq!(raw["insights"].as_array().unwrap().len(), 1);
}

#[test]
fn redaction_and_sensitive_path_patterns() {
    let none: Vec<String> = vec![];
    assert_eq!(redact_secrets("Bearer abc123xyzDEF", &none), "Bearer [REDACTED]");
    let prefixed = redact_secrets("Authorization: Bearer abc123xyzDEF", &none);
    assert!(!prefixed.contains("abc123xyzDEF") && prefixed.contains("[REDACTED]"));
    assert_eq!(redact_secrets("{\"token\":\"topsecret-value\"}", &none), "{\"token\":\"[REDACTED]\"}");
    assert_eq!(redact_secrets("api_key = abcdefgh123", &none), "api_key = [REDACTED]");
    assert_eq!(redact_secrets("call https://x with sk-literal-999 now", &["sk-literal-999".to_string()]), "call https://x with [REDACTED] now");
    assert_eq!(redact_secrets("GOON_API_KEY=short", &none), "GOON_API_KEY=[REDACTED]");
    let obj = redact_object(&json!({ "apiKey": "k", "nested": { "password": "p", "keep": "v" } }), &none);
    assert_eq!(obj, json!({ "apiKey": "[REDACTED]", "nested": { "password": "[REDACTED]", "keep": "v" } }));
    for p in [".env", ".env.local", ".env.example", "server.pem", "id_rsa", "id_ed25519.pub", "client.key", ".npmrc", ".netrc", ".git-credentials", ".ssh/id_rsa", "secrets/a.txt", ".aws/credentials", "config/gcloud/creds.json"] {
        assert!(is_sensitive_path(p), "{p}");
    }
    for p in ["src/index.ts", "README.md", "docs/PLAN.md", "package.json"] {
        assert!(!is_sensitive_path(p), "{p}");
    }
}

#[tokio::test]
async fn env_credentials_are_auto_collected_for_redaction() {
    let token = "envcred-zqxwkj-123456789";
    let f = fixture();
    let config = f.config.to_string_lossy().to_string();
    let data = f.data.to_string_lossy().to_string();
    let _env = env(&[("XDG_CONFIG_HOME", Some(&config)), ("XDG_DATA_HOME", Some(&data)), ("GITHUB_TOKEN", Some(token))]);
    let store = open_store(&f).await;
    f.write("src/deploy.ts", &format!("const token = \"{token}\";\nexport const quuxdeploy = 1;\n"));
    assert_eq!(store.ensure_repo_indexed(None).await.unwrap().reindexed, "full");
    let rag_raw = std::fs::read_to_string(rag_snapshot_path(&f.data, &project_id_for(&f.cwd))).unwrap();
    assert!(!rag_raw.contains(token));
    assert!(rag_raw.contains("[REDACTED]"));
    store.handle_event(json!({ "type": "task_started", "task": format!("deploy using {token}") })).await;
    store.handle_event(json!({ "type": "task_completed", "outcome": "done" })).await;
    store.close().await;
    assert!(!memory_json(&f).contains(token));
}

#[test]
fn config_defaults_keep_memory_and_rag_opt_in() {
    let config = config::parse_config(&json!({})).unwrap();
    assert!(!config.memory_enabled);
    assert!(!config.rag_enabled);
    assert_eq!(config.rag_top_k, 10);
    assert_eq!(config.rag_capacity, 5000);
    assert_eq!(config.rag_threshold, 0.0);
    assert_eq!(config.rag_index_max_files, 2000);
    assert_eq!(config.rag_index_max_file_bytes, 1_048_576);
}

#[test]
fn memory_is_registered_in_the_tui_command_list_next_to_memories() {
    let commands = tui::SLASH_COMMANDS;
    let pos = |c: &str| commands.iter().position(|x| *x == c).unwrap();
    assert!(pos("/memories") < pos("/memory"));
}

#[tokio::test]
async fn corrupt_store_files_do_not_prevent_opening() {
    let f = fixture();
    let _env = fixture_env(&f);
    let dir = f.data.join("memory").join(project_id_for(&f.cwd));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("memory.json"), "{broken json").unwrap();
    let store = MemoryStore::open(&f.data, &project_id_for(&f.cwd), Some(&f.cwd), &[], rag()).await.unwrap();
    assert_eq!(store.exec_size(), 0);
    store.close().await;
}
