//! Memory store: working notes (RAM), exec lessons, repo tree cache, lexical knowledge base
//! (`rag.json` + insights) and user preferences (`preferences.json`). Everything that reaches disk
//! is redacted first; writes are serialized through one queue and land atomically.

use crate::config::{atomic_json, config_dir, mkdir_private};
use crate::context_budget::{MemoryPromptContext, RagChunk};
use crate::harness::{MemoryHooks, RepoIndexOutcome};
use crate::jsutil::{now_iso, now_millis, take_chars, utf16_len};
use crate::rag::{chunk_file, index_repo, load_rag_snapshot, looks_binary, rag_snapshot_path, save_rag_snapshot, tokenize, walk_repo, AddDocument, Chunk, Embedder, LexicalIndex, RagFileRecord, RagSnapshot, RetrievedChunk};
use crate::redact::{collect_secrets, redact_secrets};
use anyhow::{bail, Result};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const EXEC_LIMIT: usize = 50;

fn truncate(text: &str, max: usize) -> String {
    let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if utf16_len(&clean) > max {
        format!("{}…", take_chars(&clean, max - 1))
    } else {
        clean
    }
}

fn estimate_tokens(text: &str) -> u64 {
    (utf16_len(text) as u64).div_ceil(4).max(1)
}

#[derive(Default, Clone)]
pub struct WorkingMemory {
    task: Option<String>,
    state: Option<String>,
    notes: Vec<String>,
}

