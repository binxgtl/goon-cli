//! Lexical RAG for repo understanding: hashed term-frequency vectors (FNV-1a), declaration-aware
//! chunking, `.goonignore`, a defensive repo walk and an in-memory LRU index persisted as
//! `rag.json` (chunks only; vectors are recomputed on load).

use crate::config::{atomic_json, mkdir_private};
use crate::jsutil::{locale_cmp, now_iso, now_millis, utf16_len};
use crate::redact::{is_sensitive_path, redact_secrets};
use anyhow::Result;
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

pub const EMBEDDING_DIM: usize = 384;

pub trait Embedder: Send + Sync {
    /// Embeds each text into a fixed-length vector.
    fn embed(&self, texts: Vec<String>, cancel: Option<CancellationToken>) -> BoxFuture<'static, Result<Vec<Vec<f64>>>>;
}

/// FNV-1a 32-bit over UTF-16 code units (`charCodeAt`).
pub fn fnv1a(text: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for unit in text.encode_utf16() {
        hash ^= unit as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// Lowercase alphanumeric/underscore tokens of length >= 2.
pub fn tokenize(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[a-z0-9_]+").unwrap());
    re.find_iter(&text.to_lowercase()).map(|m| m.as_str().to_string()).filter(|t| t.len() >= 2).collect()
}

pub struct HashEmbedder;

impl HashEmbedder {
    pub fn embed_one(text: &str) -> Vec<f64> {
        let mut v = vec![0.0f64; EMBEDDING_DIM];
        for token in tokenize(text) {
            let hash = fnv1a(&token);
            let sign = if (hash >> 31) & 1 == 1 { -1.0 } else { 1.0 };
            v[(hash as usize) % EMBEDDING_DIM] += sign;
        }
        let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        v
    }
}

impl Embedder for HashEmbedder {
    fn embed(&self, texts: Vec<String>, _cancel: Option<CancellationToken>) -> BoxFuture<'static, Result<Vec<Vec<f64>>>> {
        Box::pin(async move { Ok(texts.iter().map(|t| HashEmbedder::embed_one(t)).collect()) })
    }
}

pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chunk {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RetrievedChunk {
    pub chunk: Chunk,
    pub score: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct ChunkOptions {
    pub soft_limit_chars: usize,
    pub hard_limit_chars: usize,
    pub window_lines: usize,
    pub overlap_lines: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        ChunkOptions { soft_limit_chars: 1500, hard_limit_chars: 4000, window_lines: 40, overlap_lines: 8 }
    }
}

fn declaration_re(path: &str) -> Option<&'static Regex> {
    static TS: OnceLock<Regex> = OnceLock::new();
    static JS: OnceLock<Regex> = OnceLock::new();
    static GO: OnceLock<Regex> = OnceLock::new();
    static PY: OnceLock<Regex> = OnceLock::new();
    let base = path.rsplit('/').next().unwrap_or(path);
    let w = "[A-Za-z0-9_]";
    if [".ts", ".tsx", ".mts", ".cts"].iter().any(|e| base.ends_with(e)) {
        return Some(TS.get_or_init(|| Regex::new(&format!(r"^(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(?:function\s*\*?\s*{w}+|(?:class|interface|enum|namespace)\s+{w}+|type\s+{w}+\s*=|(?:const|let|var)\s+{w}+\s*=\s*(?:async\s*)?(?:function(?-u:\b)|\())")).unwrap()));
    }
    if [".js", ".jsx", ".mjs", ".cjs"].iter().any(|e| base.ends_with(e)) {
        return Some(JS.get_or_init(|| {
            Regex::new(&format!(r"^(?:export\s+)?(?:default\s+)?(?:async\s+)?(?:function\s*\*?\s*{w}+|class\s+{w}+|(?:const|let|var)\s+{w}+\s*=\s*(?:async\s*)?(?:function(?-u:\b)|\())")).unwrap()
        }));
    }
    if base.ends_with(".go") {
        return Some(GO.get_or_init(|| Regex::new(&format!(r"^(?:func\s+(?:\([^)]*\)\s*)?{w}+|type\s+{w}+\s+(?:struct|interface)(?-u:\b))")).unwrap()));
    }
    if base.ends_with(".py") {
        return Some(PY.get_or_init(|| Regex::new(&format!(r"^\s*(?:async\s+)?def\s+{w}+|^\s*class\s+{w}+")).unwrap()));
    }
    None
}

fn make_chunk(path: &str, lines: &[&str], start: usize, end: usize) -> Option<Chunk> {
    let slice = &lines[start.min(lines.len())..end.min(lines.len())];
    let mut s = 0;
    let mut e = slice.len();
    while s < e && slice[s].trim().is_empty() {
        s += 1;
    }
    while e > s && slice[e - 1].trim().is_empty() {
        e -= 1;
    }
    if s == e {
        return None;
    }
    Some(Chunk { path: path.into(), start_line: (start + s + 1) as u64, end_line: (start + e) as u64, content: slice[s..e].join("\n") })
}

fn split_chunk(chunk: Chunk, o: &ChunkOptions) -> Vec<Chunk> {
    if utf16_len(&chunk.content) <= o.soft_limit_chars {
        return vec![chunk];
    }
    let lines: Vec<&str> = chunk.content.split('\n').collect();
    let stride = o.window_lines.saturating_sub(o.overlap_lines).max(1);
    let offset = chunk.start_line - 1;
    let mut out = vec![];
    let mut i = 0;
    while i < lines.len() {
        let len = o.window_lines.min(lines.len() - i);
        if len == 0 {
            break;
        }
        if let Some(mut piece) = make_chunk(&chunk.path, &lines, i, i + len) {
            piece.start_line += offset;
            piece.end_line += offset;
            if utf16_len(&piece.content) > o.hard_limit_chars {
                let pl: Vec<&str> = piece.content.split('\n').collect();
                let mut take = 0;
                while take < pl.len() {
                    let mut length = utf16_len(pl[take]);
                    let mut next = take + 1;
                    while next < pl.len() && length + 1 + utf16_len(pl[next]) <= o.hard_limit_chars {
                        length += 1 + utf16_len(pl[next]);
                        next += 1;
                    }
                    out.push(Chunk { path: piece.path.clone(), start_line: piece.start_line + take as u64, end_line: piece.start_line + next as u64 - 1, content: pl[take..next].join("\n") });
                    take = next;
                }
            } else {
                out.push(piece);
            }
        }
        if i + o.window_lines >= lines.len() {
            break;
        }
        i += stride;
    }
    out
}

/// Declaration-aware chunking with 40-line/8-overlap window fallback and size caps.
pub fn chunk_file(path: &str, content: &str, options: Option<ChunkOptions>) -> Vec<Chunk> {
    let o = options.unwrap_or_default();
    let normalized: Vec<&str> = content.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let lines = normalized;
    let mut decl = vec![];
    if let Some(re) = declaration_re(path) {
        for (i, l) in lines.iter().enumerate() {
            if re.is_match(l) {
                decl.push(i);
            }
        }
    }
    let mut raw = vec![];
    if !decl.is_empty() {
        for (idx, start) in decl.iter().enumerate() {
            let end = decl.get(idx + 1).copied().unwrap_or(lines.len());
            if let Some(c) = make_chunk(path, &lines, *start, end) {
                raw.push(c);
            }
        }
    } else {
        let stride = o.window_lines.saturating_sub(o.overlap_lines).max(1);
        let mut i = 0;
        while i < lines.len() {
            let Some(c) = make_chunk(path, &lines, i, (i + o.window_lines).min(lines.len())) else { break };
            raw.push(c);
            if i + o.window_lines >= lines.len() {
                break;
            }
            i += stride;
        }
    }
    raw.into_iter().flat_map(|c| split_chunk(c, &o)).collect()
}

// ---------------------------------------------------------------------------------------------
// .goonignore
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GoonignoreRule {
    pub pattern: String,
    pub negated: bool,
    pub dir_only: bool,
    pub anchored: bool,
    pub regex: Regex,
}

fn translate_pattern(pattern: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let rest = |i: usize| -> String { chars[i..].iter().collect() };
    while i < chars.len() {
        let r = rest(i);
        if r.starts_with("/**/") {
            out.push_str("(?:/.*)?/");
            i += 4;
            continue;
        }
        if r.starts_with("**/") {
            out.push_str("(?:.*/)?");
            i += 3;
            continue;
        }
        if r.starts_with("/**") {
            out.push_str("/.*");
            i += 3;
            continue;
        }
        match chars[i] {
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            c => out.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    out
}

pub fn parse_goonignore(text: &str) -> Vec<GoonignoreRule> {
    let mut rules = vec![];
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut pattern = line;
        let negated = pattern.starts_with('!');
        if negated {
            pattern = &pattern[1..];
        }
        if pattern.is_empty() {
            continue;
        }
        let dir_only = pattern.ends_with('/');
        if dir_only {
            pattern = &pattern[..pattern.len() - 1];
        }
        if pattern.is_empty() {
            continue;
        }
        let anchored = pattern.contains('/');
        let cleaned = pattern.strip_prefix('/').unwrap_or(pattern);
        if cleaned.is_empty() {
            continue;
        }
        let body = translate_pattern(cleaned);
        let source = if anchored { format!("^{body}$") } else { format!("^(?:.*/)?{body}$") };
        if let Ok(regex) = Regex::new(&source) {
            rules.push(GoonignoreRule { pattern: line.into(), negated, dir_only, anchored, regex });
        }
    }
    rules
}

/// Gitignore-style last-match-wins over the path and each parent directory.
pub fn is_ignored(rel: &str, rules: &[GoonignoreRule]) -> bool {
    let clean = rel.replace('\\', "/");
    let clean = clean.trim_start_matches('/');
    if clean.is_empty() {
        return false;
    }
    let segments: Vec<&str> = clean.split('/').collect();
    let mut ignored = false;
    for depth in 1..=segments.len() {
        let prefix = segments[..depth].join("/");
        let is_dir = depth < segments.len();
        for rule in rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            if rule.regex.is_match(&prefix) {
                ignored = !rule.negated;
            }
        }
    }
    ignored
}

pub const DEFAULT_SKIP_DIRS: [&str; 18] =
    [".git", "node_modules", "vendor", "dist", "build", "out", ".next", ".cache", ".turbo", "coverage", ".venv", "venv", "__pycache__", ".idea", ".vscode", "target", "bin", "obj"];

#[derive(Clone, Debug)]
pub struct WalkedFile {
    pub path: String,
    pub absolute_path: PathBuf,
    pub size: u64,
    pub mtime_ms: f64,
}

pub struct WalkResult {
    pub files: Vec<WalkedFile>,
    pub truncated: bool,
}

fn within_root(root: &Path, target: &Path) -> bool {
    target != root && target.starts_with(root)
}

fn mtime_ms(meta: &std::fs::Metadata) -> f64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
}

