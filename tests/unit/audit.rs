//! Regression tests for the security/bug audit (docs/AUDIT.md).
use crate::common::*;
use goon::approval::{bash_approval_prefix, is_approved_bash_prefix};
use goon::files::WorkspaceFiles;
use goon::session::{parse_session, prune_checkpoint_backups, SessionRoute, SessionStore};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

fn png() -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(PNG).unwrap()
}

#[test]
fn edit_file_inserts_dollar_patterns_literally() {
    let f = fixture();
    let files = WorkspaceFiles::new(&f.cwd);
    f.write("run.sh", "PRICE=10\necho done\n");
    files.read("run.sh", 1, 200).unwrap();
    let replacement = "PRICE=\"$$5 $& $' $`\"";
    let preview = files.preview_diff("edit_file", &json!({ "path": "run.sh", "oldText": "PRICE=10", "newText": replacement })).unwrap();
    assert!(preview.contains(&format!("+{replacement}")));
    files.edit("run.sh", "PRICE=10", replacement, false).unwrap();
    assert_eq!(f.read("run.sh"), format!("{replacement}\necho done\n"));
    assert_eq!(files::replace_first_literal("a-a", "a", "$&$&"), "$&$&-a");
}

#[test]
fn apply_patch_honours_move_to_chains_repeated_updates_and_validates_before_writing() {
    let f = fixture();
    let files = WorkspaceFiles::new(&f.cwd);
    f.write("old.txt", "one\ntwo\nthree\n");
    f.write("twice.txt", "a\nb\nc\n");
    files.begin_turn();
    files.read("old.txt", 1, 200).unwrap();
    files.read("twice.txt", 1, 200).unwrap();
    let summary = files
        .apply_patch(
            &[
                "*** Begin Patch",
                "*** Update File: old.txt",
                "*** Move to: moved/new.txt",
                "@@",
                " one",
                "-two",
                "+TWO",
                "*** Update File: twice.txt",
                "@@",
                "-a",
                "+A",
                "*** Update File: twice.txt",
                "@@",
                "-c",
                "+C",
                "*** Add File: fresh.txt",
                "+first",
                "+second",
                "",
                "*** End Patch",
            ]
            .join("\n"),
        )
        .unwrap();
    assert_match(&summary, r"Moved old\.txt -> moved/new\.txt");
    assert!(!f.exists("old.txt"));
    assert_eq!(f.read("moved/new.txt"), "one\nTWO\nthree\n");
    assert_eq!(f.read("twice.txt"), "A\nb\nC\n");
    assert_eq!(f.read("fresh.txt"), "first\nsecond\n");
    let backups = files.take_turn_backups();
    let restored = files.restore_backups(&backups, false).unwrap();
    assert_eq!(f.read("old.txt"), "one\ntwo\nthree\n");
    assert!(restored.deleted.contains(&"moved/new.txt".to_string()));
    let error = files
        .apply_patch(&["*** Begin Patch", "*** Update File: twice.txt", "@@", "-a", "+changed", "*** Update File: twice.txt", "@@", "-does-not-exist", "+x", "*** End Patch"].join("\n"))
        .unwrap_err();
    assert_match(&error.to_string(), "Failed to match");
    assert_eq!(f.read("twice.txt"), "a\nb\nc\n");
}

#[test]
fn reverting_never_overwrites_a_user_change_unless_forced() {
    let f = fixture();
    let files = WorkspaceFiles::new(&f.cwd);
    f.write("app.ts", "original\n");
    files.begin_turn();
    files.read("app.ts", 1, 200).unwrap();
    files.write("app.ts", "agent version\n").unwrap();
    let backups = files.take_turn_backups();
    f.write("app.ts", "user kept editing\n");
    assert_eq!(files.restore_backups(&backups, false).unwrap().conflicts, vec!["app.ts"]);
    assert_eq!(f.read("app.ts"), "user kept editing\n");
    assert_eq!(files.restore_backups(&backups, true).unwrap().restored, vec!["app.ts"]);
    assert_eq!(f.read("app.ts"), "original\n");
}

