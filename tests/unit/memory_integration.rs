use crate::common::*;
use goon::memory::{MemoryAdapter, MemoryStore, RagSettings};
use goon::rag::{project_id_for, rag_snapshot_path};

const SECRET: &str = "goon-test-secret-abc123";

struct Setup {
    f: Fixture,
    _env: EnvGuard,
    store: Arc<session::SessionStore>,
    harness: Arc<harness::Harness>,
    events: Arc<Mutex<Vec<Value>>>,
    memory: Option<Arc<MemoryStore>>,
}

async fn setup(turns: Vec<Turn>, rag_enabled: bool, memory: Option<Arc<dyn harness::MemoryHooks>>) -> Setup {
    let f = fixture();
    let env = fixture_env(&f);
    f.write("src/notes.md", "The quuxflurb doc lives here.\n");
    let rt = runtime(&f.cwd, &f.data, "http://localhost/v1", json!({ "maxOutputTokens": 1024, "timeoutMs": 30000, "inlineReasoning": "off", "memoryEnabled": true, "ragEnabled": rag_enabled }));
    let store = store(&rt);
    let tools = registry(&f.cwd, tools::approve_all(), false);
    let (emit, events) = events();
    let (hooks, memory_store): (Arc<dyn harness::MemoryHooks>, Option<Arc<MemoryStore>>) = match memory {
        Some(h) => (h, None),
        None => {
            let settings = RagSettings { top_k: Some(10), capacity: Some(5000), threshold: Some(0.0), max_files: Some(2000), max_file_bytes: Some(1_048_576), embedder: None };
            let m = MemoryStore::open(&f.data, &project_id_for(&f.cwd), Some(&f.cwd), &[SECRET.to_string()], settings).await.unwrap();
            (Arc::new(MemoryAdapter(m.clone())), Some(m))
        }
    };
    let deps = harness::HarnessDeps { verify_enabled: Some(true), verify_runner: Some(Arc::new(PassRunner)), verify_policy: None, memory: Some(hooks) };
    let harness = harness::Harness::new(rt, store.clone(), tools, emit, Some(FakeModel::new(turns)), deps).unwrap();
    Setup { f, _env: env, store, harness, events, memory: memory_store }
}

fn of_type(events: &Arc<Mutex<Vec<Value>>>, kind: &str) -> Vec<Value> {
    events.lock().unwrap().iter().filter(|e| e["type"] == kind).cloned().collect()
}