impl WorkingMemory {
    pub fn set_task(&mut self, task: &str) {
        self.task = Some(truncate(task, 500)).filter(|t| !t.is_empty());
    }
    pub fn set_state(&mut self, state: &str) {
        self.state = Some(truncate(state, 200)).filter(|t| !t.is_empty());
    }
    pub fn add_note(&mut self, note: &str) {
        let clean = truncate(note, 500);
        if !clean.is_empty() {
            self.notes.push(clean);
        }
    }
    pub fn render(&self) -> Option<String> {
        let mut lines = vec![];
        if let Some(t) = &self.task {
            lines.push(format!("Task: {t}"));
        }
        if let Some(s) = &self.state {
            lines.push(format!("State: {s}"));
        }
        for n in &self.notes {
            lines.push(format!("- {n}"));
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
    pub fn clear(&mut self) {
        *self = WorkingMemory::default();
    }
}

#[derive(Clone, Debug)]
pub struct ExecLesson {
    pub text: String,
    pub at: String,
}

#[derive(Default)]
struct KnowledgeState {
    index: Option<LexicalIndex>,
    snapshot: Option<RagSnapshot>,
    snapshot_loaded: bool,
    dirty: bool,
    insights: Vec<String>,
}

pub struct EnsureResult {
    pub files: usize,
    pub chunks: usize,
    pub duration_ms: i64,
    pub reindexed: &'static str,
    pub truncated: bool,
}

pub struct KnowledgeStats {
    pub files: usize,
    pub chunks: usize,
    pub insights: usize,
    pub indexed_at: Option<String>,
}

pub struct KnowledgeMemory {
    rag_file: PathBuf,
    capacity: Option<usize>,
    threshold: Option<f64>,
    pub top_k: usize,
    max_files: usize,
    max_file_bytes: u64,
    embedder: Option<Arc<dyn Embedder>>,
    secrets: Vec<String>,
    state: tokio::sync::Mutex<KnowledgeState>,
}

fn snapshot_docs(snapshot: Option<&RagSnapshot>) -> Vec<AddDocument> {
    let mut docs = vec![];
    if let Some(s) = snapshot {
        for (path, record) in &s.files {
            for c in &record.chunks {
                docs.push(AddDocument { id: None, chunk: Chunk { path: path.clone(), start_line: c.start_line, end_line: c.end_line, content: c.content.clone() } });
            }
        }
    }
    docs
}

fn insight_docs(insights: &[String]) -> Vec<AddDocument> {
    let mut docs = vec![];
    for (i, text) in insights.iter().enumerate() {
        for c in chunk_file("memory://insight", text, None) {
            docs.push(AddDocument { id: Some(format!("insight:{i}:{}", c.start_line)), chunk: Chunk { path: "memory://insight".into(), ..c } });
        }
    }
    docs
}

impl KnowledgeMemory {
    fn new_index(&self) -> LexicalIndex {
        LexicalIndex::new(self.embedder.clone(), self.capacity, self.threshold)
    }

    async fn ensure_loaded(&self, s: &mut KnowledgeState) -> Result<()> {
        if !s.snapshot_loaded {
            s.snapshot = load_rag_snapshot(&self.rag_file);
            s.snapshot_loaded = true;
        }
        if s.index.is_none() {
            let mut index = self.new_index();
            let mut docs = snapshot_docs(s.snapshot.as_ref());
            docs.extend(insight_docs(&s.insights));
            index.add(docs).await?;
            s.index = Some(index);
        }
        Ok(())
    }

    pub async fn stats(&self) -> KnowledgeStats {
        let mut s = self.state.lock().await;
        if !s.snapshot_loaded {
            s.snapshot = load_rag_snapshot(&self.rag_file);
            s.snapshot_loaded = true;
        }
        KnowledgeStats {
            files: s.snapshot.as_ref().map(|x| x.files.len()).unwrap_or(0),
            chunks: s.snapshot.as_ref().map(|x| x.chunk_count()).unwrap_or(0),
            insights: s.insights.len(),
            indexed_at: s.snapshot.as_ref().map(|x| x.indexed_at.clone()).filter(|t| !t.is_empty()),
        }
    }

    fn chunk_path(&self, path: &str, root: &Path) -> Vec<AddDocument> {
        let Ok(buffer) = std::fs::read(root.join(path)) else { return vec![] };
        if looks_binary(&buffer) {
            return vec![];
        }
        let content = String::from_utf8_lossy(&buffer);
        chunk_file(path, &content, None)
            .into_iter()
            .map(|mut c| {
                c.content = redact_secrets(&c.content, &self.secrets);
                c
            })
            .filter(|c| !c.content.trim().is_empty())
            .map(|c| AddDocument { id: None, chunk: c })
            .collect()
    }

    /// Idempotent refresh: full index without a usable snapshot, else partial by mtime/size.
    pub async fn ensure_repo_indexed(&self, root: &Path) -> Result<EnsureResult> {
        let mut s = self.state.lock().await;
        self.ensure_loaded(&mut s).await?;
        let started = now_millis();
        let mut reindexed = "none";
        let truncated;
        let root_str = root.to_string_lossy().to_string();
        if s.snapshot.as_ref().map(|x| x.cwd != root_str).unwrap_or(true) {
            let result = index_repo(root, self.max_files, self.max_file_bytes, &self.secrets);
            let docs = snapshot_docs(Some(&result.snapshot));
            s.snapshot = Some(result.snapshot);
            let index = s.index.as_mut().unwrap();
            index.clear();
            index.add(docs).await?;
            reindexed = "full";
            truncated = result.truncated;
        } else {
            let walk = walk_repo(root, None, self.max_file_bytes, self.max_files);
            truncated = walk.truncated;
            let current: IndexMap<String, crate::rag::WalkedFile> = walk.files.iter().map(|f| (f.path.clone(), f.clone())).collect();
            let snapshot = s.snapshot.clone().unwrap();
            let changed: Vec<String> = walk.files.iter().filter(|f| match snapshot.files.get(&f.path) {
                None => true,
                Some(prev) => (prev.mtime_ms - f.mtime_ms).abs() > 0.5 || prev.size != f.size,
            }).map(|f| f.path.clone()).collect();
            let removed: Vec<String> = snapshot.files.keys().filter(|p| !current.contains_key(*p)).cloned().collect();
            if !changed.is_empty() || !removed.is_empty() {
                reindexed = "partial";
                for p in &removed {
                    s.index.as_mut().unwrap().remove_by_path(p);
                    s.snapshot.as_mut().unwrap().files.shift_remove(p);
                }
                for p in &changed {
                    s.index.as_mut().unwrap().remove_by_path(p);
                    let docs = self.chunk_path(p, root);
                    if docs.is_empty() {
                        s.snapshot.as_mut().unwrap().files.shift_remove(p);
                        continue;
                    }
                    let chunks: Vec<Chunk> = docs.iter().map(|d| d.chunk.clone()).collect();
                    s.index.as_mut().unwrap().add(docs).await?;
                    let info = &current[p];
                    s.snapshot.as_mut().unwrap().files.insert(p.clone(), RagFileRecord { mtime_ms: info.mtime_ms, size: info.size, chunks });
                }
                s.snapshot.as_mut().unwrap().indexed_at = now_iso();
            }
        }
        if reindexed != "none" {
            s.dirty = true;
            if let Some(snap) = &s.snapshot {
                save_rag_snapshot(&self.rag_file, snap)?;
            }
            s.dirty = false;
        }
        Ok(EnsureResult {
            files: s.snapshot.as_ref().map(|x| x.files.len()).unwrap_or(0),
            chunks: s.snapshot.as_ref().map(|x| x.chunk_count()).unwrap_or(0),
            duration_ms: now_millis() - started,
            reindexed,
            truncated,
        })
    }

    pub async fn add_insight(&self, text: &str) -> Result<()> {
        let clean = redact_secrets(text, &self.secrets).trim().to_string();
        if clean.is_empty() {
            return Ok(());
        }
        let mut s = self.state.lock().await;
        if s.insights.contains(&clean) {
            return Ok(());
        }
        s.insights.push(clean.clone());
        let n = s.insights.len() - 1;
        self.ensure_loaded(&mut s).await?;
        let docs: Vec<AddDocument> = chunk_file("memory://insight", &clean, None).into_iter().map(|c| AddDocument { id: Some(format!("insight:{n}:{}", c.start_line)), chunk: Chunk { path: "memory://insight".into(), ..c } }).collect();
        s.index.as_mut().unwrap().add(docs).await?;
        s.dirty = true;
        Ok(())
    }

    pub async fn retrieve(&self, query: &str, k: usize, budget_tokens: Option<u64>) -> Result<Vec<RetrievedChunk>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let mut s = self.state.lock().await;
        self.ensure_loaded(&mut s).await?;
        let hits = s.index.as_mut().unwrap().retrieve(query, k.max(1)).await?;
        let mut selected = vec![];
        let mut used = 0;
        for h in hits {
            let cost = estimate_tokens(&h.chunk.content);
            if let Some(b) = budget_tokens {
                if used + cost > b {
                    continue;
                }
            }
            used += cost;
            selected.push(h);
        }
        Ok(selected)
    }

    pub async fn insight_list(&self) -> Vec<String> {
        self.state.lock().await.insights.clone()
    }

    async fn save_if_dirty(&self) -> Result<()> {
        let mut s = self.state.lock().await;
        if !s.dirty {
            return Ok(());
        }
        if let Some(snap) = &s.snapshot {
            save_rag_snapshot(&self.rag_file, snap)?;
        }
        s.dirty = false;
        Ok(())
    }
}

pub struct PreferenceMemory {
    pub file: PathBuf,
    values: Mutex<IndexMap<String, String>>,
}

impl PreferenceMemory {
    pub fn get(&self, key: &str) -> Option<String> {
        self.values.lock().unwrap().get(key).cloned()
    }
    pub fn all(&self) -> IndexMap<String, String> {
        self.values.lock().unwrap().clone()
    }
    pub fn len(&self) -> usize {
        self.values.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn write(&self, values: &IndexMap<String, String>) -> Result<()> {
        atomic_json(&self.file, &json!(values), false)
    }
    pub fn apply_set(&self, key: &str, value: &str) -> Result<()> {
        let snapshot = {
            let mut v = self.values.lock().unwrap();
            v.insert(key.into(), value.into());
            v.clone()
        };
        self.write(&snapshot)
    }
    pub fn remove(&self, key: &str) -> Result<bool> {
        let snapshot = {
            let mut v = self.values.lock().unwrap();
            if v.shift_remove(key).is_none() {
                return Ok(false);
            }
            v.clone()
        };
        self.write(&snapshot)?;
        Ok(true)
    }
}

pub struct RagSettings {
    pub top_k: Option<usize>,
    pub capacity: Option<usize>,
    pub threshold: Option<f64>,
    pub max_files: Option<usize>,
    pub max_file_bytes: Option<u64>,
    pub embedder: Option<Arc<dyn Embedder>>,
}

pub struct MemoryStore {
    pub working: Mutex<WorkingMemory>,
    exec: Mutex<Vec<ExecLesson>>,
    repo: Mutex<(Option<String>, Option<String>)>,
    pub knowledge: KnowledgeMemory,
    pub preference: PreferenceMemory,
    pub project_id: String,
    root: Option<PathBuf>,
    secrets: Vec<String>,
    memory_file: PathBuf,
    chain: tokio::sync::Mutex<()>,
}

impl MemoryStore {
    pub async fn open(data_dir: &Path, project_id: &str, root: Option<&Path>, extra_secrets: &[String], rag: RagSettings) -> Result<Arc<MemoryStore>> {
        let mut secrets = collect_secrets();
        for s in extra_secrets {
            if !secrets.contains(s) {
                secrets.push(s.clone());
            }
        }
        let memory_dir = data_dir.join("memory").join(project_id);
        std::fs::create_dir_all(&memory_dir)?;
        mkdir_private(&memory_dir)?;
        let _ = std::fs::create_dir_all(config_dir()).and_then(|_| mkdir_private(&config_dir()));
        let memory_file = memory_dir.join("memory.json");
        let data: Option<Value> = std::fs::read_to_string(&memory_file).ok().and_then(|t| serde_json::from_str(&t).ok());
        let exec: Vec<ExecLesson> = data
            .as_ref()
            .and_then(|d| d.get("exec"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter(|e| e.get("text").map(Value::is_string).unwrap_or(false)).map(|e| ExecLesson { text: e["text"].as_str().unwrap().into(), at: e.get("at").map(crate::jsutil::js_string).unwrap_or_else(now_iso) }).collect::<Vec<_>>())
            .map(|v| v[v.len().saturating_sub(EXEC_LIMIT)..].to_vec())
            .unwrap_or_default();
        let insights: Vec<String> = data.as_ref().and_then(|d| d.get("insights")).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
        let repo = data.as_ref().and_then(|d| d.get("repo")).map(|r| (r.get("tree").and_then(Value::as_str).map(str::to_string), r.get("refreshedAt").and_then(Value::as_str).map(str::to_string))).unwrap_or((None, None));
        let pref_file = config_dir().join("preferences.json");
        let mut prefs = IndexMap::new();
        if let Some(Value::Object(m)) = std::fs::read_to_string(&pref_file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            for (k, v) in m {
                if let Value::String(s) = v {
                    prefs.insert(k, s);
                }
            }
        }
        let knowledge = KnowledgeMemory {
            rag_file: rag_snapshot_path(data_dir, project_id),
            capacity: Some(rag.capacity.unwrap_or(5000)),
            threshold: rag.threshold,
            top_k: rag.top_k.unwrap_or(10),
            max_files: rag.max_files.unwrap_or(2000),
            max_file_bytes: rag.max_file_bytes.unwrap_or(1_048_576),
            embedder: rag.embedder,
            secrets: secrets.clone(),
            state: tokio::sync::Mutex::new(KnowledgeState { insights, ..Default::default() }),
        };
        Ok(Arc::new(MemoryStore {
            working: Mutex::new(WorkingMemory::default()),
            exec: Mutex::new(exec),
            repo: Mutex::new(repo),
            knowledge,
            preference: PreferenceMemory { file: pref_file, values: Mutex::new(prefs) },
            project_id: project_id.into(),
            root: root.map(Path::to_path_buf),
            secrets,
            memory_file,
            chain: tokio::sync::Mutex::new(()),
        }))
    }

    pub fn exec_size(&self) -> usize {
        self.exec.lock().unwrap().len()
    }

    pub fn repo_tree(&self) -> Option<String> {
        self.repo.lock().unwrap().0.clone()
    }

    fn append_lesson(&self, text: &str) {
        let clean = truncate(text, 400);
        if clean.is_empty() {
            return;
        }
        let mut e = self.exec.lock().unwrap();
        e.push(ExecLesson { text: clean, at: now_iso() });
        let n = e.len();
        if n > EXEC_LIMIT {
            e.drain(..n - EXEC_LIMIT);
        }
    }

    fn apply_event(&self, event: &Value) {
        let s = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let r = |t: &str| redact_secrets(t, &self.secrets);
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "task_started" => self.working.lock().unwrap().set_task(&r(&s("task"))),
            "state_changed" => {
                let mut w = self.working.lock().unwrap();
                w.set_state(&r(&s("state")));
                if !s("note").is_empty() {
                    w.add_note(&r(&s("note")));
                }
            }
            "tool_result" => self.working.lock().unwrap().add_note(&format!("tool {}: {}", s("tool"), r(&s("summary")))),
            "patch_applied" => {
                let bytes = event.get("bytes").map(crate::jsutil::js_string).unwrap_or_else(|| "0".into());
                self.working.lock().unwrap().add_note(&format!("edited {} ({bytes} bytes)", s("path")));
                let mut repo = self.repo.lock().unwrap();
                if repo.0.is_some() {
                    *repo = (None, None);
                }
            }
            "verify_result" => {
                let issues = event.get("issues").map(crate::jsutil::js_string).unwrap_or_else(|| "0".into());
                self.working.lock().unwrap().add_note(&format!("verify {} ({issues} issue(s))", s("status")));
            }
            "assistant_message" => self.working.lock().unwrap().add_note(&r(&s("text"))),
            "task_completed" => {
                let (task, notes) = {
                    let w = self.working.lock().unwrap();
                    (w.task.clone(), w.notes.clone())
                };
                let mut parts = vec![];
                if let Some(t) = task {
                    parts.push(format!("task: {t}"));
                }
                parts.push(format!("outcome: {}", s("outcome")));
                if !notes.is_empty() {
                    parts.push(format!("notes: {}", notes[notes.len().saturating_sub(3)..].join(" | ")));
                }
                self.append_lesson(&r(&parts.join(" | ")));
                self.working.lock().unwrap().clear();
            }
            "user_preference" => {
                let key = r(&s("key")).trim().to_string();
                let value = r(&s("value"));
                if !key.is_empty() {
                    let _ = self.preference.apply_set(&key, &value);
                }
            }
            _ => {}
        }
    }

