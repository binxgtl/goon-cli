use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Tty {
    ui: Arc<Tui>,
    sink: Sink,
    size: Arc<Mutex<(usize, usize)>>,
    cancelled: Arc<AtomicUsize>,
}

impl Tty {
    fn new(cols: usize, rows: usize) -> Tty {
        let sink = Sink::default();
        let size = Arc::new(Mutex::new((cols, rows)));
        let s = size.clone();
        let cancelled = Arc::new(AtomicUsize::new(0));
        let c = cancelled.clone();
        let ui = Tui::with_io(
            "test-model",
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(sink.clone()),
            Box::new(move || *s.lock().unwrap()),
            false,
        );
        Tty { ui, sink, size, cancelled }
    }
    fn text(&self, t: &str) {
        for c in t.chars() {
            self.ui.key(Some(&c.to_string()), Key::default());
        }
    }
    fn key(&self, name: &str) {
        self.ui.key(None, Key::named(name));
    }
    fn written(&self) -> String {
        String::from_utf8_lossy(&self.sink.0.lock().unwrap()).to_string()
    }
    fn frame(&self) -> String {
        self.ui.render_now();
        crate::term::strip_vt(&self.ui.rendered_frame()).to_string()
    }
}

#[tokio::test]
async fn edits_unicode_input_recalls_history_and_restores_terminal() {
    let tty = Tty::new(40, 12);
    let ui = tty.ui.clone();
    let first = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.text("xin chào");
    tty.key("left");
    tty.key("backspace");
    tty.text("a");
    tty.key("return");
    assert_eq!(first.await.unwrap().unwrap(), "xin chao");
    let ui = tty.ui.clone();
    let second = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("up");
    tty.key("return");
    assert_eq!(second.await.unwrap().unwrap(), "xin chao");
    tty.ui.close();
    tty.ui.close();
    let written = tty.written();
    assert!(written.contains("\x1b[?1049h"));
    assert!(written.ends_with("\x1b[?25h\x1b[?1049l"));
}

#[tokio::test]
async fn cancels_approvals_handles_ctrl_c_and_narrow_terminals() {
    let tty = Tty::new(40, 12);
    let cancel = CancellationToken::new();
    let ui = tty.ui.clone();
    let c = cancel.clone();
    let approval = tokio::spawn(async move { ui.question("[y/N] ", Some(&c), false, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    cancel.cancel();
    assert!(approval.await.unwrap().unwrap_err().to_string().contains("cancelled"));
    tty.ui.key(None, Key { name: "c".into(), ctrl: true, ..Default::default() });
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 1);
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "hello\nworld\x1b[2J" }));
    tty.ui.event(&serde_json::json!({ "type": "tool", "name": "read_file", "id": "one", "state": "start" }));
    tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "child", "task": "Inspect", "state": "start" }));
    tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "child", "state": "progress", "event": { "type": "usage", "total": 15 } }));
    *tty.size.lock().unwrap() = (4, 8);
    tty.ui.resize();
    let ui = tty.ui.clone();
    let prompt = tokio::spawn(async move { ui.question("goon> ", None, false, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.text("abc");
    tokio::time::sleep(Duration::from_millis(40)).await;
    tty.key("return");
    assert_eq!(prompt.await.unwrap().unwrap(), "abc");
    tty.ui.close();
}

#[tokio::test]
async fn separates_reasoning_from_answers_and_child_progress() {
    let tty = Tty::new(100, 24);
    tty.ui.event(&serde_json::json!({ "type": "reasoning", "source": "provider", "text": "native detail" }));
    tty.ui.event(&serde_json::json!({ "type": "reasoning", "source": "inline", "text": "thinking " }));
    tty.ui.event(&serde_json::json!({ "type": "reasoning", "source": "inline", "text": "detail" }));
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "Visible answer." }));
    tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "child123", "state": "start", "task": "Inspect one file" }));
    tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "child123", "state": "progress", "event": { "type": "usage", "input": 10, "output": 5, "total": 15 } }));
    tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "child123", "state": "progress", "event": { "type": "tool", "name": "read_file", "id": "r1", "state": "done" } }));
    tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "child123", "state": "done" }));
    let frame = tty.frame();
    assert_eq!(frame.matches("∴ Thinking").count(), 2, "{frame}");
    assert!(!frame.contains("[reasoning"));
    assert!(!frame.contains("[answer]"));
    assert!(frame.contains("Visible answer."));
    assert!(frame.contains("agent child123: done"), "{frame}");
    assert!(frame.contains("15 tokens"));
    assert_eq!(frame.matches("Inspect one file").count(), 1);
}

#[tokio::test]
async fn slash_completion_mode_toggle_and_effort_cycling() {
    let tty = Tty::new(40, 12);
    let toggles = Arc::new(AtomicUsize::new(0));
    let t2 = toggles.clone();
    let ui_weak = Arc::downgrade(&tty.ui);
    tty.ui.set_on_toggle_mode(Arc::new(move || {
        t2.fetch_add(1, Ordering::SeqCst);
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_mode("auto");
        }
    }));
    let efforts = Arc::new(Mutex::new(vec![]));
    let e2 = efforts.clone();
    tty.ui.set_on_cycle_effort(Arc::new(move |d| e2.lock().unwrap().push(d)));
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.key(None, Key { name: "tab".into(), shift: true, ..Default::default() });
    assert_eq!(toggles.load(Ordering::SeqCst), 1);
    tty.ui.key(Some("."), Key { name: String::new(), meta: true, ..Default::default() });
    assert_eq!(*efforts.lock().unwrap(), vec![1]);
    tty.text("/eff");
    tty.key("tab");
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap().trim(), "/effort");
}

