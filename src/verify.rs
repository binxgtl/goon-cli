//! Verification engine: planning + execution. Never emits events or performs network access by
//! itself; callers handle approval and events.
//!
//! - Commands resolve locally (package scripts or `node_modules` binaries); bare `npx` is never used.
//! - `run_verification` is fail-closed: a plan with commands must be explicitly approved.
//! - A timeout, a missing module and a missing fixture are failures; only a missing tool skips.

use crate::config::{VerifyConfig, VerifyPolicy};
use crate::jsutil;
use crate::policy::{is_network_allowed_for_command, ExecutionPolicy};
use crate::process::{run_command, CommandError, RunOptions};
use crate::risk::{classify_bash_risk, RiskClass};
use crate::syntax::validate_syntax;
use futures_util::future::BoxFuture;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Pass,
    Fail,
    Warn,
    Skip,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Pass => "pass",
            Severity::Fail => "fail",
            Severity::Warn => "warn",
            Severity::Skip => "skip",
        }
    }
}

pub const STAGE_ORDER: [&str; 6] = ["syntax", "format", "lint", "typecheck", "build", "test"];

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyIssue {
    pub stage: String,
    pub path: String,
    pub line: Option<u64>,
    pub column: Option<u64>,
    pub message: String,
}

impl VerifyIssue {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("stage".into(), json!(self.stage));
        m.insert("path".into(), json!(self.path));
        if let Some(line) = self.line {
            m.insert("line".into(), json!(line));
        }
        if let Some(column) = self.column {
            m.insert("column".into(), json!(column));
        }
        m.insert("message".into(), json!(self.message));
        Value::Object(m)
    }
}

#[derive(Clone, Debug)]
pub struct StageResult {
    pub stage: String,
    pub severity: Severity,
    pub duration_ms: u64,
    pub issues: Vec<VerifyIssue>,
    pub output: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct VerifyVerdict {
    pub pass: bool,
    pub severity: Severity,
    pub stages: Vec<StageResult>,
    pub errors: Vec<VerifyIssue>,
    pub warnings: Vec<VerifyIssue>,
    pub reason: String,
}

impl VerifyVerdict {
    pub fn skipped(reason: &str) -> Self {
        VerifyVerdict { pass: true, severity: Severity::Skip, stages: vec![], errors: vec![], warnings: vec![], reason: reason.into() }
    }

    pub fn failed_stage(&self) -> Option<&str> {
        self.stages.iter().find(|s| s.severity == Severity::Fail).map(|s| s.stage.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyCommand {
    pub stage: String,
    pub command: String,
    pub args: Vec<String>,
    pub display: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyProject {
    pub kind: &'static str,
    pub root: PathBuf,
    pub files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct VerifyPlan {
    pub stages: Vec<String>,
    pub commands: Vec<VerifyCommand>,
    pub projects: Vec<VerifyProject>,
    pub approved: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RunnerResult {
    pub code: i32,
    pub output: String,
    pub timed_out: bool,
    pub not_found: bool,
}

pub struct RunnerOptions {
    pub cwd: PathBuf,
    pub timeout_ms: u64,
    pub cancel: CancellationToken,
    pub env: Option<BTreeMap<String, String>>,
}

pub trait VerifyRunner: Send + Sync {
    fn run(&self, command: String, args: Vec<String>, opts: RunnerOptions) -> BoxFuture<'static, RunnerResult>;
}

#[derive(Debug)]
pub struct VerificationFailed(pub VerifyVerdict);

impl std::fmt::Display for VerificationFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.reason.is_empty() { "Verification failed." } else { &self.0.reason })
    }
}

impl std::error::Error for VerificationFailed {}

#[derive(Debug)]
pub struct VerificationApprovalError;

impl std::fmt::Display for VerificationApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Verification commands require approval before running.")
    }
}

impl std::error::Error for VerificationApprovalError {}

const MAX_ISSUE_CHARS: usize = 8_000;
const MAX_ISSUES: usize = 20;
const MAX_TOTAL_ISSUE_CHARS: usize = 16 * 1024;

fn missing_tool_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:command not found|is not recognized as an internal or external command|npx canceled due to missing packages|:\s*\d+:\s*[^\n]+:\s*not found)").unwrap()
    })
}

pub fn clamp_verify_timeout(timeout_ms: u64) -> u64 {
    timeout_ms.clamp(1_000, 600_000)
}

