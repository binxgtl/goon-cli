//! Differential parity test against the TypeScript implementation (v0.7).
//!
//! `tests/parity/corpus.jsonl` holds `(module, fn, args) -> result | error` records captured from
//! the TypeScript build (`tests/parity/instrument.mjs` while running the old JS unit tests, plus
//! `tests/parity/synth.mjs` for synthetic inputs). Every record is replayed against the Rust
//! port. A mismatch fails the test unless it is listed in `DIVERGENCES` — deliberate fixes of
//! TypeScript bugs, each with the reason.
use goon::*;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Intentional behaviour changes: `(module.fn, substring of the JSON-encoded args, reason)`.
const DIVERGENCES: &[(&str, &str, &str)] = &[
    ("risk.classifyBashRisk", "core.hooksPath", "git config of keys that execute programs later (core.hooksPath, core.pager, …) is high risk; TS rated it medium"),
    ("risk.classifyBashRisk", "${HOME}", "rm -rf ${HOME} deletes the home directory; TS only recognised the bare $HOME spelling"),
    ("risk.classifyBashRisk", "$HOME/", "rm -rf \"$HOME/\" deletes the home directory; TS missed the trailing-slash spelling"),
    ("risk.tokenize", "$'\\\\x", "ANSI-C $'\\xHH' escapes decode to the real bytes (rm $'\\x2drf' is rm -rf); TS mangled them into -drf"),
];

const UNDEF: &str = "$undefined";

fn undef() -> Value {
    json!({ UNDEF: true })
}

fn is_undef(v: &Value) -> bool {
    v.as_object().is_some_and(|m| m.len() == 1 && m.contains_key(UNDEF))
}

/// JSON view of a recorded argument: `undefined` object members disappear and `undefined` array
/// items become `null`, as `JSON.stringify` (and every Rust consumer of these values) sees them.
fn clean(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().filter(|(_, v)| !is_undef(v)).map(|(k, v)| (k.clone(), clean(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(|v| if is_undef(v) { Value::Null } else { clean(v) }).collect()),
        other => other.clone(),
    }
}

/// Argument `i`, or `None` when absent / `undefined`.
fn arg(args: &[Value], i: usize) -> Option<&Value> {
    args.get(i).filter(|v| !is_undef(v))
}

fn s(args: &[Value], i: usize) -> String {
    arg(args, i).and_then(Value::as_str).unwrap_or("").to_string()
}

fn num_of(v: &Value) -> f64 {
    if let Some(n) = v.as_f64() {
        return n;
    }
    match v.get("$number").and_then(Value::as_str) {
        Some("NaN") => f64::NAN,
        Some("Infinity") => f64::INFINITY,
        Some("-Infinity") => f64::NEG_INFINITY,
        Some("-0") => -0.0,
        _ => f64::NAN,
    }
}

fn n(args: &[Value], i: usize) -> Option<f64> {
    arg(args, i).map(num_of)
}

fn strs_of(v: &Value) -> Vec<String> {
    let list = v.get("$set").unwrap_or(v);
    list.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn opt_s(v: Option<String>) -> Value {
    v.map(Value::String).unwrap_or_else(undef)
}

fn objects(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

fn bytes_of(v: &Value) -> Vec<u8> {
    use base64::Engine;
    let b64 = v.get("$bytes").and_then(Value::as_str).unwrap_or("");
    base64::engine::general_purpose::STANDARD.decode(b64).unwrap_or_default()
}

fn risk_class(text: &str) -> risk::RiskClass {
    risk::RiskClass::parse(text).unwrap_or_else(|| panic!("unknown risk class {text}"))
}

fn index_map(v: Option<&Value>) -> indexmap::IndexMap<String, String> {
    let mut out = indexmap::IndexMap::new();
    if let Some(m) = v.and_then(Value::as_object) {
        for (k, v) in m {
            out.insert(k.clone(), v.as_str().unwrap_or("").to_string());
        }
    }
    out
}

fn price_value(p: &cost::Price) -> Value {
    let mut m = Map::new();
    m.insert("inputPer1M".into(), json!(p.input_per_1m));
    m.insert("outputPer1M".into(), json!(p.output_per_1m));
    if let Some(v) = p.cached_input_per_1m {
        m.insert("cachedInputPer1M".into(), json!(v));
    }
    if let Some(v) = p.cache_write_per_1m {
        m.insert("cacheWritePer1M".into(), json!(v));
    }
    Value::Object(m)
}

fn model_of(v: &Value) -> provider::DiscoveredModel {
    provider::DiscoveredModel {
        id: v["id"].as_str().unwrap_or("").into(),
        context_window: v.get("contextWindow").and_then(Value::as_u64),
        max_output_tokens: v.get("maxOutputTokens").and_then(Value::as_u64),
        supports_vision: v.get("supportsVision").and_then(Value::as_bool),
        default_effort: v.get("defaultEffort").and_then(Value::as_str).map(str::to_string),
    }
}

fn model_value(m: &provider::DiscoveredModel) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), json!(m.id));
    if let Some(v) = m.context_window {
        out.insert("contextWindow".into(), json!(v));
    }
    if let Some(v) = m.max_output_tokens {
        out.insert("maxOutputTokens".into(), json!(v));
    }
    if let Some(v) = m.supports_vision {
        out.insert("supportsVision".into(), json!(v));
    }
    if let Some(v) = &m.default_effort {
        out.insert("defaultEffort".into(), json!(v));
    }
    Value::Object(out)
}