#[tokio::test]
async fn cells_markdown_and_diffs() {
    let tty = Tty::new(100, 30);
    tty.ui.set_status("Running");
    tty.ui.event(&serde_json::json!({ "type": "tool_stream", "id": "tc1", "name": "write_file", "state": "delta", "preview": "src/app.ts" }));
    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "r1", "name": "read_file", "state": "done", "meta": { "kind": "read", "summary": "src/index.ts" } }));
    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "s1", "name": "search", "state": "done", "meta": { "kind": "search", "summary": "createModel in src" } }));
    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "b1", "name": "bash", "state": "done", "meta": { "kind": "exec", "summary": "npm test", "durationMs": 1200, "outputPreview": ["PASS all tests"], "omittedLines": 0 } }));
    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "e1", "name": "edit_file", "state": "done", "meta": { "kind": "edit", "verb": "Edited", "summary": "src/app.ts", "added": 1, "removed": 1, "diff": "--- a/src/app.ts\n+++ b/src/app.ts\n@@ -1,1 +1,1 @@\n-const oldVal = 1;\n+const newVal = 2;" } }));
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "# Summary\n- **Todo list:** completed\n- `inline_code` works\n" }));
    tty.ui.render_now();
    let raw = tty.ui.rendered_frame();
    let frame = crate::term::strip_vt(&raw).to_string();
    assert!(tty.written().contains("\x1b[?2026h"));
    assert!(!tty.written().contains("Streaming write_file:"));
    assert!(frame.contains("Working"));
    assert!(frame.contains("Explored"));
    assert!(regex::Regex::new(r"Read.*src/index\.ts").unwrap().is_match(&frame));
    assert!(regex::Regex::new(r"Search.*createModel in src").unwrap().is_match(&frame));
    assert!(frame.contains("Ran"));
    assert!(frame.contains("Edited"));
    assert!(regex::Regex::new(r"• .*Todo list:.* completed").unwrap().is_match(&frame), "{frame}");
    let lines: Vec<&str> = raw.split("\r\n").collect();
    let bullet = lines.iter().find(|l| l.contains("Todo list:")).unwrap();
    assert!(!bullet.contains(text::ERR));
    let removed = lines.iter().find(|l| l.contains("const oldVal = 1;")).unwrap();
    assert!(removed.contains(text::ERR));
}

#[tokio::test]
async fn paste_collapses_and_expands_on_submit() {
    let tty = Tty::new(80, 20);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.paste("line one\nline two\nline three");
    let frame = tty.frame();
    assert!(frame.contains("[Pasted text #1 +3 lines]"), "{frame}");
    tty.text("tail");
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap(), "line one\nline two\nline three\ntail");
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.paste("a\nb");
    tty.key("backspace");
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap(), "");
    // A raw multi-line burst (no bracketed paste) is still one paste, never premature Enters.
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(tty.ui.raw_chunk("x\ry\rz"));
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap(), "x\ny\nz");
}

#[tokio::test]
async fn no_row_exceeds_width_and_tables_align() {
    for (cols, rows) in [(20, 10), (48, 12), (60, 20), (120, 40)] {
        let tty = Tty::new(cols, rows);
        tty.ui.write("◆ goon v0.0.0\n  model m   effort default   mode ask\n  /help commands   @ files   $ skills   ! shell   shift+tab mode\n");
        tty.ui.write("\n> a fairly long user prompt that will need wrapping across multiple lines of the terminal\n");
        tty.ui.event(&serde_json::json!({ "type": "text", "text": "| a | bb |\n|---|---|\n| ccc | d |\n\n```rust\nfn main() { println!(\"a very long line of code that should be wrapped hard\"); }\n```\n> quote line\n1. numbered item\n" }));
        tty.ui.event(&serde_json::json!({ "type": "todo", "todos": [{ "content": "first task with a long description", "status": "in_progress" }] }));
        tty.ui.event(&serde_json::json!({ "type": "stopped", "reason": "Stopped after repeated denials of the same tool call." }));
        tty.ui.render_now();
        for line in tty.ui.rendered_frame().split("\r\n") {
            assert!(visible_length(line) <= cols, "{cols}x{rows}: {line:?}");
        }
        if cols >= 60 {
            let frame = tty.frame();
            assert!(frame.contains("│ a   │ bb │"), "{frame}");
            assert!(frame.contains("│ ccc │ d  │"), "{frame}");
        }
    }
}

#[tokio::test]
async fn esc_cancel_semantics_and_transient_cleanup() {
    let tty = Tty::new(80, 20);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("Key: ", None, false, QuestionOpts { esc_cancels: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("escape");
    assert!(q.await.unwrap().unwrap_err().downcast_ref::<InputCancelled>().is_some());
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("Name: ", None, false, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("escape");
    assert_eq!(q.await.unwrap().unwrap(), "");
    tty.ui.write("before\n");
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.ask_transient("\n▲ Allow bash? [medium]\n  $ ls\n", "[y/N/a(lways)] ", None, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(tty.frame().contains("Allow bash?"));
    tty.ui.write("parallel output\n");
    tty.text("y");
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap(), "y");
    let frame = tty.frame();
    assert!(!frame.contains("Allow bash?"), "{frame}");
    assert!(frame.contains("parallel output"));
    let result = tty.ui.with_status("Connecting MCP", async { 7 }).await;
    assert_eq!(result, 7);
    assert_eq!(tty.ui.status(), "Ready");
}

#[tokio::test]
async fn backtrack_rewinds_turns_and_esc_esc_requests_revert() {
    let tty = Tty::new(80, 20);
    tty.ui.mark_turn_start();
    tty.ui.write("\n> first prompt\n");
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "first answer" }));
    tty.ui.mark_turn_start();
    tty.ui.write("\n> second prompt\n");
    assert!(tty.ui.rewind_turns(1));
    let frame = tty.frame();
    assert!(frame.contains("first prompt") && !frame.contains("second prompt"));
    tty.ui.set_input("second prompt", true);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("escape");
    assert!(tty.frame().contains("Press Esc again to rewind"));
    tty.key("escape");
    assert_eq!(q.await.unwrap().unwrap(), "/revert");
}