    pub async fn handle_event(&self, event: Value) {
        let _guard = self.chain.lock().await;
        self.apply_event(&event);
        let _ = self.persist_now().await;
    }

    pub async fn retrieve(&self, query: &str, budget_tokens: u64) -> MemoryPromptContext {
        let rag = self.knowledge.retrieve(query, self.knowledge.top_k, Some(budget_tokens)).await.unwrap_or_default();
        let mut ctx = MemoryPromptContext {
            rag: rag.into_iter().map(|h| RagChunk { path: h.chunk.path, start_line: h.chunk.start_line, end_line: h.chunk.end_line, score: h.score, content: h.chunk.content }).collect(),
            ..Default::default()
        };
        ctx.working_notes = self.working.lock().unwrap().render();
        ctx.repo_tree = self.repo_tree().filter(|t| !t.is_empty());
        let prefs = self.relevant_preferences(query);
        if !prefs.is_empty() {
            ctx.preferences = Some(prefs);
        }
        ctx
    }

    fn relevant_preferences(&self, query: &str) -> Vec<String> {
        let tokens: indexmap::IndexSet<String> = tokenize(query).into_iter().collect();
        if tokens.is_empty() {
            return vec![];
        }
        let mut hits = vec![];
        for (k, v) in self.preference.all() {
            let hay = format!("{k} {v}").to_lowercase();
            if tokens.iter().any(|t| hay.contains(t)) {
                hits.push(format!("{k}: {v}"));
                if hits.len() >= 8 {
                    break;
                }
            }
        }
        hits
    }