fn plan_of(v: &Value) -> coord::Plan {
    coord::Plan {
        id: v["id"].as_str().unwrap_or("").into(),
        goal: v["goal"].as_str().unwrap_or("").into(),
        created_at: v["createdAt"].as_str().unwrap_or("").into(),
        todos: objects(&v["todos"])
            .iter()
            .map(|t| coord::Todo {
                id: t["id"].as_str().unwrap_or("").into(),
                title: t["title"].as_str().unwrap_or("").into(),
                acceptance: t["acceptance"].as_str().unwrap_or("").into(),
                depends_on: strs_of(&t["dependsOn"]),
                status: t["status"].as_str().unwrap_or("pending").into(),
                role: t["role"].as_str().unwrap_or("").into(),
                assignee: t.get("assignee").and_then(Value::as_str).map(str::to_string),
                artifacts: strs_of(&t["artifacts"]),
                rework_cycles: t["reworkCycles"].as_u64().unwrap_or(0) as u32,
            })
            .collect(),
    }
}

fn issue_of(v: &Value) -> verify::VerifyIssue {
    verify::VerifyIssue {
        stage: v["stage"].as_str().unwrap_or("").into(),
        path: v["path"].as_str().unwrap_or("").into(),
        line: v.get("line").and_then(Value::as_u64),
        column: v.get("column").and_then(Value::as_u64),
        message: v["message"].as_str().unwrap_or("").into(),
    }
}

fn severity_of(v: &Value) -> verify::Severity {
    match v.as_str().unwrap_or("") {
        "pass" => verify::Severity::Pass,
        "fail" => verify::Severity::Fail,
        "warn" => verify::Severity::Warn,
        _ => verify::Severity::Skip,
    }
}

fn verdict_of(v: &Value) -> verify::VerifyVerdict {
    verify::VerifyVerdict {
        pass: v["pass"].as_bool().unwrap_or(false),
        severity: severity_of(&v["severity"]),
        stages: objects(&v["stages"])
            .iter()
            .map(|st| verify::StageResult {
                stage: st["stage"].as_str().unwrap_or("").into(),
                severity: severity_of(&st["severity"]),
                duration_ms: st["durationMs"].as_u64().unwrap_or(0),
                issues: objects(&st["issues"]).iter().map(issue_of).collect(),
                output: st.get("output").and_then(Value::as_str).map(str::to_string),
                reason: st.get("reason").and_then(Value::as_str).map(str::to_string),
            })
            .collect(),
        errors: objects(&v["errors"]).iter().map(issue_of).collect(),
        warnings: objects(&v["warnings"]).iter().map(issue_of).collect(),
        reason: v["reason"].as_str().unwrap_or("").into(),
    }
}

fn verify_command_of(v: &Value) -> verify::VerifyCommand {
    verify::VerifyCommand {
        stage: v["stage"].as_str().unwrap_or("").into(),
        command: v["command"].as_str().unwrap_or("").into(),
        args: strs_of(&v["args"]),
        display: v["display"].as_str().unwrap_or("").into(),
    }
}

fn verify_command_value(c: &verify::VerifyCommand) -> Value {
    json!({ "stage": c.stage, "command": c.command, "args": c.args, "display": c.display })
}

fn memory_context_of(v: &Value) -> context_budget::MemoryPromptContext {
    context_budget::MemoryPromptContext {
        rag: objects(&v["rag"])
            .iter()
            .map(|c| context_budget::RagChunk {
                path: c["path"].as_str().unwrap_or("").into(),
                start_line: c["startLine"].as_u64().unwrap_or(0),
                end_line: c["endLine"].as_u64().unwrap_or(0),
                score: c["score"].as_f64().unwrap_or(0.0),
                content: c["content"].as_str().unwrap_or("").into(),
            })
            .collect(),
        repo_tree: v.get("repoTree").and_then(Value::as_str).map(str::to_string),
        working_notes: v.get("workingNotes").and_then(Value::as_str).map(str::to_string),
        preferences: v.get("preferences").filter(|p| !is_undef(p)).map(strs_of),
    }
}