#[tokio::test]
async fn picker_filters_switches_tabs_and_resolves() {
    let tty = Tty::new(80, 20);
    let tabs = vec![
        PickerTab { label: "This folder".into(), items: vec![PickerItem { id: "a".into(), label: "alpha".into(), ..Default::default() }], empty: None },
        PickerTab {
            label: "All projects".into(),
            items: vec![
                PickerItem { id: "a".into(), label: "alpha".into(), ..Default::default() },
                PickerItem { id: "b".into(), label: "beta".into(), meta: Some("proj".into()), ..Default::default() },
            ],
            empty: None,
        },
    ];
    let ui = tty.ui.clone();
    let pick = tokio::spawn(async move { ui.pick("Resume", tabs, 0, "resume").await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(tty.frame().contains("This folder 1"));
    tty.key("right");
    tty.text("bet");
    tty.key("return");
    assert_eq!(pick.await.unwrap().unwrap().1.id, "b");
}

#[tokio::test]
async fn immediate_commands_run_while_busy_and_prompts_queue() {
    let tty = Tty::new(80, 20);
    let seen = Arc::new(Mutex::new(vec![]));
    let s2 = seen.clone();
    tty.ui.set_on_immediate_command(Arc::new(move |cmd: String| {
        let s2 = s2.clone();
        Box::pin(async move { s2.lock().unwrap().push(cmd) })
    }));
    tty.ui.set_status("Running");
    tty.text("/status");
    tty.key("return");
    tty.text("follow up");
    tty.key("return");
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(*seen.lock().unwrap(), vec!["/status".to_string()]);
    assert!(tty.ui.has_queued_prompts());
    assert!(tty.frame().contains("queued  follow up"));
    let q = tty.ui.question("goon> ", None, true, QuestionOpts::default()).await.unwrap();
    assert_eq!(q, "follow up");
    // Ctrl+T toggles the transcript overlay at any time.
    tty.ui.key(None, Key { name: "t".into(), ctrl: true, ..Default::default() });
    assert!(tty.ui.is_transcript_overlay());
    assert!(tty.frame().contains("transcript"));
    tty.key("escape");
    assert!(!tty.ui.is_transcript_overlay());
}

#[tokio::test]
async fn auto_mode_switch_approves_pending_prompt_once() {
    let tty = Tty::new(80, 20);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("[y/N/a(lways)] ", None, false, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.set_mode("auto");
    assert_eq!(q.await.unwrap().unwrap(), "y");
}

#[tokio::test]
async fn auto_mode_switch_approves_a_pending_transient_approval_once() {
    let tty = Tty::new(60, 20);
    let ui = tty.ui.clone();
    let answer = tokio::spawn(async move { ui.ask_transient("Allow bash?", "[y/N/a(lways)] ", None, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.set_mode("auto");
    assert_eq!(answer.await.unwrap().unwrap(), "y");
    tty.ui.close();
}

#[tokio::test]
async fn masked_questions_never_render_the_typed_secret() {
    let tty = Tty::new(60, 20);
    let ui = tty.ui.clone();
    let answer = tokio::spawn(async move { ui.question("API Key: ", None, false, QuestionOpts { mask: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.text("sk-super-secret");
    let frame = tty.frame();
    assert!(!tty.written().contains("sk-super-secret"));
    assert!(!frame.contains("sk-super-secret"));
    assert!(frame.contains(&"•".repeat(5)));
    tty.key("return");
    assert_eq!(answer.await.unwrap().unwrap(), "sk-super-secret");
    assert!(!tty.written().contains("sk-super-secret"));
    tty.ui.close();
}

#[tokio::test]
async fn incremental_rendering_is_identical_to_a_full_re_render() {
    let tty = Tty::new(70, 24);
    let pieces =
        ["Hello **bold** ", "\n", "- item\n", "```ts\nconst a = 1;\n", "```\n", "| a | bb |\n", "|---|---|\n", "| ccc | d |\n", "> quote\n", "# Title\n", "tiếng Việt 中文 😀 ", &"word ".repeat(20)];
    let mut seed: u64 = 7;
    let mut rnd = move || {
        seed = (seed * 1103515245 + 12345) % 2147483648;
        seed as f64 / 2147483648.0
    };
    for step in 0..600 {
        let r = rnd();
        if r < 0.6 {
            let piece = pieces[(rnd() * pieces.len() as f64) as usize];
            tty.ui.event(&serde_json::json!({ "type": "text", "text": piece }));
        } else if r < 0.7 {
            let piece = pieces[(rnd() * pieces.len() as f64) as usize];
            tty.ui.event(&serde_json::json!({ "type": "reasoning", "text": format!("thinking {piece}"), "source": "provider" }));
        } else if r < 0.8 {
            tty.ui.event(&serde_json::json!({ "type": "tool", "name": "edit_file", "id": format!("e{step}"), "state": "done", "meta": { "kind": "edit", "summary": "a.ts", "diff": "--- a/a.ts\n+++ b/a.ts\n@@ -1,2 +1,2 @@\n-x\n+y\n z", "added": 1, "removed": 1 } }));
        } else if r < 0.88 {
            tty.ui.write(&format!("\n> prompt {step}\n"));
        } else if r < 0.92 {
            tty.ui.set_view_mode(if rnd() < 0.5 { "normal" } else { "verbose" });
        } else {
            tty.size.lock().unwrap().0 = 40 + (rnd() * 60.0) as usize;
        }
        let cols = tty.size.lock().unwrap().0 - 1;
        let mut st = tty.ui.state.lock().unwrap();
        let incremental = tty.ui.render_styled_body_lines(&mut st, cols, true);
        let cached = st.body_cache.take();
        let full = tty.ui.render_styled_body_lines(&mut st, cols, true);
        assert_eq!(incremental, full, "step {step}");
        st.body_cache = cached;
    }
    tty.ui.close();
}

#[tokio::test]
async fn picker_tabs_arrows_filtering_enter_and_escape() {
    let tty = Tty::new(90, 24);
    let item = |id: &str, label: &str, meta: &str, detail: Option<&str>| PickerItem { id: id.into(), label: label.into(), meta: Some(meta.into()), detail: detail.map(Into::into), search: None };
    let tabs = vec![
        PickerTab { label: "This folder".into(), items: vec![item("a", "fix checkout", "2h", None), item("b", "add retry", "1d", None)], empty: None },
        PickerTab { label: "All projects".into(), items: vec![item("c", "docs site", "web   3d", None), item("d", "billing bug", "api   4d", Some("/home/dev/api"))], empty: None },
    ];
    let spawn = |tabs: Vec<PickerTab>| {
        let ui = tty.ui.clone();
        tokio::spawn(async move { ui.pick("Resume", tabs, 0, "resume").await })
    };
    let picking = spawn(tabs.clone());
    tokio::time::sleep(Duration::from_millis(5)).await;
    let frame = tty.frame();
    assert_match(&frame, r"Resume {3}This folder 2 {3}All projects 2");
    assert_match(&frame, r"› fix checkout");
    assert_match(&frame, r"enter resume {3}esc cancel");
    tty.key("down");
    tty.key("return");
    assert_eq!(picking.await.unwrap().unwrap().1.id, "b");

    let picking = spawn(tabs.clone());
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("right");
    tty.key("down");
    let frame = tty.frame();
    assert_match(&frame, r"› billing bug\s+api {3}4d");
    assert_match(&frame, r"/home/dev/api");
    tty.key("tab");
    tty.key("left");
    tty.text("docs");
    let frame = tty.frame();
    assert_match(&frame, r"All projects 1");
    assert_match(&frame, r"› docs");
    tty.key("return");
    let (tab, chosen) = picking.await.unwrap().unwrap();
    assert_eq!((tab, chosen.id.as_str()), (1, "c"));

    let picking = spawn(tabs);
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("escape");
    assert!(picking.await.unwrap().is_none());
    assert!(!tty.ui.is_picking());
    tty.ui.close();
}

fn assert_match(text: &str, pattern: &str) {
    assert!(regex::Regex::new(pattern).unwrap().is_match(text), "{pattern:?} did not match:\n{text}");
}

fn rows(frame: &str) -> Vec<String> {
    frame.split("\r\n").map(String::from).collect()
}

fn goal(status: &str) -> crate::session::SessionGoal {
    let now = crate::jsutil::now_iso();
    serde_json::from_value(serde_json::json!({ "goalId": "goal_test1", "objective": "Refactor parser and pass tests", "status": status, "createdAt": now, "updatedAt": now, "tokensAtStart": 0, "tokensUsed": 12500, "timeUsedSeconds": 180, "tokenBudget": 50000 })).unwrap()
}

#[test]
fn terminal_text_strips_escapes_and_wrapping_counts_wide_characters() {
    assert_eq!(crate::term::terminal_text("safe\x1b[2Jtext\x1b]52;c;c2VjcmV0\x07end"), "safetextend");
    assert_eq!(wrap_terminal("ab界c", 4), vec!["ab界", "c"]);
}

#[tokio::test]
async fn long_output_never_drifts_past_the_width_scrolls_with_up_and_pasted_image_paths_collapse() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("{25D1BD96-7B57-4392-BBF0-17E774BEEFF2}.png");
    use base64::Engine;
    std::fs::write(&image, base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==").unwrap()).unwrap();
    let image = image.to_string_lossy().to_string();
    let tty = Tty::new(80, 16);
    tty.ui.event(&serde_json::json!({ "type": "reasoning", "source": "provider", "text": format!("{}\n", "Long reasoning line that previously wrapped after adding the 4-char prefix. ".repeat(6)) }));
    for i in 1..=15 {
        tty.ui.event(&serde_json::json!({ "type": "text", "text": format!("Line {i} of assistant answer in transcript.\n") }));
    }
    tty.ui.render_now();
    for row in rows(&tty.ui.rendered_frame()) {
        assert!(visible_length(&row) <= 79, "{row:?}");
    }
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("up");
    assert!(tty.frame().contains("lines above the latest output"));
    tty.key("escape");
    assert!(!tty.frame().contains("lines above the latest output"));
    tty.ui.paste(&image);
    let frame = tty.frame();
    assert!(frame.contains("[Image #1]"), "{frame}");
    assert!(frame.contains("attached  [Image #1]"), "{frame}");
    assert!(tty.written().contains("H\x1b[?25h"));
    tty.key("return");
    let submitted = q.await.unwrap().unwrap();
    assert_eq!(submitted.trim(), format!("@\"{image}\""));

    let files = crate::files::WorkspaceFiles::new(&dir.path().join("ws"));
    let options = crate::context::MentionOptions { model: Some("space-bunny"), supports_vision: None, allow_outside_workspace: true, max_chars: 100_000 };
    let from_mention = crate::context::expand_file_mentions(&submitted, &files, &options);
    assert_eq!(from_mention.image_parts.len(), 1);
    assert_eq!(from_mention.image_parts[0]["mediaType"], "image/png");
    let bare = crate::context::expand_file_mentions(&format!("Please inspect {image}"), &files, &options);
    assert_eq!(bare.image_parts.len(), 1);
    assert_eq!(bare.image_parts[0]["mediaType"], "image/png");
    assert!(bare.expanded_prompt.contains("[Image #1: {25D1BD96-7B57-4392-BBF0-17E774BEEFF2}.png]"), "{}", bare.expanded_prompt);
}

#[tokio::test]
async fn minimal_chrome_transient_cleanup_differential_rendering_and_queued_prompts() {
    let tty = Tty::new(90, 20);
    tty.ui.write("Type /help for commands, @file to attach files, !<cmd> for shell, or enter a task.\n");
    tty.ui.event(&serde_json::json!({ "type": "reasoning", "source": "provider", "text": "Checking files without vertical bars\n" }));
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "Direct bullet answer without [answer] tag.\n" }));
    tty.ui.render_now();
    assert!(tty.ui.rendered_frame().contains(&format!("{}Type /help for commands", Theme::FAINT)));
    let ui = tty.ui.clone();
    let approval = tokio::spawn(async move { ui.ask_transient("\n▲ Allow bash?\n  $ npm test\n", "[y/N/a(lways)] ", None, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    let frame = tty.frame();
    assert!(frame.contains("▲ Allow bash?") && frame.contains("$ npm test"), "{frame}");
    tty.text("y");
    tty.key("return");
    assert_eq!(approval.await.unwrap().unwrap(), "y");
    let frame = tty.frame();
    assert!(!frame.contains("Allow bash?"));
    assert!(frame.contains("∴ Thinking"));
    assert!(frame.contains("  Checking files without vertical bars"));
    assert!(frame.contains("◆ Direct bullet answer without [answer] tag."));
    let lines = rows(&frame);
    let n = lines.len();
    assert!(!lines[0].contains("test-model"));
    assert!(!frame.contains(['╭', '╮', '╰', '╯']));
    assert!(regex::Regex::new(r"^─+$").unwrap().is_match(&lines[n - 3]), "{:?}", lines[n - 3]);
    assert!(lines[n - 2].starts_with("› Ask anything   / commands"), "{:?}", lines[n - 2]);
    assert!(regex::Regex::new(r"^ask   test-model   effort default\s+0 tokens$").unwrap().is_match(&lines[n - 1]), "{:?}", lines[n - 1]);
    assert!(!frame.contains("Ready ·") && !frame.contains("Queue"));

    let before = tty.written().len();
    tty.ui.set_status("Running");
    tokio::time::sleep(Duration::from_millis(180)).await;
    let spin = tty.written()[before..].to_string();
    assert!(!spin.contains("\x1b[2J"), "spinner ticks must never clear the screen");
    assert!(!spin.contains("Direct bullet answer"), "spinner ticks must not rewrite unchanged rows");

    tty.text("queued follow-up task");
    tty.key("return");
    let frame = tty.frame();
    assert!(regex::Regex::new(r"queued  queued follow-up task   alt\+↑ edit").unwrap().is_match(&frame), "{frame}");
    assert!(!frame.contains("Queue"));
    tty.ui.set_status("Ready");
    assert_eq!(tty.ui.question("goon> ", None, true, QuestionOpts::default()).await.unwrap(), "queued follow-up task");
}

#[tokio::test]
async fn revert_removes_the_failed_turn_and_restores_its_prompt_into_the_composer() {
    let tty = Tty::new(90, 20);
    tty.ui.write("Welcome banner line\n");
    tty.ui.mark_turn_start();
    tty.ui.write("\n> first prompt\n");
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "First answer.\n" }));
    tty.ui.mark_turn_start();
    tty.ui.write("\n> can u play valorant\n");
    tty.ui.write("\nUpstream provider returned HTTP 500:\n");
    let frame = tty.frame();
    assert!(frame.contains("› can u play valorant") && frame.contains("Upstream provider returned HTTP 500:"));
    let ui = tty.ui.clone();
    let q1 = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("escape");
    assert!(tty.frame().contains("Press Esc again to rewind and edit the previous prompt"));
    tty.key("escape");
    assert_eq!(q1.await.unwrap().unwrap(), "/revert");
    assert!(tty.ui.rewind_turns(1));
    tty.ui.set_input("can u play valorant", true);
    tty.ui.set_notice(Some("↺ Rewound 1 turn — edit prompt below, Enter to resend, or Esc Esc to rewind further"));
    let ui = tty.ui.clone();
    let q2 = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    let frame = tty.frame();
    assert!(!frame.contains("Upstream provider returned HTTP 500:"));
    assert!(frame.contains("› first prompt") && frame.contains("◆ First answer."));
    let lines = rows(&frame);
    assert!(regex::Regex::new(r"^› can u play valorant\s*$").unwrap().is_match(&lines[lines.len() - 2]), "{:?}", lines[lines.len() - 2]);
    tty.key("escape");
    tty.key("escape");
    assert_eq!(q2.await.unwrap().unwrap(), "/revert");
}

#[tokio::test]
async fn welcome_banner_is_short_and_unboxed_and_view_mode_toggles_thinking() {
    let workspace = std::path::PathBuf::from("D:\\Th\u{01B0} mu\u{0323}c m\u{01A1}\u{0301}i (2)");
    let banner = crate::cli::welcome_banner("gpt-5.4", "openai", "https://api.ezaiapi.com/v1", Some("max"), &workspace, "0f40f011-0000-4000-8000-000000000000", "ask");
    let lines: Vec<&str> = banner.trim().split('\n').collect();
    assert_eq!(lines.len(), 5, "{banner}");
    assert!(regex::Regex::new(r"^◆ goon v\d+\.\d+\.\d+").unwrap().is_match(lines[0]), "{}", lines[0]);
    assert!(regex::Regex::new(r"model gpt-5\.4 {3}effort max {3}mode ask").unwrap().is_match(&banner));
    assert!(banner.contains("gateway api.ezaiapi.com/v1 (openai)"));
    assert!(banner.contains("session 0f40f011"));
    assert!(!banner.contains(['╭', '╮', '╰', '╯', '│']));

    let tty = Tty::new(40, 20);
    tty.ui.write(&banner);
    let frame = tty.frame();
    for row in rows(&frame) {
        assert!(visible_length(&row) <= 40, "{row:?}");
    }
    assert!(frame.contains("◆ goon v"));
    tty.ui.clear();
    tty.size.lock().unwrap().0 = 90;
    tty.ui.set_view_mode("normal");
    assert_eq!(tty.ui.view_mode(), "normal");
    tty.ui.event(&serde_json::json!({ "type": "reasoning", "source": "provider", "text": "Internal chain of thought step 1.\n" }));
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "Final visible answer.\n" }));
    let frame = tty.frame();
    assert!(!frame.contains("∴ Thinking") && !frame.contains("Internal chain of thought step 1."));
    assert!(frame.contains("◆ Final visible answer."));
    tty.ui.set_view_mode("verbose");
    assert_eq!(tty.ui.view_mode(), "verbose");
    let frame = tty.frame();
    assert!(frame.contains("∴ Thinking") && frame.contains("Internal chain of thought step 1.") && frame.contains("◆ Final visible answer."));
}

#[tokio::test]
async fn ctrl_t_transcript_auto_mode_and_immediate_commands_while_working() {
    let tty = Tty::new(95, 22);
    tty.ui.set_view_mode("normal");
    tty.ui.event(&serde_json::json!({ "type": "reasoning", "source": "provider", "text": "Hidden reasoning visible in Ctrl+T.\n" }));
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "Visible answer text.\n" }));
    tty.ui.key(None, Key { name: "t".into(), ctrl: true, ..Default::default() });
    assert!(tty.ui.is_transcript_overlay());
    let frame = tty.frame();
    assert!(frame.contains("◆ transcript") && frame.contains("∴ Thinking") && frame.contains("Hidden reasoning visible in Ctrl+T."), "{frame}");
    tty.text("q");
    assert!(!tty.ui.is_transcript_overlay());
    tty.ui.set_status("Running");
    let ran = Arc::new(Mutex::new(Vec::<String>::new()));
    let (r2, weak) = (ran.clone(), Arc::downgrade(&tty.ui));
    tty.ui.set_on_immediate_command(Arc::new(move |cmd: String| {
        r2.lock().unwrap().push(cmd.clone());
        if cmd == "/auto" {
            if let Some(ui) = weak.upgrade() {
                ui.set_mode("auto");
            }
        }
        Box::pin(async {})
    }));
    tty.text("/auto");
    tty.key("return");
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(*ran.lock().unwrap(), vec!["/auto".to_string()]);
    assert!(tty.frame().contains("auto-approve"));
    tty.text("do next task after this");
    tty.key("return");
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(ran.lock().unwrap().len(), 1);
    assert!(tty.frame().contains("do next task after this"));
}

#[tokio::test]
async fn blockquotes_never_render_as_user_turns_and_cells_after_unterminated_answers_are_cells() {
    let tty = Tty::new(90, 24);
    tty.ui.mark_turn_start();
    tty.ui.write("\n> real user prompt\n");
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "Answer intro\n> quoted model text\nAnswer without trailing newline" }));
    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "b1", "name": "bash", "state": "done", "meta": { "kind": "exec", "summary": "npm test", "durationMs": 1000, "outputPreview": ["ok"] } }));
    tty.ui.render_now();
    let raw = tty.ui.rendered_frame();
    let plain = crate::term::strip_vt(&raw).to_string();
    let all = rows(&raw);
    let body = &all[..all.len() - 3];
    let users: Vec<&String> = body.iter().filter(|l| l.starts_with(&format!("{}›", Theme::BRAND_BOLD))).collect();
    assert_eq!(users.len(), 1, "{plain}");
    assert!(crate::term::strip_vt(users[0]).contains("› real user prompt"));
    assert!(plain.contains("▎ quoted model text"));
    assert!(!plain.contains("› quoted model text"));
    let ran = all.iter().find(|l| crate::term::strip_vt(l).starts_with("● Ran npm test")).expect("Ran cell row starts at column 0");
    assert!(ran.starts_with(&format!("{}\x1b[1m●", Theme::OK)), "{ran:?}");
    assert!(plain.contains("Answer without trailing newline"));
}