    pub async fn ensure_repo_indexed(&self, root: Option<&Path>) -> Result<EnsureResult> {
        let Some(target) = root.map(Path::to_path_buf).or_else(|| self.root.clone()) else { bail!("MemoryStore.open requires `root` (or pass it to ensureRepoIndexed).") };
        let _guard = self.chain.lock().await;
        self.knowledge.ensure_repo_indexed(&target).await
    }

    async fn persist_now(&self) -> Result<()> {
        let mut data = Map::new();
        data.insert("version".into(), json!(1));
        data.insert("exec".into(), Value::Array(self.exec.lock().unwrap().iter().map(|l| json!({ "text": l.text, "at": l.at })).collect()));
        data.insert("insights".into(), json!(self.knowledge.insight_list().await));
        let (tree, refreshed) = self.repo.lock().unwrap().clone();
        if tree.as_deref().map(|t| !t.is_empty()).unwrap_or(false) || refreshed.is_some() {
            let mut repo = Map::new();
            if let Some(t) = tree.filter(|t| !t.is_empty()) {
                repo.insert("tree".into(), json!(t));
            }
            if let Some(r) = refreshed {
                repo.insert("refreshedAt".into(), json!(r));
            }
            data.insert("repo".into(), Value::Object(repo));
        }
        atomic_json(&self.memory_file, &Value::Object(data), true)?;
        self.knowledge.save_if_dirty().await
    }