fn patch_op_value(op: &files::PatchOperation) -> Value {
    match op {
        files::PatchOperation::Add { path, content } => json!({ "kind": "add", "path": path, "content": content }),
        files::PatchOperation::Delete { path } => json!({ "kind": "delete", "path": path }),
        files::PatchOperation::Update { path, hunks, move_to } => {
            let mut m = Map::new();
            m.insert("kind".into(), json!("update"));
            m.insert("path".into(), json!(path));
            if let Some(to) = move_to {
                m.insert("moveTo".into(), json!(to));
            }
            m.insert("hunks".into(), Value::Array(hunks.iter().map(|h| json!({ "oldLines": h.old_lines, "newLines": h.new_lines })).collect()));
            Value::Object(m)
        }
    }
}

fn hunks_of(v: &Value) -> Vec<files::PatchHunk> {
    objects(v).iter().map(|h| files::PatchHunk { old_lines: strs_of(&h["oldLines"]), new_lines: strs_of(&h["newLines"]) }).collect()
}

fn usage_of(v: &Value) -> model::Usage {
    let tok = |v: Option<&Value>| v.filter(|v| !is_undef(v)).and_then(Value::as_u64);
    let details = v.get("inputTokenDetails");
    model::Usage {
        input_tokens: tok(v.get("inputTokens")),
        output_tokens: tok(v.get("outputTokens")),
        cache_read_tokens: tok(details.and_then(|d| d.get("cacheReadTokens"))).or_else(|| tok(v.get("cachedInputTokens"))),
        cache_write_tokens: tok(details.and_then(|d| d.get("cacheWriteTokens"))),
        reasoning_tokens: tok(v.get("reasoningTokens")),
    }
}

fn split_value(u: &cost::UsageSplit) -> Value {
    let mut m = Map::new();
    m.insert("inputTokens".into(), json!(u.input_tokens));
    m.insert("outputTokens".into(), json!(u.output_tokens));
    m.insert("totalTokens".into(), json!(u.total_tokens));
    if let Some(c) = u.cached_input_tokens {
        m.insert("cachedInputTokens".into(), json!(c));
    }
    if let Some(c) = u.cache_write_tokens {
        m.insert("cacheWriteTokens".into(), json!(c));
    }
    Value::Object(m)
}

fn goal_of(v: Option<&Value>) -> session::SessionGoal {
    serde_json::from_value(v.cloned().unwrap_or(Value::Null)).expect("session goal")
}