#[tokio::test]
async fn footer_shows_workspace_branch_and_provider_reported_context() {
    let tty = Tty::new(100, 20);
    tty.ui.set_workspace(std::path::Path::new("D:\\work\\testcli"), Some("feature/redesign"));
    tty.ui.set_context_window(200_000);
    let footer = |tty: &Tty| {
        tty.ui.render_now();
        rows(&tty.ui.rendered_frame()).pop().unwrap()
    };
    let plain = crate::term::strip_vt(&footer(&tty)).to_string();
    assert!(plain.contains("testcli (feature/redesign)"), "{plain}");
    assert!(!plain.contains("ctx"));
    assert_eq!(plain.matches("test-model").count(), 1);
    tty.ui.event(&serde_json::json!({ "type": "usage", "input": 60000, "output": 8000, "total": 68000 }));
    tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "kid", "state": "progress", "event": { "type": "usage", "input": 150000, "output": 1, "total": 150001 } }));
    assert!(crate::term::strip_vt(&footer(&tty)).contains("ctx 34%"));
    assert!(tty.frame().contains("218k tokens"));
    tty.ui.event(&serde_json::json!({ "type": "compaction", "count": 1 }));
    assert!(!crate::term::strip_vt(&footer(&tty)).contains("ctx"));
    assert!(tty.frame().contains("◌ Context compacted #1"));
    tty.ui.event(&serde_json::json!({ "type": "usage", "input": 185000, "output": 0, "total": 185000 }));
    let raw = footer(&tty);
    assert!(crate::term::strip_vt(&raw).contains("93%"));
    assert!(raw.contains(&format!("{}ctx 93%", Theme::ERR)), "{raw:?}");
}

