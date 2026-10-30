use crate::common::*;
use goon::context::estimated_tokens;
use goon::context_budget::*;

fn cfg(context_window: u64, max_output_tokens: u64) -> ContextBudgetConfig {
    ContextBudgetConfig { context_window, max_output_tokens, reserve_ratio: 0.15, conversation_ratio: 0.45, files_ratio: 0.25 }
}

fn base() -> ContextBudgetConfig {
    cfg(32768, 4096)
}

fn tool_call(id: &str) -> Value {
    json!({ "role": "assistant", "content": [{ "type": "tool-call", "toolCallId": id, "toolName": "bash", "input": {} }] })
}

fn tool_result(id: &str, value: &str) -> Value {
    json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": id, "toolName": "bash", "output": { "type": "text", "value": value } }] })
}

fn user(text: &str) -> Value {
    json!({ "role": "user", "content": text })
}

fn prepare(messages: &[Value], system: &str, config: &ContextBudgetConfig, context: Option<&MemoryPromptContext>) -> Result<ContextBudgetResult, ContextBudgetError> {
    prepare_context_budget(messages, system, config, context)
}

#[test]
fn config_defaults_for_the_three_ratios() {
    let config = config::parse_config(&json!({})).unwrap();
    assert_eq!(config.context_reserve_ratio, 0.15);
    assert_eq!(config.context_conversation_ratio, 0.45);
    assert_eq!(config.context_files_ratio, 0.25);
}

#[test]
fn allocation_invariant_parts_sum_to_the_input_budget() {
    for (window, out) in [(8192, 512), (8192, 128), (32768, 4096), (128000, 16384), (200000, 32000)] {
        let allocation = allocate_context_budget(&cfg(window, out), &format!("You are Goon. {}", "x".repeat(100)));
        assert_eq!(allocation.input_budget, (window - out).max(1024));
        assert_eq!(allocation.reserve + allocation.system_budget + allocation.conversation + allocation.files, allocation.input_budget, "{window}/{out}");
        assert!(allocation.conversation > 0 && allocation.files > 0);
    }
}

#[test]
fn minimum_context_never_truncates_the_base_system_prompt() {
    let config = cfg(8192, 512);
    let base_system = format!(
        "You are Goon, an independent terminal coding agent. Work in /tmp/proj.\nComplete the user's requested coding task, inspecting files before changing them.\nPrefer small, reviewable changes. Run relevant checks and report actual results.\nUse the provided tools; never claim an action succeeded without a tool result.\n{}",
        "Additional operating rules. ".repeat(90)
    );
    let base_tokens = estimated_tokens(&[], &base_system);
    assert!(base_tokens > (7680.0f64 * 0.15).floor() as u64);
    let allocation = allocate_context_budget(&config, &base_system);
    assert_eq!(allocation.input_budget, 7680);
    assert_eq!(allocation.system_budget, base_tokens.min((7680.0f64 * 0.35).floor() as u64));
    assert!(!allocation.truncated);
    let prepared = prepare(&[user("hi")], &base_system, &config, None).unwrap();
    assert_eq!(prepared.system, base_system);
    assert!(prepared.stats.after <= prepared.stats.input_budget);
}

#[test]
fn truncate_system_cuts_guidance_and_memories_first_identity_last() {
    let identity = "You are Goon, an independent terminal coding agent. Keep safety rules.";
    let memories = format!("\nPersistent user memories:\n{}", "- memory line\n".repeat(200));
    let guidance = format!("\nWorkspace guidance (AGENTS.md):\n{}", "workspace rule line\n".repeat(400));
    let system = format!("{identity}{memories}{guidance}");
    let full = estimated_tokens(&[], &system);
    let guidance_tokens = estimated_tokens(&[], &guidance);
    let trimmed = truncate_system_to_budget(&system, full - guidance_tokens + 20);
    assert!(!trimmed.contains("workspace rule line"));
    assert!(trimmed.contains("Persistent user memories:"));
    assert!(trimmed.starts_with(identity));
    let tightest = truncate_system_to_budget(&system, estimated_tokens(&[], identity) + 10);
    assert!(!tightest.contains("Persistent user memories:"));
    assert!(tightest.contains("Keep safety rules."));
    assert!(truncate_system_to_budget(&system, 10).contains("[system prompt truncated to fit the context budget]"));
}

#[test]
fn per_result_prune_keeps_pairing_and_shrinks_oversized_outputs() {
    let big = format!("{}END-MARKER", "a".repeat(8192));
    let messages = vec![tool_call("c1"), tool_result("c1", &big), user("summarize")];
    let (pruned, did) = prune_tool_results(&messages, 8192);
    assert!(did);
    assert_eq!(pruned.len(), messages.len());
    assert_eq!(pruned[1]["content"].as_array().unwrap().len(), 1);
    let value = pruned[1]["content"][0]["output"]["value"].as_str().unwrap();
    assert!(value.len() < big.len());
    assert!(value.contains("[... tool result middle pruned to fit context budget ...]"));
    assert!(value.contains("END-MARKER"));
    assert!(!value.contains(&"aaaaaa".repeat(2000)));
    let (untouched, did) = prune_tool_results(&[tool_result("c2", "short")], 8192);
    assert!(!did);
    assert_eq!(untouched[0]["content"][0]["output"]["value"], "short");
    assert_eq!(messages[1]["content"][0]["output"]["value"], big.as_str());
}