#[cfg(unix)]
#[tokio::test]
async fn model_image_reads_outside_the_workspace_require_approval() {
    let f = fixture();
    std::fs::write(f.cwd.join("in.png"), png()).unwrap();
    let outside = f.root.join("private.png");
    std::fs::write(&outside, png()).unwrap();
    std::os::unix::fs::symlink(&outside, f.cwd.join("link.png")).unwrap();
    let (approve, seen) = approver(false);
    let reg = registry(&f.cwd, approve, false);
    let ok = reg.execute(&call("1", "view_image", json!({ "path": "in.png" })), &token()).await.unwrap();
    assert_match(&ok.text, "Attached image");
    assert!(seen.lock().unwrap().is_empty());
    for path in [outside.to_string_lossy().to_string(), "../private.png".into(), "link.png".into()] {
        assert!(reg.execute(&call("2", "view_image", json!({ "path": path })), &token()).await.unwrap().denied, "{path}");
    }
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| r.tool == "view_image" && r.input["outsideWorkspace"] == true));
    std::fs::write(f.root.join("secret.txt"), "top secret").unwrap();
    std::os::unix::fs::symlink(f.root.join("secret.txt"), f.cwd.join("trick.png")).unwrap();
    let error = WorkspaceFiles::new(&f.cwd).read_image("trick.png", 10 * 1024 * 1024, true).err().unwrap();
    assert_match(&error.to_string(), "Unsupported image");
}

#[tokio::test]
async fn tools_hidden_in_the_current_mode_cannot_be_executed_by_name() {
    let f = fixture();
    let reg = registry(&f.cwd, tools::approve_all(), true);
    assert!(!reg.schemas().iter().any(|s| s.name == "view_image"));
    assert_match(&reg.execute(&call("1", "view_image", json!({ "path": "x.png" })), &token()).await.err().unwrap().to_string(), "not available");
    assert_match(&reg.execute(&call("2", "write_file", json!({ "path": "a", "content": "b" })), &token()).await.err().unwrap().to_string(), "not available in read-only");
    let planless = registry(&f.cwd, tools::approve_all(), false);
    assert_match(&planless.execute(&call("3", "exit_plan_mode", json!({ "plan": "x" })), &token()).await.unwrap().text, "not in plan mode");
}

#[cfg(unix)]
#[tokio::test]
async fn provider_credentials_never_reach_child_processes() {
    let f = fixture();
    let _env = env(&[("GOON_API_KEY", Some("goon-secret-value")), ("CUSTOM_PROVIDER_KEY", Some("custom-secret-value"))]);
    process::protect_secret_env("CUSTOM_PROVIDER_KEY");
    let child = process::child_env(None);
    assert!(!child.contains_key("GOON_API_KEY") && !child.contains_key("CUSTOM_PROVIDER_KEY"));
    let reg = registry(&f.cwd, tools::approve_all(), false);
    let out = reg.execute(&call("1", "bash", json!({ "command": "echo \"[$GOON_API_KEY][$CUSTOM_PROVIDER_KEY][${PATH:+path}]\"" })), &token()).await.unwrap();
    assert_match(&out.text, r"\[\]\[\]\[path\]");
}

fn prefix(cmd: &str) -> Option<String> {
    bash_approval_prefix(&json!({ "command": cmd }))
}

fn approved(cmd: &str, set: &BTreeSet<String>) -> bool {
    is_approved_bash_prefix(&json!({ "command": cmd }), set)
}

