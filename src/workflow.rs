//! Multi-step workflows (built-in, `.goon/workflows/*.json|md`, user workflows), the pure
//! complexity classifier for `dag` steps, template interpolation and execution.

use crate::config::{atomic_json, config_dir};
use crate::files::limit_text;
use crate::jsutil::{now_millis, take_chars};
use crate::policy::{is_network_allowed_for_command, network_denied_message, ExecutionPolicy};
use crate::process::{run_command, RunOptions, DEFAULT_COMMAND_TIMEOUT_MS};
use crate::risk::{classify_bash_risk, RiskClass};
use crate::tools::sanitize_bash_command;
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;
use unicode_normalization::UnicodeNormalization;

/// A workflow shell step that could not obtain an explicit approval.
#[derive(Debug)]
pub struct WorkflowApprovalError(pub String);

impl std::fmt::Display for WorkflowApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WorkflowApprovalError {}

// ---------------------------------------------------------------------------------------------
// Classifier (Unicode-aware word boundaries + Vietnamese NFD / orthography normalization)
// ---------------------------------------------------------------------------------------------

const IMPERATIVE_VERBS: [&str; 38] = [
    "add",
    "implement",
    "fix",
    "refactor",
    "create",
    "write",
    "update",
    "delete",
    "remove",
    "migrate",
    "port",
    "build",
    "review",
    "audit",
    "deploy",
    "design",
    "research",
    "install",
    "configure",
    "rename",
    "move",
    "split",
    "merge",
    "optimize",
    "document",
    "clean",
    "extract",
    "đọc",
    "thêm",
    "sửa",
    "xóa",
    "tạo",
    "viết",
    "chạy",
    "cập nhật",
    "kiểm tra",
    "xây dựng",
    "triển khai",
];

fn toned_vowel(base: char, upper: bool, tone: usize) -> char {
    match (base, upper) {
        ('o', false) => ['ò', 'ỏ', 'õ', 'ó', 'ọ'][tone],
        ('o', true) => ['Ò', 'Ỏ', 'Õ', 'Ó', 'Ọ'][tone],
        ('u', false) => ['ù', 'ủ', 'ũ', 'ú', 'ụ'][tone],
        ('u', true) => ['Ù', 'Ủ', 'Ũ', 'Ú', 'Ụ'][tone],
        _ => base,
    }
}

fn split_toned_second_vowel(c: char) -> Option<(char, bool, usize)> {
    match c {
        'à' => Some(('a', false, 0)),
        'ả' => Some(('a', false, 1)),
        'ã' => Some(('a', false, 2)),
        'á' => Some(('a', false, 3)),
        'ạ' => Some(('a', false, 4)),
        'À' => Some(('a', true, 0)),
        'Ả' => Some(('a', true, 1)),
        'Ã' => Some(('a', true, 2)),
        'Á' => Some(('a', true, 3)),
        'Ạ' => Some(('a', true, 4)),
        'è' => Some(('e', false, 0)),
        'ẻ' => Some(('e', false, 1)),
        'ẽ' => Some(('e', false, 2)),
        'é' => Some(('e', false, 3)),
        'ẹ' => Some(('e', false, 4)),
        'È' => Some(('e', true, 0)),
        'Ẻ' => Some(('e', true, 1)),
        'Ẽ' => Some(('e', true, 2)),
        'É' => Some(('e', true, 3)),
        'Ẹ' => Some(('e', true, 4)),
        'ỳ' => Some(('y', false, 0)),
        'ỷ' => Some(('y', false, 1)),
        'ỹ' => Some(('y', false, 2)),
        'ý' => Some(('y', false, 3)),
        'ỵ' => Some(('y', false, 4)),
        'Ỳ' => Some(('y', true, 0)),
        'Ỷ' => Some(('y', true, 1)),
        'Ỹ' => Some(('y', true, 2)),
        'Ý' => Some(('y', true, 3)),
        'Ỵ' => Some(('y', true, 4)),
        _ => None,
    }
}

/// Normalizes Vietnamese text across NFD, reverse-ordered combining marks (CCC=230), stroke-overlay
/// `d`/`D`, and modern open-syllable diphthong tone placement (`xoá`->`xóa`, `hoà`->`hòa`, `huỷ`->`hủy`),
/// while preserving closed syllables (`hoàn`, `toán`, `ngoài`, `huỳnh`) and `qu`-clusters (`quý`, `quỷ`).
pub fn normalize_vietnamese(input: &str) -> String {
    let nfd: Vec<char> = input.nfd().collect();
    let mut reordered = String::with_capacity(input.len());
    let mut i = 0;
    while i < nfd.len() {
        let mut base = nfd[i];
        if unicode_normalization::char::canonical_combining_class(base) != 0 {
            reordered.push(base);
            i += 1;
            continue;
        }
        i += 1;
        let mut marks = Vec::new();
        while i < nfd.len() && unicode_normalization::char::canonical_combining_class(nfd[i]) != 0 {
            marks.push(nfd[i]);
            i += 1;
        }
        if matches!(base, 'd' | 'D') {
            if let Some(pos) = marks.iter().position(|m| matches!(m, '\u{0335}' | '\u{0336}')) {
                base = if base == 'd' { 'đ' } else { 'Đ' };
                marks.remove(pos);
            }
        }
        reordered.push(base);
        if matches!(base.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y') && marks.len() > 1 {
            for &m in &marks {
                if matches!(m, '\u{0302}' | '\u{0306}' | '\u{031B}') {
                    reordered.push(m);
                }
            }
            for &m in &marks {
                if matches!(m, '\u{0300}' | '\u{0309}' | '\u{0303}' | '\u{0301}' | '\u{0323}') {
                    reordered.push(m);
                }
            }
            for &m in &marks {
                if !matches!(m, '\u{0302}' | '\u{0306}' | '\u{031B}' | '\u{0300}' | '\u{0309}' | '\u{0303}' | '\u{0301}' | '\u{0323}') {
                    reordered.push(m);
                }
            }
        } else {
            for m in marks {
                reordered.push(m);
            }
        }
    }

    let chars: Vec<char> = reordered.nfc().collect();
    let mut out = String::with_capacity(reordered.len());
    let mut idx = 0;
    while idx < chars.len() {
        let c1 = chars[idx];
        if let Some(&c2) = chars.get(idx + 1) {
            let after = chars.get(idx + 2).copied();
            if !after.is_some_and(char::is_alphabetic) {
                if let Some((v2, v2_upper, tone)) = split_toned_second_vowel(c2) {
                    if matches!(c1, 'o' | 'O') && matches!(v2, 'a' | 'e') {
                        out.push(toned_vowel('o', c1.is_uppercase(), tone));
                        out.push(if v2_upper { v2.to_ascii_uppercase() } else { v2 });
                        idx += 2;
                        continue;
                    }
                    let prev_q = idx > 0 && matches!(chars[idx - 1], 'q' | 'Q');
                    if matches!(c1, 'u' | 'U') && v2 == 'y' && !prev_q {
                        out.push(toned_vowel('u', c1.is_uppercase(), tone));
                        out.push(if v2_upper { 'Y' } else { 'y' });
                        idx += 2;
                        continue;
                    }
                }
            }
        }
        out.push(c1);
        idx += 1;
    }
    out
}

fn verb_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"(?i)\b(?:{})\b", IMPERATIVE_VERBS.iter().map(|v| regex::escape(v)).collect::<Vec<_>>().join("|"))).unwrap())
}

fn sequence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(?:and then|then|after that|tiếp theo|sau[ -]?(?:đó|do)|rồi)\b").unwrap())
}

fn weak_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(?:and|và)\b").unwrap())
}

fn keyword_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(?:plan|workflow|multi[- ]?step|multi[- ]?agent|pipeline|roadmap|checklist|phase)\b").unwrap())
}

fn path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:(?:[A-Za-z0-9_.@-]+/)+[A-Za-z0-9_@.-]+)|(?:(?-u:\b)[A-Za-z0-9_-]+\.(?:ts|tsx|mts|cts|js|jsx|mjs|cjs|py|go|rs|java|rb|php|json|ya?ml|toml|sh|md)(?-u:\b))").unwrap()
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowClassification {
    pub mode: &'static str,
    pub reason: String,
}

/// Pure, conservative complexity classifier: `single` unless the signals clearly add up.
pub fn classify_workflow_complexity(prompt: &str, file_count: usize) -> WorkflowClassification {
    let normalized = normalize_vietnamese(prompt);
    let mut kinds = std::collections::HashSet::new();
    let mut hits = 0;
    for m in verb_re().find_iter(&normalized) {
        kinds.insert(m.as_str().to_lowercase());
        hits += 1;
    }
    let connectors = sequence_re().find_iter(&normalized).count();
    let weak = weak_re().find_iter(&normalized).count();
    let paths: std::collections::HashSet<String> = path_re().find_iter(&normalized).map(|m| m.as_str().to_lowercase()).collect();
    let path_count = paths.len() + file_count;
    let keyword = keyword_re().is_match(&normalized);
    let verbs = kinds.len();
    let detail = format!(
        "{verbs} distinct imperative verb(s) in {hits} hit(s), {connectors} sequencing connector(s), {weak} conjunction(s), {path_count} path mention(s){}",
        if keyword { ", multi-step keyword" } else { "" }
    );
    let multi = |reason: String| WorkflowClassification { mode: "multi", reason };
    if verbs >= 3 {
        return multi(format!("multi: {verbs} distinct imperative verbs ({detail})"));
    }
    if connectors >= 2 {
        return multi(format!("multi: {connectors} sequencing connectors ({detail})"));
    }
    if verbs >= 2 && connectors >= 1 {
        return multi(format!("multi: {verbs} imperative verbs chained by a sequencing connector ({detail})"));
    }
    if keyword && verbs + connectors >= 2 {
        return multi(format!("multi: multi-step keyword with {verbs} verb(s) and {connectors} connector(s) ({detail})"));
    }
    if path_count >= 3 && verbs >= 2 {
        return multi(format!("multi: {path_count} path mentions with {verbs} imperative verbs ({detail})"));
    }
    WorkflowClassification { mode: "single", reason: format!("single: conservative default, no strong multi-step signal ({detail})") }
}

// ---------------------------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------------------------

/// A step keeps its JSON object (key order matters for `workflow list --json`).
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowStep(pub Map<String, Value>);

impl WorkflowStep {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }
    pub fn id(&self) -> &str {
        self.str("id").unwrap_or("")
    }
    pub fn kind(&self) -> &str {
        self.str("type").unwrap_or("prompt")
    }
    pub fn title(&self) -> Option<&str> {
        self.str("title")
    }
    pub fn list(&self, key: &str) -> Option<Vec<String>> {
        self.0.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
    }
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.0.get(key).and_then(Value::as_bool)
    }
    pub fn rounds(&self) -> Option<u32> {
        self.0.get("rounds").and_then(Value::as_u64).map(|r| r as u32)
    }
}

fn step(value: Value) -> WorkflowStep {
    WorkflowStep(value.as_object().cloned().unwrap_or_default())
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowDefinition {
    pub name: String,
    pub description: String,
    pub scope: &'static str,
    pub path: Option<PathBuf>,
    pub inputs: Option<IndexMap<String, String>>,
    pub steps: Vec<WorkflowStep>,
}

impl WorkflowDefinition {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        m.insert("description".into(), json!(self.description));
        m.insert("scope".into(), json!(self.scope));
        if let Some(p) = &self.path {
            m.insert("path".into(), json!(p.to_string_lossy()));
        }
        if let Some(i) = &self.inputs {
            m.insert("inputs".into(), json!(i));
        }
        m.insert("steps".into(), Value::Array(self.steps.iter().map(|s| Value::Object(s.0.clone())).collect()));
        Value::Object(m)
    }
}

