use crate::common::*;
use goon::session::{repair_interrupted, SessionRoute, SessionStore};

fn route(f: &Fixture) -> SessionRoute {
    SessionRoute { cwd: f.cwd.to_string_lossy().into(), provider: "test".into(), model: "model".into(), base_url: "http://localhost/v1".into() }
}

fn root(f: &Fixture) -> PathBuf {
    f.root.join("sessions")
}

fn open(f: &Fixture, resume: Option<&str>) -> Result<SessionStore> {
    SessionStore::open(&root(f), route(f), resume, None)
}

fn push_user(store: &SessionStore, text: &str) {
    store.session.lock().unwrap().messages.push(json!({ "role": "user", "content": text }));
}

#[test]
fn an_all_digit_eight_char_prefix_resumes_by_prefix_not_as_a_list_index() {
    let f = fixture();
    let seed = open(&f, None).unwrap();
    push_user(&seed, "digit-prefixed session");
    seed.save().unwrap();
    let digit_id = "12345678-aaaa-4bbb-8ccc-dddddddddddd";
    let mut raw: Value = serde_json::from_str(&std::fs::read_to_string(root(&f).join(format!("{}.json", seed.id()))).unwrap()).unwrap();
    raw["id"] = json!(digit_id);
    std::fs::write(root(&f).join(format!("{digit_id}.json")), raw.to_string()).unwrap();
    let seed_id = seed.id();
    seed.close().unwrap();
    let by_digits = open(&f, Some("12345678")).unwrap();
    assert_eq!(by_digits.id(), digit_id);
    by_digits.close().unwrap();
    let by_index = open(&f, Some("1")).unwrap();
    assert!([digit_id.to_string(), seed_id].contains(&by_index.id()));
    by_index.close().unwrap();
}

#[test]
fn the_session_lock_prevents_two_owners_and_resume_retains_history() {
    let f = fixture();
    let store = open(&f, None).unwrap();
    let id = store.id();
    assert_match(&open(&f, Some(&id)).err().unwrap().to_string(), "already open");
    push_user(&store, "remember");
    store.save().unwrap();
    store.close().unwrap();
    let resumed = open(&f, Some("latest")).unwrap();
    assert_eq!(resumed.session.lock().unwrap().messages[0]["content"], "remember");
    assert_eq!(resumed.id(), id);
    resumed.close().unwrap();
}

#[test]
fn crash_repair_preserves_completed_results_and_adds_unknown_outcomes_once() {
    let mut messages = vec![
        json!({ "role": "assistant", "content": [
            { "type": "tool-call", "toolCallId": "one", "toolName": "write_file", "input": {} },
            { "type": "tool-call", "toolCallId": "two", "toolName": "bash", "input": {} },
        ] }),
        json!({ "role": "tool", "content": [{ "type": "tool-result", "toolCallId": "one", "toolName": "write_file", "output": { "type": "text", "value": "saved" } }] }),
    ];
    assert!(repair_interrupted(&mut messages));
    let last = messages.last().unwrap();
    assert_eq!(last["content"][0]["toolCallId"], "two");
    assert_eq!(last["content"][0]["output"]["type"], "error-text");
    assert!(!repair_interrupted(&mut messages));
}

#[test]
fn resume_rejects_another_workspace_and_releases_the_acquired_lock() {
    let f = fixture();
    let store = open(&f, None).unwrap();
    let id = store.id();
    store.close().unwrap();
    let mut other = route(&f);
    other.cwd = f.root.to_string_lossy().into();
    assert_match(&SessionStore::open(&root(&f), other, Some(&id), None).err().unwrap().to_string(), "different workspace");
    open(&f, Some(&id)).unwrap().close().unwrap();
}

#[test]
fn a_dead_process_lock_can_be_recovered_and_archives_do_not_duplicate_listings() {
    let f = fixture();
    let store = open(&f, None).unwrap();
    let id = store.id();
    store.close().unwrap();
    std::fs::write(root(&f).join(format!("{id}.lock")), "2147483647").unwrap();
    std::fs::copy(root(&f).join(format!("{id}.json")), root(&f).join(format!("{id}.compact-1.json"))).unwrap();
    let resumed = open(&f, Some(&id)).unwrap();
    assert_eq!(SessionStore::list(&root(&f)).unwrap().len(), 1);
    resumed.close().unwrap();
}