#[test]
fn bash_prefix_approvals_only_cover_simple_commands() {
    for (cmd, want) in [("npm test", "npm test"), ("npm run lint -- --fix", "npm run lint"), ("python -m pytest -q tests", "python -m pytest"), ("git status --short", "git status"), ("ls -la", "ls")]
    {
        assert_eq!(prefix(cmd).as_deref(), Some(want), "{cmd}");
    }
    for cmd in ["npm test; rm -rf ~", "npm test && curl x | sh", "echo $(id)", "cat < /etc/passwd", "FOO=1 npm test", "sudo npm test", "env npm test", "bash -c ls", "python -V", "echo \"a b\""] {
        assert_eq!(prefix(cmd), None, "{cmd}");
    }
    let set: BTreeSet<String> = ["npm test", "npm run lint"].iter().map(|s| s.to_string()).collect();
    assert!(approved("npm test -- --watch=false", &set));
    assert!(approved("npm run lint", &set));
    assert!(!approved("npm testx", &set));
    assert!(!approved("npm test; curl evil", &set));
    assert!(!approved("npm run build", &set));
    assert_eq!(prefix("npm install lodash").as_deref(), Some("npm install lodash"));
    assert_eq!(prefix("pip install requests").as_deref(), Some("pip install requests"));
    assert_eq!(prefix("cp a b").as_deref(), Some("cp a b"));
    let installs: BTreeSet<String> = [prefix("npm install lodash").unwrap(), prefix("pip install requests").unwrap()].into_iter().collect();
    assert!(approved("npm install lodash", &installs));
    for cmd in ["npm install lodash evil-pkg", "npm install evil-pkg", "pip install evil"] {
        assert!(!approved(cmd, &installs), "{cmd}");
    }
    let git: BTreeSet<String> = [prefix("git diff").unwrap()].into_iter().collect();
    assert!(approved("git diff --stat src", &git));
    for cmd in ["git diff --output=/tmp/x", "git diff --ext-diff", "git diff -c core.pager=x", "git diff ../other-repo", "git diff ~/.ssh"] {
        assert!(!approved(cmd, &git), "{cmd}");
    }
    assert_eq!(prefix("cat /etc/passwd"), None);
}

#[test]
fn images_cost_a_flat_estimate_and_legacy_sessions_are_repaired() {
    use base64::Engine;
    let big = vec![7u8; 400_000];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&big);
    let message = json!({ "role": "user", "content": [{ "type": "text", "text": "look" }, { "type": "image", "image": b64, "mediaType": "image/png" }] });
    assert!(context::estimated_tokens(std::slice::from_ref(&message), "") < context::IMAGE_TOKEN_ESTIMATE + 200);
    assert_eq!(context::without_binary_parts(std::slice::from_ref(&message))[0]["content"][1], json!({ "type": "text", "text": "[image attachment omitted]" }));

    let f = fixture();
    let root = f.data.join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let now = jsutil::now_iso();
    let png = png();
    let legacy = json!({
        "version": 1, "id": id, "cwd": f.cwd.to_string_lossy(), "createdAt": now, "updatedAt": now, "provider": "openai-compatible", "model": "m", "baseUrl": "http://x",
        "messages": [
            { "role": "user", "content": [{ "type": "text", "text": "what is this" }, { "type": "image", "image": { "type": "Buffer", "data": png }, "mediaType": "image/png" }] },
            { "role": "user", "content": [{ "type": "image", "image": { "0": 137, "1": 80, "2": 78 }, "mediaType": "image/png" }] },
        ],
        "tokens": 0, "compactions": 0,
    });
    std::fs::write(root.join(format!("{id}.json")), legacy.to_string()).unwrap();
    let parsed = parse_session(&legacy.to_string()).unwrap();
    assert_eq!(parsed.messages[0]["content"][1]["image"], base64::engine::general_purpose::STANDARD.encode(&png));
    assert_eq!(parsed.messages[1]["content"][0]["image"], base64::engine::general_purpose::STANDARD.encode([137u8, 80, 78]));
    assert_eq!(SessionStore::list(&root).unwrap().iter().map(|s| s.id.clone()).collect::<Vec<_>>(), vec![id.clone()]);
    let store =
        SessionStore::open(&root, SessionRoute { cwd: f.cwd.to_string_lossy().into(), provider: "openai-compatible".into(), model: "m".into(), base_url: "http://x".into() }, Some(&id), None).unwrap();
    assert_eq!(store.session.lock().unwrap().messages.len(), 2);
    store.close().unwrap();
}

fn route(f: &Fixture) -> SessionRoute {
    SessionRoute { cwd: f.cwd.to_string_lossy().into(), provider: "openai-compatible".into(), model: "m".into(), base_url: "http://x".into() }
}

