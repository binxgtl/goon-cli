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
        let ui = Tui::with_io("test-model", Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }), Box::new(sink.clone()), Box::new(move || *s.lock().unwrap()), false);
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
        PickerTab { label: "All projects".into(), items: vec![PickerItem { id: "a".into(), label: "alpha".into(), ..Default::default() }, PickerItem { id: "b".into(), label: "beta".into(), meta: Some("proj".into()), ..Default::default() }], empty: None },
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
    let pieces = ["Hello **bold** ", "\n", "- item\n", "```ts\nconst a = 1;\n", "```\n", "| a | bb |\n", "|---|---|\n", "| ccc | d |\n", "> quote\n", "# Title\n", "tiếng Việt 中文 😀 ", &"word ".repeat(20)];
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