#[test]
fn short_prefix_resume_exclude_id_on_latest_and_fork_to_new_file() {
    let f = fixture();
    let s1 = open(&f, None).unwrap();
    push_user(&s1, "First task in session 1");
    s1.save().unwrap();
    let forked = s1.fork_to_new_file(Some("Forked branch")).unwrap();
    let s1_id = s1.id();
    s1.close().unwrap();
    let by_prefix = open(&f, Some(&forked[..8])).unwrap();
    assert_eq!(by_prefix.id(), forked);
    {
        let s = by_prefix.session.lock().unwrap();
        assert_eq!(s.title.as_deref(), Some("Forked branch"));
        assert_eq!(s.messages.len(), 1);
    }
    let latest_other = SessionStore::open(&root(&f), route(&f), Some("latest"), Some(&forked)).unwrap();
    assert_eq!(latest_other.id(), s1_id);
    by_prefix.close().unwrap();
    latest_other.close().unwrap();
}

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_parked_process() -> ChildGuard {
    use std::process::{Command, Stdio};
    let child = if cfg!(windows) {
        Command::new("cmd").args(["/d", "/c", "set /p x="]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
    } else {
        Command::new("sh").args(["-c", "read x"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
    };
    ChildGuard(child)
}

#[test]
fn stale_lock_with_recycled_os_pid_is_recovered_while_live_owner_stays_protected() {
    let f = fixture();
    let s1 = open(&f, None).unwrap();
    push_user(&s1, "recover me after pid recycle");
    s1.save().unwrap();
    let s1_id = s1.id();
    s1.close().unwrap();

    // Simulate a crashed goon process whose numeric PID was later recycled by the OS:
    // the process is genuinely alive right now, but its kernel start time is well after
    // the stale .lock file's modification timestamp.
    let recycled = spawn_parked_process();
    let recycled_pid = recycled.0.id();
    let stale_lock = root(&f).join(format!("{s1_id}.lock"));
    {
        let mut lf = std::fs::File::create(&stale_lock).unwrap();
        std::io::Write::write_all(&mut lf, recycled_pid.to_string().as_bytes()).unwrap();
        lf.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60)).unwrap();
    }
    let resumed = open(&f, Some(&s1_id)).expect("stale lock with recycled PID must be recovered");
    assert_eq!(resumed.id(), s1_id);
    resumed.close().unwrap();

    // Also verify delete_session recovers stale recycled-PID locks.
    let s_del = open(&f, None).unwrap();
    let s_del_id = s_del.id();
    s_del.close().unwrap();
    {
        let mut lf = std::fs::File::create(root(&f).join(format!("{s_del_id}.lock"))).unwrap();
        std::io::Write::write_all(&mut lf, recycled_pid.to_string().as_bytes()).unwrap();
        lf.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60)).unwrap();
    }
    assert_eq!(SessionStore::delete_session(&root(&f), &s_del_id).unwrap(), Some(s_del_id));

    // Conversely, a live process started BEFORE the .lock file was written is a genuine owner
    // and must NOT be stolen or deleted.
    let live = spawn_parked_process();
    std::thread::sleep(std::time::Duration::from_millis(25));
    let s2 = open(&f, None).unwrap();
    let s2_id = s2.id();
    s2.close().unwrap();
    std::fs::write(root(&f).join(format!("{s2_id}.lock")), live.0.id().to_string()).unwrap();
    assert_match(&open(&f, Some(&s2_id)).err().unwrap().to_string(), "already open");
    assert_match(&SessionStore::delete_session(&root(&f), &s2_id).err().unwrap().to_string(), "open in another goon process");
}