pub fn builtin_workflows() -> Vec<WorkflowDefinition> {
    let def = |name: &str, description: &str, steps: Vec<Value>| WorkflowDefinition {
        name: name.into(),
        description: description.into(),
        scope: "builtin",
        path: None,
        inputs: None,
        steps: steps.into_iter().map(step).collect(),
    };
    vec![
        def(
            "feature-dev",
            "Brainstorm with Agent Network -> Plan & Implement -> Run Verification",
            vec![
                json!({ "id": "brainstorm", "title": "Agent Network Architecture Brainstorm", "type": "network", "topic": "Design and plan implementation for: $input", "roles": ["architect", "researcher", "critic", "pragmatist"], "rounds": 2 }),
                json!({ "id": "implement", "title": "Implement Feature & Tests", "type": "prompt", "prompt": "Implement the feature requested below based on the Agent Network consensus blueprint.\n\nUser Request:\n$input\n\nAgent Network Blueprint:\n$prev\n\nWrite clean, well-typed code and add or update unit tests." }),
                json!({ "id": "verify", "title": "Verify & Summarize", "type": "prompt", "prompt": "Run relevant typecheck/lint/tests to verify the implementation for \"$input\", fix any issues, and summarize the changes." }),
            ],
        ),
        def(
            "bug-hunt",
            "Parallel Subagent Root-Cause Investigation -> Fix -> Regression Test",
            vec![
                json!({ "id": "investigate", "title": "Subagent Root-Cause Analysis", "type": "subagent", "readOnly": true, "tasks": [
                    "Search the codebase and locate the exact files, functions, and root cause related to: $input",
                    "Find existing tests covering the affected area and identify what regression test is needed for: $input"
                ] }),
                json!({ "id": "fix", "title": "Apply Minimal Fix & Regression Test", "type": "prompt", "prompt": "Fix the bug described below using the investigation findings.\n\nBug Report:\n$input\n\nInvestigation Findings:\n$prev\n\nApply the minimal root-cause fix and add a regression test." }),
            ],
        ),
        def(
            "tdd-cycle",
            "Write Failing Test First (Red) -> Implement Fix/Feature (Green) -> Refactor",
            vec![
                json!({ "id": "red", "title": "Write Failing Test (Red Phase)", "type": "prompt", "prompt": "TDD Step 1 (Red): Write a focused test for \"$input\" and run it to confirm it fails or tests the new behavior accurately. Do not implement the production code yet." }),
                json!({ "id": "green", "title": "Implement to Pass Test (Green Phase)", "type": "prompt", "prompt": "TDD Step 2 (Green): Implement the minimal production code required so the test for \"$input\" passes, then run the test suite." }),
                json!({ "id": "refactor", "title": "Refactor & Lint Check", "type": "prompt", "prompt": "TDD Step 3 (Refactor): Clean up any duplication, ensure types and lint pass, and provide a concise summary." }),
            ],
        ),
        def(
            "security-audit",
            "Multi-Agent Security & Reliability Review of Workspace Changes",
            vec![
                json!({ "id": "git_diff", "title": "Collect Git Status & Diff", "type": "shell", "command": "git status --short && git diff HEAD", "continueOnError": true }),
                json!({ "id": "council_audit", "title": "Security & Critic Agent Network Audit", "type": "network", "topic": "Perform a security, edge-case, and reliability audit on this repository. Focus: $input\n\nRecent git diff:\n$prev", "roles": ["security", "critic", "architect"], "rounds": 2 }),
            ],
        ),
    ]
}

fn strip_quotes(v: &str) -> String {
    let mut s = v.to_string();
    if s.starts_with('"') || s.starts_with('\'') {
        s.remove(0);
    }
    if s.ends_with('"') || s.ends_with('\'') {
        s.pop();
    }
    s
}

pub struct ParsedArgs {
    pub full_input: String,
    pub positional: Vec<String>,
    pub named: IndexMap<String, String>,
}

pub fn parse_workflow_args(raw: &str, defaults: Option<&IndexMap<String, String>>) -> ParsedArgs {
    static TOK: OnceLock<Regex> = OnceLock::new();
    static KV: OnceLock<Regex> = OnceLock::new();
    let tok = TOK.get_or_init(|| Regex::new(r#"(?:[^\s"']+|"[^"]*"|'[^']*')+"#).unwrap());
    let kv = KV.get_or_init(|| Regex::new(r"(?s)^(?:--)?([A-Za-z_][A-Za-z0-9_-]*)=(.*)$").unwrap());
    let mut named = defaults.cloned().unwrap_or_default();
    let mut positional = vec![];
    let mut remaining = vec![];
    for m in tok.find_iter(raw) {
        let t = m.as_str();
        if let Some(c) = kv.captures(t) {
            named.insert(c[1].to_string(), strip_quotes(&c[2]));
        } else {
            positional.push(strip_quotes(t));
            remaining.push(t.to_string());
        }
    }
    let joined = remaining.join(" ").trim().to_string();
    ParsedArgs { full_input: if joined.is_empty() { raw.trim().to_string() } else { joined }, positional, named }
}

#[derive(Clone, Debug)]
enum HeredocInner {
    DanglingBackslash,
    AfterDollar,
    Sub(Box<ShellCtx>),
}

#[derive(Clone, Debug)]
enum ShellCtx {
    Code(u8),
    Backtick { outer_double_quoted: bool, dangling_bs: bool, inner: Box<ShellCtx> },
    Heredoc { expand: bool, strip_tabs: bool, delim: String, inner: Option<HeredocInner> },
}

#[derive(Clone, Debug)]
struct HeredocSpec {
    expand: bool,
    strip_tabs: bool,
    delim: String,
}

#[derive(Clone, Debug)]
enum CasePhase {
    ExpectWord { saw_word_char: bool },
    ExpectIn,
    ExpectPat { started_token: bool, leading_paren: bool, after_pipe: bool, extglob: usize },
    InBody,
}

impl CasePhase {
    fn new_expect_pat() -> Self {
        CasePhase::ExpectPat { started_token: false, leading_paren: false, after_pipe: false, extglob: 0 }
    }

    fn note_word_char(&mut self) {
        match self {
            CasePhase::ExpectWord { saw_word_char } => *saw_word_char = true,
            CasePhase::ExpectPat { started_token, .. } => *started_token = true,
            _ => {}
        }
    }
}

#[derive(Clone, Debug)]
struct CmdFrame {
    outer: u8,
    is_root: bool,
    brace: usize,
    paren: usize,
    arith_cmd: usize,
    cases: Vec<CasePhase>,
    cmd_pos: bool,
    in_assign_rhs: bool,
    heredocs: VecDeque<HeredocSpec>,
}

impl CmdFrame {
    fn new_root() -> Self {
        Self { outer: 0, is_root: true, brace: 0, paren: 0, arith_cmd: 0, cases: Vec::new(), cmd_pos: true, in_assign_rhs: false, heredocs: VecDeque::new() }
    }

    fn new_sub(outer: u8) -> Self {
        Self { outer, is_root: false, brace: 0, paren: 0, arith_cmd: 0, cases: Vec::new(), cmd_pos: true, in_assign_rhs: false, heredocs: VecDeque::new() }
    }
}

#[derive(Clone, Debug)]
enum LexFrame {
    Cmd(CmdFrame),
    FuncSub(CmdFrame),
    Param { outer: u8 },
    Arith { outer: u8, paren: usize },
}

#[inline]
fn is_shell_delim(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>')
}

fn scan_backtick(chars: &[char], mut i: usize, outer_dq: bool) -> (Option<usize>, String, bool) {
    i += 1;
    let mut inner = String::new();
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' {
            if i + 1 >= chars.len() {
                inner.push('\\');
                return (None, inner, true);
            }
            let nxt = chars[i + 1];
            if nxt == '\n' {
                i += 2;
                continue;
            }
            if matches!(nxt, '$' | '`' | '\\') || (outer_dq && nxt == '"') {
                inner.push(nxt);
                i += 2;
                continue;
            }
            inner.push('\\');
            inner.push(nxt);
            i += 2;
            continue;
        }
        if ch == '`' {
            return (Some(i + 1), inner, false);
        }
        inner.push(ch);
        i += 1;
    }
    (None, inner, false)
}

fn escape_for_backtick(s: &str, outer_dq: bool) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(c, '\\' | '$' | '`') || (outer_dq && c == '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

enum HeredocHeader {
    Incomplete(ShellCtx),
    None(usize),
    Ok { next_i: usize, expand: bool, delim: String },
}

fn parse_heredoc_delim(chars: &[char], mut i: usize) -> HeredocHeader {
    while i < chars.len() && matches!(chars[i], ' ' | '\t') {
        i += 1;
    }
    let mut delim = String::new();
    let mut quoted = false;
    let mut started = false;
    while i < chars.len() && !is_shell_delim(chars[i]) && chars[i] != '\r' {
        started = true;
        let ch = chars[i];
        if ch == '\\' {
            quoted = true;
            if i + 1 >= chars.len() {
                return HeredocHeader::Incomplete(ShellCtx::Code(4));
            }
            if chars[i + 1] == '\n' {
                i += 2;
                continue;
            }
            delim.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if ch == '\'' {
            quoted = true;
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                delim.push(chars[i]);
                i += 1;
            }
            if i >= chars.len() {
                return HeredocHeader::Incomplete(ShellCtx::Code(1));
            }
            i += 1;
            continue;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'\'') {
            quoted = true;
            i += 2;
            while i < chars.len() {
                if chars[i] == '\\' {
                    if i + 1 >= chars.len() {
                        return HeredocHeader::Incomplete(ShellCtx::Code(8));
                    }
                    delim.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                if chars[i] == '\'' {
                    break;
                }
                delim.push(chars[i]);
                i += 1;
            }
            if i >= chars.len() {
                return HeredocHeader::Incomplete(ShellCtx::Code(3));
            }
            i += 1;
            continue;
        }
        if ch == '"' || (ch == '$' && chars.get(i + 1) == Some(&'"')) {
            quoted = true;
            i += if ch == '$' { 2 } else { 1 };
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    if i + 1 >= chars.len() {
                        return HeredocHeader::Incomplete(ShellCtx::Code(5));
                    }
                    let nxt = chars[i + 1];
                    if matches!(nxt, '\\' | '"' | '$' | '`') {
                        delim.push(nxt);
                    } else if nxt != '\n' {
                        delim.push('\\');
                        delim.push(nxt);
                    }
                    i += 2;
                    continue;
                }
                delim.push(chars[i]);
                i += 1;
            }
            if i >= chars.len() {
                return HeredocHeader::Incomplete(ShellCtx::Code(2));
            }
            i += 1;
            continue;
        }
        delim.push(ch);
        i += 1;
    }
    if !started && !quoted {
        return HeredocHeader::None(i);
    }
    HeredocHeader::Ok { next_i: i, expand: !quoted, delim }
}

fn is_heredoc_delim_line(line: &str, delim: &str, strip_tabs: bool) -> bool {
    let check = |s: &str| {
        let s = if strip_tabs { s.trim_start_matches('\t') } else { s };
        s == delim
    };
    check(line) || line.strip_suffix('\r').is_some_and(check)
}