enum Outcome {
    Value(Value),
    Error(String),
    /// The Rust port has no equivalent (documented in the reason).
    Skip(&'static str),
}

use Outcome::{Error as Err_, Skip, Value as Ok_};

fn res<T>(r: Result<T, impl std::fmt::Display>, f: impl FnOnce(T) -> Value) -> Outcome {
    match r {
        Ok(v) => Ok_(f(v)),
        Err(e) => Err_(e.to_string()),
    }
}

fn call(module: &str, func: &str, a: &[Value]) -> Outcome {
    let key = format!("{module}.{func}");
    match key.as_str() {
        // ---------------------------------------------------------------- approval
        "approval.bashApprovalExact" => Ok_(opt_s(approval::bash_approval_exact(&a[0]))),
        "approval.bashApprovalPrefix" => Ok_(opt_s(approval::bash_approval_prefix(&a[0]))),
        "approval.canAutoApprove" => Ok_(json!(approval::can_auto_approve(&s(a, 0), risk_class(&s(a, 1))))),
        "approval.isApprovedBashExact" => {
            let set: BTreeSet<String> = strs_of(&a[1]).into_iter().collect();
            Ok_(json!(approval::is_approved_bash_exact(&a[0], &set)))
        }
        "approval.isApprovedBashPrefix" => {
            let set: BTreeSet<String> = strs_of(&a[1]).into_iter().collect();
            Ok_(json!(approval::is_approved_bash_prefix(&a[0], &set)))
        }
        // ---------------------------------------------------------------- config
        "config.normalizeGatewayUrl" => Ok_(json!(config::normalize_gateway_url(&s(a, 0)))),
        // ---------------------------------------------------------------- context-budget
        "context-budget.allocateContextBudget" => {
            let c = &a[0];
            let cfg = context_budget::ContextBudgetConfig {
                context_window: c["contextWindow"].as_u64().unwrap_or(0),
                max_output_tokens: c["maxOutputTokens"].as_u64().unwrap_or(0),
                // Schema defaults for configs recorded before normalization.
                reserve_ratio: c["contextReserveRatio"].as_f64().unwrap_or(0.15),
                conversation_ratio: c["contextConversationRatio"].as_f64().unwrap_or(0.45),
                files_ratio: c["contextFilesRatio"].as_f64().unwrap_or(0.25),
            };
            let r = context_budget::allocate_context_budget(&cfg, &s(a, 1));
            Ok_(json!({ "inputBudget": r.input_budget, "reserve": r.reserve, "systemBudget": r.system_budget, "conversation": r.conversation, "files": r.files, "truncated": r.truncated }))
        }
        "context-budget.pruneConversation" => res(context_budget::prune_conversation(&objects(&a[0]), n(a, 1).unwrap_or(0.0) as u64), Value::Array),
        "context-budget.pruneToolResults" => {
            let max = n(a, 1).map(|v| v as usize).unwrap_or(context_budget::MAX_TOOL_RESULT_CHARS);
            let (messages, pruned) = context_budget::prune_tool_results(&objects(&a[0]), max);
            Ok_(json!({ "messages": messages, "pruned": pruned }))
        }
        "context-budget.renderMemoryContext" => {
            let ctx = arg(a, 0).map(memory_context_of);
            Ok_(opt_s(context_budget::render_memory_context(ctx.as_ref())))
        }
        "context-budget.truncateSystemToBudget" => Ok_(json!(context_budget::truncate_system_to_budget(&s(a, 0), n(a, 1).unwrap_or(0.0) as u64))),
        // ---------------------------------------------------------------- context
        "context.buildGoalBudgetLimitPrompt" => Ok_(json!(context::build_goal_budget_limit_prompt(&goal_of(arg(a, 0))))),
        "context.buildGoalContinuationPrompt" => Ok_(json!(context::build_goal_continuation_prompt(&goal_of(arg(a, 0)), arg(a, 1).and_then(Value::as_bool).unwrap_or(false)))),
        "context.buildGoalObjectiveUpdatedPrompt" => Ok_(json!(context::build_goal_objective_updated_prompt(&goal_of(arg(a, 0))))),
        "context.buildUserGoalContextMessage" => {
            let o = &a[0];
            Ok_(json!(context::build_user_goal_context_message(
                o.get("cleared").and_then(Value::as_bool).unwrap_or(false),
                o.get("objective").and_then(Value::as_str),
                o.get("status").and_then(Value::as_str),
            )))
        }
        "context.estimatedTokens" => Ok_(json!(context::estimated_tokens(&objects(&a[0]), &s(a, 1)))),
        "context.extractCandidatePaths" => {
            Ok_(Value::Array(context::extract_candidate_paths(&s(a, 0)).iter().map(|c| json!({ "rawToken": c.raw_token, "path": c.path, "isExplicitMention": c.is_explicit_mention })).collect()))
        }
        "context.withoutBinaryParts" => Ok_(Value::Array(context::without_binary_parts(&objects(&a[0])))),
        // ---------------------------------------------------------------- coord
        "coord.candidateCap" => {
            let req = n(a, 0).filter(|v| v.is_finite()).map(|v| v.trunc().max(0.0) as usize);
            Ok_(json!(coord::candidate_cap(req)))
        }
        "coord.diffPaths" => Ok_(json!(coord::diff_paths(&s(a, 0)))),
        "coord.rerank" => {
            let inputs = objects(&a[0]);
            let candidates: Vec<coord::Candidate> = inputs
                .iter()
                .map(|c| coord::Candidate {
                    index: c["index"].as_u64().unwrap_or(0) as usize,
                    task: c["task"].as_str().unwrap_or("").into(),
                    report: c["report"].as_str().unwrap_or("").into(),
                    cost_usd: c["costUsd"].as_f64().unwrap_or(0.0),
                    passed: c["passed"].as_bool().unwrap_or(false),
                    warnings: c["warnings"].as_u64().unwrap_or(0) as usize,
                    reason: c["reason"].as_str().unwrap_or("").into(),
                    backups: serde_json::from_value(c["backups"].clone()).unwrap_or_default(),
                })
                .collect();
            let ranked = coord::rerank(&candidates);
            Ok_(Value::Array(ranked.iter().map(|r| inputs.iter().find(|c| c["index"].as_u64() == Some(r.index as u64)).cloned().unwrap()).collect()))
        }
        "coord.roleToolset" => Ok_(json!(coord::role_toolset(&s(a, 0), &arg(a, 1).map(strs_of).unwrap_or_default()))),
        "coord.scopeRoundCap" => Ok_(json!(coord::scope_round_cap(n(a, 0).unwrap_or(0.0).max(0.0) as u64))),
        "coord.validatePlan" => res(coord::validate_plan(&plan_of(&a[0])), |_| undef()),
        // ---------------------------------------------------------------- cost
        "cost.formatDurationMs" => Ok_(json!(cost::format_duration_ms(n(a, 0).unwrap_or(f64::NAN)))),
        "cost.formatUsd" => Ok_(json!(cost::format_usd(n(a, 0).unwrap_or(f64::NAN)))),
        "cost.normalizePricing" => {
            let table = cost::normalize_pricing(arg(a, 0));
            let mut models = Map::new();
            for (k, p) in &table.models {
                models.insert(k.clone(), price_value(p));
            }
            let mut out = Map::new();
            out.insert("models".into(), Value::Object(models));
            if let Some(d) = &table.default {
                out.insert("default".into(), price_value(d));
            }
            Ok_(Value::Object(out))
        }
        "cost.toUsageSplit" => {
            let usage = arg(a, 0).map(usage_of);
            let fb = |i| n(a, i).unwrap_or(0.0).max(0.0) as u64;
            Ok_(split_value(&cost::to_usage_split(usage.as_ref(), fb(1), fb(2))))
        }
        // ---------------------------------------------------------------- files
        "files.applyHunksToContent" => res(files::apply_hunks_to_content(&s(a, 0), &hunks_of(&a[1]), &s(a, 2)), Value::String),
        "files.computeUnifiedDiff" => Ok_(json!(files::compute_unified_diff(&s(a, 0), &s(a, 1), &s(a, 2), n(a, 3).map(|v| v as usize).unwrap_or(3)))),
        "files.limitTextBytes" => Ok_(json!(files::limit_text_bytes(&s(a, 0), n(a, 1).map(|v| v as usize).unwrap_or(64 * 1024)))),
        "files.parsePatchText" => res(files::parse_patch_text(&s(a, 0)), |ops| Value::Array(ops.iter().map(patch_op_value).collect())),
        "files.replaceFirstLiteral" => Ok_(json!(files::replace_first_literal(&s(a, 0), &s(a, 1), &s(a, 2)))),
        // ---------------------------------------------------------------- mcp
        "mcp.mcpToolName" => Ok_(json!(mcp::mcp_tool_name(&s(a, 0), &s(a, 1)))),
        "mcp.parseCodexMcpServers" => {
            // Compare parsed configs structurally: the TypeScript result is the zod-normalized
            // object, which `parse_server` accepts verbatim.
            let servers = mcp::parse_codex_mcp_servers(&s(a, 0));
            let mut out = Map::new();
            for (name, server) in servers {
                out.insert(name, json!({ "$config": format!("{server:?}") }));
            }
            Ok_(Value::Object(out))
        }
        "mcp.sanitizeAndCompactToolSchema" => Ok_(mcp::sanitize_and_compact_tool_schema(arg(a, 0).unwrap_or(&Value::Null), n(a, 1).map(|v| v as usize).unwrap_or(mcp::DEFAULT_SCHEMA_MAX_BYTES))),
        // ---------------------------------------------------------------- policy
        "policy.assessCommandNetwork" => {
            let r = policy::assess_command_network(&s(a, 0));
            Ok_(json!({ "network": r.network, "hosts": r.hosts, "unknown": r.unknown }))
        }
        "policy.extractNetworkHost" => Ok_(opt_s(policy::extract_network_host(&s(a, 0)))),
        "policy.isNetworkAllowedForCommand" => {
            let net = &a[0]["network"];
            let mode = match net["mode"].as_str().unwrap_or("deny") {
                "allow" => policy::NetworkMode::Allow,
                "allowlist" => policy::NetworkMode::Allowlist,
                _ => policy::NetworkMode::Deny,
            };
            let hosts = strs_of(&net["hosts"]);
            let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
            Ok_(json!(policy::is_network_allowed_for_command(&policy::ExecutionPolicy::with_mode(mode, &hosts), &s(a, 1))))
        }
        "policy.networkDeniedMessage" => Ok_(json!(policy::network_denied_message(&s(a, 0)))),
        "policy.normalizeNetworkHost" => Ok_(opt_s(policy::normalize_network_host(&s(a, 0)))),
        "policy.parseNetworkHostList" => {
            let values: Vec<String> = match arg(a, 0) {
                Some(Value::String(v)) => vec![v.clone()],
                Some(v) => strs_of(v),
                None => vec![],
            };
            Ok_(json!(policy::parse_network_host_list(values.iter().map(String::as_str))))
        }
        // ---------------------------------------------------------------- provider
        "provider.formatModelBadge" => Ok_(json!(provider::format_model_badge(&model_of(&a[0])))),
        "provider.resolveModelCapabilities" => {
            let discovered = arg(a, 1).map(model_of);
            Ok_(model_value(&provider::resolve_model_capabilities(&s(a, 0), discovered.as_ref())))
        }
        // ---------------------------------------------------------------- rag
        "rag.chunkFile" => {
            if arg(a, 2).is_some() {
                return Skip("custom chunk options are not reachable from the CLI");
            }
            Ok_(serde_json::to_value(rag::chunk_file(&s(a, 0), &s(a, 1), None)).unwrap())
        }
        "rag.cosineSimilarity" => {
            let vec_of = |v: &Value| v.as_array().map(|a| a.iter().map(num_of).collect::<Vec<f64>>()).unwrap_or_default();
            Ok_(json!(rag::cosine_similarity(&vec_of(&a[0]), &vec_of(&a[1]))))
        }
        "rag.fnv1a" => Ok_(json!(rag::fnv1a(&s(a, 0)))),
        "rag.isIgnored" => {
            let mut rules = vec![];
            for r in objects(&a[1]) {
                rules.extend(rag::parse_goonignore(r["pattern"].as_str().unwrap_or("")));
            }
            Ok_(json!(rag::is_ignored(&s(a, 0), &rules)))
        }
        "rag.looksBinary" => Ok_(json!(rag::looks_binary(&bytes_of(&a[0])))),
        "rag.tokenize" => Ok_(json!(rag::tokenize(&s(a, 0)))),
        // ---------------------------------------------------------------- reasoning
        "reasoning.lastAssistantAnswer" => Ok_(json!(reasoning::last_assistant_answer(&objects(&a[0]), &s(a, 1)))),
        "reasoning.visibleAnswer" => Ok_(json!(reasoning::visible_answer(&s(a, 0), &s(a, 1)))),
        "reasoning.withReasoningReplay" => Ok_(Value::Array(reasoning::with_reasoning_replay(&objects(&a[0]), &s(a, 1), arg(a, 2).and_then(Value::as_str)))),
        // ---------------------------------------------------------------- redact
        "redact.isSensitivePath" => Ok_(json!(redact::is_sensitive_path(&s(a, 0)))),
        "redact.redactObject" => Ok_(redact::redact_object(&a[0], &arg(a, 1).map(strs_of).unwrap_or_default())),
        "redact.redactSecrets" => Ok_(json!(redact::redact_secrets(&s(a, 0), &arg(a, 1).map(strs_of).unwrap_or_default()))),
        // ---------------------------------------------------------------- risk
        "risk.baseName" => Ok_(json!(risk::base_name(&s(a, 0)))),
        "risk.classifyBashRisk" => Ok_(json!(risk::classify_bash_risk(&s(a, 0)).as_str())),
        "risk.classifyMcpRisk" => {
            let config = match mcp::parse_server(&a[3]) {
                Ok(c) => c,
                Err(e) => return Err_(format!("config: {e}")),
            };
            let r = risk::classify_mcp_risk(&s(a, 0), &s(a, 1), arg(a, 2), &config);
            Ok_(json!({ "risk": r.risk.as_str(), "net": r.net, "reason": r.reason }))
        }
        "risk.isGenericRunner" => Ok_(json!(risk::is_generic_runner(&s(a, 0)))),
        "risk.riskLevel" => Ok_(json!(risk::risk_level(risk_class(&s(a, 0))))),
        "risk.tokenize" => Ok_(json!(risk::tokenize(&s(a, 0)))),
        // ---------------------------------------------------------------- session
        "session.formatGoalElapsedSeconds" => Ok_(json!(session::format_goal_elapsed_seconds(n(a, 0).unwrap_or(f64::NAN)))),
        "session.formatGoalStatusIndicator" => Ok_(json!(session::format_goal_status_indicator(&goal_of(arg(a, 0)), n(a, 1).unwrap_or(0.0)))),
        "session.formatGoalSummaryLines" => Ok_(json!(session::format_goal_summary_lines(&goal_of(arg(a, 0)), n(a, 1).unwrap_or(0.0)))),
        "session.formatGoalTokensCompact" => Ok_(json!(session::format_goal_tokens_compact(n(a, 0).unwrap_or(f64::NAN)))),
        "session.goalStatusLabel" => Ok_(json!(session::goal_status_label(&s(a, 0)))),
        "session.parseGoalDraftArgs" => Ok_(match session::parse_goal_draft_args(&s(a, 0)) {
            session::GoalDraft::Ok { objective, token_budget } => {
                json!({ "objective": objective, "tokenBudget": token_budget.map(|b| json!(b)).unwrap_or_else(undef) })
            }
            session::GoalDraft::Error(e) => json!({ "error": e }),
        }),
        "session.sameWorkspace" => Ok_(json!(session::same_workspace(&s(a, 0), &s(a, 1)))),
        // ---------------------------------------------------------------- syntax
        "syntax.isJsoncPath" => Ok_(json!(syntax::is_jsonc_path(&s(a, 0)))),
        "syntax.validateSyntax" => res(syntax::validate_syntax(&s(a, 0), &s(a, 1)), |_| undef()),
        // ---------------------------------------------------------------- tools
        "tools.formatTodos" => {
            let todos: Vec<session::TodoItem> = serde_json::from_value(a[0].clone()).expect("todos");
            Ok_(json!(tools::format_todos(&todos)))
        }
        "tools.normalizeToolInput" => Ok_(tools::normalize_tool_input(&s(a, 0), arg(a, 1).unwrap_or(&Value::Null))),
        "tools.normalizeToolName" => {
            if arg(a, 1).is_some() {
                return Skip("hasExact callback cannot be recorded");
            }
            Ok_(json!(tools::normalize_tool_name(&s(a, 0), None)))
        }
        "tools.sanitizeBashCommand" => Ok_(json!(tools::sanitize_bash_command(&s(a, 0)))),
        "tools.toolApprovalSignature" => Ok_(json!(tools::tool_approval_signature(&s(a, 0), arg(a, 1).unwrap_or(&Value::Null)))),
        // ---------------------------------------------------------------- tui
        "tui.formatInlineMarkdown" => Ok_(json!(match arg(a, 1).and_then(Value::as_str) {
            Some(base) => tui::text::format_inline_markdown(&s(a, 0), base),
            None => tui::text::format_inline_markdown_default(&s(a, 0)),
        })),
        "tui.formatTokenCount" => Ok_(json!(tui::text::format_token_count(n(a, 0).unwrap_or(f64::NAN)))),
        "tui.padEndVisible" => Ok_(json!(tui::text::pad_end_visible(&s(a, 0), n(a, 1).unwrap_or(0.0).max(0.0) as usize))),
        "tui.sliceAnsiLine" => Ok_(json!(tui::text::slice_ansi_line(&s(a, 0), n(a, 1).unwrap_or(0.0).max(0.0) as usize))),
        "tui.sliceVisible" => Ok_(json!(tui::text::slice_visible(&s(a, 0), n(a, 1).unwrap_or(0.0).max(0.0) as usize))),
        "tui.terminalText" => Ok_(json!(term::terminal_text(&s(a, 0)))),
        "tui.visibleLength" => Ok_(json!(tui::text::visible_length(&s(a, 0)))),
        "tui.wrapTerminal" => Ok_(json!(tui::text::wrap_terminal(&s(a, 0), n(a, 1).unwrap_or(0.0).max(0.0) as usize))),
        // ---------------------------------------------------------------- verify
        "verify.clampVerifyTimeout" => Ok_(json!(verify::clamp_verify_timeout(n(a, 0).unwrap_or(0.0).max(0.0) as u64))),
        "verify.formatVerifyErrors" => Ok_(json!(verify::format_verify_errors(&verdict_of(&a[0])))),
        "verify.formatVerifyWarnings" => Ok_(json!(verify::format_verify_warnings(&verdict_of(&a[0])))),
        "verify.parseConfiguredCommand" => Ok_(verify::parse_configured_command(&s(a, 0)).map(|c| verify_command_value(&c)).unwrap_or_else(undef)),
        "verify.parseIssues" => Ok_(Value::Array(verify::parse_issues(&s(a, 0), &s(a, 1)).iter().map(verify::VerifyIssue::to_value).collect())),
        "verify.redactText" => {
            let secrets = arg(a, 1).map(strs_of).unwrap_or_default();
            let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
            Ok_(json!(verify::redact_text(&s(a, 0), &refs)))
        }
        "verify.verifyCommandLine" => Ok_(json!(verify::verify_command_line(&verify_command_of(&a[0])))),
        // ---------------------------------------------------------------- workflow
        "workflow.classifyWorkflowComplexity" => {
            let files = arg(a, 1).and_then(|o| o.get("fileCount")).map(num_of).unwrap_or(0.0);
            let r = workflow::classify_workflow_complexity(&s(a, 0), files.max(0.0) as usize);
            Ok_(json!({ "mode": r.mode, "reason": r.reason }))
        }
        "workflow.interpolateWorkflowTemplate" => {
            let c = &a[1];
            let positional = strs_of(&c["positional"]);
            let named = index_map(c.get("named"));
            let steps = index_map(c.get("stepOutputs"));
            let ctx =
                workflow::InterpContext { input: c["input"].as_str().unwrap_or(""), positional: &positional, named: &named, prev_output: c["prevOutput"].as_str().unwrap_or(""), step_outputs: &steps };
            let shell = arg(a, 2).and_then(|o| o.get("mode")).and_then(Value::as_str) == Some("shell");
            Ok_(json!(workflow::interpolate_workflow_template(&s(a, 0), &ctx, shell)))
        }
        "workflow.parseMarkdownWorkflow" => {
            let scope: &'static str = if s(a, 2) == "user" { "user" } else { "workspace" };
            Ok_(workflow::parse_markdown_workflow(&s(a, 0), &s(a, 1), scope, std::path::Path::new(&s(a, 3))).to_value())
        }
        "workflow.parseWorkflowArgs" => {
            let defaults = arg(a, 1).map(|d| index_map(Some(d)));
            let r = workflow::parse_workflow_args(&s(a, 0), defaults.as_ref());
            Ok_(json!({ "fullInput": r.full_input, "positional": r.positional, "named": r.named }))
        }
        "workflow.shellEscape" => {
            let ctx = match arg(a, 1).and_then(Value::as_str) {
                Some("single") => 1,
                Some("double") => 2,
                _ => 0,
            };
            Ok_(json!(workflow::shell_escape(&s(a, 0), ctx)))
        }
        _ => Skip("no adapter"),
    }
}

/// Drops `undefined` object members and compares numbers numerically.
fn same(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            a == b || (a - b).abs() <= 1e-12 * a.abs().max(1.0)
        }
        (Value::Object(a), Value::Object(b)) if !is_undef(expected) && !is_undef(actual) => {
            let keys: BTreeSet<&String> = a.iter().chain(b.iter()).filter(|(_, v)| !is_undef(v)).map(|(k, _)| k).collect();
            keys.into_iter().all(|k| match (a.get(k).filter(|v| !is_undef(v)), b.get(k).filter(|v| !is_undef(v))) {
                (Some(x), Some(y)) => same(x, y),
                _ => false,
            })
        }
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y)),
        _ => expected == actual,
    }
}

