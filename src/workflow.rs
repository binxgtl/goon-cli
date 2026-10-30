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
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio_util::sync::CancellationToken;

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
// Classifier (JS regex semantics: `\b` and `\w` are ASCII-only)
// ---------------------------------------------------------------------------------------------

const IMPERATIVE_VERBS: [&str; 37] = [
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

fn verb_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"(?i)(?-u:\b)(?:{})(?-u:\b)", IMPERATIVE_VERBS.iter().map(|v| regex::escape(v)).collect::<Vec<_>>().join("|"))).unwrap())
}

fn ascii_word(c: Option<char>) -> bool {
    c.map(|c| c.is_ascii_alphanumeric() || c == '_').unwrap_or(false)
}

/// Counts matches of `base` (alternatives listed first) plus `tail` matches not followed by an
/// ASCII word character (JS `(?![a-z0-9_])` under the `i` flag).
fn count_with_tail(text: &str, re: &Regex, tail_group: usize) -> usize {
    re.captures_iter(text)
        .filter(|c| match c.get(tail_group) {
            Some(m) => !ascii_word(text[m.end()..].chars().next()),
            None => true,
        })
        .count()
}

fn sequence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?-u:\b)(?:and then|then|after that|tiếp theo)(?-u:\b)|(?-u:\b)(sau[ -]?(?:đó|do))").unwrap())
}

fn weak_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?-u:\b)and(?-u:\b)|(?-u:\b)(và)").unwrap())
}

fn keyword_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?-u:\b)(?:plan|workflow|multi[- ]?step|multi[- ]?agent|pipeline|roadmap|checklist|phase)(?-u:\b)").unwrap())
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
    let mut kinds = std::collections::HashSet::new();
    let mut hits = 0;
    for m in verb_re().find_iter(prompt) {
        kinds.insert(m.as_str().to_lowercase());
        hits += 1;
    }
    let connectors = count_with_tail(prompt, sequence_re(), 1);
    let weak = count_with_tail(prompt, weak_re(), 1);
    let paths: std::collections::HashSet<String> = path_re().find_iter(prompt).map(|m| m.as_str().to_lowercase()).collect();
    let path_count = paths.len() + file_count;
    let keyword = keyword_re().is_match(prompt);
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

fn shell_quote_context(prefix: &str) -> u8 {
    let chars: Vec<char> = prefix.chars().collect();
    let mut state = 0u8; // 0 none, 1 single, 2 double
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match state {
            0 => {
                if ch == '\\' {
                    i += 1;
                } else if ch == '\'' {
                    state = 1;
                } else if ch == '"' {
                    state = 2;
                }
            }
            1 => {
                if ch == '\'' {
                    state = 0;
                }
            }
            _ => {
                if ch == '\\' {
                    i += 1;
                } else if ch == '"' {
                    state = 0;
                }
            }
        }
        i += 1;
    }
    state
}