pub fn walk_repo(root: &Path, rules: Option<&[GoonignoreRule]>, max_file_bytes: u64, max_files: usize) -> WalkResult {
    let owned;
    let rules = match rules {
        Some(r) => r,
        None => {
            owned = std::fs::read_to_string(root.join(".goonignore")).map(|t| parse_goonignore(&t)).unwrap_or_default();
            &owned
        }
    };
    let root_real = std::fs::canonicalize(root).map(crate::config::strip_verbatim).unwrap_or_else(|_| root.to_path_buf());
    let mut files = vec![];
    let mut truncated = false;
    fn visit(dir: &Path, rel_dir: &str, depth: usize, root_real: &Path, rules: &[GoonignoreRule], max_bytes: u64, max_files: usize, files: &mut Vec<WalkedFile>, truncated: &mut bool) {
        if files.len() >= max_files {
            *truncated = true;
            return;
        }
        if depth > 32 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by(|a, b| locale_cmp(&a.file_name().to_string_lossy(), &b.file_name().to_string_lossy()));
        for entry in entries {
            if files.len() >= max_files {
                *truncated = true;
                return;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let rel = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if DEFAULT_SKIP_DIRS.contains(&name.as_str()) || is_ignored(&rel, rules) {
                    continue;
                }
                visit(&dir.join(&name), &rel, depth + 1, root_real, rules, max_bytes, max_files, files, truncated);
                continue;
            }
            if !ft.is_file() || is_ignored(&rel, rules) || is_sensitive_path(&rel) {
                continue;
            }
            let abs = dir.join(&name);
            let Ok(meta) = std::fs::symlink_metadata(&abs) else { continue };
            if meta.file_type().is_symlink() || meta.len() > max_bytes {
                continue;
            }
            let Ok(real) = std::fs::canonicalize(&abs).map(crate::config::strip_verbatim) else { continue };
            if !within_root(root_real, &real) {
                continue;
            }
            files.push(WalkedFile { path: rel, absolute_path: abs, size: meta.len(), mtime_ms: mtime_ms(&meta) });
        }
    }
    visit(&root_real, "", 0, &root_real, rules, max_file_bytes, max_files, &mut files, &mut truncated);
    files.sort_by(|a, b| locale_cmp(&a.path, &b.path));
    WalkResult { files, truncated }
}