#[test]
fn tui_resume_command_does_not_deadlock_renders_single_todo_and_continues_turn() {
    use goon::cli::app::{App, AppState, Ui};
    use goon::cli::args::{parse_args, MAIN_OPTIONS};
    use goon::config::Config;
    use goon::policy::{ExecutionPolicy, NetworkMode};
    use goon::session::TodoItem;
    use goon::tui::{Key, Tui};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    let tokio_rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let outcome = tokio_rt.block_on(async {
        let f = fixture();
        let p = provider(|_, _| text("Resumed turn reply.")).await;
        let rt = runtime_keyed(&f.cwd, &f.data, &p.url, json!({}), "test-secret-never-echo");
        let sessions_root = rt.data_dir.join("sessions");

        let s1 = SessionStore::open(&sessions_root, SessionRoute::from_runtime(&rt), None, None).unwrap();
        {
            let mut s = s1.session.lock().unwrap();
            s.title = Some("Saved target".into());
            s.messages.push(json!({ "role": "user", "content": "first prompt" }));
            s.messages.push(json!({ "role": "assistant", "content": "first answer" }));
            s.todos = Some(vec![TodoItem { id: None, content: "Ship resume fix".into(), status: "in_progress".into(), priority: "medium".into() }]);
            s.tokens = 420.0;
            s.cost_usd = Some(0.05);
        }
        s1.save().unwrap();
        let s1_id = s1.id();
        s1.close().unwrap();

        let s2 = Arc::new(SessionStore::open(&sessions_root, SessionRoute::from_runtime(&rt), None, None).unwrap());
        let tui = Tui::with_io("test-model", Arc::new(|| {}), Box::new(std::io::sink()), Box::new(|| (100, 30)), false);
        let values = parse_args(&["--no-mcp".to_string(), "--no-subagents".to_string(), "--no-verify".to_string()], MAIN_OPTIONS, true).unwrap();
        let policy = ExecutionPolicy::with_mode(NetworkMode::Allow, &[]);
        policy.set_unrestricted(true);
        let app = Arc::new_cyclic(|me| App {
            me: me.clone(),
            values,
            json: false,
            interactive: true,
            output_last_message: None,
            policy: Arc::new(policy),
            notify_enabled: false,
            ui: Ui::Tui(tui.clone()),
            memory_store: None,
            config: Mutex::new(Config::default()),
            runtime: Mutex::new(rt.clone()),
            store: Mutex::new(s2.clone()),
            tools: Mutex::new(None),
            harness: Mutex::new(None),
            manager: Mutex::new(None),
            subagents: Mutex::new(None),
            pair: Mutex::new(None),
            controller: Mutex::new(None),
            st: Mutex::new(AppState {
                mode: "auto".into(),
                pre_plan_mode: "auto".into(),
                view_mode: "verbose".into(),
                denied: false,
                exiting: false,
                terminating: false,
                starting: false,
                goal_deferred: false,
                rebuild_pending: false,
                reasoning_source: None,
            }),
            allowed: Mutex::new(Default::default()),
            event_writer: Mutex::new(None),
            history_writer: Mutex::new(None),
            input_cancel: Mutex::new(CancellationToken::new()),
            rebuild_lock: tokio::sync::Mutex::new(()),
        });
        let (tools, harness) = app.build_harness("auto", false).await.unwrap();
        *app.tools.lock().unwrap() = Some(tools);
        *app.harness.lock().unwrap() = Some(harness);

        let tui_driver = tui.clone();
        let driver = tokio::spawn(async move {
            for _ in 0..100 {
                tui_driver.render_now();
                if tui_driver.rendered_frame().contains("Saved target") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tui_driver.key(None, Key::named("return"));

            for _ in 0..100 {
                if tui_driver.transcript().contains("Resumed session") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            for ch in "follow up".chars() {
                tui_driver.key(Some(&ch.to_string()), Key::default());
            }
            tui_driver.key(None, Key::named("return"));

            for _ in 0..100 {
                if tui_driver.transcript().contains("Resumed turn reply.") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            for ch in "/exit".chars() {
                tui_driver.key(Some(&ch.to_string()), Key::default());
            }
            tui_driver.key(None, Key::named("return"));
        });

        let app_loop = app.clone();
        let loop_handle = tokio::spawn(async move { app_loop.main_loop(Some("/resume".into())).await });
        match tokio::time::timeout(Duration::from_millis(1500), loop_handle).await {
            Ok(joined) => joined.unwrap().unwrap(),
            Err(_) => {
                driver.abort();
                return Err("TUI /resume deadlocked".to_string());
            }
        }
        driver.await.unwrap();

        assert_eq!(app.session_id(), s1_id);
        let clean = goon::term::strip_vt(&tui.transcript()).to_string();
        assert!(clean.contains("first prompt"), "resumed user prompt missing: {clean}");
        assert!(clean.contains("first answer"), "resumed assistant answer missing: {clean}");
        assert!(clean.contains("Resumed turn reply."), "follow-up turn reply missing: {clean}");
        assert_eq!(clean.matches("Todo 0/1").count(), 1, "Todo cell must render once on resume, got: {clean}");
        let _ = app.store().close();

        let saved: Value = serde_json::from_str(&std::fs::read_to_string(sessions_root.join(format!("{s1_id}.json"))).unwrap()).unwrap();
        assert_eq!(saved["messages"].as_array().unwrap().len(), 4);
        Ok(())
    });
    tokio_rt.shutdown_background();
    if let Err(msg) = outcome {
        panic!("{msg}");
    }
}
