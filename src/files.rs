//! Workspace file access: containment, read-before-write revisions, edits, `apply_patch`, turn
//! backups and restore, diffs, image reads and search fallbacks.

use crate::jsutil;
use crate::syntax::validate_syntax;
use anyhow::{anyhow, bail, Result};
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileBackup {
    pub path: String,
    pub existed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_hash: Option<String>,
    /// Set when the agent deleted the file; restore keeps it absent or restores the original.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_deleted: Option<bool>,
    /// Content dropped to keep the session file bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned: Option<bool>,
}

pub fn windows_reserved_device_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\..*)?$").unwrap())
}

/// Deletes a file; a missing file is not an error.
pub fn unlink_safe(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            {
                let extended = PathBuf::from(format!(r"\\?\{}", path.display()));
                match std::fs::remove_file(&extended) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(_) => {}
                }
            }
            Err(error.into())
        }
    }
}

/// Replaces the first literal occurrence of `search`; the replacement is inserted verbatim.
pub fn replace_first_literal(text: &str, search: &str, replacement: &str) -> String {
    match text.find(search) {
        Some(index) => format!("{}{}{}", &text[..index], replacement, &text[index + search.len()..]),
        None => text.to_string(),
    }
}

fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().to_string();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// True when `candidate` (absolute, resolved) lies inside `root`.
pub fn is_within_root(root: &Path, candidate: &Path) -> bool {
    let root_parts: Vec<String> = root.components().map(|c| path_key(Path::new(c.as_os_str()))).collect();
    let cand_parts: Vec<String> = candidate.components().map(|c| path_key(Path::new(c.as_os_str()))).collect();
    cand_parts.len() >= root_parts.len() && cand_parts[..root_parts.len()] == root_parts[..]
}

/// `path.resolve(base, requested)` without touching the filesystem.
pub fn resolve_lexical(base: &Path, requested: &str) -> PathBuf {
    let req = Path::new(requested);
    let start = if req.is_absolute() || has_windows_root(requested) { PathBuf::new() } else { base.to_path_buf() };
    let mut out: Vec<Component> = vec![];
    for component in start.components().chain(req.components()) {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                if matches!(component, Component::Prefix(_)) {
                    out.clear();
                } else {
                    out.retain(|c| matches!(c, Component::Prefix(_)));
                }
                out.push(component);
            }
            other => out.push(other),
        }
    }
    let mut result = PathBuf::new();
    for component in out {
        result.push(component.as_os_str());
    }
    if result.as_os_str().is_empty() {
        result.push("/");
    }
    result
}

fn has_windows_root(text: &str) -> bool {
    cfg!(windows) && (text.starts_with('\\') || text.as_bytes().get(1) == Some(&b':'))
}

/// `path.relative(root, file)` with forward slashes kept as the platform separator.
pub fn relative_path(root: &Path, file: &Path) -> String {
    let root_parts: Vec<_> = root.components().collect();
    let file_parts: Vec<_> = file.components().collect();
    let mut common = 0;
    while common < root_parts.len() && common < file_parts.len() && path_key(Path::new(root_parts[common].as_os_str())) == path_key(Path::new(file_parts[common].as_os_str())) {
        common += 1;
    }
    let mut parts: Vec<String> = vec![];
    for _ in common..root_parts.len() {
        parts.push("..".into());
    }
    for part in &file_parts[common..] {
        parts.push(part.as_os_str().to_string_lossy().to_string());
    }
    parts.join(std::path::MAIN_SEPARATOR_STR)
}

pub fn to_posix(path: &str) -> String {
    path.replace('\\', "/")
}

/// Head/tail character truncation with a marker (`limitText`).
pub fn limit_text(text: &str, limit: usize) -> String {
    let length = jsutil::char_len(text);
    if length <= limit {
        return text.to_string();
    }
    let head_len = (limit as f64 * 0.65).floor() as usize;
    let tail_len = limit.saturating_sub(head_len);
    let omitted = length - head_len - tail_len;
    format!("{}\n...[truncated {} chars; narrow the request]...\n{}", jsutil::take_chars(text, head_len), omitted, jsutil::tail_chars(text, tail_len))
}

pub fn limit_text_default(text: &str) -> String {
    limit_text(text, 32_000)
}

fn slice_by_bytes(text: &str, max_bytes: usize, from_end: bool) -> &str {
    if max_bytes == 0 {
        return "";
    }
    if text.len() <= max_bytes {
        return text;
    }
    if from_end {
        let mut start = text.len() - max_bytes;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        &text[start..]
    } else {
        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }
}

/// Byte-bounded head/tail truncation; the result never exceeds `max_bytes`.
pub fn limit_text_bytes(text: &str, max_bytes: usize) -> String {
    let total = text.len();
    if total <= max_bytes {
        return text.to_string();
    }
    let budget = max_bytes.saturating_sub(128);
    let mut head = slice_by_bytes(text, (budget as f64 * 0.65).floor() as usize, false);
    let mut tail = slice_by_bytes(text, budget - (budget as f64 * 0.65).floor() as usize, true);
    let omitted = total - head.len() - tail.len();
    let marker = format!("\n...[truncated {omitted} bytes; narrow the request]...\n");
    while head.len() + marker.len() + tail.len() > max_bytes && (!tail.is_empty() || !head.is_empty()) {
        if !tail.is_empty() {
            let first = tail.chars().next().map(char::len_utf8).unwrap_or(1);
            tail = &tail[first..];
        } else {
            let last = head.chars().last().map(char::len_utf8).unwrap_or(1);
            head = &head[..head.len() - last];
        }
    }
    let result = format!("{head}{marker}{tail}");
    if result.len() <= max_bytes {
        result
    } else {
        slice_by_bytes(marker.trim(), max_bytes, false).to_string()
    }
}

