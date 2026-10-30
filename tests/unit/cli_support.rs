//! Setup wizard, footer helpers, Windows device names and the /goal system.
use crate::common::*;
use goon::session::{self as sess, GoalDraft, SessionGoal};

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn scripted(answers: Vec<&'static str>, then_cancel: bool) -> setup::AskFn {
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(answers)));
    Arc::new(move |_label: String| {
        let next = queue.lock().unwrap().pop_front();
        Box::pin(async move {
            match next {
                Some(a) => Ok(a.to_string()),
                None if then_cancel => Err(anyhow::Error::new(goon::term::InputCancelled)),
                None => Ok(String::new()),
            }
        })
    })
}

#[tokio::test]
async fn setup_wizard_discovers_models_dynamically_and_keeps_the_key_out_of_config() {
    assert_eq!(config::EFFORT_LEVELS, ["low", "medium", "high", "xhigh", "max"]);
    let f = fixture();
    let _env = fixture_env(&f);
    let output = Arc::new(Mutex::new(String::new()));
    let out = output.clone();
    let io = setup::SetupIo {
        write: Some(Arc::new(move |t: &str| out.lock().unwrap().push_str(t))),
        ask: Some(scripted(vec!["4", "https://openrouter.ai/api/v1", "sk-or-wizard-secret-key", "1", "xhigh"], false)),
        ask_secret: None,
        discover: Some(Arc::new(|url, key, _| {
            Box::pin(async move {
                assert_eq!(url, "https://openrouter.ai/api/v1");
                assert_eq!(key.as_deref(), Some("sk-or-wizard-secret-key"));
                vec![provider::DiscoveredModel { id: "dynamic/discovered-model-v1".into(), context_window: Some(200_000), ..Default::default() }]
            })
        })),
    };
    let updated = setup::run_setup_wizard(None, io).await.unwrap();
    assert_eq!(updated.provider, "openai-compatible");
    assert_eq!(updated.base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
    assert_eq!(updated.model.as_deref(), Some("dynamic/discovered-model-v1"));
    assert_eq!(updated.effort.as_deref(), Some("xhigh"));
    assert_match(&output.lock().unwrap(), "Setup saved");
    let raw = std::fs::read_to_string(f.config.join("goon/config.json")).unwrap();
    assert!(!raw.contains("sk-or-wizard-secret-key"));
    let rt = config::resolve_runtime(&updated, &f.cwd).unwrap();
    assert_eq!(rt.api_key, "sk-or-wizard-secret-key");
    assert_eq!(rt.effort.as_deref(), Some("xhigh"));
}

#[tokio::test]
async fn cancelling_the_wizard_midway_persists_neither_the_key_nor_the_config() {
    let f = fixture();
    let _env = fixture_env(&f);
    let io = setup::SetupIo {
        write: Some(Arc::new(|_| {})),
        ask: Some(scripted(vec!["4", "https://openrouter.ai/api/v1", "sk-cancelled-secret-key"], true)),
        ask_secret: None,
        discover: Some(Arc::new(|_, _, _| Box::pin(async { vec![] }))),
    };
    let error = setup::run_setup_wizard(None, io).await.unwrap_err();
    assert!(error.downcast_ref::<goon::term::InputCancelled>().is_some(), "{error}");
    for file in walk(&f.config) {
        assert!(!std::fs::read_to_string(&file).unwrap_or_default().contains("sk-cancelled-secret-key"), "secret leaked into {}", file.display());
        assert!(!file.ends_with("config.json"), "config must not be written on cancel");
    }
}

#[test]
fn format_token_count_and_read_git_branch_agree_with_oracles() {
    use goon::tui::text::format_token_count;
    for (n, want) in [(0.0, "0"), (999.0, "999"), (1000.0, "1k"), (1250.0, "1.3k"), (48_210.0, "48k"), (2_500_000.0, "2.5M")] {
        assert_eq!(format_token_count(n), want);
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let w = |p: &str, c: &str| {
        let p = root.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    };
    std::fs::create_dir_all(root.join("repo/src/deep")).unwrap();
    w("repo/.git/HEAD", "ref: refs/heads/feature/x\n");
    assert_eq!(cli::read_git_branch(&root.join("repo/src/deep")).as_deref(), Some("feature/x"));
    w("repo/.git/HEAD", "7869954a1b2c3d4e5f60718293a4b5c6d7e8f901\n");
    assert_eq!(cli::read_git_branch(&root.join("repo")).as_deref(), Some("7869954"));
    w("wt-gitdir/HEAD", "ref: refs/heads/wt-branch\n");
    w("wt/.git", &format!("gitdir: {}\n", root.join("wt-gitdir").display()));
    assert_eq!(cli::read_git_branch(&root.join("wt")).as_deref(), Some("wt-branch"));
    w("repo/.git/HEAD", "not a ref\n");
    assert_eq!(cli::read_git_branch(&root.join("repo")), None);
    let here = std::env::current_dir().unwrap();
    if let Ok(out) = std::process::Command::new("git").args(["rev-parse", "--abbrev-ref", "HEAD"]).current_dir(&here).output() {
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !branch.is_empty() && branch != "HEAD" {
            assert_eq!(cli::read_git_branch(&here), Some(branch));
        }
    }
}

#[test]
fn unified_diffs_are_never_capped() {
    let content: String = (1..=250).map(|i| format!("const line_{i} = {i};\n")).collect();
    let diff = files::compute_unified_diff("src/big.ts", "", &content, 3);
    assert_match(&diff, r"--- a/src/big\.ts \(new \+250 -0\)");
    assert_eq!(diff.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count(), 250);
}

#[test]
fn windows_reserved_device_names_are_rejected() {
    let re = files::windows_reserved_device_re();
    assert!(re.is_match("nul") && re.is_match("NUL.txt") && re.is_match("CON"));
    assert!(!re.is_match("normal.ts"));
    #[cfg(windows)]
    {
        assert_eq!(tools::sanitize_bash_command("dir /a /b 2>nul"), "dir /a /b 2>/dev/null");
        assert_eq!(tools::sanitize_bash_command("git status >nul 2>&1"), "git status >/dev/null 2>&1");
        assert_eq!(tools::sanitize_bash_command("cmd >> NUL"), "cmd >> /dev/null");
        assert_eq!(tools::sanitize_bash_command("cat foo >/dev/nul"), "cat foo >/dev/null");
    }
    let f = fixture();
    let ws = files::WorkspaceFiles::new(&f.cwd);
    for path in ["nul", "src/CON.txt"] {
        assert_match(&ws.write(path, "bad").unwrap_err().to_string(), "Windows reserved device name");
    }
}

fn sample_goal(status: &str) -> SessionGoal {
    let now = jsutil::now_iso();
    serde_json::from_value(json!({ "goalId": "goal_test1", "objective": "Refactor parser and pass tests", "status": status, "createdAt": now, "updatedAt": now, "tokensAtStart": 0, "tokensUsed": 12500, "timeUsedSeconds": 180, "tokenBudget": 50000 })).unwrap()
}

#[test]
fn goal_formatting_draft_parsing_and_steering_prompts() {
    assert_eq!(sess::format_goal_elapsed_seconds(45.0), "45s");
    assert_eq!(sess::format_goal_elapsed_seconds(125.0), "2m");
    assert_eq!(sess::format_goal_elapsed_seconds(3720.0), "1h 2m");
    assert_eq!(sess::format_goal_tokens_compact(950.0), "950");
    assert_eq!(sess::format_goal_tokens_compact(12_500.0), "12.5K");
    assert_eq!(sess::format_goal_tokens_compact(1_500_000.0), "1.5M");
    assert_eq!(sess::parse_goal_draft_args("--budget 50k Fix all failing tests"), GoalDraft::Ok { objective: "Fix all failing tests".into(), token_budget: Some(50_000.0) });
    assert_eq!(sess::parse_goal_draft_args("--token-budget=1.5m Ship v1"), GoalDraft::Ok { objective: "Ship v1".into(), token_budget: Some(1_500_000.0) });
    assert!(matches!(sess::parse_goal_draft_args("--budget invalid Fix tests"), GoalDraft::Error(_)));
    let goal = sample_goal("active");
    assert_eq!(sess::format_goal_status_indicator(&goal, 0.0), "Pursuing goal (12.5K / 50K)");
    assert_eq!(sess::format_goal_status_indicator(&sample_goal("paused"), 0.0), "Goal paused (/goal resume)");
    assert_eq!(sess::format_goal_status_indicator(&sample_goal("blocked"), 0.0), "Goal stalled (/goal resume)");
    assert_eq!(sess::format_goal_status_indicator(&sample_goal("complete"), 0.0), "Goal achieved (12.5K tokens)");
    let summary = sess::format_goal_summary_lines(&goal, 0.0);
    assert_match(&summary, "Status: active");
    assert_match(&summary, "Token budget: 50K");
    let cont = context::build_goal_continuation_prompt(&goal, true);
    assert!(cont.contains("<codex_internal_context source=\"goal\">"));
    assert!(cont.contains("Completion audit:") && cont.contains("update_goal"));
    assert_match(&context::build_goal_budget_limit_prompt(&goal), "The active thread goal has reached its token budget");
    assert_match(&context::build_goal_objective_updated_prompt(&goal), "The active thread goal objective was edited by the user");
    assert!(context::build_user_goal_context_message(false, Some("New obj"), Some("active")).contains("<codex_internal_context source=\"user_goal\">"));
}

#[tokio::test]
async fn the_harness_executes_create_get_and_update_goal_tools() {
    let f = fixture();
    let p = provider(|_, n| match n {
        1 => calls(vec![tc("cg1", "create_goal", json!({ "objective": "Verify goal tools end-to-end" }))]),
        2 => calls(vec![tc("gg1", "get_goal", json!({})), tc("ug1", "update_goal", json!({ "status": "complete" }))]),
        _ => text("Goal completed and verified."),
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "maxOutputTokens": 4096, "runTokenBudget": 0, "inlineReasoning": "leading-think", "showReasoning": false }));
    let st = store(&rt);
    let (emit, log) = events();
    let h = harness::Harness::new(rt, st.clone(), registry(&f.cwd, tools::approve_all(), false), emit, None, Default::default()).unwrap();
    assert_eq!(h.run("Create and complete a session goal.", &token(), None).await.unwrap(), "Goal completed and verified.");
    let goal = st.session.lock().unwrap().goal.clone().unwrap();
    assert_eq!(goal.objective, "Verify goal tools end-to-end");
    assert_eq!(goal.status, "complete");
    assert!(log.lock().unwrap().iter().any(|e| e["type"] == "goal" && e["goal"]["status"] == "complete"));
}