fn read_workspace_file_safely(path: &Path, root_real: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > max_bytes {
        return None;
    }
    let real = std::fs::canonicalize(path).map(crate::config::strip_verbatim).ok()?;
    if !within_root(root_real, &real) {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    (bytes.len() as u64 <= max_bytes).then_some(bytes)
}

/// NUL byte in the first 8 KiB, or more than 30% control bytes.
pub fn looks_binary(buffer: &[u8]) -> bool {
    let sample = &buffer[..buffer.len().min(8192)];
    let mut suspicious = 0;
    for &b in sample {
        if b == 0 {
            return true;
        }
        if b < 9 || (b > 13 && b < 32) {
            suspicious += 1;
        }
    }
    !sample.is_empty() && suspicious as f64 / sample.len() as f64 > 0.3
}

pub const RAG_SNAPSHOT_VERSION: u64 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RagFileRecord {
    #[serde(serialize_with = "crate::jsutil::js_f64::serialize")]
    pub mtime_ms: f64,
    pub size: u64,
    pub chunks: Vec<Chunk>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RagSnapshot {
    pub cwd: String,
    pub indexed_at: String,
    pub files: IndexMap<String, RagFileRecord>,
}

impl RagSnapshot {
    pub fn to_value(&self) -> Value {
        serde_json::json!({ "version": RAG_SNAPSHOT_VERSION, "cwd": self.cwd, "indexedAt": self.indexed_at, "files": self.files })
    }

    pub fn chunk_count(&self) -> usize {
        self.files.values().map(|r| r.chunks.len()).sum()
    }
}

pub struct RepoIndexResult {
    pub snapshot: RagSnapshot,
    pub files: usize,
    pub chunks: usize,
    pub duration_ms: i64,
    pub truncated: bool,
}

/// Reads, redacts and chunks every eligible file under `root`.
pub fn index_repo(root: &Path, max_files: usize, max_file_bytes: u64, secrets: &[String]) -> RepoIndexResult {
    let started = now_millis();
    let walk = walk_repo(root, None, max_file_bytes, max_files);
    let root_real = std::fs::canonicalize(root).map(crate::config::strip_verbatim).unwrap_or_else(|_| root.to_path_buf());
    let mut files = IndexMap::new();
    let mut count = 0;
    for f in &walk.files {
        let Some(buffer) = read_workspace_file_safely(&f.absolute_path, &root_real, max_file_bytes) else { continue };
        if looks_binary(&buffer) {
            continue;
        }
        let content = String::from_utf8_lossy(&buffer);
        let chunks: Vec<Chunk> = chunk_file(&f.path, &content, None)
            .into_iter()
            .map(|mut c| {
                c.content = redact_secrets(&c.content, secrets);
                c
            })
            .filter(|c| !c.content.trim().is_empty())
            .collect();
        if chunks.is_empty() {
            continue;
        }
        count += chunks.len();
        files.insert(f.path.clone(), RagFileRecord { mtime_ms: f.mtime_ms, size: f.size, chunks });
    }
    let n = files.len();
    RepoIndexResult {
        snapshot: RagSnapshot { cwd: root.to_string_lossy().to_string(), indexed_at: now_iso(), files },
        files: n,
        chunks: count,
        duration_ms: now_millis() - started,
        truncated: walk.truncated,
    }
}

#[derive(Clone, Debug)]
pub struct AddDocument {
    pub id: Option<String>,
    pub chunk: Chunk,
}

struct LexicalEntry {
    chunk: Chunk,
    vector: Vec<f64>,
    last_access: u64,
}

/// In-memory lexical index: linear-scan cosine, deterministic ties, LRU eviction.
pub struct LexicalIndex {
    entries: IndexMap<String, LexicalEntry>,
    embedder: Arc<dyn Embedder>,
    capacity: usize,
    threshold: f64,
    clock: u64,
}

impl LexicalIndex {
    pub fn new(embedder: Option<Arc<dyn Embedder>>, capacity: Option<usize>, threshold: Option<f64>) -> Self {
        LexicalIndex {
            entries: IndexMap::new(),
            embedder: embedder.unwrap_or_else(|| Arc::new(HashEmbedder)),
            capacity: capacity.unwrap_or(5000).max(1),
            threshold: threshold.unwrap_or(0.0),
            clock: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub async fn add(&mut self, docs: Vec<AddDocument>) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }
        let vectors = self.embedder.embed(docs.iter().map(|d| d.chunk.content.clone()).collect(), None).await?;
        for (i, doc) in docs.into_iter().enumerate() {
            let id = doc.id.clone().unwrap_or_else(|| format!("{}:{}", doc.chunk.path, doc.chunk.start_line));
            self.clock += 1;
            let vector = vectors.get(i).cloned().unwrap_or_else(|| vec![0.0; EMBEDDING_DIM]);
            self.entries.insert(id, LexicalEntry { chunk: doc.chunk, vector, last_access: self.clock });
        }
        if self.entries.len() > self.capacity {
            let overflow = self.entries.len() - self.capacity;
            let mut by_access: Vec<(String, u64)> = self.entries.iter().map(|(k, e)| (k.clone(), e.last_access)).collect();
            by_access.sort_by_key(|(_, a)| *a);
            for (k, _) in by_access.into_iter().take(overflow) {
                self.entries.shift_remove(&k);
            }
        }
        Ok(())
    }

    pub fn remove_by_path(&mut self, path: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, e| e.chunk.path != path);
        before - self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub async fn retrieve(&mut self, query: &str, k: usize) -> Result<Vec<RetrievedChunk>> {
        if query.trim().is_empty() || k == 0 || self.entries.is_empty() {
            return Ok(vec![]);
        }
        let vectors = self.embedder.embed(vec![query.to_string()], None).await?;
        let Some(qv) = vectors.into_iter().next() else { return Ok(vec![]) };
        let mut scored = vec![];
        for e in self.entries.values_mut() {
            self.clock += 1;
            e.last_access = self.clock;
            let score = cosine_similarity(&qv, &e.vector);
            if score < self.threshold {
                continue;
            }
            scored.push(RetrievedChunk { chunk: e.chunk.clone(), score });
        }
        scored.sort_by(|a, b| {
            b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| locale_cmp(&a.chunk.path, &b.chunk.path)).then(a.chunk.start_line.cmp(&b.chunk.start_line))
        });
        scored.truncate(k);
        Ok(scored)
    }
}

pub fn rag_snapshot_path(data_dir: &Path, project_id: &str) -> PathBuf {
    data_dir.join("memory").join(project_id).join("rag.json")
}

pub fn load_rag_snapshot(file: &Path) -> Option<RagSnapshot> {
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    if raw.get("version").and_then(Value::as_u64) != Some(RAG_SNAPSHOT_VERSION) {
        return None;
    }
    let files: IndexMap<String, RagFileRecord> = raw.get("files").cloned().map(|f| serde_json::from_value(f).unwrap_or_default()).unwrap_or_default();
    Some(RagSnapshot { cwd: raw.get("cwd").map(crate::jsutil::js_string).unwrap_or_default(), indexed_at: raw.get("indexedAt").map(crate::jsutil::js_string).unwrap_or_default(), files })
}

pub fn save_rag_snapshot(file: &Path, snapshot: &RagSnapshot) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = mkdir_private(dir);
    }
    atomic_json(file, &snapshot.to_value(), true)
}