fn extension_of(path: &str) -> String {
    let base = path.replace('\\', "/").rsplit('/').next().unwrap_or("").to_string();
    match base.rfind('.') {
        Some(dot) if dot > 0 => base[dot..].to_lowercase(),
        _ => String::new(),
    }
}

fn normalize_relative(path: &str) -> Option<String> {
    let posix = path.replace('\\', "/");
    if posix.is_empty() || posix == "." {
        return None;
    }
    if posix.starts_with('/') || Regex::new(r"^[A-Za-z]:/").unwrap().is_match(&posix) || Path::new(path).is_absolute() {
        return None;
    }
    let mut normalized = posix.as_str();
    while let Some(rest) = normalized.strip_prefix("./") {
        normalized = rest;
    }
    if normalized.is_empty() || normalized == ".." || normalized.starts_with("../") {
        return None;
    }
    Some(normalized.to_string())
}

fn normalized_list(files: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for file in files {
        if let Some(rel) = normalize_relative(file) {
            if !out.contains(&rel) {
                out.push(rel);
            }
        }
    }
    out
}

fn kind_extensions(kind: &str) -> &'static [&'static str] {
    match kind {
        "node" => &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"],
        "python" => &[".py", ".pyi"],
        "rust" => &[".rs"],
        "go" => &[".go"],
        _ => &[],
    }
}

/// Detects project kinds from manifests in `cwd` only (never climbing to a parent).
pub fn detect_projects(cwd: &Path, changed_files: &[String]) -> Vec<VerifyProject> {
    let root = cwd.to_path_buf();
    let mut kinds: Vec<&'static str> = vec![];
    let exists = |name: &str| root.join(name).exists();
    if exists("package.json") || exists("tsconfig.json") || exists("jsconfig.json") {
        kinds.push("node");
    }
    if exists("pyproject.toml") || exists("setup.cfg") || exists("requirements.txt") {
        kinds.push("python");
    }
    if exists("Cargo.toml") {
        kinds.push("rust");
    }
    if exists("go.mod") {
        kinds.push("go");
    }
    let files = normalized_list(changed_files);
    if kinds.is_empty() {
        return vec![VerifyProject { kind: "unknown", root, files }];
    }
    kinds
        .into_iter()
        .map(|kind| VerifyProject { kind, root: root.clone(), files: files.iter().filter(|f| kind_extensions(kind).contains(&extension_of(f).as_str())).cloned().collect() })
        .collect()
}

/// Existing workspace-relative files belonging to the project, deduped and sorted.
pub fn scope_files(changed_files: &[String], root: &Path) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for file in changed_files {
        let Some(rel) = normalize_relative(file) else { continue };
        if rel.split('/').any(|s| ["node_modules", ".git", "dist"].contains(&s)) {
            continue;
        }
        if !root.join(&rel).exists() {
            continue;
        }
        if !out.contains(&rel) {
            out.push(rel);
        }
    }
    out.sort();
    out
}

fn node_executable() -> String {
    crate::process::which("node").map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| "node".into())
}

/// A tool shipped in `node_modules`, run with Node so a spawn never depends on PATH shims.
pub fn resolve_local_tool(root: &Path, tool: &str) -> Option<(String, Vec<String>)> {
    let candidates: &[&str] = match tool {
        "tsc" => &["node_modules/typescript/bin/tsc"],
        "eslint" => &["node_modules/eslint/bin/eslint.js"],
        "prettier" => &["node_modules/prettier/bin/prettier.cjs", "node_modules/prettier/bin-prettier.js"],
        "oxlint" => &["node_modules/oxlint/bin/oxlint"],
        _ => &[],
    };
    for rel in candidates {
        let file = root.join(rel);
        if file.exists() {
            return Some((node_executable(), vec![file.to_string_lossy().to_string()]));
        }
    }
    None
}

fn npm_invocation(root: &Path, args: &[&str]) -> (String, Vec<String>) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    if let Ok(execpath) = std::env::var("npm_execpath") {
        if Path::new(&execpath).exists() {
            return (node_executable(), std::iter::once(execpath).chain(args).collect());
        }
    }
    let local = root.join("node_modules/npm/bin/npm-cli.js");
    if local.exists() {
        return (node_executable(), std::iter::once(local.to_string_lossy().to_string()).chain(args).collect());
    }
    if cfg!(windows) {
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into());
        return (comspec, ["/d", "/s", "/c", "npm"].iter().map(|s| s.to_string()).chain(args).collect());
    }
    ("npm".into(), args)
}