#[test]
fn two_hundred_large_tool_results_prune_to_the_conversation_budget_with_no_orphans() {
    let mut messages = vec![user("run the sweep")];
    for i in 0..200 {
        messages.push(tool_call(&format!("c{i}")));
        messages.push(tool_result(&format!("c{i}"), &format!("result-{i}-{}{}", format!("{i:0>4}").repeat(600), "x".repeat(7500))));
    }
    messages.push(user("summarize what happened"));
    let prepared = prepare(&messages, "", &cfg(32768, 4096), None).unwrap();
    assert!(prepared.stats.after <= prepared.stats.input_budget, "after={} budget={}", prepared.stats.after, prepared.stats.input_budget);
    let mut call_ids = HashSet::new();
    for msg in &prepared.messages {
        if msg["role"] == "assistant" {
            for part in msg["content"].as_array().into_iter().flatten() {
                if part["type"] == "tool-call" {
                    call_ids.insert(part["toolCallId"].as_str().unwrap().to_string());
                }
            }
        }
    }
    let mut saw_omitted = false;
    for msg in prepared.messages.iter().filter(|m| m["role"] == "tool") {
        for part in msg["content"].as_array().unwrap() {
            assert!(call_ids.contains(part["toolCallId"].as_str().unwrap()));
            if part["output"]["value"].as_str().unwrap().contains("[tool result omitted to fit context; re-run if needed]") {
                saw_omitted = true;
            }
        }
    }
    assert!(saw_omitted);
    assert!(prepared.messages.iter().any(|m| m["role"] == "user" && m["content"] == "summarize what happened"));
    assert!(prepared.stats.truncated);
}

#[test]
fn duplicate_tool_results_collapse_to_a_marker_keeping_the_part() {
    let value = "identical-output-".repeat(80);
    let messages = vec![tool_call("a"), tool_result("a", &value), tool_call("b"), tool_result("b", &value), tool_call("c"), tool_result("c", &"different-output-".repeat(60))];
    let prepared = prepare(&messages, "", &base(), None).unwrap();
    let results: Vec<String> = prepared.messages.iter().filter(|m| m["role"] == "tool").map(|m| m["content"][0]["output"]["value"].as_str().unwrap().to_string()).collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], value);
    assert_eq!(results[1], "[duplicate tool result omitted: same as earlier call]");
    assert_ne!(results[2], "[duplicate tool result omitted: same as earlier call]");
}

fn call_still_present(messages: &[Value], idx: usize) -> bool {
    let ids: HashSet<&str> = messages[idx]["content"].as_array().unwrap().iter().filter_map(|p| p["toolCallId"].as_str()).collect();
    messages[..idx]
        .iter()
        .rev()
        .filter(|m| m["role"] == "assistant")
        .any(|m| m["content"].as_array().into_iter().flatten().any(|p| p["type"] == "tool-call" && ids.contains(p["toolCallId"].as_str().unwrap_or(""))))
}

#[test]
fn aggregate_prune_keeps_the_newest_user_and_assistant_and_never_orphans_tool_results() {
    let final_q = format!("final question {}", "new ".repeat(400));
    let working = format!("working on it {}", "q".repeat(300));
    let messages = vec![
        user(&format!("first request {}", "old ".repeat(500))),
        tool_call("old-call"),
        tool_result("old-call", &format!("old result {}", "z".repeat(400))),
        user(&format!("second request {}", "mid ".repeat(500))),
        json!({ "role": "assistant", "content": working }),
        user(&final_q),
    ];
    let budget = estimated_tokens(&[user(&final_q), json!({ "role": "assistant", "content": working })], "") + 50;
    let pruned = prune_conversation(&messages, budget).unwrap();
    assert_eq!(pruned.last().unwrap()["content"], final_q.as_str());
    assert!(pruned[pruned.len() - 2]["content"].as_str().unwrap().starts_with("working on it"));
    assert_ne!(pruned[0]["role"], "tool");
    for (i, msg) in pruned.iter().enumerate() {
        if msg["role"] == "tool" {
            assert!(call_still_present(&pruned, i));
        }
    }
    assert!(pruned.len() < messages.len());
    assert_eq!(messages.len(), 6);
}

#[test]
fn untranscribable_conversation_errors_and_never_exceeds_budget() {
    let config = cfg(8192, 512);
    let huge = "g".repeat(60_000);
    let messages = vec![user("old small"), json!({ "role": "assistant", "content": "ok" }), user(&huge)];
    assert!(prepare(&messages, "", &config, None).is_err());
    assert!(prune_conversation(&messages, 1000).is_err());
    assert!(prune_conversation(&[user(&huge)], 1000).is_err());
}