#[tokio::test]
async fn slash_popup_lists_matches_with_descriptions_and_arrows_keep_the_input() {
    let tty = Tty::new(100, 24);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.text("/rev");
    let frame = tty.frame();
    assert!(regex::Regex::new(r"› /revert\s+rewind turn\(s\)").unwrap().is_match(&frame), "{frame}");
    assert!(regex::Regex::new(r"/review\s+run code review").unwrap().is_match(&frame));
    assert!(!frame.contains("/resume"));
    tty.key("tab");
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap().trim(), "/revert");
    assert!(!tty.frame().contains("rewind turn(s)"));
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.text("/rev");
    tty.key("down");
    let frame = tty.frame();
    let lines = rows(&frame);
    assert!(regex::Regex::new(r"^› /rev\s*$").unwrap().is_match(&lines[lines.len() - 2]), "{:?}", lines[lines.len() - 2]);
    assert!(regex::Regex::new(r"› /review\s+run code review").unwrap().is_match(&frame));
    tty.key("down");
    tty.key("up");
    assert!(regex::Regex::new(r"› /review\s+run code review").unwrap().is_match(&tty.frame()));
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap(), "/review");
}

#[tokio::test]
async fn markdown_tables_align_columns_across_rows() {
    let tty = Tty::new(100, 24);
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "Summary:\n| file | change |\n|---|---|\n| src/tui.ts | **meter** |\n| a | b |\n" }));
    let frame = tty.frame();
    let table: Vec<String> = rows(&frame).into_iter().filter(|l| l.contains(['│', '├']) && !l.contains(['╭', '╰']) && !l.contains('›')).collect();
    assert_eq!(table.len(), 4, "{}", table.join("\n"));
    let bars = |row: &str| row.chars().enumerate().filter(|(_, c)| matches!(c, '│' | '┼' | '├' | '┤')).map(|(i, _)| i).collect::<Vec<_>>();
    let expected = bars(&table[0]);
    for row in &table {
        assert_eq!(bars(row), expected, "misaligned row: {row}");
    }
}