fn package_script(root: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(root.join("package.json")).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let value = parsed.get("scripts")?.get(name)?.as_str()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn npm_command(root: &Path, stage: &str, script: &str) -> VerifyCommand {
    let (command, args) = npm_invocation(root, &["run", script]);
    VerifyCommand { stage: stage.into(), command, args, display: format!("npm run {script}") }
}

fn local_command(tool: (String, Vec<String>), logical: &str, args: &[String], stage: &str) -> VerifyCommand {
    let mut all = tool.1;
    all.extend(args.iter().cloned());
    let display = std::iter::once(logical.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
    VerifyCommand { stage: stage.into(), command: tool.0, args: all, display }
}

fn bare_command(command: &str, args: &[String], stage: &str) -> VerifyCommand {
    let display = std::iter::once(command.to_string()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
    VerifyCommand { stage: stage.into(), command: command.into(), args: args.to_vec(), display }
}

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|x| x.to_string()).collect()
}

fn node_commands(root: &Path, files: &[String], policy: &VerifyPolicy) -> Vec<VerifyCommand> {
    let mut out = vec![];
    if policy.require_lint {
        if package_script(root, "format:check").is_some() {
            out.push(npm_command(root, "format", "format:check"));
        } else if let Some(prettier) = resolve_local_tool(root, "prettier") {
            if !files.is_empty() {
                let mut args = s(&["--check"]);
                args.extend(files.iter().cloned());
                out.push(local_command(prettier, "prettier", &args, "format"));
            }
        }
        if package_script(root, "lint").is_some() {
            out.push(npm_command(root, "lint", "lint"));
        } else if let (Some(eslint), false) = (resolve_local_tool(root, "eslint"), files.is_empty()) {
            out.push(local_command(eslint, "eslint", files, "lint"));
        } else if let (Some(oxlint), false) = (resolve_local_tool(root, "oxlint"), files.is_empty()) {
            out.push(local_command(oxlint, "oxlint", files, "lint"));
        }
    }
    if policy.require_typecheck {
        let tsconfig = if root.join("tsconfig.json").exists() {
            Some("tsconfig.json")
        } else if root.join("jsconfig.json").exists() {
            Some("jsconfig.json")
        } else {
            None
        };
        if package_script(root, "typecheck").is_some() {
            out.push(npm_command(root, "typecheck", "typecheck"));
        } else if let (Some(cfg), Some(tsc)) = (tsconfig, resolve_local_tool(root, "tsc")) {
            out.push(local_command(tsc, "tsc", &s(&["--noEmit", "-p", cfg]), "typecheck"));
        }
        if package_script(root, "build").is_some() {
            out.push(npm_command(root, "build", "build"));
        }
    }
    if policy.require_tests {
        if package_script(root, "test").is_some() {
            out.push(npm_command(root, "test", "test"));
        } else {
            out.push(bare_command("node", &s(&["--test"]), "test"));
        }
    }
    out
}

fn with_files(base: &[&str], files: &[String]) -> Vec<String> {
    let mut v = s(base);
    v.extend(files.iter().cloned());
    v
}

fn python_commands(files: &[String], policy: &VerifyPolicy) -> Vec<VerifyCommand> {
    let mut out = vec![];
    if policy.require_lint && !files.is_empty() {
        out.push(bare_command("black", &with_files(&["--check"], files), "format"));
        out.push(bare_command("ruff", &with_files(&["check"], files), "lint"));
    }
    if policy.require_typecheck && !files.is_empty() {
        out.push(bare_command("mypy", files, "typecheck"));
    }
    if policy.require_tests {
        out.push(bare_command("pytest", &[], "test"));
    }
    out
}

fn rust_commands(files: &[String], policy: &VerifyPolicy) -> Vec<VerifyCommand> {
    let mut out = vec![];
    if policy.require_lint && !files.is_empty() {
        out.push(bare_command("rustfmt", &with_files(&["--check"], files), "format"));
        out.push(bare_command("cargo", &s(&["clippy", "--offline"]), "lint"));
    }
    if policy.require_typecheck {
        out.push(bare_command("cargo", &s(&["check", "--offline"]), "typecheck"));
        out.push(bare_command("cargo", &s(&["check", "--offline"]), "build"));
    }
    if policy.require_tests {
        out.push(bare_command("cargo", &s(&["test", "--offline"]), "test"));
    }
    out
}

fn go_commands(files: &[String], policy: &VerifyPolicy) -> Vec<VerifyCommand> {
    let mut out = vec![];
    if policy.require_lint && !files.is_empty() {
        out.push(bare_command("gofmt", &with_files(&["-l"], files), "format"));
    }
    if policy.require_lint {
        out.push(bare_command("go", &s(&["vet", "./..."]), "lint"));
    }
    if policy.require_typecheck {
        out.push(bare_command("go", &s(&["build", "./..."]), "typecheck"));
        out.push(bare_command("go", &s(&["build", "./..."]), "build"));
    }
    if policy.require_tests {
        out.push(bare_command("go", &s(&["test", "./..."]), "test"));
    }
    out
}

fn infer_configured_stage(command: &str) -> String {
    let lower = command.to_lowercase();
    if lower.contains("format") || lower.contains("prettier") {
        return "format".into();
    }
    if ["lint", "eslint", "oxlint", "ruff", "clippy"].iter().any(|k| lower.contains(k)) {
        return "lint".into();
    }
    if ["typecheck", "type-check", "tsc", "mypy"].iter().any(|k| lower.contains(k)) {
        return "typecheck".into();
    }
    if lower.contains("build") {
        return "build".into();
    }
    "test".into()
}

/// A configured `verify.commands` entry: plain argv split, or `!cmd` via bash.
pub fn parse_configured_command(raw: &str) -> Option<VerifyCommand> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(script) = trimmed.strip_prefix('!') {
        let script = script.trim();
        if script.is_empty() {
            return None;
        }
        return Some(VerifyCommand { stage: infer_configured_stage(script), command: "bash".into(), args: vec!["-c".into(), script.into()], display: trimmed.into() });
    }
    let mut parts = trimmed.split_whitespace();
    let command = parts.next()?.to_string();
    Some(VerifyCommand { stage: infer_configured_stage(trimmed), command, args: parts.map(str::to_string).collect(), display: trimmed.into() })
}