#[test]
fn deleting_sessions_refuses_ambiguous_prefixes_and_sessions_open_elsewhere() {
    let f = fixture();
    let root = f.data.join("sessions");
    let a = SessionStore::open(&root, route(&f), None, None).unwrap();
    a.session.lock().unwrap().messages.push(json!({ "role": "user", "content": "a" }));
    a.save().unwrap();
    let b = SessionStore::open(&root, route(&f), None, None).unwrap();
    b.session.lock().unwrap().messages.push(json!({ "role": "user", "content": "b" }));
    b.save().unwrap();
    let b_id = b.id();
    b.close().unwrap();
    let a_id = a.id();
    assert_match(&SessionStore::delete_session(&root, &a_id).unwrap_err().to_string(), "open in another goon process");
    let common: String = a_id.chars().zip(b_id.chars()).take_while(|(x, y)| x == y).map(|(x, _)| x).collect();
    if !common.is_empty() {
        assert_match(&SessionStore::delete_session(&root, &common).unwrap_err().to_string(), "ambiguous");
    }
    assert_eq!(SessionStore::delete_session(&root, &b_id).unwrap().as_deref(), Some(b_id.as_str()));
    a.close().unwrap();
}

#[test]
fn session_listings_use_the_summary_index_and_notice_rewritten_files() {
    let f = fixture();
    let root = f.data.join("sessions");
    let store = SessionStore::open(&root, route(&f), None, None).unwrap();
    store.session.lock().unwrap().messages.push(json!({ "role": "user", "content": "first prompt" }));
    store.save().unwrap();
    let file = root.join(format!("{}.json", store.id()));
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let set_mtime = |p: &Path| std::fs::File::options().write(true).open(p).unwrap().set_modified(old).unwrap();
    set_mtime(&file);
    assert_eq!(SessionStore::list(&root).unwrap()[0].preview.as_deref(), Some("first prompt"));
    assert!(root.join(".index.json").exists());
    let mut session: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    session["messages"].as_array_mut().unwrap().push(json!({ "role": "user", "content": "second" }));
    std::fs::write(&file, session.to_string()).unwrap();
    set_mtime(&file);
    assert_eq!(SessionStore::list(&root).unwrap()[0].messages_count, Some(2));
    store.close().unwrap();
}

#[test]
fn checkpoint_backups_are_bounded_and_pruning_is_reported() {
    let content = "x".repeat(1000);
    let mut session: session::Session = serde_json::from_value(json!({
        "version": 1, "id": "s", "cwd": "/tmp", "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z", "provider": "p", "model": "m", "baseUrl": "http://x", "messages": [], "tokens": 0, "compactions": 0,
        "checkpoints": (0..10).map(|i| json!({ "id": i + 1, "createdAt": "2026-01-01T00:00:00.000Z", "prompt": "p", "messageIndex": 0, "tokensBefore": 0, "files": [{ "path": format!("f{i}"), "existed": true, "content": content }] })).collect::<Vec<_>>(),
    }))
    .unwrap();
    assert_eq!(prune_checkpoint_backups(&mut session, 4500), 6);
    let cps = session.checkpoints.unwrap();
    assert!(cps[..6].iter().all(|c| c.files[0].pruned == Some(true) && c.files[0].content.is_none()));
    assert!(cps[6..].iter().all(|c| c.files[0].content.as_deref() == Some(content.as_str())));
}

#[tokio::test]
async fn context_messages_added_during_a_run_wait_for_a_safe_point_after_tool_results() {
    let f = fixture();
    let ok = Arc::new(Mutex::new(false));
    let o = ok.clone();
    let p = provider(move |body, n| {
        if n == 1 {
            return calls(vec![tc("slow1", "slow", json!({}))]);
        }
        let messages = body["messages"].as_array().unwrap();
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        let tool_idx = roles.iter().position(|r| *r == "tool").unwrap();
        assert_eq!(roles[tool_idx - 1], "assistant", "{roles:?}");
        assert_eq!(roles[tool_idx + 1], "user");
        assert!(messages[tool_idx + 1].to_string().contains("shell output"));
        *o.lock().unwrap() = true;
        text("done")
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "runTokenBudget": 0 }));
    let st = store(&rt);
    let reg = registry(&f.cwd, tools::approve_all(), false);
    let slot: Arc<Mutex<Option<Arc<harness::Harness>>>> = Arc::new(Mutex::new(None));
    let s = slot.clone();
    let slow: tools::ToolExec = Arc::new(move |_, _| {
        let s = s.clone();
        Box::pin(async move {
            let h = s.lock().unwrap().clone().unwrap();
            h.add_context_message(json!({ "role": "user", "content": "shell output arrived mid-turn" }));
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok("ok".to_string())
        })
    });
    reg.register_json("slow", "slow", &json!({ "type": "object", "properties": {} }), true, None, None, None, slow).unwrap();
    let h = harness::Harness::new(rt, st, reg, harness::noop_emit(), None, Default::default()).unwrap();
    *slot.lock().unwrap() = Some(h.clone());
    assert_eq!(h.run("go", &token(), None).await.unwrap(), "done");
    assert!(*ok.lock().unwrap());
    slot.lock().unwrap().take();
}