fn analyze_unquoted_heredoc_body(body: &[char]) -> Option<HeredocInner> {
    let mut i = 0;
    while i < body.len() {
        let ch = body[i];
        if ch == '\\' {
            if i + 1 >= body.len() {
                return Some(HeredocInner::DanglingBackslash);
            }
            i += 2;
            continue;
        }
        if ch == '`' {
            let (end_i, inner_s, dangling_bs) = scan_backtick(body, i, false);
            let Some(next_i) = end_i else {
                return Some(HeredocInner::Sub(Box::new(ShellCtx::Backtick { outer_double_quoted: false, dangling_bs, inner: Box::new(analyze_shell_context(&inner_s)) })));
            };
            i = next_i;
            continue;
        }
        if ch == '$' {
            match body.get(i + 1) {
                Some('(' | '{') => {
                    let (end_i, sub_ctx) = run_shell_lexer(body, i, true, 2);
                    if let Some(next_i) = end_i {
                        i = next_i;
                        continue;
                    }
                    return Some(HeredocInner::Sub(Box::new(sub_ctx)));
                }
                None => return Some(HeredocInner::AfterDollar),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn analyze_shell_context(prefix: &str) -> ShellCtx {
    let chars: Vec<char> = prefix.chars().collect();
    run_shell_lexer(&chars, 0, false, 0).1
}

fn is_ident_assign_prefix(chars: &[char], start: usize) -> Option<usize> {
    let first = *chars.get(start)?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut j = start + 1;
    while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    if chars.get(j) == Some(&'+') && chars.get(j + 1) == Some(&'=') {
        return Some(j + 2);
    }
    if chars.get(j) == Some(&'=') {
        return Some(j + 1);
    }
    None
}

fn run_shell_lexer(chars: &[char], start_i: usize, stop_when_sub_closes: bool, sub_outer: u8) -> (Option<usize>, ShellCtx) {
    let mut stack: Vec<LexFrame> = if stop_when_sub_closes { Vec::new() } else { vec![LexFrame::Cmd(CmdFrame::new_root())] };
    let mut state: u8 = 0;
    let mut i = start_i;
    let mut last_unescaped_dollar: Option<usize> = None;

    while i < chars.len() {
        let ch = chars[i];
        match state {
            0 => {
                if ch == '\\' {
                    if i + 1 >= chars.len() {
                        return (None, ShellCtx::Code(4));
                    }
                    if chars[i + 1] == '\n' {
                        i += 2;
                        continue;
                    }
                    if let Some(LexFrame::Cmd(cf) | LexFrame::FuncSub(cf)) = stack.last_mut() {
                        if !cf.in_assign_rhs {
                            cf.cmd_pos = false;
                        }
                        if let Some(c) = cf.cases.last_mut() {
                            c.note_word_char();
                        }
                    }
                    i += 2;
                    continue;
                }

                if ch == '`' {
                    if let Some(LexFrame::Cmd(cf) | LexFrame::FuncSub(cf)) = stack.last_mut() {
                        if !cf.in_assign_rhs {
                            cf.cmd_pos = false;
                        }
                        if let Some(c) = cf.cases.last_mut() {
                            c.note_word_char();
                        }
                    }
                    let (end_i, inner_s, dangling_bs) = scan_backtick(chars, i, false);
                    let Some(next_i) = end_i else {
                        return (None, ShellCtx::Backtick { outer_double_quoted: false, dangling_bs, inner: Box::new(analyze_shell_context(&inner_s)) });
                    };
                    i = next_i;
                    continue;
                }

                let in_param = matches!(stack.last(), Some(LexFrame::Param { .. }));
                let in_arith = matches!(stack.last(), Some(LexFrame::Arith { .. }));

                if !in_param && !in_arith && ch == '#' && (i == start_i || is_shell_delim(chars[i - 1])) {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return (None, ShellCtx::Code(6));
                    }
                    continue;
                }

                if ch == '\n' {
                    if let Some(LexFrame::Cmd(cf) | LexFrame::FuncSub(cf)) = stack.last_mut() {
                        cf.in_assign_rhs = false;
                        match cf.cases.last_mut() {
                            Some(CasePhase::ExpectWord { saw_word_char }) if *saw_word_char => {
                                *cf.cases.last_mut().unwrap() = CasePhase::ExpectIn;
                            }
                            Some(CasePhase::ExpectWord { .. } | CasePhase::ExpectIn | CasePhase::ExpectPat { .. }) => {}
                            _ => cf.cmd_pos = true,
                        }
                        if !cf.heredocs.is_empty() {
                            i += 1;
                            while let Some(spec) = cf.heredocs.front().cloned() {
                                let body_start = i;
                                let mut closed = false;
                                while i < chars.len() {
                                    let mut line = String::new();
                                    while i < chars.len() {
                                        if spec.expand && chars[i] == '\\' && chars.get(i + 1) == Some(&'\n') {
                                            i += 2;
                                            continue;
                                        }
                                        if chars[i] == '\n' {
                                            break;
                                        }
                                        line.push(chars[i]);
                                        i += 1;
                                    }
                                    if i < chars.len() && chars[i] == '\n' {
                                        i += 1;
                                        if is_heredoc_delim_line(&line, &spec.delim, spec.strip_tabs) {
                                            cf.heredocs.pop_front();
                                            closed = true;
                                            break;
                                        }
                                    } else {
                                        break;
                                    }
                                }
                                if !closed {
                                    let inner = if spec.expand { analyze_unquoted_heredoc_body(&chars[body_start..]) } else { None };
                                    return (None, ShellCtx::Heredoc { expand: spec.expand, strip_tabs: spec.strip_tabs, delim: spec.delim, inner });
                                }
                            }
                            continue;
                        }
                    }
                    i += 1;
                    continue;
                }

                if ch == '$' {
                    if let Some(LexFrame::Cmd(cf) | LexFrame::FuncSub(cf)) = stack.last_mut() {
                        if !cf.in_assign_rhs {
                            cf.cmd_pos = false;
                        }
                        if let Some(c) = cf.cases.last_mut() {
                            c.note_word_char();
                        }
                    }
                    let eff_outer = if stop_when_sub_closes && stack.is_empty() { sub_outer } else { 0 };
                    match chars.get(i + 1) {
                        Some('\'') => {
                            state = 3;
                            i += 2;
                            continue;
                        }
                        Some('"') => {
                            state = 2;
                            i += 2;
                            continue;
                        }
                        Some('(') if chars.get(i + 2) == Some(&'(') => {
                            stack.push(LexFrame::Arith { outer: eff_outer, paren: 1 });
                            state = 0;
                            i += 3;
                            continue;
                        }
                        Some('(') => {
                            stack.push(LexFrame::Cmd(CmdFrame::new_sub(eff_outer)));
                            state = 0;
                            i += 2;
                            continue;
                        }
                        Some('{') => {
                            if matches!(chars.get(i + 2), Some(' ' | '\t' | '\n' | '|')) {
                                stack.push(LexFrame::FuncSub(CmdFrame::new_sub(eff_outer)));
                            } else {
                                stack.push(LexFrame::Param { outer: eff_outer });
                            }
                            state = 0;
                            i += 2;
                            continue;
                        }
                        None => {
                            last_unescaped_dollar = Some(i);
                            i += 1;
                            continue;
                        }
                        _ => {
                            i += 1;
                            continue;
                        }
                    }
                }

                if ch == '}' {
                    if matches!(stack.last(), Some(LexFrame::Param { .. })) {
                        let Some(LexFrame::Param { outer }) = stack.pop() else { unreachable!() };
                        state = outer;
                        i += 1;
                        if stop_when_sub_closes && stack.is_empty() {
                            return (Some(i), ShellCtx::Code(state));
                        }
                        continue;
                    }
                    if let Some(LexFrame::FuncSub(cf)) = stack.last_mut() {
                        if cf.brace > 0 {
                            cf.brace -= 1;
                            cf.cmd_pos = true;
                            cf.in_assign_rhs = false;
                            i += 1;
                            continue;
                        }
                        let outer = cf.outer;
                        stack.pop();
                        state = outer;
                        i += 1;
                        if stop_when_sub_closes && stack.is_empty() {
                            return (Some(i), ShellCtx::Code(state));
                        }
                        continue;
                    }
                }

                if let Some(LexFrame::Arith { outer, paren }) = stack.last_mut() {
                    if ch == '(' {
                        *paren += 1;
                        i += 1;
                        continue;
                    }
                    if ch == ')' {
                        *paren = paren.saturating_sub(1);
                        if *paren == 0 {
                            let saved_outer = *outer;
                            if chars.get(i + 1) == Some(&')') {
                                stack.pop();
                                state = saved_outer;
                                i += 2;
                                if stop_when_sub_closes && stack.is_empty() {
                                    return (Some(i), ShellCtx::Code(state));
                                }
                                continue;
                            }
                            let mut sub = CmdFrame::new_sub(saved_outer);
                            sub.cmd_pos = false;
                            *stack.last_mut().unwrap() = LexFrame::Cmd(sub);
                            i += 1;
                            continue;
                        }
                        i += 1;
                        continue;
                    }
                    if ch == '\'' {
                        state = 1;
                        i += 1;
                        continue;
                    }
                    if ch == '"' {
                        state = 2;
                        i += 1;
                        continue;
                    }
                    i += 1;
                    continue;
                }

                if ch == '\'' || ch == '"' {
                    if let Some(LexFrame::Cmd(cf) | LexFrame::FuncSub(cf)) = stack.last_mut() {
                        if !cf.in_assign_rhs {
                            cf.cmd_pos = false;
                        }
                        if let Some(c) = cf.cases.last_mut() {
                            c.note_word_char();
                        }
                    }
                    state = if ch == '\'' { 1 } else { 2 };
                    i += 1;
                    continue;
                }

                if in_param {
                    i += 1;
                    continue;
                }

                let is_funcsub = matches!(stack.last(), Some(LexFrame::FuncSub(_)));
                let Some(LexFrame::Cmd(cf) | LexFrame::FuncSub(cf)) = stack.last_mut() else {
                    i += 1;
                    continue;
                };

                if matches!(ch, ' ' | '\t') {
                    if cf.in_assign_rhs {
                        cf.in_assign_rhs = false;
                        cf.cmd_pos = true;
                    }
                    if matches!(cf.cases.last(), Some(CasePhase::ExpectWord { saw_word_char: true })) {
                        *cf.cases.last_mut().unwrap() = CasePhase::ExpectIn;
                    }
                    i += 1;
                    continue;
                }

                if ch == ';' && cf.paren == 0 && cf.arith_cmd == 0 && matches!(cf.cases.last(), Some(CasePhase::InBody)) {
                    if chars.get(i + 1) == Some(&';') {
                        i += if chars.get(i + 2) == Some(&'&') { 3 } else { 2 };
                        *cf.cases.last_mut().unwrap() = CasePhase::new_expect_pat();
                        cf.cmd_pos = false;
                        cf.in_assign_rhs = false;
                        continue;
                    }
                    if chars.get(i + 1) == Some(&'&') {
                        i += 2;
                        *cf.cases.last_mut().unwrap() = CasePhase::new_expect_pat();
                        cf.cmd_pos = false;
                        cf.in_assign_rhs = false;
                        continue;
                    }
                }

                if ch == '<' && chars.get(i + 1) == Some(&'<') && cf.arith_cmd == 0 {
                    if chars.get(i + 2) == Some(&'<') {
                        cf.cmd_pos = false;
                        cf.in_assign_rhs = false;
                        i += 3;
                        continue;
                    }
                    let strip_tabs = chars.get(i + 2) == Some(&'-');
                    let after_op = i + if strip_tabs { 3 } else { 2 };
                    match parse_heredoc_delim(chars, after_op) {
                        HeredocHeader::Incomplete(ctx) => return (None, ctx),
                        HeredocHeader::Ok { next_i, expand, delim } => {
                            cf.heredocs.push_back(HeredocSpec { expand, strip_tabs, delim });
                            i = next_i;
                            continue;
                        }
                        HeredocHeader::None(next_i) => {
                            i = next_i;
                            continue;
                        }
                    }
                }

                if ch == '(' {
                    cf.in_assign_rhs = false;
                    if let Some(CasePhase::ExpectPat { started_token, leading_paren, extglob, .. }) = cf.cases.last_mut() {
                        if !*started_token && !*leading_paren {
                            *leading_paren = true;
                        } else {
                            *extglob += 1;
                        }
                        i += 1;
                        continue;
                    }
                    if cf.cmd_pos && chars.get(i + 1) == Some(&'(') {
                        cf.arith_cmd += 1;
                        cf.cmd_pos = false;
                        i += 2;
                        continue;
                    }
                    cf.paren += 1;
                    cf.cmd_pos = true;
                    i += 1;
                    continue;
                }

                if ch == ')' {
                    cf.in_assign_rhs = false;
                    if cf.arith_cmd > 0 && chars.get(i + 1) == Some(&')') {
                        cf.arith_cmd -= 1;
                        cf.cmd_pos = false;
                        i += 2;
                        continue;
                    }
                    if cf.paren == 0 {
                        if let Some(CasePhase::ExpectPat { extglob, .. }) = cf.cases.last_mut() {
                            if *extglob > 0 {
                                *extglob -= 1;
                                i += 1;
                                continue;
                            }
                            *cf.cases.last_mut().unwrap() = CasePhase::InBody;
                            cf.cmd_pos = true;
                            i += 1;
                            continue;
                        }
                    }
                    if cf.paren > 0 {
                        cf.paren -= 1;
                        cf.cmd_pos = false;
                        i += 1;
                        continue;
                    }
                    if !cf.is_root && !is_funcsub {
                        let outer = cf.outer;
                        stack.pop();
                        state = outer;
                        i += 1;
                        if stop_when_sub_closes && stack.is_empty() {
                            return (Some(i), ShellCtx::Code(state));
                        }
                        continue;
                    }
                    i += 1;
                    continue;
                }

                if matches!(ch, ';' | '&' | '|') {
                    cf.in_assign_rhs = false;
                    if ch == '|' {
                        if let Some(CasePhase::ExpectPat { started_token, after_pipe, .. }) = cf.cases.last_mut() {
                            *started_token = false;
                            *after_pipe = true;
                            i += 1;
                            continue;
                        }
                    }
                    cf.cmd_pos = true;
                    i += 1;
                    continue;
                }

                if matches!(ch, '<' | '>') {
                    i += 1;
                    continue;
                }

                let prev_is_delim = i == start_i || is_shell_delim(chars[i - 1]);
                if prev_is_delim {
                    if cf.cmd_pos && matches!(cf.cases.last(), None | Some(CasePhase::InBody)) {
                        if let Some(after_eq) = is_ident_assign_prefix(chars, i) {
                            cf.in_assign_rhs = true;
                            cf.cmd_pos = false;
                            i = after_eq;
                            continue;
                        }
                    }
                    let mut j = i;
                    while j < chars.len() && (chars[j].is_alphanumeric() || matches!(chars[j], '_' | '{' | '!')) {
                        if matches!(chars[j], '{' | '!') && j > i {
                            break;
                        }
                        j += 1;
                        if matches!(chars[j - 1], '{' | '!') {
                            break;
                        }
                    }
                    let next_is_delim = j == chars.len() || is_shell_delim(chars[j]);
                    if j > i && next_is_delim {
                        let word: String = chars[i..j].iter().collect();
                        if let Some(cphase) = cf.cases.last_mut() {
                            match cphase {
                                CasePhase::ExpectWord { saw_word_char } => {
                                    *saw_word_char = true;
                                    i = j;
                                    continue;
                                }
                                CasePhase::ExpectIn => {
                                    if word == "in" {
                                        *cphase = CasePhase::new_expect_pat();
                                    }
                                    i = j;
                                    continue;
                                }
                                CasePhase::ExpectPat { started_token, leading_paren, after_pipe, .. } => {
                                    if word == "esac" && !*leading_paren && !*started_token && !*after_pipe {
                                        cf.cases.pop();
                                        cf.cmd_pos = false;
                                        i = j;
                                        continue;
                                    }
                                    *started_token = true;
                                    i = j;
                                    continue;
                                }
                                CasePhase::InBody => {}
                            }
                        }
                        if cf.cmd_pos {
                            if word == "case" {
                                cf.cases.push(CasePhase::ExpectWord { saw_word_char: false });
                                cf.cmd_pos = false;
                                i = j;
                                continue;
                            }
                            if word == "esac" && matches!(cf.cases.last(), Some(CasePhase::InBody)) {
                                cf.cases.pop();
                                cf.cmd_pos = false;
                                i = j;
                                continue;
                            }
                            if word == "{" && is_funcsub {
                                cf.brace += 1;
                                cf.cmd_pos = true;
                                i = j;
                                continue;
                            }
                            if matches!(word.as_str(), "do" | "then" | "else" | "elif" | "!" | "{") {
                                cf.cmd_pos = true;
                                i = j;
                                continue;
                            }
                        }
                        cf.cmd_pos = false;
                        i = j;
                        continue;
                    }
                }

                if let Some(c) = cf.cases.last_mut() {
                    c.note_word_char();
                }
                if !cf.in_assign_rhs {
                    cf.cmd_pos = false;
                }
                i += 1;
                continue;
            }
            1 => {
                if ch == '\'' {
                    state = 0;
                }
                i += 1;
                continue;
            }
            3 => {
                if ch == '\\' {
                    if i + 1 >= chars.len() {
                        return (None, ShellCtx::Code(8));
                    }
                    i += 2;
                    continue;
                }
                if ch == '\'' {
                    state = 0;
                }
                i += 1;
                continue;
            }
            _ => {
                if ch == '\\' {
                    if i + 1 >= chars.len() {
                        return (None, ShellCtx::Code(5));
                    }
                    i += 2;
                    continue;
                }
                if ch == '`' {
                    let (end_i, inner_s, dangling_bs) = scan_backtick(chars, i, true);
                    let Some(next_i) = end_i else {
                        return (None, ShellCtx::Backtick { outer_double_quoted: true, dangling_bs, inner: Box::new(analyze_shell_context(&inner_s)) });
                    };
                    i = next_i;
                    continue;
                }
                if ch == '$' {
                    match chars.get(i + 1) {
                        Some('(') if chars.get(i + 2) == Some(&'(') => {
                            stack.push(LexFrame::Arith { outer: 2, paren: 1 });
                            state = 0;
                            i += 3;
                            continue;
                        }
                        Some('(') => {
                            stack.push(LexFrame::Cmd(CmdFrame::new_sub(2)));
                            state = 0;
                            i += 2;
                            continue;
                        }
                        Some('{') => {
                            if matches!(chars.get(i + 2), Some(' ' | '\t' | '\n' | '|')) {
                                stack.push(LexFrame::FuncSub(CmdFrame::new_sub(2)));
                            } else {
                                stack.push(LexFrame::Param { outer: 2 });
                            }
                            state = 0;
                            i += 2;
                            continue;
                        }
                        None => {
                            last_unescaped_dollar = Some(i);
                            i += 1;
                            continue;
                        }
                        _ => {
                            i += 1;
                            continue;
                        }
                    }
                }
                if ch == '"' {
                    state = 0;
                }
                i += 1;
                continue;
            }
        }
    }

    let trailing_dollar = last_unescaped_dollar.is_some_and(|idx| idx + 1 == chars.len());
    if state == 0 {
        match stack.last() {
            Some(LexFrame::Arith { .. }) => return (None, ShellCtx::Code(10)),
            Some(LexFrame::Param { outer: 2 }) => return (None, ShellCtx::Code(10)),
            _ => {}
        }
        if trailing_dollar {
            return (None, ShellCtx::Code(7));
        }
        return (None, ShellCtx::Code(0));
    }
    if state == 2 && trailing_dollar {
        return (None, ShellCtx::Code(9));
    }
    (None, ShellCtx::Code(state))
}

#[allow(dead_code)]
fn shell_quote_context(prefix: &str) -> u8 {
    match analyze_shell_context(prefix) {
        ShellCtx::Code(c) => c,
        ShellCtx::Backtick { outer_double_quoted, .. } => {
            if outer_double_quoted {
                13
            } else {
                12
            }
        }
        ShellCtx::Heredoc { .. } => 11,
    }
}

pub fn shell_escape(value: &str, context: u8) -> String {
    match context {
        1 => value.replace('\'', "'\\''"),
        2 => {
            let mut out = String::with_capacity(value.len());
            for c in value.chars() {
                if matches!(c, '\\' | '"' | '$' | '`') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        3 => {
            let mut out = String::with_capacity(value.len());
            for c in value.chars() {
                if matches!(c, '\\' | '\'') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        4 => format!("\\'{}'", value.replace('\'', "'\\''")),
        5 => format!("\\{}", shell_escape(value, 2)),
        6 => value.chars().map(|c| if matches!(c, '\n' | '\r') { ' ' } else { c }).collect(),
        7 => format!("\"\"'{}'", value.replace('\'', "'\\''")),
        8 => format!("\\{}", shell_escape(value, 3)),
        9 => format!("\"\"{}", shell_escape(value, 2)),
        10 => format!("\"{}\"", shell_escape(value, 2)),
        11 => {
            let mut out = String::with_capacity(value.len());
            for c in value.chars() {
                if matches!(c, '\\' | '$' | '`') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        12 => escape_for_backtick(&shell_escape(value, 0), false),
        13 => escape_for_backtick(&shell_escape(value, 0), true),
        _ => format!("'{}'", value.replace('\'', "'\\''")),
    }
}

fn neutralize_heredoc_lines(rendered: &str, line_prefix: &str, line_suffix: &str, delim: &str, strip_tabs: bool) -> String {
    let mut lines: Vec<String> = rendered.split('\n').map(str::to_string).collect();
    let last_idx = lines.len().saturating_sub(1);
    for (idx, seg) in lines.iter_mut().enumerate() {
        let pre = if idx == 0 { line_prefix } else { "" };
        let suf = if idx == last_idx { line_suffix } else { "" };
        if idx == last_idx && !suf.is_empty() {
            while is_heredoc_delim_line(&format!("{pre}{seg}{suf}"), delim, strip_tabs) {
                seg.insert(0, ' ');
            }
        } else {
            while is_heredoc_delim_line(&format!("{pre}{seg}"), delim, strip_tabs) {
                if seg.ends_with('\r') {
                    let cr_pos = seg.len() - 1;
                    seg.insert(cr_pos, ' ');
                } else {
                    seg.push(' ');
                }
            }
        }
    }
    lines.join("\n")
}

fn render_shell_ctx(value: &str, ctx: &ShellCtx, line_prefix: &str, line_suffix: &str) -> String {
    match ctx {
        ShellCtx::Code(code) => shell_escape(value, *code),
        ShellCtx::Backtick { outer_double_quoted, dangling_bs, inner } => {
            let inner_rendered = render_shell_ctx(value, inner, "", "");
            let escaped = escape_for_backtick(&inner_rendered, *outer_double_quoted);
            if *dangling_bs {
                format!("\\{escaped}")
            } else {
                escaped
            }
        }
        ShellCtx::Heredoc { expand, strip_tabs, delim, inner } => {
            let raw = if !*expand {
                value.to_string()
            } else {
                match inner {
                    None => shell_escape(value, 11),
                    Some(HeredocInner::DanglingBackslash) => format!("\\{}", shell_escape(value, 11)),
                    Some(HeredocInner::AfterDollar) => {
                        let esc = shell_escape(value, 11);
                        if esc.starts_with('(') || esc.starts_with('{') {
                            format!("\\{esc}")
                        } else {
                            esc
                        }
                    }
                    Some(HeredocInner::Sub(sub)) => render_shell_ctx(value, sub, "", ""),
                }
            };
            neutralize_heredoc_lines(&raw, line_prefix, line_suffix, delim, *strip_tabs)
        }
    }
}

pub struct InterpContext<'a> {
    pub input: &'a str,
    pub positional: &'a [String],
    pub named: &'a IndexMap<String, String>,
    pub prev_output: &'a str,
    pub step_outputs: &'a IndexMap<String, String>,
}

fn replace_each(src: &str, re: &Regex, mut f: impl FnMut(&regex::Captures, usize, usize, &str, &str) -> String) -> String {
    let mut out = String::new();
    let mut last = 0;
    for c in re.captures_iter(src) {
        let m = c.get(0).unwrap();
        out.push_str(&src[last..m.start()]);
        let replaced = f(&c, m.start(), m.end(), src, &out);
        out.push_str(&replaced);
        last = m.end();
    }
    out.push_str(&src[last..]);
    out
}

pub fn interpolate_workflow_template(template: &str, ctx: &InterpContext, shell: bool) -> String {
    static SHELL_RE: OnceLock<Regex> = OnceLock::new();
    static TEXT_RE: OnceLock<Regex> = OnceLock::new();
    if shell {
        let re = SHELL_RE.get_or_init(|| {
            Regex::new(
                r"\{\{\s*steps\.([A-Za-z0-9_-]+)\.output\s*\}\}|\{\{\s*(input)\s*\}\}|\$(input)(?-u:\b)|\{\{\s*(prev)\s*\}\}|\$(prev)(?-u:\b)|\{\{\s*([0-9]+)\s*\}\}|\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}",
            )
            .unwrap()
        });
        return replace_each(template, re, |c, start, end, src, out_so_far| {
            let fmt = |val: &str| {
                let sctx = analyze_shell_context(&src[..start]);
                let line_prefix = out_so_far.rsplit('\n').next().unwrap_or(out_so_far);
                let rem = &src[end..];
                let line_suffix = rem.split('\n').next().unwrap_or(rem);
                render_shell_ctx(val, &sctx, line_prefix, line_suffix)
            };
            if let Some(step_id) = c.get(1) {
                return fmt(ctx.step_outputs.get(step_id.as_str()).map(String::as_str).unwrap_or(""));
            }
            if c.get(2).is_some() || c.get(3).is_some() {
                return fmt(ctx.input);
            }
            if c.get(4).is_some() || c.get(5).is_some() {
                return fmt(ctx.prev_output);
            }
            if let Some(num) = c.get(6) {
                let idx = num.as_str().parse::<usize>().unwrap_or(0);
                return fmt(idx.checked_sub(1).and_then(|i| ctx.positional.get(i)).map(String::as_str).unwrap_or(""));
            }
            if let Some(name) = c.get(7) {
                let key = name.as_str();
                if let Some(v) = ctx.named.get(key) {
                    return fmt(v);
                }
                if let Some(v) = ctx.step_outputs.get(key) {
                    return fmt(v);
                }
            }
            c[0].to_string()
        });
    }
    let re = TEXT_RE.get_or_init(|| {
        Regex::new(
            r"\{\{\s*steps\.([A-Za-z0-9_-]+)\.output\s*\}\}|\{\{\s*(input)\s*\}\}|\$(input)(?-u:\b)|\{\{\s*(prev)\s*\}\}|\$(prev)(?-u:\b)|\{\{\s*([0-9]+)\s*\}\}|\$([0-9]+)(?-u:\b)|\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}|\$([A-Za-z_][A-Za-z0-9_-]*)(?-u:\b)",
        )
        .unwrap()
    });
    replace_each(template, re, |c, _, _, _, _| {
        if let Some(step_id) = c.get(1) {
            return ctx.step_outputs.get(step_id.as_str()).cloned().unwrap_or_default();
        }
        if c.get(2).is_some() || c.get(3).is_some() {
            return ctx.input.to_string();
        }
        if c.get(4).is_some() || c.get(5).is_some() {
            return ctx.prev_output.to_string();
        }
        if let Some(num) = c.get(6).or_else(|| c.get(7)) {
            let idx = num.as_str().parse::<usize>().unwrap_or(0);
            return idx.checked_sub(1).and_then(|i| ctx.positional.get(i)).cloned().unwrap_or_default();
        }
        if let Some(name) = c.get(8).or_else(|| c.get(9)) {
            let key = name.as_str();
            if let Some(v) = ctx.named.get(key) {
                return v.clone();
            }
            if let Some(v) = ctx.step_outputs.get(key) {
                return v.clone();
            }
        }
        c[0].to_string()
    })
}

const STEP_TYPES: [&str; 6] = ["prompt", "shell", "subagent", "network", "approval", "dag"];

pub fn parse_markdown_workflow(filename: &str, content: &str, scope: &'static str, path: &Path) -> WorkflowDefinition {
    static EXT: OnceLock<Regex> = OnceLock::new();
    static FM: OnceLock<Regex> = OnceLock::new();
    static LINE: OnceLock<Regex> = OnceLock::new();
    static STEP: OnceLock<Regex> = OnceLock::new();
    let mut name = EXT.get_or_init(|| Regex::new(r"(?i)\.(?:md|json)$").unwrap()).replace(filename, "").to_string();
    let mut description = String::new();
    let mut inputs = IndexMap::new();
    let mut body = content.to_string();
    if let Some(m) = FM.get_or_init(|| Regex::new(r"^---\r?\n((?s).*?)\r?\n---\r?\n?").unwrap()).captures(content) {
        body = content[m.get(0).unwrap().end()..].to_string();
        let line_re = LINE.get_or_init(|| Regex::new(r"^([A-Za-z0-9_-]+)\s*:\s*(.+)$").unwrap());
        for line in m[1].split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let Some(c) = line_re.captures(line) else { continue };
            let k = c[1].to_lowercase();
            let v = strip_quotes(c[2].trim());
            if k == "name" && !v.is_empty() {
                name = v;
            } else if k == "description" && !v.is_empty() {
                description = v;
            } else if let Some(key) = k.strip_prefix("input.").filter(|_| !v.is_empty()) {
                inputs.insert(key.to_string(), v);
            }
        }
    }
    let step_re = STEP.get_or_init(|| Regex::new(r"(?im)^##[ \t]+(?:Step[ \t]*[0-9]*[ \t]*:[ \t]*)?(.+?)(?:[ \t]+\[([^\]\n]+)\])?[ \t]*\r?$").unwrap());
    let matches: Vec<regex::Captures> = step_re.captures_iter(&body).collect();
    let mut steps = vec![];
    if !matches.is_empty() {
        for (i, m) in matches.iter().enumerate() {
            let title = m[1].trim().to_string();
            let attr = m.get(2).map(|g| g.as_str()).unwrap_or("");
            let start = m.get(0).unwrap().end();
            let end = matches.get(i + 1).map(|n| n.get(0).unwrap().start()).unwrap_or(body.len());
            let section = body[start..end].trim().to_string();
            let mut kind = "prompt".to_string();
            let mut id = format!("step_{}", i + 1);
            let mut roles: Option<Vec<String>> = None;
            let mut rounds: Option<u64> = None;
            for part in attr.split(|c: char| c.is_whitespace() || c == ',').filter(|p| !p.is_empty()) {
                let mut kv = part.split('=');
                let k = kv.next().unwrap_or("");
                let v = kv.next();
                match v {
                    None if STEP_TYPES.contains(&k.to_lowercase().as_str()) => kind = k.to_lowercase(),
                    Some(v) if k == "type" && STEP_TYPES.contains(&v.to_lowercase().as_str()) => kind = v.to_lowercase(),
                    Some(v) if k == "id" && !v.is_empty() => id = v.into(),
                    Some(v) if k == "roles" && !v.is_empty() => roles = Some(v.split('|').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()),
                    Some(v) if k == "rounds" && !v.is_empty() => {
                        if let Ok(r) = v.parse::<u64>() {
                            if (1..=4).contains(&r) {
                                rounds = Some(r);
                            }
                        }
                    }
                    _ => {}
                }
            }
            let mut s = Map::new();
            s.insert("id".into(), json!(id));
            s.insert("title".into(), json!(title));
            s.insert("type".into(), json!(kind));
            match kind.as_str() {
                "shell" => {
                    let code = crate::re!(r"(?i)```(?:bash|sh|shell)?\r?\n((?s).*?)```");
                    let command = code.captures(&section).map(|c| c[1].to_string()).unwrap_or_else(|| section.clone());
                    s.insert("command".into(), json!(command.trim()));
                }
                "network" => {
                    s.insert("topic".into(), json!(section));
                    if let Some(r) = roles {
                        s.insert("roles".into(), json!(r));
                    }
                    if let Some(r) = rounds {
                        s.insert("rounds".into(), json!(r));
                    }
                }
                "subagent" => {
                    let bullet = crate::re!(r"^\s*[-*]\s+");
                    let bullets: Vec<String> = section.split('\n').map(|l| bullet.replace(l.strip_suffix('\r').unwrap_or(l), "").trim().to_string()).filter(|l| !l.is_empty()).collect();
                    s.insert("tasks".into(), json!(if bullets.is_empty() { vec![section.clone()] } else { bullets }));
                    s.insert("readOnly".into(), json!(true));
                }
                "approval" => {
                    s.insert("message".into(), json!(if section.is_empty() { title.clone() } else { section.clone() }));
                }
                _ => {
                    s.insert("prompt".into(), json!(section));
                }
            }
            steps.push(WorkflowStep(s));
        }
    } else {
        let trimmed = body.trim();
        steps.push(step(json!({ "id": "main", "title": name, "type": "prompt", "prompt": if trimmed.is_empty() { "Complete workflow task: $input" } else { trimmed } })));
    }
    if description.is_empty() {
        let first = body.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).find(|l| !l.trim().is_empty() && !l.trim().starts_with('#')).unwrap_or("");
        description = take_chars(first.trim(), 120).to_string();
        if description.is_empty() {
            description = format!("Custom workflow ({} step{})", steps.len(), if steps.len() > 1 { "s" } else { "" });
        }
    }
    WorkflowDefinition { name, description, scope, path: Some(path.to_path_buf()), inputs: (!inputs.is_empty()).then_some(inputs), steps }
}

/// Validates a workflow JSON file like the zod schema (trimmed strings, defaults, stripped keys).
fn parse_workflow_json(raw: &Value) -> Result<(Option<String>, Option<String>, Option<IndexMap<String, String>>, Vec<Map<String, Value>>)> {
    let obj = raw.as_object().ok_or_else(|| anyhow!("expected object"))?;
    let trimmed = |v: Option<&Value>, min: usize, max: usize| -> Result<Option<String>> {
        match v {
            None => Ok(None),
            Some(Value::String(s)) => {
                let t = s.trim().to_string();
                let n = t.chars().count();
                if n < min || n > max {
                    bail!("invalid length");
                }
                Ok(Some(t))
            }
            _ => bail!("expected string"),
        }
    };
    let name = trimmed(obj.get("name"), 1, 64)?;
    let description = trimmed(obj.get("description"), 0, 240)?;
    let inputs = match obj.get("inputs") {
        None => None,
        Some(Value::Object(m)) => Some(m.iter().map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())).ok_or_else(|| anyhow!("expected string"))).collect::<Result<IndexMap<_, _>>>()?),
        Some(_) => bail!("expected record"),
    };
    let steps = obj.get("steps").and_then(Value::as_array).ok_or_else(|| anyhow!("expected steps"))?;
    if steps.is_empty() || steps.len() > 20 {
        bail!("invalid steps");
    }
    let mut out = vec![];
    for s in steps {
        let so = s.as_object().ok_or_else(|| anyhow!("expected step object"))?;
        let mut m = Map::new();
        if let Some(id) = trimmed(so.get("id"), 1, 64)? {
            m.insert("id".into(), json!(id));
        }
        if let Some(t) = trimmed(so.get("title"), 0, 140)? {
            m.insert("title".into(), json!(t));
        }
        let kind = match so.get("type") {
            None => "prompt".to_string(),
            Some(Value::String(t)) if STEP_TYPES.contains(&t.as_str()) => t.clone(),
            _ => bail!("invalid type"),
        };
        m.insert("type".into(), json!(kind));
        for key in ["prompt", "command"] {
            match so.get(key) {
                None => {}
                Some(Value::String(v)) => {
                    m.insert(key.into(), json!(v));
                }
                _ => bail!("expected string"),
            }
        }
        if let Some(v) = so.get("tasks") {
            let a = v.as_array().filter(|a| a.iter().all(Value::is_string)).ok_or_else(|| anyhow!("expected string array"))?;
            m.insert("tasks".into(), json!(a));
        }
        if let Some(v) = so.get("topic") {
            m.insert("topic".into(), json!(v.as_str().ok_or_else(|| anyhow!("expected string"))?));
        }
        if let Some(v) = so.get("roles") {
            let a = v.as_array().filter(|a| a.iter().all(Value::is_string)).ok_or_else(|| anyhow!("expected string array"))?;
            m.insert("roles".into(), json!(a));
        }
        if let Some(v) = so.get("rounds") {
            let r = v.as_f64().filter(|r| r.fract() == 0.0 && (1.0..=4.0).contains(r)).ok_or_else(|| anyhow!("invalid rounds"))?;
            m.insert("rounds".into(), json!(r as u64));
        }
        if let Some(v) = so.get("readOnly") {
            m.insert("readOnly".into(), json!(v.as_bool().ok_or_else(|| anyhow!("expected boolean"))?));
        }
        for (key, allowed) in [("mode", &["ask", "auto", "plan", "readonly"][..]), ("effort", &["low", "medium", "high", "xhigh", "max"][..])] {
            if let Some(v) = so.get(key) {
                let s = v.as_str().filter(|s| allowed.contains(s)).ok_or_else(|| anyhow!("invalid {key}"))?;
                m.insert(key.into(), json!(s));
            }
        }
        if let Some(v) = so.get("continueOnError") {
            m.insert("continueOnError".into(), json!(v.as_bool().ok_or_else(|| anyhow!("expected boolean"))?));
        }
        if let Some(v) = so.get("message") {
            m.insert("message".into(), json!(v.as_str().ok_or_else(|| anyhow!("expected string"))?));
        }
        out.push(m);
    }
    Ok((name, description, inputs, out))
}

const MAX_WORKFLOW_BYTES: u64 = 256 * 1024;

fn read_contained_workflow_file(full: &Path, root_canon: &Path) -> Option<String> {
    let lmeta = std::fs::symlink_metadata(full).ok()?;
    if lmeta.file_type().is_symlink() || !lmeta.file_type().is_file() || lmeta.len() > MAX_WORKFLOW_BYTES {
        return None;
    }
    let full_canon = std::fs::canonicalize(full).ok()?;
    if !full_canon.starts_with(root_canon) {
        return None;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(full).ok()?;
    let fmeta = file.metadata().ok()?;
    if !fmeta.file_type().is_file() || fmeta.len() > MAX_WORKFLOW_BYTES {
        return None;
    }
    use std::io::Read;
    let mut buf = Vec::with_capacity((fmeta.len() as usize).min(MAX_WORKFLOW_BYTES as usize));
    file.take(MAX_WORKFLOW_BYTES + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > MAX_WORKFLOW_BYTES {
        return None;
    }
    String::from_utf8(buf).ok()
}

pub const BUILTIN_WORKFLOW_NAMES: [&str; 4] = ["feature-dev", "bug-hunt", "tdd-cycle", "security-audit"];

fn strip_workflow_scope_prefixes(raw: &str) -> &str {
    let mut cur = raw.trim();
    loop {
        let bytes = cur.as_bytes();
        let advance = if bytes.get(..8).is_some_and(|p| p.eq_ignore_ascii_case(b"builtin:")) {
            Some(8)
        } else if bytes.get(..10).is_some_and(|p| p.eq_ignore_ascii_case(b"workspace:")) {
            Some(10)
        } else if bytes.get(..5).is_some_and(|p| p.eq_ignore_ascii_case(b"user:")) {
            Some(5)
        } else {
            None
        };
        match advance {
            Some(n) => cur = cur[n..].trim_start(),
            None => return cur,
        }
    }
}

pub fn is_builtin_workflow_name(name: &str) -> bool {
    let bare = strip_workflow_scope_prefixes(name);
    BUILTIN_WORKFLOW_NAMES.iter().any(|b| b.eq_ignore_ascii_case(bare))
}

pub fn discover_workflows(cwd: &Path) -> Vec<WorkflowDefinition> {
    let workspace_canon = std::fs::canonicalize(cwd).ok();
    let cfg_dir = config_dir();
    let user_workflows_dir = cfg_dir.join("workflows");
    let roots: [(PathBuf, PathBuf, Option<PathBuf>, &'static str); 3] = [
        (cwd.join(".goon"), cwd.join(".goon").join("workflows"), workspace_canon.clone(), "workspace"),
        (cwd.join(".agents"), cwd.join(".agents").join("workflows"), workspace_canon, "workspace"),
        (cfg_dir.clone(), user_workflows_dir, std::fs::canonicalize(&cfg_dir).ok(), "user"),
    ];
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for (parent_dir, dir, boundary_canon, scope) in roots {
        let Some(boundary) = boundary_canon else { continue };
        let Ok(pmeta) = std::fs::symlink_metadata(&parent_dir) else { continue };
        if pmeta.file_type().is_symlink() || !pmeta.file_type().is_dir() {
            continue;
        }
        let Ok(lmeta) = std::fs::symlink_metadata(&dir) else { continue };
        if lmeta.file_type().is_symlink() || !lmeta.file_type().is_dir() {
            continue;
        }
        let Ok(dir_canon) = std::fs::canonicalize(&dir) else { continue };
        if !dir_canon.starts_with(&boundary) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by(|a, b| crate::jsutil::locale_cmp(&a.file_name().to_string_lossy(), &b.file_name().to_string_lossy()));
        for entry in entries {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if !(file_name.ends_with(".json") || file_name.ends_with(".md")) {
                continue;
            }
            let full = dir.join(&file_name);
            let Some(raw) = read_contained_workflow_file(&full, &dir_canon) else { continue };
            let mut def = if file_name.ends_with(".json") {
                let Ok(value) = serde_json::from_str::<Value>(&raw) else { continue };
                let Ok((name, description, inputs, steps)) = parse_workflow_json(&value) else { continue };
                let count = steps.len();
                WorkflowDefinition {
                    name: name.unwrap_or_else(|| file_name[..file_name.len() - 5].to_string()),
                    description: description.unwrap_or_else(|| format!("Workflow ({count} steps)")),
                    scope,
                    path: Some(full.clone()),
                    inputs,
                    steps: steps
                        .into_iter()
                        .enumerate()
                        .map(|(i, mut s)| {
                            if !s.contains_key("id") {
                                s.insert("id".into(), json!(format!("step_{}", i + 1)));
                            }
                            WorkflowStep(s)
                        })
                        .collect(),
                }
            } else {
                parse_markdown_workflow(&file_name, &raw, scope, &full)
            };
            let mut bare = strip_workflow_scope_prefixes(&def.name).to_string();
            if bare.is_empty() {
                let stem = file_name.strip_suffix(".json").or_else(|| file_name.strip_suffix(".md")).unwrap_or(&file_name);
                bare = strip_workflow_scope_prefixes(stem).to_string();
            }
            if bare.is_empty() {
                continue;
            }
            def.name = if BUILTIN_WORKFLOW_NAMES.iter().any(|b| b.eq_ignore_ascii_case(&bare)) { format!("{scope}:{}", bare.to_ascii_lowercase()) } else { bare };
            if seen.insert(def.name.to_lowercase()) {
                out.push(def);
            }
        }
    }
    for b in builtin_workflows() {
        if seen.insert(b.name.to_lowercase()) {
            out.push(b);
        }
    }
    out
}

pub fn resolve_workflow(cwd: &Path, spec: &str) -> Option<WorkflowDefinition> {
    let query = spec.trim();
    if query.is_empty() {
        return None;
    }
    let all = discover_workflows(cwd);
    let bytes = query.as_bytes();
    if bytes.get(..8).is_some_and(|p| p.eq_ignore_ascii_case(b"builtin:")) {
        let target = query[8..].trim();
        return all.into_iter().find(|w| w.scope == "builtin" && w.name.eq_ignore_ascii_case(target));
    }
    if bytes.get(..10).is_some_and(|p| p.eq_ignore_ascii_case(b"workspace:")) {
        let target = query[10..].trim();
        let qualified = format!("workspace:{target}");
        return all.into_iter().find(|w| w.scope == "workspace" && (w.name.eq_ignore_ascii_case(&qualified) || w.name.eq_ignore_ascii_case(target)));
    }
    if bytes.get(..5).is_some_and(|p| p.eq_ignore_ascii_case(b"user:")) {
        let target = query[5..].trim();
        let qualified = format!("user:{target}");
        return all.into_iter().find(|w| w.scope == "user" && (w.name.eq_ignore_ascii_case(&qualified) || w.name.eq_ignore_ascii_case(target)));
    }
    if BUILTIN_WORKFLOW_NAMES.iter().any(|b| b.eq_ignore_ascii_case(query)) {
        return all.into_iter().find(|w| w.scope == "builtin" && w.name.eq_ignore_ascii_case(query));
    }
    all.into_iter().find(|w| w.name.to_lowercase() == query.to_lowercase())
}

pub fn create_workflow_scaffold(cwd: &Path, name: &str, description: Option<&str>, global: bool, overwrite: bool) -> Result<(PathBuf, WorkflowDefinition)> {
    static BAD: OnceLock<Regex> = OnceLock::new();
    let stripped = strip_workflow_scope_prefixes(name);
    if is_builtin_workflow_name(name) {
        bail!("Workflow name \"{stripped}\" is reserved for a built-in workflow.");
    }
    let clean = BAD.get_or_init(|| Regex::new(r"[^A-Za-z0-9_-]").unwrap()).replace_all(stripped, "-").trim_matches('-').to_string();
    if clean.is_empty() {
        bail!("Workflow name cannot be empty.");
    }
    if is_builtin_workflow_name(&clean) {
        bail!("Workflow name \"{clean}\" is reserved for a built-in workflow.");
    }
    let dir = if global {
        let cfg = config_dir();
        if let Ok(m) = std::fs::symlink_metadata(&cfg) {
            if m.file_type().is_symlink() || !m.file_type().is_dir() {
                bail!("Refusing to write workflow under symlink or non-directory config path: {}", cfg.display());
            }
        }
        let wf_dir = cfg.join("workflows");
        if let Ok(m) = std::fs::symlink_metadata(&wf_dir) {
            if m.file_type().is_symlink() || !m.file_type().is_dir() {
                bail!("Refusing to write workflow under symlink or non-directory path: {}", wf_dir.display());
            }
        }
        std::fs::create_dir_all(&wf_dir)?;
        let cfg_canon = std::fs::canonicalize(&cfg)?;
        let dir_canon = std::fs::canonicalize(&wf_dir)?;
        if !dir_canon.starts_with(&cfg_canon) {
            bail!("Workflow directory escapes config boundary: {}", wf_dir.display());
        }
        wf_dir
    } else {
        let ws = if cwd.is_absolute() { cwd.to_path_buf() } else { crate::files::resolve_lexical(&std::env::current_dir()?, &cwd.to_string_lossy()) };
        let ws_canon = std::fs::canonicalize(&ws).map_err(|e| anyhow!("Cannot resolve workspace directory {}: {e}", ws.display()))?;
        let dot_goon = ws.join(".goon");
        if let Ok(m) = std::fs::symlink_metadata(&dot_goon) {
            if m.file_type().is_symlink() || !m.file_type().is_dir() {
                bail!("Refusing to write workflow under symlink or non-directory .goon path: {}", dot_goon.display());
            }
        }
        let wf_dir = dot_goon.join("workflows");
        if let Ok(m) = std::fs::symlink_metadata(&wf_dir) {
            if m.file_type().is_symlink() || !m.file_type().is_dir() {
                bail!("Refusing to write workflow under symlink or non-directory workflows path: {}", wf_dir.display());
            }
        }
        std::fs::create_dir_all(&wf_dir)?;
        if std::fs::symlink_metadata(&dot_goon)?.file_type().is_symlink() || std::fs::symlink_metadata(&wf_dir)?.file_type().is_symlink() {
            bail!("Refusing to write workflow through symlinked directory: {}", wf_dir.display());
        }
        let dir_canon = std::fs::canonicalize(&wf_dir)?;
        if !dir_canon.starts_with(&ws_canon) {
            bail!("Workflow directory escapes workspace boundary: {}", wf_dir.display());
        }
        wf_dir
    };
    let file = dir.join(format!("{clean}.json"));
    let md = dir.join(format!("{clean}.md"));
    for existing in [&file, &md] {
        if let Ok(meta) = std::fs::symlink_metadata(existing) {
            if meta.file_type().is_symlink() {
                bail!("Refusing to overwrite symlinked workflow file: {}", existing.display());
            }
            if !overwrite {
                bail!("Workflow \"{clean}\" already exists at {}. Remove it first with \"goon workflow rm {clean}\".", existing.display());
            }
        }
    }
    let description = description.map(str::trim).filter(|d| !d.is_empty()).map(str::to_string).unwrap_or_else(|| format!("Custom multi-step workflow: {clean}"));
    let mut inputs = IndexMap::new();
    inputs.insert("focus".to_string(), "quality and tests".to_string());
    let steps = vec![
        step(
            json!({ "id": "brainstorm", "title": "Brainstorm Ideas with Agent Network", "type": "network", "topic": "Brainstorm approach for: $input (Focus: $focus)", "roles": ["architect", "critic", "pragmatist"], "rounds": 1 }),
        ),
        step(json!({ "id": "execute", "title": "Implement Solution", "type": "prompt", "prompt": "Execute task \"$input\" following this plan:\n\n$prev" })),
    ];
    let def = WorkflowDefinition {
        name: clean.clone(),
        description: description.clone(),
        scope: if global { "user" } else { "workspace" },
        path: Some(file.clone()),
        inputs: Some(inputs.clone()),
        steps: steps.clone(),
    };
    atomic_json(&file, &json!({ "name": clean, "description": description, "inputs": inputs, "steps": steps.iter().map(|s| Value::Object(s.0.clone())).collect::<Vec<_>>() }), false)?;
    Ok((file, def))
}

pub fn remove_custom_workflow(cwd: &Path, name: &str) -> Result<PathBuf> {
    let Some(found) = resolve_workflow(cwd, name) else { bail!("Workflow \"{name}\" not found.") };
    match (&found.path, found.scope) {
        (Some(p), s) if s != "builtin" => {
            let meta = std::fs::symlink_metadata(p)?;
            if meta.file_type().is_symlink() || !meta.file_type().is_file() {
                bail!("Refusing to delete non-regular workflow file: {}", p.display());
            }
            std::fs::remove_file(p)?;
            Ok(p.clone())
        }
        _ => bail!("Workflow \"{}\" is a built-in workflow and cannot be deleted.", found.name),
    }
}

pub type StepFuture = BoxFuture<'static, Result<String>>;

#[derive(Clone, Default)]
pub struct WorkflowHooks {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    pub effective_read_only: bool,
    pub policy: Option<Arc<ExecutionPolicy>>,
    pub on_progress: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    pub approve_shell: Option<Arc<dyn Fn(String, CancellationToken, RiskClass) -> BoxFuture<'static, bool> + Send + Sync>>,
    pub run_prompt: Option<Arc<dyn Fn(String, WorkflowStep, CancellationToken) -> StepFuture + Send + Sync>>,
    pub run_subagents: Option<Arc<dyn Fn(Vec<String>, bool, CancellationToken) -> StepFuture + Send + Sync>>,
    pub run_network: Option<Arc<dyn Fn(String, Option<Vec<String>>, Option<u32>, CancellationToken) -> StepFuture + Send + Sync>>,
    pub run_dag: Option<Arc<dyn Fn(String, CancellationToken) -> StepFuture + Send + Sync>>,
    pub multi_agent_enabled: bool,
    pub on_event: Option<Arc<dyn Fn(Value) + Send + Sync>>,
    pub confirm_step: Option<Arc<dyn Fn(String, CancellationToken) -> BoxFuture<'static, bool> + Send + Sync>>,
}

#[derive(Clone, Debug)]
pub struct WorkflowStepResult {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: &'static str,
    pub output: String,
    pub duration_ms: i64,
}

#[derive(Clone, Debug)]
pub struct WorkflowRunResult {
    pub workflow: String,
    pub status: &'static str,
    pub steps: Vec<WorkflowStepResult>,
    pub final_output: String,
}

impl WorkflowRunResult {
    pub fn to_value(&self) -> Value {
        json!({
            "workflow": self.workflow,
            "status": self.status,
            "steps": self.steps.iter().map(|s| json!({ "id": s.id, "title": s.title, "type": s.kind, "status": s.status, "output": s.output, "durationMs": s.duration_ms })).collect::<Vec<_>>(),
            "finalOutput": self.final_output,
        })
    }
}

async fn run_prompt(hooks: &WorkflowHooks, prompt: String, step: &WorkflowStep) -> Result<String> {
    let f = hooks.run_prompt.as_ref().ok_or_else(|| anyhow!("Workflow prompt steps need a runner."))?;
    f(prompt, step.clone(), hooks.cancel.clone()).await
}

pub async fn execute_workflow(workflow: &WorkflowDefinition, raw_args: &str, hooks: &WorkflowHooks) -> Result<WorkflowRunResult> {
    let parsed = parse_workflow_args(raw_args, workflow.inputs.as_ref());
    let mut outputs: IndexMap<String, String> = IndexMap::new();
    let mut results = vec![];
    let mut prev = String::new();
    let progress = |t: &str| {
        if let Some(p) = &hooks.on_progress {
            p(t);
        }
    };
    let count = workflow.steps.len();
    progress(&format!(
        "\n╭─── Workflow: {} ({count} step{}) ───╮\n  Description : {}\n{}",
        workflow.name,
        if count > 1 { "s" } else { "" },
        workflow.description,
        if parsed.full_input.is_empty() { String::new() } else { format!("  Input       : {}\n", parsed.full_input) }
    ));
    for (i, step) in workflow.steps.iter().enumerate() {
        if hooks.cancel.is_cancelled() {
            return Err(crate::harness::Aborted::default().into());
        }
        let title = step.title().map(str::to_string).unwrap_or_else(|| format!("{} ({})", step.kind(), step.id()));
        let started = now_millis();
        let ctx = InterpContext { input: &parsed.full_input, positional: &parsed.positional, named: &parsed.named, prev_output: &prev, step_outputs: &outputs };
        let interp = |t: Option<&str>, shell: bool| interpolate_workflow_template(t.unwrap_or(""), &ctx, shell);
        progress(&format!("\n◆ Step [{}/{count}] {title} [{}]\n", i + 1, step.kind()));
        let outcome: Result<Option<String>> = async {
            match step.kind() {
                "approval" => {
                    let default = format!("Approve workflow step \"{title}\"?");
                    let msg = interp(Some(step.str("message").or(step.str("prompt")).unwrap_or(&default)), false);
                    let ok = match &hooks.confirm_step {
                        Some(c) => c(msg, hooks.cancel.clone()).await,
                        None => true,
                    };
                    if !ok {
                        return Ok(None);
                    }
                    Ok(Some("Approved.".into()))
                }
                "shell" => {
                    let mode = step.str("mode");
                    if hooks.effective_read_only || step.bool("readOnly") == Some(true) || mode == Some("plan") || mode == Some("readonly") {
                        bail!("Shell step \"{}\" is blocked in read-only or plan mode.", step.id());
                    }
                    let raw = interp(Some(step.str("command").or(step.str("prompt")).unwrap_or("")), true);
                    if raw.trim().is_empty() {
                        bail!("Shell step \"{}\" has no command.", step.id());
                    }
                    let cmd = sanitize_bash_command(&raw);
                    let risk = classify_bash_risk(&cmd);
                    let unrestricted = hooks.policy.as_ref().map(|p| p.is_unrestricted()).unwrap_or(false);
                    if risk == RiskClass::Critical && !unrestricted {
                        return Err(WorkflowApprovalError(format!("Permission denied: workflow shell step \"{}\" is critical risk and cannot run.", step.id())).into());
                    }
                    if !unrestricted {
                        if let Some(p) = &hooks.policy {
                            if !is_network_allowed_for_command(p, &cmd) {
                                return Err(WorkflowApprovalError(network_denied_message(&cmd)).into());
                            }
                        }
                    }
                    let Some(approve) = &hooks.approve_shell else {
                        return Err(WorkflowApprovalError(format!("Workflow shell step \"{}\" requires explicit approval, but no approval handler is available.", step.id())).into());
                    };
                    if !approve(cmd.clone(), hooks.cancel.clone(), risk).await {
                        return Err(WorkflowApprovalError(format!("Permission denied by the user for shell command: {cmd}")).into());
                    }
                    progress(&format!("  $ {cmd}\n"));
                    let clean_env = |v: &str| v.replace('\0', "");
                    let mut env = crate::process::child_env(None);
                    env.retain(|k, _| !k.trim().to_ascii_uppercase().starts_with("GOON_WF_"));
                    env.insert("GOON_WF_INPUT".into(), clean_env(&parsed.full_input));
                    env.insert("GOON_WF_PREV".into(), clean_env(&prev));
                    for (idx, p) in parsed.positional.iter().enumerate() {
                        env.insert(format!("GOON_WF_ARG_{}", idx + 1), clean_env(p));
                    }
                    for (k, v) in &parsed.named {
                        let key: String = k.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect::<String>().to_uppercase();
                        env.insert(format!("GOON_WF_VAR_{key}"), clean_env(v));
                    }
                    let limits = hooks.policy.as_ref().map(|p| p.limits.clone());
                    let timeout = limits
                        .as_ref()
                        .map(|l| l.default_tool_timeout_ms)
                        .unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS)
                        .min(limits.as_ref().map(|l| l.max_tool_timeout_ms).unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS))
                        .max(1000);
                    let res = run_command(
                        "bash",
                        &["-c".into(), cmd],
                        RunOptions {
                            cwd: &hooks.cwd,
                            cancel: &hooks.cancel,
                            timeout_ms: timeout,
                            env: Some(env),
                            output_limit_bytes: limits.as_ref().map(|l| l.max_output_bytes),
                            hard_output_bytes: limits.as_ref().map(|l| l.hard_output_bytes),
                        },
                    )
                    .await
                    .map_err(|e| match e {
                        crate::process::CommandError::Cancelled => crate::harness::Aborted::default().into(),
                        other => anyhow!(other.to_string()),
                    })?;
                    let out = limit_text(format!("Exit code: {}\n{}", res.code, res.output).trim(), 12_000);
                    if res.code != 0 && step.bool("continueOnError") != Some(true) {
                        bail!("Command exited with code {}:\n{}", res.code, take_chars(&res.output, 1000));
                    }
                    Ok(Some(out))
                }
                "network" => {
                    let topic = interp(Some(step.str("topic").or(step.str("prompt")).unwrap_or("$input")), false);
                    match &hooks.run_network {
                        None => {
                            let roles = step.list("roles").unwrap_or_else(|| vec!["architect".into(), "critic".into()]);
                            Ok(Some(run_prompt(hooks, format!("Brainstorm from multiple perspectives ({}):\n\n{topic}", roles.join(", ")), step).await?))
                        }
                        Some(f) => Ok(Some(f(topic, step.list("roles"), step.rounds(), hooks.cancel.clone()).await?)),
                    }
                }
                "subagent" => {
                    let raw = step.list("tasks").filter(|t| !t.is_empty()).unwrap_or_else(|| vec![step.str("prompt").unwrap_or("$input").to_string()]);
                    let tasks: Vec<String> = raw.iter().map(|t| interp(Some(t), false)).filter(|t| !t.is_empty()).collect();
                    match &hooks.run_subagents {
                        None => Ok(Some(run_prompt(hooks, tasks.join("\n\n"), step).await?)),
                        Some(f) => Ok(Some(f(tasks, step.bool("readOnly") != Some(false), hooks.cancel.clone()).await?)),
                    }
                }
                "dag" => {
                    let goal = interp(Some(step.str("prompt").or(step.str("topic")).or(step.str("command")).unwrap_or("$input")), false).trim().to_string();
                    if goal.is_empty() {
                        bail!("DAG step \"{}\" has no goal.", step.id());
                    }
                    let c = classify_workflow_complexity(&goal, 0);
                    if let Some(e) = &hooks.on_event {
                        e(json!({ "type": "workflow_classified", "mode": c.mode, "reason": c.reason }));
                    }
                    match &hooks.run_dag {
                        Some(f) if c.mode == "multi" && hooks.multi_agent_enabled => Ok(Some(f(goal, hooks.cancel.clone()).await?)),
                        _ => Ok(Some(run_prompt(hooks, goal, step).await?)),
                    }
                }
                _ => {
                    let prompt = interp(Some(step.str("prompt").unwrap_or("$input")), false);
                    Ok(Some(run_prompt(hooks, prompt, step).await?))
                }
            }
        }
        .await;
        let duration = now_millis() - started;
        match outcome {
            Ok(None) => {
                results.push(WorkflowStepResult {
                    id: step.id().into(),
                    title,
                    kind: step.kind().into(),
                    status: "skipped",
                    output: "Aborted at approval gate by user.".into(),
                    duration_ms: duration,
                });
                progress(&format!("╰─── Workflow \"{}\" stopped at approval gate ───╯\n", workflow.name));
                return Ok(WorkflowRunResult { workflow: workflow.name.clone(), status: "aborted", steps: results, final_output: prev });
            }
            Ok(Some(out)) => {
                prev = out.clone();
                outputs.insert(step.id().into(), out.clone());
                results.push(WorkflowStepResult { id: step.id().into(), title, kind: step.kind().into(), status: "completed", output: out, duration_ms: duration });
            }
            Err(error) => {
                if hooks.cancel.is_cancelled() || crate::harness::is_aborted(&error) {
                    return Err(crate::harness::Aborted::default().into());
                }
                let msg = error.to_string();
                results.push(WorkflowStepResult { id: step.id().into(), title: title.clone(), kind: step.kind().into(), status: "failed", output: msg.clone(), duration_ms: duration });
                if step.bool("continueOnError") != Some(true) {
                    progress(&format!("  ✗ Step \"{title}\" failed: {msg}\n╰─── Workflow \"{}\" failed ───╯\n", workflow.name));
                    return Ok(WorkflowRunResult { workflow: workflow.name.clone(), status: "failed", steps: results, final_output: msg });
                }
                prev = msg.clone();
                outputs.insert(step.id().into(), msg);
            }
        }
    }
    progress(&format!("\n╰─── Workflow \"{}\" completed ({} steps) ───╯\n", workflow.name, results.len()));
    Ok(WorkflowRunResult { workflow: workflow.name.clone(), status: "completed", steps: results, final_output: prev })
}

pub fn format_workflow_details(wf: &WorkflowDefinition) -> String {
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut lines = vec![format!("Workflow: {} [{}]{}", wf.name, wf.scope, wf.path.as_ref().map(|p| format!(" ({})", p.display())).unwrap_or_default()), format!("Description: {}", wf.description)];
    if let Some(i) = wf.inputs.as_ref().filter(|i| !i.is_empty()) {
        lines.push(format!("Default inputs: {}", i.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect::<Vec<_>>().join(", ")));
    }
    lines.push(format!("Steps ({}):", wf.steps.len()));
    for (i, s) in wf.steps.iter().enumerate() {
        let detail = match s.kind() {
            "shell" => format!("$ {}", s.str("command").unwrap_or("")),
            "network" => format!(
                "roles=[{}] rounds={} topic=\"{}\"",
                s.list("roles").unwrap_or_else(|| ["architect", "researcher", "critic", "pragmatist"].iter().map(|x| x.to_string()).collect()).join(", "),
                s.rounds().unwrap_or(2),
                take_chars(s.str("topic").unwrap_or(""), 80)
            ),
            "subagent" => format!("{} subagent task(s)", s.list("tasks").map(|t| t.len()).unwrap_or(1)),
            "dag" => format!("dag goal=\"{}\"", take_chars(&collapse(s.str("prompt").or(s.str("topic")).unwrap_or("")), 80)),
            _ => take_chars(&collapse(s.str("prompt").or(s.str("message")).unwrap_or("")), 90).to_string(),
        };
        lines.push(format!("  {}. [{}] {} ({}) — {detail}", i + 1, s.id(), s.title().unwrap_or(s.id()), s.kind()));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_examples() {
        assert_eq!(classify_workflow_complexity("fix the typo in README.md", 0).mode, "single");
        assert_eq!(classify_workflow_complexity("review the code and fix issues if needed", 0).mode, "single");
        assert_eq!(classify_workflow_complexity("add a parser, then write tests and then update docs", 0).mode, "multi");
        assert_eq!(classify_workflow_complexity("thêm tính năng, sau đó viết test", 0).mode, "multi");
        assert!(classify_workflow_complexity("a và b", 0).reason.contains("1 conjunction(s)"));
    }

    #[test]
    fn classifier_parity_cases() {
        for p in [
            "fix the login bug",
            "add a test for parseUser",
            "refactor src/utils.ts",
            "write docs for the CLI",
            "update package.json",
            "run the tests",
            "review the code and fix issues if needed",
            "plan the feature",
            "chạy kiểm tra hồi quy",
            "",
            "Fix the bug and update the tests",
            "update src/a.ts, src/b.ts, src/c.ts",
        ] {
            let r = classify_workflow_complexity(p, 0);
            assert_eq!(r.mode, "single", "{p}: {}", r.reason);
        }
        assert!(classify_workflow_complexity("Implement auth, fix the login bug and update the docs", 0).reason.contains("3 distinct imperative verbs"));
        for p in [
            "Fix the bug then update the tests",
            "Write the parser, after that update the docs",
            "thêm handler rồi... sau đó sửa test",
            "run the suite, then run lint, then run build",
            "multi-step task: implement the parser and write tests",
            "plan the workflow then implement it",
            "fix and update src/a.ts, src/b.ts, src/c.ts",
        ] {
            let r = classify_workflow_complexity(p, 0);
            assert_eq!(r.mode, "multi", "{p}: {}", r.reason);
        }
        assert_eq!(classify_workflow_complexity("update the handlers", 3).mode, "single");
        assert_eq!(classify_workflow_complexity("fix and update the handlers", 3).mode, "multi");
    }

    #[test]
    fn interpolation() {
        let named: IndexMap<String, String> = [("focus".to_string(), "it's".to_string())].into_iter().collect();
        let outputs = IndexMap::new();
        let positional = vec!["one".to_string()];
        let ctx = InterpContext { input: "do x", positional: &positional, named: &named, prev_output: "p", step_outputs: &outputs };
        assert_eq!(interpolate_workflow_template("$input / $1 / $focus / $prev / $missing", &ctx, false), "do x / one / it's / p / $missing");
        assert_eq!(interpolate_workflow_template("echo $input \"{{focus}}\" '$prev' $1", &ctx, true), "echo 'do x' \"it's\" 'p' $1");
        let args = parse_workflow_args("hello \"big world\" --mode=fast", None);
        assert_eq!(args.positional, vec!["hello", "big world"]);
        assert_eq!(args.named["mode"], "fast");
        assert_eq!(args.full_input, "hello \"big world\"");
    }

    #[test]
    fn markdown_workflow() {
        let md = "---\nname: rel\ndescription: Release\n---\n## Step 1: Build [shell]\n```bash\nnpm run build\n```\n## Review [network roles=architect|critic rounds=1]\nCheck $prev\n";
        let wf = parse_markdown_workflow("rel.md", md, "workspace", Path::new("/x/rel.md"));
        assert_eq!(wf.name, "rel");
        assert_eq!(wf.steps[0].str("command"), Some("npm run build"));
        assert_eq!(wf.steps[1].list("roles"), Some(vec!["architect".to_string(), "critic".to_string()]));
    }
}