#[tokio::test]
async fn no_row_exceeds_the_width_at_any_size_across_every_cell_kind() {
    for columns in [48, 57, 64, 80, 101, 132] {
        let tty = Tty::new(columns, 60);
        tty.ui.set_workspace(std::path::Path::new("/home/user/a-very-long-workspace-directory-name-for-testing"), Some("feature/an-extremely-long-branch-name"));
        tty.ui.set_context_window(100_000);
        tty.ui.set_context_usage(Some(42_000));
        tty.ui.set_tokens(123_456.0);
        tty.ui.write(&crate::cli::welcome_banner(
            "model-with-a-really-long-identifier-v1",
            "openai-compatible",
            "https://gateway.example.com/v1",
            Some("max"),
            std::path::Path::new("/home/user/project"),
            "0f40f011-0000-4000-8000-000000000000",
            "auto",
        ));
        tty.ui.write(&format!("\n> {}\n", "long user prompt words ".repeat(12)));
        tty.ui.event(&serde_json::json!({ "type": "reasoning", "source": "provider", "text": format!("{}\n", "reasoning words ".repeat(30)) }));
        tty.ui.event(&serde_json::json!({ "type": "tool", "id": "r", "name": "read_file", "state": "done", "meta": { "kind": "read", "summary": format!("src/{}file.ts", "deep/".repeat(20)) } }));
        tty.ui.event(&serde_json::json!({ "type": "tool", "id": "b", "name": "bash", "state": "done", "meta": { "kind": "exec", "summary": format!("echo {}", "x".repeat(200)), "durationMs": 65000, "outputPreview": ["y".repeat(300)] } }));
        tty.ui.event(&serde_json::json!({ "type": "tool", "id": "e", "name": "edit_file", "state": "done", "meta": { "kind": "edit", "verb": "Edited", "summary": "src/app.ts", "added": 1, "removed": 1, "diff": format!("@@ -1,1 +1,1 @@\n-{}\n+{}", "old ".repeat(60), "new ".repeat(60)) } }));
        tty.ui.event(&serde_json::json!({ "type": "todo", "todos": [{ "content": "todo item ".repeat(20), "status": "in_progress" }] }));
        tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "abcdef123456", "state": "start", "task": "task words ".repeat(30) }));
        tty.ui.event(&serde_json::json!({ "type": "text", "text": format!("# {}\n- {}\n> {}\n```js\n{}\n```\n| {}| d |\n|---|---|\n| e | f |\n", "Heading ".repeat(20), "bullet ".repeat(40), "quote ".repeat(40), "const z = 1; ".repeat(30), "c ".repeat(80)) }));
        tty.ui.set_status("Running");
        let ui = tty.ui.clone();
        let _q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        tty.text("/re");
        tty.ui.render_now();
        for line in rows(&tty.ui.rendered_frame()) {
            assert!(visible_length(&line) < columns, "cols={columns}: {:?}", crate::term::strip_vt(&line));
        }
        tty.ui.close();
    }
}