/// Shell-equivalent command line for a verify command (used for risk and network checks).
pub fn verify_command_line(command: &VerifyCommand) -> String {
    if command.command == "bash" && command.args.len() == 2 && command.args[0] == "-c" {
        return command.args[1].clone();
    }
    let quote = |part: &str| {
        if !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
            part.to_string()
        } else {
            format!("'{}'", part.replace('\'', "'\\''"))
        }
    };
    std::iter::once(command.command.as_str()).chain(command.args.iter().map(String::as_str)).map(quote).collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifyPlanAssessment {
    pub risk: RiskClass,
    pub denied: Option<String>,
}

/// Verification commands are shell commands too: they get the same critical-risk and network
/// policy gates as the bash tool, and their risk is passed to the approval callback.
pub fn assess_verify_plan(plan: &VerifyPlan, policy: Option<&ExecutionPolicy>) -> VerifyPlanAssessment {
    let mut risk = RiskClass::Low;
    for command in &plan.commands {
        let line = verify_command_line(command);
        let command_risk = classify_bash_risk(&line);
        risk = risk.max(command_risk);
        if command_risk == RiskClass::Critical {
            return VerifyPlanAssessment { risk: RiskClass::Critical, denied: Some(format!("verification command is critical risk and cannot run: {}", command.display)) };
        }
        if let Some(policy) = policy {
            if !is_network_allowed_for_command(policy, &line) {
                return VerifyPlanAssessment { risk, denied: Some(format!("verification command needs network access blocked by the execution policy: {}", command.display)) };
            }
        }
    }
    VerifyPlanAssessment { risk, denied: None }
}