/// First 16 hex chars of sha256 of the absolute cwd.
pub fn project_id_for(cwd: &Path) -> String {
    use sha2::{Digest, Sha256};
    let abs = crate::files::resolve_lexical(&std::env::current_dir().unwrap_or_default(), &cwd.to_string_lossy());
    hex::encode(Sha256::digest(abs.to_string_lossy().as_bytes()))[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_and_chunking() {
        assert_eq!(fnv1a(""), 0x811c9dc5);
        assert_eq!(fnv1a("a"), 0xe40c292c);
        assert_eq!(tokenize("Hello, World_1 a"), vec!["hello", "world_1"]);
        let text = "import x\n\nexport function a() {\n  return 1;\n}\n\nexport class B {}\n";
        let chunks = chunk_file("src/a.ts", text, None);
        assert_eq!(chunks.len(), 2);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (3, 5));
        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        let windows = chunk_file("notes.txt", &lines.join("\n"), None);
        assert_eq!((windows[1].start_line, windows[1].end_line), (33, 72));
        let rules = parse_goonignore("build/\n*.log\n!keep.log\n/docs/**");
        assert!(is_ignored("build/x.txt", &rules));
        assert!(is_ignored("a/b.log", &rules));
        assert!(!is_ignored("keep.log", &rules));
        assert!(is_ignored("docs/a/b.md", &rules));
        assert!(!is_ignored("src/docs/a.md", &rules));
    }

    #[tokio::test]
    async fn index_retrieve() {
        let mut idx = LexicalIndex::new(None, Some(2), None);
        let doc = |p: &str, c: &str| AddDocument { id: None, chunk: Chunk { path: p.into(), start_line: 1, end_line: 1, content: c.into() } };
        idx.add(vec![doc("a", "parse the user config"), doc("b", "render widgets"), doc("c", "user config loader")]).await.unwrap();
        assert_eq!(idx.len(), 2);
        let hits = idx.retrieve("user config", 5).await.unwrap();
        assert_eq!(hits[0].chunk.path, "c");
    }
}