    pub async fn persist(&self) -> Result<()> {
        let _guard = self.chain.lock().await;
        self.persist_now().await
    }

    pub async fn close(&self) {
        let _ = self.persist().await;
    }
}

/// Harness seam implementation.
pub struct MemoryAdapter(pub Arc<MemoryStore>);

impl MemoryHooks for MemoryAdapter {
    fn handle_event(&self, event: Value) -> BoxFuture<'static, Result<()>> {
        let store = self.0.clone();
        Box::pin(async move {
            store.handle_event(event).await;
            Ok(())
        })
    }

    fn retrieve(&self, query: String, budget_tokens: u64) -> BoxFuture<'static, Result<MemoryPromptContext>> {
        let store = self.0.clone();
        Box::pin(async move { Ok(store.retrieve(&query, budget_tokens).await) })
    }

    fn ensure_repo_indexed(&self, cwd: PathBuf) -> BoxFuture<'static, Result<RepoIndexOutcome>> {
        let store = self.0.clone();
        Box::pin(async move {
            let r = store.ensure_repo_indexed(Some(&cwd)).await?;
            Ok(RepoIndexOutcome { files: r.files as u64, chunks: r.chunks as u64, duration_ms: r.duration_ms.max(0) as u64, reindexed: r.reindexed.into() })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_index_and_retrieve() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("auth.ts"), "export function login(user: string) {\n  return checkPassword(user);\n}\n").unwrap();
        std::env::set_var("XDG_CONFIG_HOME", dir.path().join("cfg"));
        let store = MemoryStore::open(&dir.path().join("data"), "p1", Some(&ws), &[], RagSettings { top_k: None, capacity: None, threshold: None, max_files: None, max_file_bytes: None, embedder: None }).await.unwrap();
        let r = store.ensure_repo_indexed(None).await.unwrap();
        assert_eq!((r.files, r.reindexed), (1, "full"));
        assert_eq!(store.ensure_repo_indexed(None).await.unwrap().reindexed, "none");
        store.handle_event(json!({"type": "task_started", "task": "fix login"})).await;
        store.handle_event(json!({"type": "tool_result", "tool": "read_file", "summary": "auth.ts"})).await;
        let ctx = store.retrieve("login password", 1024).await;
        assert_eq!(ctx.rag[0].path, "auth.ts");
        assert!(ctx.working_notes.unwrap().contains("Task: fix login"));
        store.handle_event(json!({"type": "task_completed", "outcome": "done"})).await;
        assert_eq!(store.exec_size(), 1);
    }
}