#[tokio::test]
async fn question_hints_match_esc_behaviour_and_with_status_always_restores() {
    let tty = Tty::new(100, 20);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("Base URL: ", None, false, QuestionOpts { esc_cancels: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(regex::Regex::new(r"enter submit {3}esc cancel").unwrap().is_match(&tty.frame()));
    tty.text("https://typed-but-cancelled");
    tty.key("escape");
    assert!(q.await.unwrap().unwrap_err().downcast_ref::<InputCancelled>().is_some());
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("[y/N/a(lways)] ", None, false, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(tty.frame().contains("esc deny"));
    tty.key("escape");
    assert_eq!(q.await.unwrap().unwrap(), "");

    tty.ui.set_status("Ready");
    let ui = tty.ui.clone();
    let value = tty
        .ui
        .with_status("Connecting MCP", async move {
            assert_eq!(ui.status(), "Connecting MCP");
            42
        })
        .await;
    assert_eq!(value, 42);
    assert_eq!(tty.ui.status(), "Ready");
    assert!(!tty.frame().contains("Working"));
    let failed: anyhow::Result<()> = tty.ui.with_status("Connecting MCP", async { Err(anyhow::anyhow!("mcp boom")) }).await;
    assert!(failed.unwrap_err().to_string().contains("mcp boom"));
    assert_eq!(tty.ui.status(), "Ready");
    tty.ui.set_status("Running");
    tty.ui.with_status("Connecting MCP", async {}).await;
    assert_eq!(tty.ui.status(), "Running");
    tty.ui.set_status("Ready");
}

#[tokio::test]
async fn pastes_collapse_into_pills_backspace_atomically_and_expand_on_submit_and_recall() {
    let tty = Tty::new(100, 24);
    let ask = |tty: &Tty| {
        let ui = tty.ui.clone();
        tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await })
    };
    let q1 = ask(&tty);
    tokio::time::sleep(Duration::from_millis(5)).await;
    let lines25 = (1..=25).map(|i| format!("line {i}: const value_{i} = {i};")).collect::<Vec<_>>().join("\r\n");
    tty.ui.paste(&lines25);
    let frame = tty.frame();
    assert!(frame.contains("[Pasted text #1 +25 lines]") && frame.contains("attached  [Pasted text #1 +25 lines]"), "{frame}");
    assert!(!frame.contains("const value_15"));
    tty.text("sửa goon thành paste nội dung dài");
    tty.key("return");
    let submitted = q1.await.unwrap().unwrap();
    assert_eq!(submitted, format!("{}\nsửa goon thành paste nội dung dài", lines25.replace("\r\n", "\n")));
    tty.ui.write(&format!("\n> {submitted}\n"));
    let frame = tty.frame();
    assert!(frame.contains("› [Pasted text #1 +25 lines]"), "{frame}");
    assert!(!frame.contains("const value_15"));

    let q2 = ask(&tty);
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.paste(&"x".repeat(200));
    assert!(tty.frame().contains("[Pasted text #1 (200 chars)]"));
    tty.key("backspace");
    let frame = tty.frame();
    assert!(!frame.contains("[Pasted text #1 (200 chars)]") && !frame.contains("attached  ["), "{frame}");
    assert!(tty.ui.raw_chunk("alpha line\r\nbeta line\r\ngamma line"));
    assert!(tty.frame().contains("[Pasted text #1 +3 lines]"));
    tty.key("return");
    assert_eq!(q2.await.unwrap().unwrap(), "alpha line\nbeta line\ngamma line");

    let q3 = ask(&tty);
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("up");
    assert!(tty.frame().contains("[Pasted text #1 +3 lines]"));
    tty.key("return");
    assert_eq!(q3.await.unwrap().unwrap(), "alpha line\nbeta line\ngamma line");
}

#[tokio::test]
async fn goal_indicators_render_in_the_status_area() {
    let tty = Tty::new(110, 24);
    tty.ui.set_goal(Some(goal("active")));
    assert!(tty.frame().contains("Pursuing goal (12.5K / 50K)"));
    tty.ui.event(&serde_json::json!({ "type": "goal", "goal": serde_json::to_value(goal("complete")).unwrap() }));
    assert!(tty.frame().contains("Goal achieved (12.5K tokens)"));
}

#[tokio::test]
async fn verify_events_render_rows_and_skips_are_silent() {
    let tty = Tty::new(40, 12);
    tty.ui.event(&serde_json::json!({ "type": "verify_start", "files": ["src/a.ts", "src/b.ts"], "commands": ["npm test"] }));
    tty.ui.event(&serde_json::json!({ "type": "verify_result", "status": "fail", "stage": "test", "reason": "1 failing test", "errors": [], "warnings": [], "durationMs": 5 }));
    tty.ui.event(&serde_json::json!({ "type": "verify_rollback", "reason": "Verification failed (attempt 2/2)", "restored": ["src/a.ts"], "deleted": ["src/new.ts"], "conflicts": [] }));
    tty.ui.event(&serde_json::json!({ "type": "verify_result", "status": "skip", "reason": "approval denied", "errors": [], "warnings": [], "durationMs": 0 }));
    let transcript = tty.ui.transcript();
    assert!(transcript.contains("Verify 2 file(s) · npm test"), "{transcript}");
    assert!(transcript.contains("Verify fail: 1 failing test"));
    assert!(transcript.contains("Verify rollback"));
    assert!(!transcript.contains("approval denied"));
}