/// Builds the plan without running anything.
pub fn plan_verification(cwd: &Path, changed_files: &[String], config: &VerifyConfig, policy: &VerifyPolicy) -> VerifyPlan {
    let projects = detect_projects(cwd, changed_files);
    let mut commands: Vec<VerifyCommand> = vec![];
    if !config.commands.is_empty() {
        commands = config.commands.iter().filter_map(|c| parse_configured_command(c)).collect();
    } else {
        for project in &projects {
            let files = scope_files(changed_files, &project.root);
            let files: Vec<String> = files.into_iter().filter(|f| project.kind == "unknown" || kind_extensions(project.kind).contains(&extension_of(f).as_str())).collect();
            match project.kind {
                "node" => commands.extend(node_commands(&project.root, &files, policy)),
                "python" => commands.extend(python_commands(&files, policy)),
                "rust" => commands.extend(rust_commands(&files, policy)),
                "go" => commands.extend(go_commands(&files, policy)),
                _ => {}
            }
        }
    }
    let mut seen: Vec<(String, Vec<String>)> = vec![];
    commands.retain(|c| {
        let key = (c.command.clone(), c.args.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    let mut stages: Vec<String> = vec![];
    if !changed_files.is_empty() {
        stages.push("syntax".into());
    }
    for c in &commands {
        if !stages.contains(&c.stage) {
            stages.push(c.stage.clone());
        }
    }
    let ordered = STAGE_ORDER.iter().filter(|s| stages.iter().any(|x| x == *s)).map(|s| s.to_string()).collect();
    VerifyPlan { stages: ordered, commands, projects, approved: false }
}

fn truncate_issue_text(text: &str) -> String {
    let len = jsutil::char_len(text);
    if len <= MAX_ISSUE_CHARS {
        text.to_string()
    } else {
        format!("{}\n...[truncated {} chars]", jsutil::take_chars(text, MAX_ISSUE_CHARS), len - MAX_ISSUE_CHARS)
    }
}

fn cap_issues(issues: &[VerifyIssue]) -> Vec<VerifyIssue> {
    let mut capped = vec![];
    let mut total = 0;
    let mut dropped = 0;
    for issue in issues {
        let message = truncate_issue_text(&issue.message);
        let len = jsutil::char_len(&message);
        if capped.len() >= MAX_ISSUES || total + len > MAX_TOTAL_ISSUE_CHARS {
            dropped += 1;
            continue;
        }
        total += len;
        capped.push(VerifyIssue { message, ..issue.clone() });
    }
    if dropped > 0 {
        let stage = issues.last().map(|i| i.stage.clone()).unwrap_or_else(|| "test".into());
        capped.push(VerifyIssue { stage, path: String::new(), line: None, column: None, message: format!("[+{dropped} more]") });
    }
    capped
}

fn clean_path(path: &str) -> String {
    let mut p = path.trim().replace('\\', "/");
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    p
}

/// Extracts `path:line[:column]: message` and `path(line,column): message` issues.
pub fn parse_issues(stage: &str, output: &str) -> Vec<VerifyIssue> {
    static PAREN: OnceLock<Regex> = OnceLock::new();
    static COLON: OnceLock<Regex> = OnceLock::new();
    let paren = PAREN.get_or_init(|| Regex::new(r"^(.+?)\((\d+),(\d+)\):\s*(.+)$").unwrap());
    let colon = COLON.get_or_init(|| Regex::new(r"^(.+?):(\d+)(?::(\d+))?[:,]?\s+(.+)$").unwrap());
    let mut issues = vec![];
    for raw in output.split('\n') {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        if let Some(c) = paren.captures(line) {
            issues.push(VerifyIssue { stage: stage.into(), path: clean_path(&c[1]), line: c[2].parse().ok(), column: c[3].parse().ok(), message: c[4].to_string() });
            continue;
        }
        if let Some(c) = colon.captures(line) {
            issues.push(VerifyIssue { stage: stage.into(), path: clean_path(&c[1]), line: c[2].parse().ok(), column: c.get(3).and_then(|m| m.as_str().parse().ok()), message: c[4].to_string() });
        }
    }
    issues
}

fn parse_warnings(stage: &str, output: &str) -> Vec<VerifyIssue> {
    static WARN: OnceLock<Regex> = OnceLock::new();
    static NONE: OnceLock<Regex> = OnceLock::new();
    let warn = WARN.get_or_init(|| Regex::new(r"(?i)\bwarning\b").unwrap());
    let none = NONE.get_or_init(|| Regex::new(r"(?i)\b(?:no|0)\s+warnings?\b").unwrap());
    let mut issues = vec![];
    for raw in output.split('\n') {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() || !warn.is_match(line) || none.is_match(line) {
            continue;
        }
        let parsed = parse_issues(stage, line);
        issues.push(parsed.into_iter().next().unwrap_or(VerifyIssue { stage: stage.into(), path: String::new(), line: None, column: None, message: line.into() }));
    }
    issues
}

/// Interprets one command result.
pub fn parse_command_result(stage: &str, result: &RunnerResult, policy: &VerifyPolicy) -> StageResult {
    let output = truncate_issue_text(&result.output);
    let base = |severity, issues, reason: Option<&str>| StageResult { stage: stage.into(), severity, duration_ms: 0, issues, output: Some(output.clone()), reason: reason.map(str::to_string) };
    if result.timed_out {
        let detail = if output.trim().is_empty() { String::new() } else { format!("\n{}", output.trim()) };
        return base(Severity::Fail, vec![VerifyIssue { stage: stage.into(), path: String::new(), line: None, column: None, message: format!("Command timed out.{detail}") }], Some("timeout"));
    }
    if result.not_found {
        return base(Severity::Skip, vec![], Some("tool not found"));
    }
    if result.code == 0 {
        if stage == "format" && !result.output.trim().is_empty() {
            let mut issues: Vec<VerifyIssue> = result
                .output
                .trim()
                .split('\n')
                .map(|l| l.trim_end_matches('\r'))
                .filter(|l| !l.is_empty())
                .map(|l| VerifyIssue { stage: stage.into(), path: clean_path(l), line: None, column: None, message: "File is not formatted.".into() })
                .collect();
            if issues.is_empty() {
                issues.push(VerifyIssue { stage: stage.into(), path: String::new(), line: None, column: None, message: "Files are not formatted.".into() });
            }
            return base(if policy.allow_warnings { Severity::Warn } else { Severity::Fail }, issues, Some(if policy.allow_warnings { "not formatted" } else { "format required" }));
        }
        let warnings = parse_warnings(stage, &output);
        if !warnings.is_empty() {
            return base(if policy.allow_warnings { Severity::Warn } else { Severity::Fail }, warnings, Some(if policy.allow_warnings { "warnings" } else { "warnings not allowed" }));
        }
        return base(Severity::Pass, vec![], None);
    }
    if missing_tool_re().is_match(&output) {
        return base(Severity::Skip, vec![], Some("tool not found"));
    }
    let mut issues = parse_issues(stage, &output);
    if issues.is_empty() {
        let lines: Vec<&str> = output.split('\n').map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        let detail = lines[lines.len().saturating_sub(5)..].join("\n");
        let text = if detail.is_empty() { format!("Command exited with code {}.", result.code) } else { detail };
        issues.push(VerifyIssue { stage: stage.into(), path: String::new(), line: None, column: None, message: truncate_issue_text(&text) });
    }
    let reason = format!("exit code {}", result.code);
    base(Severity::Fail, issues, Some(&reason))
}

fn build_verdict(stages: Vec<StageResult>) -> VerifyVerdict {
    let failed = stages.iter().find(|s| s.severity == Severity::Fail);
    let warned = stages.iter().find(|s| s.severity == Severity::Warn);
    let errors = cap_issues(&stages.iter().filter(|s| s.severity == Severity::Fail).flat_map(|s| s.issues.clone()).collect::<Vec<_>>());
    let warnings = cap_issues(&stages.iter().filter(|s| s.severity == Severity::Warn).flat_map(|s| s.issues.clone()).collect::<Vec<_>>());
    let severity = if failed.is_some() {
        Severity::Fail
    } else if warned.is_some() {
        Severity::Warn
    } else if stages.is_empty() || stages.iter().all(|s| s.severity == Severity::Skip) {
        Severity::Skip
    } else {
        Severity::Pass
    };
    let reason = if let Some(f) = failed {
        format!("{} failed{}", f.stage, f.reason.as_ref().map(|r| format!(" ({r})")).unwrap_or_default())
    } else if let Some(w) = warned {
        format!("warnings from {}", w.stage)
    } else if severity == Severity::Skip {
        "no runnable checks".into()
    } else {
        "ok".into()
    };
    VerifyVerdict { pass: severity != Severity::Fail, severity, stages, errors, warnings, reason }
}

fn run_syntax_stage(cwd: &Path, changed_files: &[String]) -> StageResult {
    let started = std::time::Instant::now();
    let files = scope_files(changed_files, cwd);
    let mut issues = vec![];
    static LINE: OnceLock<Regex> = OnceLock::new();
    let line_re = LINE.get_or_init(|| Regex::new(r"\bline (\d+)\b").unwrap());
    for file in files {
        let Ok(content) = std::fs::read_to_string(cwd.join(&file)) else { continue };
        if let Err(error) = validate_syntax(&file, &content) {
            let message = error.to_string();
            let line = line_re.captures(&message).and_then(|c| c[1].parse::<u64>().ok()).filter(|l| *l > 0);
            issues.push(VerifyIssue { stage: "syntax".into(), path: file, line, column: None, message });
        }
    }
    let failed = !issues.is_empty();
    StageResult {
        stage: "syntax".into(),
        severity: if failed { Severity::Fail } else { Severity::Pass },
        duration_ms: started.elapsed().as_millis() as u64,
        issues,
        output: None,
        reason: failed.then(|| "syntax".to_string()),
    }
}

fn env_for_command(command: &VerifyCommand) -> Option<BTreeMap<String, String>> {
    let name = command.command.trim_end_matches(".exe");
    if name == "go" || name.ends_with("/go") {
        // Offline-by-default: never download modules while verifying.
        let mut env: BTreeMap<String, String> = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
        env.insert("GOPROXY".into(), "off".into());
        env.insert("GOFLAGS".into(), "-mod=readonly".into());
        return Some(env);
    }
    None
}

/// Runs a verification plan. `approve` must present the commands; denial returns a skipped
/// verdict (pass=true). A plan with commands and no approval callback fails closed.
pub async fn run_verification(
    cwd: &Path,
    changed_files: &[String],
    config: &VerifyConfig,
    runner: &dyn VerifyRunner,
    cancel: &CancellationToken,
    plan: &mut VerifyPlan,
    approve: Option<&(dyn Fn(&VerifyPlan) -> BoxFuture<'static, bool> + Send + Sync)>,
) -> anyhow::Result<VerifyVerdict> {
    if !plan.commands.is_empty() && !plan.approved {
        let Some(approve) = approve else {
            return Err(VerificationApprovalError.into());
        };
        if !approve(plan).await {
            return Ok(VerifyVerdict::skipped("approval denied"));
        }
        plan.approved = true;
    }
    let mut stages = vec![];
    if !changed_files.is_empty() {
        let syntax = run_syntax_stage(cwd, changed_files);
        let failed = syntax.severity == Severity::Fail;
        stages.push(syntax);
        if failed {
            return Ok(build_verdict(stages));
        }
    }
    let timeout_ms = clamp_verify_timeout(config.timeout_ms);
    for command in &plan.commands {
        if cancel.is_cancelled() {
            stages.push(StageResult {
                stage: command.stage.clone(),
                severity: Severity::Fail,
                duration_ms: 0,
                issues: vec![VerifyIssue { stage: command.stage.clone(), path: String::new(), line: None, column: None, message: "Command cancelled.".into() }],
                output: None,
                reason: Some("cancelled".into()),
            });
            break;
        }
        let started = std::time::Instant::now();
        let result = runner
            .run(command.command.clone(), command.args.clone(), RunnerOptions { cwd: cwd.to_path_buf(), timeout_ms, cancel: cancel.clone(), env: env_for_command(command) })
            .await;
        let mut stage_result = parse_command_result(&command.stage, &result, &config.policy);
        stage_result.duration_ms = started.elapsed().as_millis() as u64;
        let failed = stage_result.severity == Severity::Fail;
        stages.push(stage_result);
        if failed {
            break;
        }
    }
    Ok(build_verdict(stages))
}

/// Short user/model-facing error text (caller redacts).
pub fn format_verify_errors(verdict: &VerifyVerdict) -> String {
    let mut lines = vec![format!("Verification failed: {}", verdict.reason)];
    for issue in verdict.errors.iter().take(MAX_ISSUES) {
        lines.push(format!("- {}{}: {}", issue.stage, location(issue), truncate_issue_text(&issue.message)));
    }
    if verdict.errors.len() > MAX_ISSUES {
        lines.push(format!("[+{} more]", verdict.errors.len() - MAX_ISSUES));
    }
    lines.join("\n")
}

fn location(issue: &VerifyIssue) -> String {
    if issue.path.is_empty() {
        return String::new();
    }
    match (issue.line, issue.column) {
        (Some(l), Some(c)) => format!(" {}:{l}:{c}", issue.path),
        (Some(l), None) => format!(" {}:{l}", issue.path),
        _ => format!(" {}", issue.path),
    }
}

pub fn format_verify_warnings(verdict: &VerifyVerdict) -> String {
    if verdict.warnings.is_empty() {
        return String::new();
    }
    let mut lines = vec!["Verification warnings:".to_string()];
    for issue in verdict.warnings.iter().take(MAX_ISSUES) {
        lines.push(format!("- {}{}: {}", issue.stage, location(issue), truncate_issue_text(&issue.message)));
    }
    if verdict.warnings.len() > MAX_ISSUES {
        lines.push(format!("[+{} more]", verdict.warnings.len() - MAX_ISSUES));
    }
    lines.join("\n")
}

/// Redacts literal secrets, bearer tokens and key/value credentials.
pub fn redact_text(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.chars().count() >= 4 {
            out = out.replace(secret, "[redacted]");
        }
    }
    static BEARER: OnceLock<Regex> = OnceLock::new();
    static KV: OnceLock<Regex> = OnceLock::new();
    out = BEARER.get_or_init(|| Regex::new(r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=-]+").unwrap()).replace_all(&out, "${1} [redacted]").to_string();
    out = KV
        .get_or_init(|| Regex::new(r#"(?i)("?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|client[_-]?secret|token|secret|password|passwd|pwd)"?\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s"',}]+)"#).unwrap())
        .replace_all(&out, "${1}[redacted]")
        .to_string();
    out
}

pub fn redact_verdict(verdict: &VerifyVerdict, secrets: &[&str]) -> VerifyVerdict {
    let clean = |t: &str| redact_text(t, secrets);
    let clean_issue = |i: &VerifyIssue| VerifyIssue { message: clean(&i.message), ..i.clone() };
    VerifyVerdict {
        pass: verdict.pass,
        severity: verdict.severity,
        reason: clean(&verdict.reason),
        stages: verdict
            .stages
            .iter()
            .map(|s| StageResult { output: s.output.as_deref().map(clean), reason: s.reason.as_deref().map(clean), issues: s.issues.iter().map(clean_issue).collect(), ..s.clone() })
            .collect(),
        errors: verdict.errors.iter().map(clean_issue).collect(),
        warnings: verdict.warnings.iter().map(clean_issue).collect(),
    }
}

/// Real runner backed by `run_command`.
pub struct CommandRunner;

impl VerifyRunner for CommandRunner {
    fn run(&self, command: String, args: Vec<String>, opts: RunnerOptions) -> BoxFuture<'static, RunnerResult> {
        Box::pin(async move {
            let result = run_command(&command, &args, RunOptions { cwd: &opts.cwd, cancel: &opts.cancel, timeout_ms: opts.timeout_ms, env: opts.env, output_limit_bytes: None, hard_output_bytes: None }).await;
            match result {
                Ok(r) => RunnerResult { code: r.code, output: r.output, ..Default::default() },
                Err(CommandError::Cancelled) => RunnerResult { code: 130, output: "Command cancelled.".into(), ..Default::default() },
                Err(CommandError::NotFound(message)) => RunnerResult { code: 127, output: message, not_found: true, ..Default::default() },
                Err(error @ CommandError::TimedOut(_)) => RunnerResult { code: 124, output: error.to_string(), timed_out: true, ..Default::default() },
                Err(error) => RunnerResult { code: 1, output: error.to_string(), ..Default::default() },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_respects_policy_and_configured_commands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"scripts":{"test":"node --test","lint":"oxlint"}}"#).unwrap();
        std::fs::write(dir.path().join("a.ts"), "export {}\n").unwrap();
        let config = VerifyConfig::default();
        let plan = plan_verification(dir.path(), &["a.ts".into()], &config, &config.policy);
        assert!(plan.commands.is_empty());
        assert_eq!(plan.stages, vec!["syntax"]);
        let policy = VerifyPolicy { require_tests: true, require_lint: true, ..VerifyPolicy::default() };
        let plan = plan_verification(dir.path(), &["a.ts".into()], &config, &policy);
        let displays: Vec<&str> = plan.commands.iter().map(|c| c.display.as_str()).collect();
        assert_eq!(displays, vec!["npm run lint", "npm run test"]);
        let configured = VerifyConfig { commands: vec!["!curl https://x.test".into()], ..VerifyConfig::default() };
        let plan = plan_verification(dir.path(), &[], &configured, &configured.policy);
        let denied = assess_verify_plan(&plan, Some(&ExecutionPolicy::with_mode(crate::policy::NetworkMode::Deny, &[])));
        assert!(denied.denied.unwrap().contains("network"));
    }

    #[test]
    fn parses_issues_and_results() {
        let issues = parse_issues("lint", "src/a.ts(3,4): error TS1\n./b.ts:10:2: bad thing");
        assert_eq!(issues[0].path, "src/a.ts");
        assert_eq!(issues[1].line, Some(10));
        let policy = VerifyPolicy::default();
        let failed = parse_command_result("test", &RunnerResult { code: 1, output: "boom".into(), ..Default::default() }, &policy);
        assert_eq!(failed.severity, Severity::Fail);
        let skipped = parse_command_result("lint", &RunnerResult { code: 127, output: "x: command not found".into(), ..Default::default() }, &policy);
        assert_eq!(skipped.severity, Severity::Skip);
        let warned = parse_command_result("lint", &RunnerResult { code: 0, output: "a.ts:1:1: warning unused".into(), ..Default::default() }, &policy);
        assert_eq!(warned.severity, Severity::Warn);
    }

    #[test]
    fn redaction() {
        assert_eq!(redact_text("key abcd1234 Bearer xyz token=1", &["abcd1234"]), "key [redacted] Bearer [redacted] token=[redacted]");
    }
}