pub fn shell_escape(value: &str, context: u8) -> String {
    match context {
        1 => value.replace('\'', "'\\''"),
        2 => {
            let mut out = String::new();
            for c in value.chars() {
                if matches!(c, '\\' | '"' | '$' | '`' | '!') {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        _ => format!("'{}'", value.replace('\'', "'\\''")),
    }
}

pub struct InterpContext<'a> {
    pub input: &'a str,
    pub positional: &'a [String],
    pub named: &'a IndexMap<String, String>,
    pub prev_output: &'a str,
    pub step_outputs: &'a IndexMap<String, String>,
}

/// Replaces matches of `re` (ASCII `\b` already encoded), computing each value from the source.
fn replace_each(src: &str, re: &Regex, mut f: impl FnMut(&regex::Captures, usize, &str) -> String) -> String {
    let mut out = String::new();
    let mut last = 0;
    for c in re.captures_iter(src) {
        let m = c.get(0).unwrap();
        out.push_str(&src[last..m.start()]);
        out.push_str(&f(&c, m.start(), src));
        last = m.end();
    }
    out.push_str(&src[last..]);
    out
}

pub fn interpolate_workflow_template(template: &str, ctx: &InterpContext, shell: bool) -> String {
    static STEPS: OnceLock<Regex> = OnceLock::new();
    static INPUT: OnceLock<Regex> = OnceLock::new();
    static PREV: OnceLock<Regex> = OnceLock::new();
    static NUM_BRACE: OnceLock<Regex> = OnceLock::new();
    static NAME_BRACE: OnceLock<Regex> = OnceLock::new();
    static NUM: OnceLock<Regex> = OnceLock::new();
    static NAME: OnceLock<Regex> = OnceLock::new();
    let steps_re = STEPS.get_or_init(|| Regex::new(r"\{\{\s*steps\.([A-Za-z0-9_-]+)\.output\s*\}\}").unwrap());
    let input_re = INPUT.get_or_init(|| Regex::new(r"\{\{\s*input\s*\}\}|\$input(?-u:\b)").unwrap());
    let prev_re = PREV.get_or_init(|| Regex::new(r"\{\{\s*prev\s*\}\}|\$prev(?-u:\b)").unwrap());
    let num_brace = NUM_BRACE.get_or_init(|| Regex::new(r"\{\{\s*([0-9]+)\s*\}\}").unwrap());
    let name_brace = NAME_BRACE.get_or_init(|| Regex::new(r"\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}").unwrap());
    let num_re = NUM.get_or_init(|| Regex::new(r"\{\{\s*([0-9]+)\s*\}\}|\$([0-9]+)(?-u:\b)").unwrap());
    let name_re = NAME.get_or_init(|| Regex::new(r"\{\{\s*([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}|\$([A-Za-z_][A-Za-z0-9_-]*)(?-u:\b)").unwrap());
    let fmt = |val: &str, offset: usize, src: &str| if shell { shell_escape(val, shell_quote_context(&src[..offset])) } else { val.to_string() };
    let mut out = replace_each(template, steps_re, |c, o, s| fmt(ctx.step_outputs.get(&c[1]).map(String::as_str).unwrap_or(""), o, s));
    out = replace_each(&out, input_re, |_, o, s| fmt(ctx.input, o, s));
    out = replace_each(&out, prev_re, |_, o, s| fmt(ctx.prev_output, o, s));
    if shell {
        out = replace_each(&out, num_brace, |c, o, s| {
            let idx = c[1].parse::<usize>().unwrap_or(0);
            fmt(idx.checked_sub(1).and_then(|i| ctx.positional.get(i)).map(String::as_str).unwrap_or(""), o, s)
        });
        out = replace_each(&out, name_brace, |c, o, s| {
            let key = &c[1];
            if let Some(v) = ctx.named.get(key) {
                return fmt(v, o, s);
            }
            if let Some(v) = ctx.step_outputs.get(key) {
                return fmt(v, o, s);
            }
            c[0].to_string()
        });
        return out;
    }
    out = replace_each(&out, num_re, |c, _, _| {
        let idx = c.get(1).or(c.get(2)).map(|m| m.as_str().parse::<usize>().unwrap_or(0)).unwrap_or(0);
        idx.checked_sub(1).and_then(|i| ctx.positional.get(i)).cloned().unwrap_or_default()
    });
    replace_each(&out, name_re, |c, _, _| {
        let key = c.get(1).or(c.get(2)).map(|m| m.as_str()).unwrap_or("");
        if let Some(v) = ctx.named.get(key) {
            return v.clone();
        }
        if let Some(v) = ctx.step_outputs.get(key) {
            return v.clone();
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

pub fn discover_workflows(cwd: &Path) -> Vec<WorkflowDefinition> {
    let roots: [(PathBuf, &'static str); 3] = [(cwd.join(".goon").join("workflows"), "workspace"), (cwd.join(".agents").join("workflows"), "workspace"), (config_dir().join("workflows"), "user")];
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for (dir, scope) in roots {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by(|a, b| crate::jsutil::locale_cmp(&a.file_name().to_string_lossy(), &b.file_name().to_string_lossy()));
        for entry in entries {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) || !(file_name.ends_with(".json") || file_name.ends_with(".md")) {
                continue;
            }
            let full = dir.join(&file_name);
            let Ok(meta) = std::fs::metadata(&full) else { continue };
            if meta.len() > 256 * 1024 {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&full) else { continue };
            let def = if file_name.ends_with(".json") {
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

pub fn create_workflow_scaffold(cwd: &Path, name: &str, description: Option<&str>, global: bool, overwrite: bool) -> Result<(PathBuf, WorkflowDefinition)> {
    static BAD: OnceLock<Regex> = OnceLock::new();
    let clean = BAD.get_or_init(|| Regex::new(r"[^A-Za-z0-9_-]").unwrap()).replace_all(name.trim(), "-").trim_matches('-').to_string();
    if clean.is_empty() {
        bail!("Workflow name cannot be empty.");
    }
    let dir = if global { config_dir().join("workflows") } else { crate::files::resolve_lexical(&std::env::current_dir()?, &cwd.to_string_lossy()).join(".goon").join("workflows") };
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{clean}.json"));
    let md = dir.join(format!("{clean}.md"));
    if !overwrite {
        for existing in [&file, &md] {
            if existing.exists() {
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
    let all = discover_workflows(cwd);
    let Some(found) = all.into_iter().find(|w| w.name.to_lowercase() == name.trim().to_lowercase()) else { bail!("Workflow \"{name}\" not found.") };
    match (&found.path, found.scope) {
        (Some(p), s) if s != "builtin" => {
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
                    if risk == RiskClass::Critical {
                        return Err(WorkflowApprovalError(format!("Permission denied: workflow shell step \"{}\" is critical risk and cannot run.", step.id())).into());
                    }
                    if let Some(p) = &hooks.policy {
                        if !is_network_allowed_for_command(p, &cmd) {
                            return Err(WorkflowApprovalError(network_denied_message(&cmd)).into());
                        }
                    }
                    let Some(approve) = &hooks.approve_shell else {
                        return Err(WorkflowApprovalError(format!("Workflow shell step \"{}\" requires explicit approval, but no approval handler is available.", step.id())).into());
                    };
                    if !approve(cmd.clone(), hooks.cancel.clone(), risk).await {
                        return Err(WorkflowApprovalError(format!("Permission denied by the user for shell command: {cmd}")).into());
                    }
                    progress(&format!("  $ {cmd}\n"));
                    let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
                    env.insert("GOON_WF_INPUT".into(), parsed.full_input.clone());
                    env.insert("GOON_WF_PREV".into(), prev.clone());
                    for (idx, p) in parsed.positional.iter().enumerate() {
                        env.insert(format!("GOON_WF_ARG_{}", idx + 1), p.clone());
                    }
                    for (k, v) in &parsed.named {
                        let key: String = k.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect::<String>().to_uppercase();
                        env.insert(format!("GOON_WF_VAR_{key}"), v.clone());
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
                    .map_err(|e| anyhow!(e.to_string()))?;
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