fn split_lines_for_diff(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    let normalized = text.replace("\r\n", "\n");
    let body = normalized.strip_suffix('\n').unwrap_or(&normalized);
    body.split('\n').map(str::to_string).collect()
}

pub fn compute_unified_diff(path: &str, before: &str, after: &str, context_lines: usize) -> String {
    let mut old_lines = split_lines_for_diff(before);
    let mut new_lines = split_lines_for_diff(after);
    if before != after && old_lines == new_lines {
        old_lines = if before.is_empty() { vec![] } else { before.replace("\r\n", "\n").split('\n').map(str::to_string).collect() };
        new_lines = if after.is_empty() { vec![] } else { after.replace("\r\n", "\n").split('\n').map(str::to_string).collect() };
    }
    let mut prefix = 0;
    while prefix < old_lines.len() && prefix < new_lines.len() && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    if prefix == old_lines.len() && prefix == new_lines.len() {
        return format!("No changes in {path}.");
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix && suffix < new_lines.len() - prefix && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix] {
        suffix += 1;
    }
    let old_changed = &old_lines[prefix..old_lines.len() - suffix];
    let new_changed = &new_lines[prefix..new_lines.len() - suffix];
    let ctx_start = prefix.saturating_sub(context_lines);
    let ctx_end_old = old_lines.len().min(old_lines.len() - suffix + context_lines);
    let before_ctx = &old_lines[ctx_start..prefix];
    let after_ctx = &old_lines[old_lines.len() - suffix..ctx_end_old];
    let old_start = if old_lines.is_empty() { 0 } else { ctx_start + 1 };
    let old_count = before_ctx.len() + old_changed.len() + after_ctx.len();
    let new_start = if new_lines.is_empty() { 0 } else { ctx_start + 1 };
    let new_count = before_ctx.len() + new_changed.len() + after_ctx.len();
    let max_diff_lines = 5000;
    let kind_prefix = if before.is_empty() && !after.is_empty() {
        "new "
    } else if after.is_empty() && !before.is_empty() {
        "deleted "
    } else {
        ""
    };
    let mut out: Vec<String> =
        vec![format!("--- a/{path} ({kind_prefix}+{} -{})", new_changed.len(), old_changed.len()), format!("+++ b/{path}"), format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@")];
    out.extend(before_ctx.iter().map(|l| format!(" {l}")));
    out.extend(old_changed.iter().take(max_diff_lines).map(|l| format!("-{l}")));
    if old_changed.len() > max_diff_lines {
        out.push(format!("... (-{} more removed lines)", old_changed.len() - max_diff_lines));
    }
    out.extend(new_changed.iter().take(max_diff_lines).map(|l| format!("+{l}")));
    if new_changed.len() > max_diff_lines {
        out.push(format!("... (+{} more added lines)", new_changed.len() - max_diff_lines));
    }
    out.extend(after_ctx.iter().map(|l| format!(" {l}")));
    out.join("\n")
}

struct TrimmedMatch {
    count: usize,
    result: Option<String>,
}

fn match_trimmed_lines(current: &str, old_text: &str, new_text: &str) -> TrimmedMatch {
    let eol = if current.contains("\r\n") { "\r\n" } else { "\n" };
    let normalized = current.replace("\r\n", "\n");
    let file_lines: Vec<&str> = normalized.split('\n').collect();
    let needle_norm = old_text.replace("\r\n", "\n");
    let needle_lines: Vec<&str> = needle_norm.split('\n').collect();
    if needle_lines.is_empty() || needle_lines.len() > file_lines.len() {
        return TrimmedMatch { count: 0, result: None };
    }
    let mut matches = vec![];
    for i in 0..=file_lines.len() - needle_lines.len() {
        if (0..needle_lines.len()).all(|j| file_lines[i + j].trim() == needle_lines[j].trim()) {
            matches.push(i);
        }
    }
    if matches.len() != 1 {
        return TrimmedMatch { count: matches.len(), result: None };
    }
    let idx = matches[0];
    let replacement_norm = new_text.replace("\r\n", "\n");
    let mut updated: Vec<&str> = file_lines[..idx].to_vec();
    updated.extend(replacement_norm.split('\n'));
    updated.extend(&file_lines[idx + needle_lines.len()..]);
    TrimmedMatch { count: 1, result: Some(updated.join(eol)) }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PatchHunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatchOperation {
    Add { path: String, content: String },
    Delete { path: String },
    Update { path: String, hunks: Vec<PatchHunk>, move_to: Option<String> },
}

impl PatchOperation {
    pub fn path(&self) -> &str {
        match self {
            PatchOperation::Add { path, .. } | PatchOperation::Delete { path } | PatchOperation::Update { path, .. } => path,
        }
    }
}

pub fn parse_patch_text(raw_patch: &str) -> Result<Vec<PatchOperation>> {
    let normalized = raw_patch.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut ops = vec![];
    let mut i = 0;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i < lines.len() && lines[i].trim() == "*** Begin Patch" {
        i += 1;
    }
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == "*** End Patch" {
            break;
        }
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Add File: ") {
            let path = rest.trim().to_string();
            i += 1;
            let mut added: Vec<&str> = vec![];
            while i < lines.len() && !lines[i].starts_with("*** ") {
                added.push(lines[i]);
                i += 1;
            }
            // Bare blank lines before the next header are patch formatting, not file content.
            while added.last() == Some(&"") {
                added.pop();
            }
            let content: String = added.iter().map(|l| format!("{}\n", l.strip_prefix('+').unwrap_or(l))).collect();
            ops.push(PatchOperation::Add { path, content });
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOperation::Delete { path: rest.trim().to_string() });
            i += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("*** Update File: ") {
            let path = rest.trim().to_string();
            i += 1;
            let mut move_to = None;
            if i < lines.len() {
                if let Some(target) = lines[i].strip_prefix("*** Move to: ") {
                    let t = target.trim();
                    if !t.is_empty() {
                        move_to = Some(t.to_string());
                    }
                    i += 1;
                }
            }
            let mut hunks = vec![];
            let mut old_lines: Vec<String> = vec![];
            let mut new_lines: Vec<String> = vec![];
            let mut in_hunk = false;
            let flush = |hunks: &mut Vec<PatchHunk>, old: &mut Vec<String>, new: &mut Vec<String>, in_hunk: bool| {
                if in_hunk && (!old.is_empty() || !new.is_empty()) {
                    hunks.push(PatchHunk { old_lines: std::mem::take(old), new_lines: std::mem::take(new) });
                }
                old.clear();
                new.clear();
            };
            while i < lines.len() && !lines[i].starts_with("*** ") {
                let hl = lines[i];
                if hl.starts_with("@@") {
                    flush(&mut hunks, &mut old_lines, &mut new_lines, in_hunk);
                    in_hunk = true;
                    i += 1;
                    continue;
                }
                if hl == "\\ No newline at end of file" {
                    i += 1;
                    continue;
                }
                in_hunk = true;
                if let Some(t) = hl.strip_prefix('-') {
                    old_lines.push(t.to_string());
                } else if let Some(t) = hl.strip_prefix('+') {
                    new_lines.push(t.to_string());
                } else if let Some(t) = hl.strip_prefix(' ') {
                    old_lines.push(t.to_string());
                    new_lines.push(t.to_string());
                } else if hl.is_empty() {
                    old_lines.push(String::new());
                    new_lines.push(String::new());
                } else {
                    old_lines.push(hl.to_string());
                    new_lines.push(hl.to_string());
                }
                i += 1;
            }
            flush(&mut hunks, &mut old_lines, &mut new_lines, in_hunk);
            let move_to = move_to.filter(|m| m != &path);
            ops.push(PatchOperation::Update { path, hunks, move_to });
            continue;
        }
        bail!("Invalid patch header line: {line}");
    }
    if ops.is_empty() {
        bail!("Patch contained no file operations. Use \"*** Begin Patch\" with \"*** Update File: <path>\", \"*** Add File: <path>\", or \"*** Delete File: <path>\".");
    }
    Ok(ops)
}