#[test]
fn retrieved_memory_is_injected_as_an_untrusted_user_block_never_in_the_system_prompt() {
    let context = MemoryPromptContext {
        rag: vec![
            RagChunk { path: "src/a.ts".into(), start_line: 1, end_line: 12, content: "export const alphaToken = \"A\".repeat(300);".into(), score: 0.91 },
            RagChunk { path: "src/b.ts".into(), start_line: 1, end_line: 12, content: "export const betaToken = \"B\".repeat(300);".into(), score: 0.42 },
        ],
        repo_tree: Some("src/\n  a.ts\n  b.ts".into()),
        working_notes: Some("Task: wire the widget".into()),
        preferences: Some(vec!["style: concise".into()]),
    };
    let prepared = prepare(&[user("wire the widget")], "You are Goon.", &base(), Some(&context)).unwrap();
    let last = prepared.messages.last().unwrap();
    assert_eq!(last["role"], "user");
    let text = last["content"].as_str().unwrap_or("");
    assert!(text.starts_with("<untrusted_context source=\"rag,repo\" trust=\"untrusted\">"));
    assert!(text.contains("not instructions"));
    assert!(text.contains("src/a.ts") && text.contains("betaToken"));
    assert!(text.contains("<repo_tree>") && text.contains("Task: wire the widget") && text.contains("style: concise"));
    assert!(!prepared.system.contains("alphaToken"));
    assert!(prepared.stats.after <= prepared.stats.input_budget);
    assert!(prepared.stats.files > 0);
}

#[test]
fn oversized_memory_context_is_trimmed_to_the_files_budget_lowest_score_first() {
    let config = cfg(8192, 512);
    let context = MemoryPromptContext {
        rag: (0..8).map(|i| RagChunk { path: format!("src/f{i}.ts"), start_line: 1, end_line: 20, content: format!("chunk {i}: {}", "payload".repeat(200)), score: 1.0 - i as f64 * 0.1 }).collect(),
        ..Default::default()
    };
    let prepared = prepare(&[user("explore")], "", &config, Some(&context)).unwrap();
    let last = prepared.messages.last().unwrap();
    assert_eq!(last["role"], "user");
    let text = last["content"].as_str().unwrap_or("");
    assert!(text.contains("chunk 0:"));
    assert!(text.len() < 8000, "block was trimmed (got {})", text.len());
    assert!(prepared.stats.after <= prepared.stats.input_budget);
    let none = ContextBudgetConfig { context_window: 8192, max_output_tokens: 512, reserve_ratio: 0.5, conversation_ratio: 1.0, files_ratio: 0.0 };
    let no_files = prepare(&[user("explore")], "", &none, Some(&context)).unwrap();
    let last = no_files.messages.last().unwrap();
    assert_eq!(last["role"], "user");
    assert!(!last["content"].as_str().unwrap_or("").contains("<untrusted_context"));
}

#[test]
fn referenced_files_are_truncated_to_the_files_budget_with_a_read_file_marker() {
    let config = cfg(8192, 512);
    let block = |body: &str| format!("<referenced_file path=\"src/big.ts\">\n{body}\n</referenced_file>");
    let messages = vec![user(&format!("explain\n\n{}\n\n{}", block(&"r".repeat(5000)), block(&"s".repeat(5000))))];
    let prepared = prepare(&messages, "", &config, None).unwrap();
    let text = prepared.messages[0]["content"].as_str().unwrap();
    assert!(text.contains("[... referenced file truncated to fit the context budget; use read_file for the full content ...]"));
    assert!(text.contains("[referenced file omitted: context budget]"));
    let re = regex::Regex::new(r"(?s)<referenced_file[^>]*>(.*?)</referenced_file>").unwrap();
    let marker = regex::Regex::new(r"\[[^\]]*\]").unwrap();
    let kept: Vec<usize> = re.captures_iter(text).map(|c| marker.replace_all(&c[1], "").len()).collect();
    assert!(kept[0] > 0 && kept[0] <= 3842 + 120, "first block truncated to budget (got {})", kept[0]);
    assert!(prepared.stats.after <= prepared.stats.input_budget);
    assert!(prepared.stats.reason.as_deref().unwrap_or("").contains("referenced files truncated"));
    assert!(messages[0]["content"].as_str().unwrap().contains(&"r".repeat(5000)));
}

#[test]
fn prepare_never_mutates_the_input_messages() {
    let messages = vec![tool_call("x1"), tool_result("x1", &"m".repeat(9000)), user("go")];
    let snapshot = serde_json::to_string(&messages).unwrap();
    prepare(&messages, "sys", &cfg(8192, 512), None).unwrap();
    assert_eq!(serde_json::to_string(&messages).unwrap(), snapshot);
}

#[test]
fn stats_report_before_after_and_truncation_reasons() {
    let messages = vec![tool_call("d1"), tool_result("d1", &"x".repeat(20_000)), user("done")];
    let prepared = prepare(&messages, "", &base(), None).unwrap();
    assert_eq!(prepared.stats.input_budget, 28672);
    assert!(prepared.stats.before > prepared.stats.after);
    assert!(prepared.stats.truncated);
    let reason = prepared.stats.reason.clone().unwrap_or_default();
    assert_match(&reason, "tool results pruned");
    assert!(!reason.contains("system truncated"));
}