#[tokio::test]
async fn auxiliary_calls_send_a_low_effort_and_the_override_header_never_leaves_the_process() {
    let f = fixture();
    let efforts = Arc::new(Mutex::new(vec![]));
    let e = efforts.clone();
    let p = provider(move |body, n| {
        e.lock().unwrap().push(body["reasoning_effort"].clone());
        text(if n == 1 { "main answer" } else { "side answer" })
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "effort": "max", "runTokenBudget": 0 }));
    let st = store(&rt);
    let h = harness::Harness::new(rt, st, registry(&f.cwd, tools::approve_all(), false), harness::noop_emit(), None, Default::default()).unwrap();
    h.run("hello", &token(), None).await.unwrap();
    h.ask_side_question("quick?", &token()).await.unwrap();
    assert_eq!(*efforts.lock().unwrap(), vec![json!("max"), json!("low")]);
    assert_eq!(p.header(0, "x-goon-effort"), None);
    assert_eq!(p.header(1, "x-goon-effort"), None);
}

#[tokio::test]
async fn retry_after_from_a_429_is_honoured_before_retrying() {
    let f = fixture();
    let p = provider(|_, n| {
        if n == 1 {
            let mut r = json_response(429, json!({ "error": { "message": "slow down" } }));
            r.headers.push(("retry-after".into(), "0.4".into()));
            return r;
        }
        text("ok")
    })
    .await;
    let rt = runtime(&f.cwd, &f.data, &p.url, json!({ "runTokenBudget": 0 }));
    let st = store(&rt);
    let started = std::time::Instant::now();
    let h = harness::Harness::new(rt, st, registry(&f.cwd, tools::approve_all(), false), harness::noop_emit(), None, Default::default()).unwrap();
    assert_eq!(h.run("x", &token(), None).await.unwrap(), "ok");
    assert!(started.elapsed().as_millis() >= 380, "retried after {:?}", started.elapsed());
    assert_eq!(p.count(), 2);
}

#[tokio::test]
async fn the_setup_wizard_always_offers_the_model_step_and_hides_the_api_key() {
    let f = fixture();
    let _env = fixture_env(&f);
    let asked = Arc::new(Mutex::new(vec![]));
    let secret = Arc::new(Mutex::new(vec![]));
    let answers = Arc::new(Mutex::new(std::collections::VecDeque::from(vec!["4", "", "2", ""])));
    let (a, s, ans) = (asked.clone(), secret.clone(), answers.clone());
    let io = setup::SetupIo {
        write: Some(Arc::new(|_| {})),
        ask: Some(Arc::new(move |label: String| {
            a.lock().unwrap().push(label);
            let next = ans.lock().unwrap().pop_front().unwrap_or("").to_string();
            Box::pin(async move { Ok(next) })
        })),
        ask_secret: Some(Arc::new(move |label: String| {
            s.lock().unwrap().push(label);
            Box::pin(async { Ok("sk-hidden".to_string()) })
        })),
        discover: Some(Arc::new(|_, _, _| {
            Box::pin(async { vec![provider::DiscoveredModel { id: "new-model-a".into(), ..Default::default() }, provider::DiscoveredModel { id: "new-model-b".into(), ..Default::default() }] })
        })),
    };
    let initial = config::parse_config(&json!({ "provider": "openai-compatible", "model": "old-model", "keyEnv": "GOON_TEST_UNUSED_KEY", "contextWindow": 128000, "maxOutputTokens": 16384, "timeoutMs": 300000, "runTokenBudget": 0 })).unwrap();
    let updated = setup::run_setup_wizard(Some(initial), io).await.unwrap();
    assert_eq!(updated.model.as_deref(), Some("new-model-b"));
    let asked = asked.lock().unwrap();
    assert!(asked.iter().any(|l| l.to_lowercase().contains("model")));
    assert_eq!(secret.lock().unwrap().len(), 1);
    assert!(!asked.iter().any(|l| l.to_lowercase().contains("api key")));
}