/// Finds the single occurrence of `needle` at or after `start_idx`: exact, right-trimmed, then
/// fully trimmed. More than one match in a pass is ambiguous.
pub fn find_hunk_index(haystack: &[String], needle: &[String], start_idx: usize, path: &str) -> Result<Option<usize>> {
    if needle.is_empty() {
        return Ok(Some(start_idx));
    }
    type Eq = fn(&str, &str) -> bool;
    let passes: [Eq; 3] = [|a, b| a == b, |a, b| a.trim_end() == b.trim_end(), |a, b| a.trim() == b.trim()];
    for eq in passes {
        let mut hits = vec![];
        if haystack.len() >= needle.len() {
            for i in start_idx..=haystack.len() - needle.len() {
                if (0..needle.len()).all(|j| eq(&haystack[i + j], &needle[j])) {
                    hits.push(i);
                }
            }
        }
        if hits.len() > 1 {
            bail!("Patch hunk is ambiguous in {path} (matched {} times); add more surrounding context.", hits.len());
        }
        if hits.len() == 1 {
            return Ok(Some(hits[0]));
        }
    }
    Ok(None)
}

pub fn apply_hunks_to_content(current: &str, hunks: &[PatchHunk], path: &str) -> Result<String> {
    let eol = if current.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = current.replace("\r\n", "\n").split('\n').map(str::to_string).collect();
    let mut cursor = 0;
    for hunk in hunks {
        let Some(idx) = find_hunk_index(&lines, &hunk.old_lines, cursor, path)? else {
            bail!("Failed to match patch hunk in {path} near: \"{}\"", hunk.old_lines.first().map(String::as_str).unwrap_or(""));
        };
        lines.splice(idx..idx + hunk.old_lines.len(), hunk.new_lines.iter().cloned());
        cursor = idx + hunk.new_lines.len();
    }
    Ok(lines.join(eol))
}

fn digest(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().map(|e| e.kind() == std::io::ErrorKind::NotFound).unwrap_or(false)
}

#[derive(Debug, Default, Clone)]
pub struct RestoreResult {
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicts: Vec<String>,
    pub pruned: Vec<String>,
}

pub struct ImageData {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub resolved_path: PathBuf,
}

#[derive(Default)]
struct FilesState {
    revisions: HashMap<PathBuf, String>,
    active_backups: IndexMap<String, FileBackup>,
}

pub struct WorkspaceFiles {
    /// Canonical (symlink-free) workspace root.
    pub root: PathBuf,
    state: Mutex<FilesState>,
}

#[derive(Clone)]
struct Slot {
    file: PathBuf,
    rel_path: String,
    existed: bool,
    before: Option<String>,
    mode: u32,
    /// None = untouched, Some(None) = deleted by this patch, Some(Some) = new content.
    after: Option<Option<String>>,
}