/// Records whose expected value is a zod-normalized MCP server config are compared structurally.
fn same_codex(expected: &Value, actual: &Value) -> bool {
    let (Some(e), Some(a)) = (expected.as_object(), actual.as_object()) else { return false };
    e.len() == a.len()
        && e.iter().all(|(name, cfg)| {
            let parsed = mcp::parse_server(cfg).map(|c| format!("{c:?}")).unwrap_or_else(|e| e.to_string());
            a.get(name).and_then(|v| v.get("$config")).and_then(Value::as_str) == Some(parsed.as_str())
        })
}

fn short(v: &Value) -> String {
    let text = v.to_string();
    if text.chars().count() > 600 {
        format!("{}…", text.chars().take(600).collect::<String>())
    } else {
        text
    }
}

#[test]
fn typescript_parity() {
    let corpus = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/parity/corpus.jsonl")).expect("corpus");
    let mut checked = 0usize;
    let mut skipped: BTreeMap<String, (usize, &'static str)> = BTreeMap::new();
    let mut allowed = 0usize;
    let mut failures: Vec<String> = vec![];
    let mut per_fn: BTreeMap<String, usize> = BTreeMap::new();
    let only = std::env::var("PARITY_ONLY").ok();
    for line in corpus.lines().filter(|l| !l.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).expect("corpus line");
        let module = record["module"].as_str().unwrap();
        let func = record["fn"].as_str().unwrap();
        let key = format!("{module}.{func}");
        if only.as_deref().is_some_and(|o| !key.contains(o)) {
            continue;
        }
        let args = record["args"].as_array().cloned().unwrap_or_default();
        let decoded: Vec<Value> = args.iter().map(|a| if is_undef(a) { a.clone() } else { clean(a) }).collect();
        let outcome = std::panic::catch_unwind(|| call(module, func, &decoded));
        let encoded_args = Value::Array(args.clone()).to_string();
        let mismatch = match outcome {
            Err(_) => Some("panicked".to_string()),
            Ok(Skip(reason)) => {
                let entry = skipped.entry(key.clone()).or_insert((0, reason));
                entry.0 += 1;
                continue;
            }
            Ok(Ok_(actual)) => match (record.get("result"), record.get("error")) {
                (Some(expected), _) => {
                    let ok = if key == "mcp.parseCodexMcpServers" { same_codex(expected, &actual) } else { same(expected, &actual) };
                    (!ok).then(|| format!("expected {}\n      actual   {}", short(expected), short(&actual)))
                }
                (None, Some(error)) => Some(format!("expected error {}\n      actual   {}", short(error), short(&actual))),
                _ => None,
            },
            Ok(Err_(message)) => match record.get("error").and_then(Value::as_str) {
                Some(expected) if expected == message => None,
                Some(expected) => Some(format!("expected error {expected:?}\n      actual error {message:?}")),
                None => Some(format!("expected {}\n      actual error {message:?}", short(record.get("result").unwrap_or(&Value::Null)))),
            },
        };
        checked += 1;
        *per_fn.entry(key.clone()).or_default() += 1;
        if let Some(detail) = mismatch {
            if DIVERGENCES.iter().any(|(k, needle, _)| *k == key && encoded_args.contains(needle)) {
                allowed += 1;
                continue;
            }
            failures.push(format!("{key}({})\n      {detail}", short(&Value::Array(args))));
        }
    }
    eprintln!("parity: {checked} records checked across {} functions, {allowed} documented divergences", per_fn.len());
    for (key, (count, reason)) in &skipped {
        eprintln!("parity: skipped {count} {key} records ({reason})");
    }
    if !failures.is_empty() {
        let mut by_fn: BTreeMap<&str, usize> = BTreeMap::new();
        for f in &failures {
            *by_fn.entry(f.split('(').next().unwrap()).or_default() += 1;
        }
        let limit: usize = std::env::var("PARITY_SHOW").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
        panic!("{} parity mismatches {:?}:\n\n{}", failures.len(), by_fn, failures.iter().take(limit).cloned().collect::<Vec<_>>().join("\n\n"));
    }
}