fn memory_json(f: &Fixture) -> Value {
    serde_json::from_str(&std::fs::read_to_string(f.data.join("memory").join(project_id_for(&f.cwd)).join("memory.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn one_run_wires_index_retrieval_and_events_and_redacts_tool_results() {
    let s = setup(
        vec![
            invoke(vec![tc("w1", "write_file", json!({ "path": "note.ts", "content": "export const quuxnote = 1;\n" }))]),
            say("Wrote note.ts with the quuxnote export."),
            invoke(vec![tc("b1", "bash", json!({ "command": format!("echo {SECRET}") }))]),
            say("Status is fine."),
        ],
        true,
        None,
    )
    .await;
    let text = s.harness.run("create note.ts using the quuxflurb docs", &token(), None).await.unwrap();
    assert_match(&text, r"Wrote note\.ts");
    let indexed = of_type(&s.events, "memory_indexed");
    assert!(!indexed.is_empty() && indexed[0]["files"].as_u64().unwrap() >= 1 && indexed[0]["chunks"].as_u64().unwrap() >= 1);
    let retrieved = of_type(&s.events, "memory_retrieved");
    assert!(!retrieved.is_empty() && retrieved[0]["chunks"].as_u64().unwrap() >= 1);
    let budget = of_type(&s.events, "context_budget").into_iter().next().expect("context_budget event");
    assert_eq!(budget["inputBudget"], 128000 - 1024);
    let (sys, conv, files, input) = (budget["system"].as_u64().unwrap(), budget["conversation"].as_u64().unwrap(), budget["files"].as_u64().unwrap(), budget["inputBudget"].as_u64().unwrap());
    assert!(sys > 0 && conv > 0 && files > 0 && sys + conv + files <= input);
    assert!(!serde_json::to_string(&s.store.session.lock().unwrap().messages).unwrap().contains("<untrusted_context>"));

    let memory = s.memory.clone().unwrap();
    memory.persist().await.unwrap();
    let raw = memory_json(&s.f);
    assert_eq!(raw["exec"].as_array().unwrap().len(), 1);
    let lesson = raw["exec"][0]["text"].as_str().unwrap();
    assert_match(lesson, r"task: create note\.ts");
    assert_match(lesson, r"outcome: Wrote note\.ts");
    assert_match(lesson, r"edited note\.ts \(\d+ bytes\)");
    assert_match(lesson, "verify pass");
    let rag_raw = std::fs::read_to_string(rag_snapshot_path(&s.f.data, &project_id_for(&s.f.cwd))).unwrap();
    assert!(rag_raw.contains("quuxnote"));
    assert!(!rag_raw.contains(SECRET));

    let before = of_type(&s.events, "memory_indexed").len();
    s.harness.run(&format!("echo {SECRET} for status"), &token(), None).await.unwrap();
    assert_eq!(of_type(&s.events, "memory_indexed").len(), before);
    memory.persist().await.unwrap();
    let raw2 = memory_json(&s.f);
    let exec = raw2["exec"].as_array().unwrap();
    assert!(exec.len() >= 2);
    assert_match(exec.last().unwrap()["text"].as_str().unwrap(), r"echo \[REDACTED\]");
    assert!(!raw2.to_string().contains(SECRET));
    memory.close().await;
}

#[tokio::test]
async fn rag_disabled_never_indexes() {
    let s = setup(vec![say("Done.")], false, None).await;
    assert_match(&s.harness.run("just answer", &token(), None).await.unwrap(), r"Done\.");
    assert!(of_type(&s.events, "memory_indexed").is_empty());
    assert!(of_type(&s.events, "memory_retrieved").iter().all(|e| e["chunks"] == 0));
    let memory = s.memory.clone().unwrap();
    memory.persist().await.unwrap();
    assert_match(memory_json(&s.f)["exec"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap(), r"outcome: Done\.");
    assert!(!rag_snapshot_path(&s.f.data, &project_id_for(&s.f.cwd)).exists());
    memory.close().await;
}

struct BrokenMemory;

impl harness::MemoryHooks for BrokenMemory {
    fn handle_event(&self, _event: Value) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { Err(anyhow!("event boom")) })
    }
    fn retrieve(&self, _query: String, _budget: u64) -> BoxFuture<'static, Result<context_budget::MemoryPromptContext>> {
        Box::pin(async { Err(anyhow!("retrieve boom")) })
    }
    fn ensure_repo_indexed(&self, _cwd: PathBuf) -> BoxFuture<'static, Result<harness::RepoIndexOutcome>> {
        Box::pin(async { Err(anyhow!("index boom")) })
    }
}

#[tokio::test]
async fn memory_failures_surface_as_memory_error_and_never_fail_the_run() {
    let s = setup(vec![say("Still fine.")], true, Some(Arc::new(BrokenMemory))).await;
    assert_match(&s.harness.run("run with broken memory", &token(), None).await.unwrap(), r"Still fine\.");
    let errors: Vec<String> = of_type(&s.events, "memory_error").iter().map(|e| e["message"].as_str().unwrap_or("").to_string()).collect();
    assert!(errors.iter().any(|m| m.contains("index boom")), "{errors:?}");
    assert!(errors.iter().any(|m| m.contains("retrieve boom")), "{errors:?}");
    assert!(errors.iter().any(|m| m.contains("rejected an event")), "{errors:?}");
    assert!(!of_type(&s.events, "context_budget").is_empty());
}

#[tokio::test]
async fn a_failed_run_still_writes_task_completed_with_an_error_outcome() {
    let s = setup(vec![say("never reached")], true, None).await;
    let cancel = token();
    cancel.cancel();
    assert!(s.harness.run("will abort", &cancel, None).await.is_err());
    let memory = s.memory.clone().unwrap();
    memory.persist().await.unwrap();
    let raw = memory_json(&s.f);
    let exec = raw["exec"].as_array().unwrap();
    assert!(!exec.is_empty());
    assert_match(exec.last().unwrap()["text"].as_str().unwrap(), "outcome: error:");
    memory.close().await;
}