impl WorkspaceFiles {
    pub fn new(root: &Path) -> Self {
        let absolute = resolve_lexical(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")), &root.to_string_lossy());
        let canonical = std::fs::canonicalize(&absolute).map(crate::config::strip_verbatim).unwrap_or(absolute);
        WorkspaceFiles { root: canonical, state: Mutex::new(FilesState::default()) }
    }

    pub fn begin_turn(&self) {
        self.state.lock().unwrap().active_backups.clear();
    }

    pub fn peek_turn_backups(&self) -> Vec<FileBackup> {
        self.state.lock().unwrap().active_backups.values().cloned().collect()
    }

    pub fn turn_changed_files(&self) -> Vec<String> {
        self.state.lock().unwrap().active_backups.keys().cloned().collect()
    }

    pub fn has_turn_changes(&self) -> bool {
        !self.state.lock().unwrap().active_backups.is_empty()
    }

    pub fn clear_turn_backups(&self) {
        self.state.lock().unwrap().active_backups.clear();
    }

    pub fn replace_turn_backups(&self, backups: Vec<FileBackup>) {
        let mut state = self.state.lock().unwrap();
        state.active_backups.clear();
        for mut b in backups {
            let key = to_posix(&b.path);
            b.path = key.clone();
            state.active_backups.insert(key, b);
        }
    }

    pub fn take_turn_backups(&self) -> Vec<FileBackup> {
        let mut state = self.state.lock().unwrap();
        let list = state.active_backups.values().cloned().collect();
        state.active_backups.clear();
        list
    }

    pub fn merge_backups(&self, backups: Vec<FileBackup>) {
        let mut state = self.state.lock().unwrap();
        for mut b in backups {
            let key = to_posix(&b.path);
            match state.active_backups.get_mut(&key) {
                None => {
                    b.path = key.clone();
                    state.active_backups.insert(key, b);
                }
                Some(existing) => {
                    // `existing` is the earliest pre-turn state; only the follow-on state merges.
                    existing.after_hash = b.after_hash.clone();
                    if b.after_deleted == Some(true) {
                        existing.after_deleted = Some(true);
                        existing.after_hash = None;
                    } else {
                        existing.after_deleted = None;
                    }
                    if existing.content.is_none() && b.content.is_some() {
                        existing.content = b.content;
                    }
                    if existing.mode.is_none() && b.mode.is_some() {
                        existing.mode = b.mode;
                    }
                }
            }
        }
    }

    fn record_backup(&self, rel_path: &str, existed: bool, content: Option<String>, mode: Option<u32>, after_content: Option<&str>, after_deleted: bool) {
        let key = to_posix(rel_path);
        let mut state = self.state.lock().unwrap();
        let entry = state.active_backups.entry(key.clone()).or_insert_with(|| FileBackup { path: key, existed, content, mode, ..Default::default() });
        if after_deleted {
            entry.after_deleted = Some(true);
            entry.after_hash = None;
        } else {
            entry.after_hash = after_content.map(digest);
            entry.after_deleted = None;
        }
    }

    /// Resolves a requested path inside the workspace, following symlinks of existing ancestors.
    pub fn path(&self, requested: &str) -> Result<PathBuf> {
        let lexical = resolve_lexical(&self.root, requested);
        self.assert_within(&lexical)?;
        let mut parent = lexical.clone();
        let mut suffix: Vec<std::ffi::OsString> = vec![];
        loop {
            match std::fs::canonicalize(&parent) {
                Ok(resolved) => {
                    let resolved = crate::config::strip_verbatim(resolved);
                    self.assert_within(&resolved)?;
                    let mut out = resolved;
                    for part in suffix.iter().rev() {
                        out.push(part);
                    }
                    return Ok(out);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let Some(up) = parent.parent().map(Path::to_path_buf) else {
                        return Err(error.into());
                    };
                    if up == parent {
                        return Err(error.into());
                    }
                    suffix.push(parent.file_name().map(|n| n.to_os_string()).unwrap_or_default());
                    parent = up;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn assert_within(&self, file: &Path) -> Result<()> {
        if !is_within_root(&self.root, file) {
            bail!("File path escapes the workspace.");
        }
        Ok(())
    }

    fn assert_not_windows_reserved(&self, file: &Path) -> Result<()> {
        let rel = relative_path(&self.root, file);
        for seg in rel.split(['/', '\\']) {
            if windows_reserved_device_re().is_match(seg) {
                bail!("Refusing to create Windows reserved device name \"{seg}\" (use /dev/null instead of nul).");
            }
        }
        Ok(())
    }

    pub fn rel(&self, file: &Path) -> String {
        relative_path(&self.root, file)
    }

    /// Reads a regular UTF-8 text file (≤ 2 MiB, no NUL bytes).
    pub fn text(&self, file: &Path) -> Result<String> {
        let info = std::fs::metadata(file)?;
        if !info.is_file() || info.len() > MAX_FILE_BYTES {
            bail!("Expected a regular text file no larger than 2 MiB.");
        }
        let bytes = std::fs::read(file)?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        if text.contains('\0') {
            bail!("Binary files are not supported.");
        }
        Ok(text)
    }

    pub fn read(&self, path: &str, offset: usize, lines: usize) -> Result<String> {
        let file = self.path(path)?;
        let text = self.text(&file)?;
        self.state.lock().unwrap().revisions.insert(file, digest(&text));
        let mut all: Vec<&str> = text.split('\n').collect();
        // A trailing newline terminates the last line; it does not start another one.
        if all.len() > 1 && all.last() == Some(&"") {
            all.pop();
        }
        let offset = offset.max(1);
        let body: Vec<String> = all.iter().skip(offset - 1).take(lines).enumerate().map(|(i, line)| format!("{}: {}", offset + i, line)).collect();
        Ok(limit_text_default(&format!("{path} ({} lines)\n{}", all.len(), body.join("\n"))))
    }

    fn write_raw(&self, file: &Path, content: &str, mode: u32, existing: bool) -> Result<()> {
        use std::io::Write;
        self.assert_not_windows_reserved(file)?;
        let dir = file.parent().ok_or_else(|| anyhow!("Invalid path"))?;
        std::fs::create_dir_all(dir)?;
        // Re-check containment after creating parents: a directory swapped for a symlink must never
        // redirect the write outside the workspace.
        self.assert_within(&crate::config::strip_verbatim(std::fs::canonicalize(dir)?))?;
        let temporary = dir.join(format!(".goon-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let result = (|| -> std::io::Result<()> {
            let mut handle = options.open(&temporary)?;
            handle.write_all(content.as_bytes())?;
            #[cfg(unix)]
            if existing {
                use std::os::unix::fs::PermissionsExt;
                handle.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            let _ = existing;
            handle.sync_all()?;
            drop(handle);
            std::fs::rename(&temporary, file)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.into());
        }
        self.state.lock().unwrap().revisions.insert(file.to_path_buf(), digest(content));
        Ok(())
    }

    fn file_mode(file: &Path) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(file).map(|m| m.permissions().mode() & 0o777).unwrap_or(0o644)
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            0o644
        }
    }

    pub fn write(&self, path: &str, content: &str) -> Result<String> {
        let file = self.path(path)?;
        self.assert_not_windows_reserved(&file)?;
        if content.len() as u64 > MAX_FILE_BYTES {
            bail!("Write exceeds 2 MiB.");
        }
        let mut mode = 0o644;
        let mut existing = false;
        let mut current = String::new();
        match self.text(&file) {
            Ok(text) => {
                let rev = self.state.lock().unwrap().revisions.get(&file).cloned();
                if rev.as_deref() != Some(digest(&text).as_str()) {
                    bail!("Read the current file before overwriting; it is unread or has changed.");
                }
                current = text;
                mode = Self::file_mode(&file);
                existing = true;
            }
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error),
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Re-resolve after creating parents to reject a pre-existing symlink escape.
        if self.path(path)? != file {
            bail!("Path changed while preparing the write.");
        }
        let rel = self.rel(&file);
        self.record_backup(&rel, existing, existing.then_some(current), existing.then_some(mode), Some(content), false);
        self.write_raw(&file, content, mode, existing)?;
        Ok(format!("Wrote {path} ({} bytes).", content.len()))
    }

    pub fn edit(&self, path: &str, old_text: &str, new_text: &str, replace_all: bool) -> Result<String> {
        let file = self.path(path)?;
        let current = self.text(&file)?;
        let rev = self.state.lock().unwrap().revisions.get(&file).cloned();
        if rev.as_deref() != Some(digest(&current).as_str()) {
            bail!("Read the current file before editing; it is unread or has changed.");
        }
        let mut target_old = old_text.to_string();
        let mut target_new = new_text.to_string();
        let mut count = current.matches(old_text).count();
        // CRLF/LF normalization when the file uses \r\n and the model emitted \n.
        if count == 0 && current.contains("\r\n") && !old_text.contains("\r\n") {
            let crlf_old = old_text.replace('\n', "\r\n");
            let crlf_count = current.matches(crlf_old.as_str()).count();
            if crlf_count > 0 {
                target_old = crlf_old;
                target_new = new_text.replace("\r\n", "\n").replace('\n', "\r\n");
                count = crlf_count;
            }
        }
        // Line-trimmed fallback for unique multi-line blocks when whitespace drifts.
        if count == 0 && !replace_all && old_text.contains('\n') {
            let trimmed = match_trimmed_lines(&current, old_text, new_text);
            if let Some(result) = trimmed.result {
                return self.write(path, &result);
            }
            if trimmed.count > 1 {
                count = trimmed.count;
            }
        }
        if count == 0 || (!replace_all && count != 1) {
            bail!("Expected one exact match; found {count}. Use replaceAll explicitly for multiple matches.");
        }
        let next = if replace_all { current.replace(target_old.as_str(), &target_new) } else { replace_first_literal(&current, &target_old, &target_new) };
        self.write(path, &next)
    }

    fn revision_matches(&self, file: &Path, text: &str) -> bool {
        self.state.lock().unwrap().revisions.get(file).map(String::as_str) == Some(digest(text).as_str())
    }

    /// Plans every operation against a virtual view of the files it touches, validates the whole
    /// patch, and only then writes. A patch that fails validation changes nothing.
    pub fn apply_patch(&self, patch_text: &str) -> Result<String> {
        let ops = parse_patch_text(patch_text)?;
        let mut slots: IndexMap<PathBuf, Slot> = IndexMap::new();
        let slot_for = |this: &Self, requested: &str, for_write: bool, check_revision: bool, slots: &mut IndexMap<PathBuf, Slot>| -> Result<PathBuf> {
            let file = this.path(requested)?;
            if for_write {
                this.assert_not_windows_reserved(&file)?;
            }
            if !slots.contains_key(&file) {
                let mut slot = Slot { file: file.clone(), rel_path: this.rel(&file), existed: false, before: None, mode: 0o644, after: None };
                match this.text(&file) {
                    Ok(text) => {
                        slot.mode = Self::file_mode(&file);
                        slot.existed = true;
                        if check_revision && !this.revision_matches(&file, &text) {
                            bail!("Read the current file before patching; it is unread or has changed.");
                        }
                        slot.before = Some(text);
                    }
                    Err(error) if is_not_found(&error) => {}
                    Err(error) => return Err(error),
                }
                slots.insert(file.clone(), slot);
            }
            Ok(file)
        };
        let current_of = |slot: &Slot| -> Option<String> {
            match &slot.after {
                None => slot.before.clone(),
                Some(after) => after.clone(),
            }
        };
        let mut summary: Vec<String> = vec![];
        for op in &ops {
            match op {
                PatchOperation::Add { path, content } => {
                    let key = slot_for(self, path, true, true, &mut slots)?;
                    let slot = slots.get_mut(&key).unwrap();
                    summary.push(format!("{} {path}", if current_of(slot).is_none() { "Added" } else { "Updated" }));
                    slot.after = Some(Some(content.clone()));
                }
                PatchOperation::Delete { path } => {
                    let key = slot_for(self, path, false, true, &mut slots)?;
                    let slot = slots.get_mut(&key).unwrap();
                    if current_of(slot).is_none() {
                        bail!("Cannot delete {path}: file does not exist.");
                    }
                    slot.after = Some(None);
                    summary.push(format!("Deleted {path}"));
                }
                PatchOperation::Update { path, hunks, move_to } => {
                    let key = slot_for(self, path, true, true, &mut slots)?;
                    let Some(current) = current_of(&slots[&key]) else {
                        bail!("Cannot update {path}: file does not exist.");
                    };
                    let next = apply_hunks_to_content(&current, hunks, path)?;
                    if let Some(target) = move_to {
                        let dest_key = slot_for(self, target, true, false, &mut slots)?;
                        if dest_key != key {
                            if current_of(&slots[&dest_key]).is_some() {
                                bail!("Cannot move {path} to {target}: destination exists.");
                            }
                            let mode = slots[&key].mode;
                            let dest = slots.get_mut(&dest_key).unwrap();
                            dest.mode = mode;
                            dest.after = Some(Some(next));
                            slots.get_mut(&key).unwrap().after = Some(None);
                            summary.push(format!("Moved {path} -> {target}"));
                            continue;
                        }
                    }
                    slots.get_mut(&key).unwrap().after = Some(Some(next));
                    summary.push(format!("Updated {path}"));
                }
            }
        }
        // Validate syntax of every resulting file before touching disk.
        for slot in slots.values() {
            if let Some(Some(after)) = &slot.after {
                if let Err(error) = validate_syntax(&slot.rel_path, after) {
                    bail!("Syntax validation failed for {}: {}", slot.rel_path, error);
                }
            }
        }
        for slot in slots.values() {
            let Some(after) = &slot.after else { continue };
            if after.is_none() && !slot.existed {
                continue; // created and removed within this patch
            }
            self.record_backup(&slot.rel_path, slot.existed, slot.before.clone(), slot.existed.then_some(slot.mode), after.as_deref(), after.is_none());
            match after {
                None => {
                    unlink_safe(&slot.file)?;
                    self.state.lock().unwrap().revisions.remove(&slot.file);
                }
                Some(content) => self.write_raw(&slot.file, content, slot.mode, slot.existed)?,
            }
        }
        Ok(summary.join("\n"))
    }

    /// Restores turn backups. A file changed after the agent's turn (hash mismatch) is a conflict
    /// and left untouched unless `force` is set.
    pub fn restore_backups(&self, backups: &[FileBackup], force: bool) -> Result<RestoreResult> {
        let mut result = RestoreResult::default();
        for b in backups {
            let file = self.path(&b.path)?;
            if b.after_deleted == Some(true) {
                let exists = match self.text(&file) {
                    Ok(_) => true,
                    Err(error) => !is_not_found(&error),
                };
                if !force && exists {
                    result.conflicts.push(b.path.clone());
                    continue;
                }
                self.restore_one(b, &file, &mut result)?;
                continue;
            }
            if !force {
                let (current, readable) = match self.text(&file) {
                    Ok(text) => (Some(text), true),
                    Err(error) => (None, is_not_found(&error)),
                };
                let conflict = b.after_hash.is_some() && (!readable || current.as_ref().map(|c| Some(digest(c)) != b.after_hash).unwrap_or(false));
                if conflict {
                    result.conflicts.push(b.path.clone());
                    continue;
                }
            }
            self.restore_one(b, &file, &mut result)?;
        }
        Ok(result)
    }

    fn restore_one(&self, b: &FileBackup, file: &Path, result: &mut RestoreResult) -> Result<()> {
        if !b.existed {
            let _ = unlink_safe(file);
            self.state.lock().unwrap().revisions.remove(file);
            result.deleted.push(b.path.clone());
        } else if let Some(content) = &b.content {
            self.write_raw(file, content, b.mode.unwrap_or(0o644), true)?;
            result.restored.push(b.path.clone());
        } else if b.pruned == Some(true) {
            result.pruned.push(b.path.clone());
        }
        Ok(())
    }

    pub fn list_workspace_files(&self, max_files: usize) -> Vec<String> {
        let mut files = vec![];
        let mut queue = std::collections::VecDeque::from([self.root.clone()]);
        while let Some(dir) = queue.pop_front() {
            if files.len() >= max_files {
                break;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                if files.len() >= max_files {
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Ok(kind) = entry.file_type() else { continue };
                if [".git", "node_modules", "dist", ".refs", "target"].contains(&name.as_str()) || kind.is_symlink() {
                    continue;
                }
                let full = dir.join(&name);
                if kind.is_dir() {
                    queue.push_back(full);
                } else if kind.is_file() {
                    files.push(to_posix(&self.rel(&full)));
                }
            }
        }
        files.sort();
        files
    }

    pub fn locate_image(&self, user_path: &str) -> Result<(PathBuf, bool)> {
        let (file, _) = self.resolve_image_path(user_path, true)?;
        let inside = is_within_root(&self.root, &file);
        Ok((file, inside))
    }

    fn resolve_image_path(&self, user_path: &str, allow_outside: bool) -> Result<(PathBuf, String)> {
        let mut cleaned = user_path.trim().to_string();
        if cleaned.len() >= 2 && ((cleaned.starts_with('"') && cleaned.ends_with('"')) || (cleaned.starts_with('\'') && cleaned.ends_with('\''))) {
            cleaned = cleaned[1..cleaned.len() - 1].trim().to_string();
        }
        if cleaned.starts_with("file://") {
            if let Ok(url) = url::Url::parse(&cleaned) {
                if let Ok(p) = url.to_file_path() {
                    cleaned = p.to_string_lossy().to_string();
                }
            }
        }
        static EXT: OnceLock<Regex> = OnceLock::new();
        let ext_re = EXT.get_or_init(|| Regex::new(r"(?i)\.(png|jpe?g|webp|gif)$").unwrap());
        let Some(ext) = ext_re.captures(&cleaned).map(|c| c[1].to_lowercase()) else {
            bail!("Unsupported image extension.");
        };
        let media_type = match ext.as_str() {
            "png" => "image/png",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/jpeg",
        }
        .to_string();
        let is_abs = Path::new(&cleaned).is_absolute() || Regex::new(r"^[A-Za-z]:[\\/]").unwrap().is_match(&cleaned) || cleaned.starts_with("\\\\");
        let file = if is_abs {
            let resolved = crate::config::strip_verbatim(std::fs::canonicalize(&cleaned)?);
            if !allow_outside {
                self.assert_within(&resolved)?;
            }
            resolved
        } else if !allow_outside {
            self.path(&cleaned)?
        } else {
            match self.path(&cleaned) {
                Ok(p) => p,
                Err(_) => crate::config::strip_verbatim(std::fs::canonicalize(resolve_lexical(&self.root, &cleaned))?),
            }
        };
        // The extension check applies to the requested name; a symlink must also point at an image.
        if !ext_re.is_match(&file.to_string_lossy()) {
            bail!("Unsupported image extension.");
        }
        Ok((file, media_type))
    }

    pub fn read_image(&self, user_path: &str, max_bytes: u64, allow_outside: bool) -> Result<ImageData> {
        let (file, media_type) = self.resolve_image_path(user_path, allow_outside)?;
        let info = std::fs::metadata(&file)?;
        if !info.is_file() || info.len() > max_bytes {
            bail!("Expected a regular image file no larger than 5 MiB.");
        }
        let bytes = std::fs::read(&file)?;
        Ok(ImageData { bytes, media_type, resolved_path: file })
    }

    /// Diff shown in the approval prompt (and tool events) before an edit tool runs.
    pub fn preview_diff(&self, tool: &str, input: &serde_json::Value) -> Option<String> {
        self.preview_diff_inner(tool, input).ok().flatten()
    }

    fn preview_diff_inner(&self, tool: &str, input: &serde_json::Value) -> Result<Option<String>> {
        let Some(raw) = input.as_object() else { return Ok(None) };
        let normalized_tool = match tool {
            "write" => "write_file",
            "edit" => "edit_file",
            other => other,
        };
        let get = |keys: &[&str]| keys.iter().find_map(|k| raw.get(*k).and_then(|v| v.as_str()).map(str::to_string));
        let path_val = ["path", "filePath", "file_path", "directory"].iter().find_map(|k| raw.get(*k)).cloned();
        let old_val = get(&["oldText", "old_string", "old_str"]);
        let new_val = get(&["newText", "new_string", "new_str"]);
        if normalized_tool == "apply_patch" {
            if let Some(patch) = raw.get("patch").and_then(|v| v.as_str()) {
                let ops = parse_patch_text(patch)?;
                let mut diffs = vec![];
                let mut virtual_files: HashMap<PathBuf, String> = HashMap::new();
                for op in &ops {
                    let file = self.path(op.path())?;
                    let before = match virtual_files.get(&file) {
                        Some(text) => text.clone(),
                        None => self.text(&file).unwrap_or_default(),
                    };
                    match op {
                        PatchOperation::Add { path, content } => {
                            diffs.push(compute_unified_diff(path, &before, content, 3));
                            virtual_files.insert(file, content.clone());
                        }
                        PatchOperation::Delete { path } => {
                            diffs.push(compute_unified_diff(path, &before, "", 3));
                            virtual_files.insert(file, String::new());
                        }
                        PatchOperation::Update { path, hunks, move_to } => {
                            let after = apply_hunks_to_content(&before, hunks, path)?;
                            if let Some(target) = move_to {
                                let destination = self.path(target)?;
                                diffs.push(compute_unified_diff(&format!("{path} -> {target}"), &before, &after, 3));
                                virtual_files.insert(file, String::new());
                                virtual_files.insert(destination, after);
                            } else {
                                diffs.push(compute_unified_diff(path, &before, &after, 3));
                                virtual_files.insert(file, after);
                            }
                        }
                    }
                }
                return Ok(Some(diffs.join("\n")));
            }
        }
        let Some(serde_json::Value::String(path_val)) = path_val else { return Ok(None) };
        let file = self.path(&path_val)?;
        let before = self.text(&file).unwrap_or_default();
        if normalized_tool == "write_file" {
            if let Some(content) = raw.get("content").and_then(|v| v.as_str()) {
                return Ok(Some(compute_unified_diff(&path_val, &before, content, 3)));
            }
        }
        if normalized_tool == "edit_file" {
            if let (Some(old), Some(new)) = (old_val, new_val) {
                let replace_all = raw.get("replaceAll").and_then(|v| v.as_bool()) == Some(true);
                let mut target_old = old.clone();
                let mut target_new = new.clone();
                if !before.contains(&target_old) && before.contains("\r\n") && !target_old.contains("\r\n") {
                    target_old = target_old.replace('\n', "\r\n");
                    target_new = target_new.replace("\r\n", "\n").replace('\n', "\r\n");
                }
                if !target_old.is_empty() && before.contains(&target_old) {
                    let after = if replace_all { before.replace(target_old.as_str(), &target_new) } else { replace_first_literal(&before, &target_old, &target_new) };
                    return Ok(Some(compute_unified_diff(&path_val, &before, &after, 3)));
                }
                let trimmed = match_trimmed_lines(&before, &old, &new);
                if let Some(result) = trimmed.result {
                    return Ok(Some(compute_unified_diff(&path_val, &before, &result, 3)));
                }
            }
        }
        Ok(None)
    }

    pub fn list(&self, path: &str) -> Result<String> {
        let file = self.path(path)?;
        let mut entries: Vec<(String, std::fs::FileType)> = std::fs::read_dir(&file)?.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().to_string(), e.file_type().ok()?))).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let total = entries.len();
        let lines: Vec<String> = entries
            .iter()
            .take(300)
            .map(|(name, kind)| {
                format!(
                    "{name}{}",
                    if kind.is_dir() {
                        "/"
                    } else if kind.is_symlink() {
                        "@"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        let suffix = if total > 300 { "\n...[listing limited to 300 entries]" } else { "" };
        Ok(limit_text_default(&format!("{}{suffix}", lines.join("\n"))))
    }

    /// Portable literal search used when ripgrep is unavailable.
    pub fn fallback_search(&self, query: &str, path: &str, cancel: &tokio_util::sync::CancellationToken) -> Result<String> {
        let start = self.path(path)?;
        let mut queue = std::collections::VecDeque::from([start]);
        let mut hits: Vec<String> = vec![];
        let mut visited = 0usize;
        while let Some(dir) = queue.pop_front() {
            if visited >= 3000 || hits.len() >= 100 {
                queue.push_front(dir);
                break;
            }
            if cancel.is_cancelled() {
                bail!("Cancelled.");
            }
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                visited += 1;
                if visited > 3000 || hits.len() >= 100 {
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let Ok(kind) = entry.file_type() else { continue };
                if name == ".git" || name == "node_modules" || kind.is_symlink() {
                    continue;
                }
                let file = dir.join(&name);
                if kind.is_dir() {
                    queue.push_back(file);
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let Ok(text) = self.text(&file) else { continue };
                for (i, line) in text.split('\n').enumerate() {
                    if hits.len() < 100 && line.contains(query) {
                        hits.push(format!("{}:{}:{}", self.rel(&file), i + 1, jsutil::take_chars(line, 1000)));
                    }
                }
            }
        }
        let body = if hits.is_empty() { "No matches found.".to_string() } else { hits.join("\n") };
        let limited = !queue.is_empty() || visited >= 3000 || hits.len() >= 100;
        Ok(format!("{}{}", limit_text_default(&body), if limited { "\n[Search reached its limit; narrow the path.]" } else { "" }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, WorkspaceFiles) {
        let dir = tempfile::tempdir().unwrap();
        let files = WorkspaceFiles::new(dir.path());
        (dir, files)
    }

    #[test]
    fn write_requires_read_and_tracks_backups() {
        let (dir, files) = ws();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        assert!(files.write("a.txt", "two\n").unwrap_err().to_string().contains("Read the current file"));
        files.read("a.txt", 1, 10).unwrap();
        files.write("a.txt", "two\n").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "two\n");
        let backups = files.take_turn_backups();
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].content.as_deref(), Some("one\n"));
        let restored = files.restore_backups(&backups, false).unwrap();
        assert_eq!(restored.restored, vec!["a.txt"]);
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\n");
    }

    #[test]
    fn read_counts_lines_without_phantom_trailing_line() {
        let (dir, files) = ws();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert_eq!(files.read("a.txt", 1, 10).unwrap(), "a.txt (2 lines)\n1: one\n2: two");
    }

    #[test]
    fn escape_is_rejected() {
        let (_dir, files) = ws();
        assert!(files.path("../x").unwrap_err().to_string().contains("escapes"));
    }

    #[test]
    fn apply_patch_moves_and_validates() {
        let (dir, files) = ws();
        std::fs::write(dir.path().join("old.txt"), "one\ntwo\nthree\n").unwrap();
        files.read("old.txt", 1, 10).unwrap();
        let summary = files.apply_patch("*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n@@\n one\n-two\n+TWO\n three\n*** End Patch").unwrap();
        assert_eq!(summary, "Moved old.txt -> new.txt");
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("new.txt")).unwrap(), "one\nTWO\nthree\n");
        let err = files.apply_patch("*** Begin Patch\n*** Add File: bad.json\n+{\n*** End Patch").unwrap_err();
        assert!(err.to_string().contains("Syntax validation failed"));
        assert!(!dir.path().join("bad.json").exists());
    }

    #[test]
    fn edit_literal_replacement_is_not_expanded() {
        let (dir, files) = ws();
        std::fs::write(dir.path().join("a.sh"), "echo X\n").unwrap();
        files.read("a.sh", 1, 10).unwrap();
        files.edit("a.sh", "X", "$& $$ $' $`", false).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.sh")).unwrap(), "echo $& $$ $' $`\n");
    }

    #[test]
    fn diff_and_limits() {
        let diff = compute_unified_diff("f", "a\nb\n", "a\nc\n", 3);
        assert!(diff.contains("-b") && diff.contains("+c"));
        let text = "é".repeat(5000);
        for max in [0usize, 10, 40, 80, 127, 128, 200, 1000] {
            assert!(limit_text_bytes(&text, max).len() <= max);
        }
    }
}
