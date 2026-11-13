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
        Self::with_mode(cols, rows, true)
    }
    fn with_mode(cols: usize, rows: usize, alternate: bool) -> Tty {
        let sink = Sink::default();
        let size = Arc::new(Mutex::new((cols, rows)));
        let s = size.clone();
        let cancelled = Arc::new(AtomicUsize::new(0));
        let c = cancelled.clone();
        let ui = Tui::with_io_mode(
            "test-model",
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            }),
            Box::new(sink.clone()),
            Box::new(move || *s.lock().unwrap()),
            false,
            alternate,
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
async fn selection_arrow_keys_move_endpoint_and_reveal_it_without_touching_composer() {
    let tty = Tty::new(40, 16);
    tty.ui.write(&format!("a👩‍💻b\n{}", (0..35).map(|i| format!("line {i:02} content\n")).collect::<String>()));
    tty.ui.set_input("unsent draft", false);
    tty.ui.set_status("Running");
    tty.frame();
    let mut st = tty.ui.state.lock().unwrap();
    st.ensure_body_layout();
    let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout);
    selected.end = selection::Point { line: 0, offset: 1 };
    st.selection = Some(selected);
    let cursor = st.cursor;
    let mut actions = vec![];
    tty.ui.keypress_inner(&mut st, None, &Key::named("right"), &mut actions);
    assert!(actions.is_empty());
    assert_eq!(st.selection.as_ref().unwrap().end.offset, 12);
    assert_eq!(st.cursor, cursor);
    assert_eq!(st.input.iter().collect::<String>(), "unsent draft");
    assert!(st.scroll > 0, "offscreen endpoint is revealed rather than moving draft caret");
    for _ in 0..30 {
        tty.ui.keypress_inner(&mut st, None, &Key { shift: true, ..Key::named("down") }, &mut actions);
    }
    assert!(st.selection.as_ref().unwrap().end.line >= 30);
    tty.ui.render(&mut st);
    let end_row = st.body_layout.row_for(st.selection.as_ref().unwrap().end).unwrap();
    assert!((st.transcript_top_row..st.transcript_top_row + st.transcript_view_rows.len()).contains(&end_row));
    assert_eq!(st.cursor, cursor);
    assert_eq!(st.input.iter().collect::<String>(), "unsent draft");
    assert!(st.queued_prompts.is_empty());
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn repeated_mouse_clicks_select_unicode_words_and_whole_wrapped_logical_lines() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(36, 24);
    let line = "café 👩‍💻 words repeated enough to wrap onto several screen rows";
    tty.ui.write(&format!("{line}\nnext line\n"));
    tty.frame();
    let row = {
        let st = tty.ui.state.lock().unwrap();
        st.transcript_screen_row + st.transcript_view_rows.iter().position(|r| r.contains("café")).unwrap()
    };
    let mouse = |kind, column| tty.ui.handle_mouse(MouseEvent { kind, column, row: row as u16, modifiers: KeyModifiers::NONE });
    let click = |column| {
        mouse(MouseEventKind::Down(MouseButton::Left), column);
        mouse(MouseEventKind::Up(MouseButton::Left), column);
    };
    click(1);
    assert!(tty.ui.selected_transcript_text().is_none());
    click(1);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("café"));
    click(1);
    assert_eq!(tty.ui.selected_transcript_text(), Some(format!("{line}\n")), "triple click selects logical line, not a visual wrap");
    *tty.size.lock().unwrap() = (70, 24);
    tty.ui.resize();
    tty.frame();
    assert_eq!(tty.ui.selected_transcript_text(), Some(format!("{line}\n")));
    // Keyboard action breaks the multi-click chain before another word gesture.
    tty.key("escape");
    click(5);
    click(5);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("👩‍💻"), "ZWJ emoji remains an atomic word-bound segment");
}

#[tokio::test]
async fn shift_click_extends_original_character_and_word_anchors_in_both_directions() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(80, 24);
    tty.ui.write("one café three four\n");
    tty.ui.set_input("keep draft", false);
    tty.frame();
    let row = {
        let st = tty.ui.state.lock().unwrap();
        st.transcript_screen_row + st.transcript_view_rows.iter().position(|r| r.contains("one café")).unwrap()
    };
    let click = |column, modifiers| {
        for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
            tty.ui.handle_mouse(MouseEvent { kind, column, row: row as u16, modifiers });
        }
    };
    let mouse = |kind, column| tty.ui.handle_mouse(MouseEvent { kind, column, row: row as u16, modifiers: KeyModifiers::NONE });
    mouse(MouseEventKind::Down(MouseButton::Left), 4);
    mouse(MouseEventKind::Up(MouseButton::Left), 8);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("café"));
    click(14, KeyModifiers::SHIFT);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("café three"));
    click(0, KeyModifiers::SHIFT);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("one "), "character anchor remains at original caret");
    tty.key("escape");
    click(5, KeyModifiers::NONE);
    click(5, KeyModifiers::NONE);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("café"));
    click(11, KeyModifiers::SHIFT);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("café three"));
    click(1, KeyModifiers::SHIFT);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("one café"), "word anchor retains entire initial word on reversal");
    assert_eq!(draft(&tty), "keep draft");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn windows_copy_shortcuts_decode_without_altgr_becoming_copy() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(80, 24);
    tty.ui.write("selected text\n");
    tty.ui.set_input("draft", false);
    tty.frame();
    let mut st = tty.ui.state.lock().unwrap();
    st.ensure_body_layout();
    let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout);
    selected.end = selection::Point { line: 0, offset: 8 };
    st.selection = Some(selected);
    for event in [
        KeyEvent::new(KeyCode::Insert, KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
    ] {
        let (text, key) = key_from_event(&event).unwrap();
        let mut actions = vec![];
        tty.ui.keypress_inner(&mut st, text.as_deref(), &key, &mut actions);
        assert!(matches!(actions.as_slice(), [Action::CopySelection(_)]), "{event:?}");
    }
    let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL | KeyModifiers::ALT);
    let (text, key) = key_from_event(&event).unwrap();
    assert!(!key.ctrl && !key.meta, "AltGr remains text");
    assert_eq!(text.as_deref(), Some("c"));
    assert_eq!(st.input.iter().collect::<String>(), "draft");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
    st.selection = None;
    st.status = "Running".into();
    for event in [KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL), KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL | KeyModifiers::SHIFT)] {
        let (text, key) = key_from_event(&event).unwrap();
        let mut actions = vec![];
        tty.ui.keypress_inner(&mut st, text.as_deref(), &key, &mut actions);
        assert!(actions.is_empty(), "copy-only chord without selection cannot interrupt");
        assert_eq!(st.input.iter().collect::<String>(), "draft");
    }
}

#[tokio::test]
async fn selection_copy_acknowledgment_preserves_retry_and_rejects_stale_results() {
    use crate::cli::ClipboardStatus;
    let tty = Tty::new(80, 24);
    tty.ui.write("selected text\nnext line\n");
    tty.ui.set_input("unsent draft", false);
    tty.frame();
    let install = || {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout);
        selected.end = selection::Point { line: 0, offset: 8 };
        st.selection = Some(selected);
        st.scroll = 3;
    };
    install();
    let request = tty.ui.state.lock().unwrap().selection.as_ref().unwrap().copy_request(true).unwrap();
    for status in [ClipboardStatus::Failed, ClipboardStatus::Unconfirmed] {
        tty.ui.finish_selection_copy(&request, status);
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("selected"));
        assert_eq!(tty.ui.state.lock().unwrap().scroll, 3);
    }
    {
        let mut st = tty.ui.state.lock().unwrap();
        let selected = st.selection.as_mut().unwrap();
        selected.extend(selection::Point { line: 0, offset: 9 });
        selected.extend(selection::Point { line: 0, offset: 8 });
    }
    tty.ui.finish_selection_copy(&request, ClipboardStatus::Confirmed);
    assert!(tty.ui.state.lock().unwrap().selection.is_some(), "changing away and back is not the original pending copy range");
    tty.ui.state.lock().unwrap().selection.as_mut().unwrap().end.offset = 9;
    tty.ui.finish_selection_copy(&request, ClipboardStatus::Confirmed);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("selected "), "extension cannot be cleared by old delivery");
    install(); // Same content/anchors, but a new gesture identity.
    tty.ui.finish_selection_copy(&request, ClipboardStatus::Confirmed);
    assert!(tty.ui.state.lock().unwrap().selection.is_some());
    let request = tty.ui.state.lock().unwrap().selection.as_ref().unwrap().copy_request(false).unwrap();
    tty.ui.finish_selection_copy(&request, ClipboardStatus::Confirmed);
    assert!(tty.ui.selected_transcript_text().is_none());
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 3, "ordinary copy keeps reading position");
    install();
    let request = tty.ui.state.lock().unwrap().selection.as_ref().unwrap().copy_request(true).unwrap();
    tty.ui.finish_selection_copy(&request, ClipboardStatus::Confirmed);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 0, "Enter copy follows after confirmed delivery");
    assert_eq!(draft(&tty), "unsent draft");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn selection_copy_ack_cannot_undo_scrolling_even_after_returning_to_the_same_position() {
    use crate::cli::ClipboardStatus;
    for follow in [false, true] {
        for return_to_start in [false, true] {
            let tty = Tty::new(80, 16);
            tty.ui.write(&format!("selected text\n{}", "another line\n".repeat(40)));
            tty.ui.set_input("unsent draft", false);
            tty.frame();
            let request = {
                let mut st = tty.ui.state.lock().unwrap();
                st.ensure_body_layout();
                let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout);
                selected.end = selection::Point { line: 0, offset: 8 };
                st.selection = Some(selected);
                st.scroll = 3;
                st.status_notice = Some("newer notice".into());
                let request = st.selection.as_ref().unwrap().copy_request(follow).unwrap();
                tty.ui.scroll_by(&mut st, 2);
                if return_to_start {
                    tty.ui.scroll_by(&mut st, -2);
                }
                request
            };
            tty.ui.finish_selection_copy(&request, ClipboardStatus::Confirmed);
            let st = tty.ui.state.lock().unwrap();
            assert!(st.selection.is_some(), "reading navigation invalidates pending copy ownership");
            assert_eq!(st.scroll, if return_to_start { 3 } else { 5 });
            assert_eq!(st.status_notice.as_deref(), Some("newer notice"));
            assert_eq!(st.input.iter().collect::<String>(), "unsent draft");
            assert!(st.queued_prompts.is_empty());
            assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn selection_copy_ack_cannot_undo_home_or_unchanged_endpoint_reveal() {
    for home in [false, true] {
        let tty = Tty::new(80, 16);
        tty.ui.set_transcript_overlay(true);
        tty.ui.write(&format!("selected text\n{}", "another line\n".repeat(40)));
        tty.frame();
        let (request, scroll) = {
            let mut st = tty.ui.state.lock().unwrap();
            st.ensure_body_layout();
            let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 8 }, &st.transcript, &st.body_layout);
            selected.end = selection::Point { line: 0, offset: 0 };
            st.selection = Some(selected);
            let request = st.selection.as_ref().unwrap().copy_request(true).unwrap();
            let mut actions = vec![];
            tty.ui.keypress_inner(&mut st, None, &Key::named(if home { "home" } else { "left" }), &mut actions);
            assert!(actions.is_empty());
            assert_eq!(st.selection.as_ref().unwrap().end.offset, 0, "logical endpoint did not move");
            assert!(st.scroll > 0, "navigation revealed earlier history");
            (request, st.scroll)
        };
        tty.ui.finish_selection_copy(&request, crate::cli::ClipboardStatus::Confirmed);
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("selected"));
        assert_eq!(tty.ui.state.lock().unwrap().scroll, scroll);
    }
}

#[tokio::test]
async fn selection_copy_ack_survives_noop_scroll_but_not_actual_resize() {
    use crate::cli::ClipboardStatus;
    for resize in [false, true] {
        let tty = Tty::new(80, 24);
        tty.ui.write("selected text\n");
        tty.frame();
        let request = {
            let mut st = tty.ui.state.lock().unwrap();
            st.ensure_body_layout();
            let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout);
            selected.end = selection::Point { line: 0, offset: 8 };
            st.selection = Some(selected);
            let request = st.selection.as_ref().unwrap().copy_request(true).unwrap();
            tty.ui.scroll_by(&mut st, -2); // Already at Latest; no reading-position change.
            request
        };
        if resize {
            *tty.size.lock().unwrap() = (60, 20);
        }
        tty.ui.resize();
        tty.ui.finish_selection_copy(&request, ClipboardStatus::Confirmed);
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), resize.then_some("selected"), "only actual geometry changes invalidate the pending acknowledgment");
    }
}

#[tokio::test]
async fn enter_and_control_copy_capture_selection_without_submitting_draft_or_cancelling() {
    let tty = Tty::new(80, 24);
    tty.ui.write("selected text\n");
    tty.ui.set_input("unsent draft", false);
    tty.ui.set_status("Running");
    tty.frame();
    let mut st = tty.ui.state.lock().unwrap();
    st.ensure_body_layout();
    let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout);
    selected.end = selection::Point { line: 0, offset: 8 };
    st.selection = Some(selected);
    for (key, follow) in [(Key::named("return"), true), (Key { ctrl: true, ..Key::named("c") }, false), (Key { ctrl: true, ..Key::named("o") }, false)] {
        st.scroll = 3;
        let mut actions = vec![];
        tty.ui.keypress_inner(&mut st, None, &key, &mut actions);
        assert_eq!(actions.len(), 1);
        let Action::CopySelection(request) = actions.remove(0) else { panic!("selection owns copy key") };
        assert_eq!(request.text, "selected");
        // Completion is exercised separately; no real clipboard writes in these tests.
        drop(st);
        tty.ui.finish_selection_copy(&request, crate::cli::ClipboardStatus::Confirmed);
        st = tty.ui.state.lock().unwrap();
        assert!(st.selection.is_none());
        assert_eq!(st.scroll, if follow { 0 } else { 3 });
        assert_eq!(st.input.iter().collect::<String>(), "unsent draft");
        assert!(st.queued_prompts.is_empty());
        assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
        let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout);
        selected.end = selection::Point { line: 0, offset: 8 };
        st.selection = Some(selected);
    }
}

#[tokio::test]
async fn clipboard_ack_does_not_follow_behind_a_new_modal_prompt() {
    let tty = Tty::new(80, 24);
    tty.ui.write("selected text\n");
    tty.frame();
    let request = {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        let mut selected = selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout);
        selected.end = selection::Point { line: 0, offset: 8 };
        st.selection = Some(selected);
        st.scroll = 3;
        st.selection.as_ref().unwrap().copy_request(true).unwrap()
    };
    let ui = tty.ui.clone();
    let prompt = tokio::spawn(async move { ui.question("Approval: ", None, false, QuestionOpts { esc_cancels: true, ..Default::default() }).await });
    tokio::task::yield_now().await;
    assert!(tty.ui.state.lock().unwrap().pending.is_some());
    tty.ui.finish_selection_copy(&request, crate::cli::ClipboardStatus::Confirmed);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 3, "late completion cannot navigate a modal");
    tty.key("escape");
    assert!(prompt.await.unwrap().unwrap_err().is::<InputCancelled>());
}

#[cfg(unix)]
#[tokio::test]
async fn suspend_yields_rendering_and_restores_both_modes_even_on_failure() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    for alternate in [true, false] {
        let tty = Tty::with_mode(80, 24, alternate);
        tty.text("unsent draft");
        tty.frame();
        let mut decoder = InputDecoder::default();
        decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL))], std::time::Instant::now(), false);
        assert!(tty.ui.state.lock().unwrap().suspend_requested);
        tty.ui.suspend_with(|| {
            let before = tty.written();
            tty.ui.render_now();
            assert_eq!(before, tty.written());
            assert!(tty.ui.state.lock().unwrap().input_yielded);
            Err(std::io::ErrorKind::TimedOut.into())
        });
        let st = tty.ui.state.lock().unwrap();
        assert!(!st.input_yielded);
        assert_eq!(st.input.iter().collect::<String>(), "unsent draft");
        assert!(st.status_notice.as_ref().unwrap().contains("Suspend/resume failed"));
        drop(st);
        assert!(tty.frame().contains("unsent draft"));
        assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
        if !alternate {
            assert!(!tty.written().contains("\x1b[?1049"));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn suspend_is_excluded_from_paste_and_editor_and_defers_new_questions() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(80, 24);
    let mut decoder = InputDecoder::default();
    decoder.feed_batch(&tty.ui, vec![Event::Paste("pasted\u{1a}text".into())]);
    assert!(!tty.ui.state.lock().unwrap().suspend_requested);
    tty.ui.state.lock().unwrap().editor_active = true;
    tty.ui.request_suspend();
    assert!(!tty.ui.state.lock().unwrap().suspend_requested);
    tty.ui.state.lock().unwrap().editor_active = false;
    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL))], std::time::Instant::now(), false);
    let ui = tty.ui.clone();
    let question = tokio::spawn(async move { ui.question("Approval: ", None, false, QuestionOpts { esc_cancels: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(tty.ui.state.lock().unwrap().pending.is_none());
    tty.ui.suspend_with(|| Ok(()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(tty.ui.state.lock().unwrap().pending.is_some());
    tty.key("escape");
    assert!(question.await.unwrap().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn closing_while_yielded_never_reenters_raw_or_alternate_screen() {
    for alternate in [true, false] {
        let tty = Tty::with_mode(80, 24, alternate);
        tty.ui.request_suspend();
        let mut yielded = String::new();
        tty.ui.suspend_with(|| {
            tty.ui.close();
            assert!(!tty.ui.state.lock().unwrap().terminal_restored);
            yielded = tty.written();
            Ok(())
        });
        let st = tty.ui.state.lock().unwrap();
        assert!(st.closed && st.terminal_restored && !st.input_yielded);
        assert_eq!(tty.written(), yielded, "closed yielded terminal must not move the shell's cursor again");
    }
}

#[tokio::test]
async fn external_editor_shortcut_is_modal_safe_and_applies_literal_draft_without_submission() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(100, 24);
    tty.ui.paste("large\npasted\ncontent");
    let token = {
        let mut st = tty.ui.state.lock().unwrap();
        st.attached_images.insert("[Image #1]".into(), "one.png".into());
        st.attached_images.insert("[Image #2]".into(), "two.png".into());
        st.pasted_texts.keys().next().unwrap().clone()
    };
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL), std::time::Instant::now(), true);
    assert!(tty.ui.state.lock().unwrap().editor_requested);
    tty.text("ignored after handoff request");
    {
        let mut st = tty.ui.state.lock().unwrap();
        assert!(!st.input.iter().collect::<String>().contains("ignored"));
        st.editor_requested = false;
        Tui::apply_external_edit(&mut st, "edited [Image #2] text\n\n");
        assert!(st.pasted_texts.is_empty());
        assert!(!st.input.iter().collect::<String>().contains(&token));
        assert_eq!(st.input.iter().collect::<String>(), "edited [Image #2] text");
        assert_eq!(st.attached_images.len(), 1);
        assert!(st.attached_images.contains_key("[Image #2]"));
        assert_eq!(st.cursor, st.input.len());
        assert!(st.history.is_empty());
    }

    let ui = tty.ui.clone();
    let modal = tokio::spawn(async move { ui.question("Secret: ", None, false, QuestionOpts { mask: true, esc_cancels: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.key(None, modified("g", true, false, false));
    assert!(!tty.ui.state.lock().unwrap().editor_requested);
    tty.key("escape");
    assert!(modal.await.unwrap().is_err());
}

#[tokio::test]
async fn close_during_editor_waits_for_handoff_before_terminal_teardown() {
    let tty = Tty::new(100, 24);
    tty.text("owned draft");
    tty.ui.state.lock().unwrap().editor_active = true;
    tty.ui.submit_external_prompt("must not submit");
    assert!(!tty.ui.has_queued_prompts());
    let guard = editor::EditorGuard(&tty.ui);
    tty.sink.0.lock().unwrap().clear();
    tty.ui.close();
    assert!(tty.ui.is_closed());
    assert!(tty.written().is_empty(), "terminal must remain owned by editor until cleanup");
    let ui = tty.ui.clone();
    let waiter = tokio::spawn(async move { ui.wait_for_editor_cleanup().await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!waiter.is_finished());
    drop(guard);
    tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
    assert!(tty.written().ends_with("\x1b[?25h\x1b[?1049l"));
    tty.sink.0.lock().unwrap().clear();
    tty.ui.close();
    assert!(tty.written().is_empty(), "teardown is idempotent");
}

#[tokio::test]
async fn editor_handoff_suppresses_rendering_and_defers_cancellable_approvals() {
    let tty = Tty::new(100, 24);
    tty.text("owned draft");
    tty.ui.state.lock().unwrap().editor_active = true;
    tty.sink.0.lock().unwrap().clear();
    tty.ui.write("background output\n");
    tty.ui.render_now();
    assert!(tty.written().is_empty());
    let ui = tty.ui.clone();
    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let modal = tokio::spawn(async move { ui.question("Approve? ", Some(&c), false, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!modal.is_finished());
    assert_eq!(draft(&tty), "owned draft");
    cancel.cancel();
    assert!(modal.await.unwrap().is_err());
    tty.ui.state.lock().unwrap().editor_active = false;
}

#[tokio::test]
async fn approval_waits_for_requested_editor_and_restores_the_edited_draft() {
    let tty = Tty::new(100, 24);
    tty.text("before edit");
    tty.ui.state.lock().unwrap().editor_requested = true;
    let ui = tty.ui.clone();
    let modal = tokio::spawn(async move { ui.question("Approve? ", None, false, QuestionOpts { esc_cancels: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(tty.ui.state.lock().unwrap().pending.is_none());
    assert_eq!(draft(&tty), "before edit");
    {
        let mut st = tty.ui.state.lock().unwrap();
        Tui::apply_external_edit(&mut st, "after edit");
        st.editor_requested = false;
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(tty.ui.state.lock().unwrap().pending.is_some());
    tty.key("escape");
    assert!(modal.await.unwrap().is_err());
    assert_eq!(draft(&tty), "after edit");
}

#[tokio::test]
async fn native_resize_does_not_reemit_committed_transcript() {
    let tty = Tty::with_mode(90, 20, false);
    let mut st = tty.ui.state.lock().unwrap();
    st.transcript = (0..35).map(|i| format!("committed-{i:02} Unicode 👩‍💻 content to wrap after resizing the terminal\n")).collect();
    tty.ui.render(&mut st);
    assert!(tty.written().contains("committed-00"));
    tty.sink.0.lock().unwrap().clear();
    drop(st);
    *tty.size.lock().unwrap() = (35, 16);
    tty.ui.resize();
    tty.ui.render_now();
    assert!(!tty.written().contains("committed-00"), "resize must not duplicate source committed at the old width");
    assert!(tty.frame().contains("committed-34") || tty.frame().contains("Unicode"));
    tty.ui.close();
}

#[tokio::test]
async fn transcript_presentation_changes_never_reinterpret_stale_selection_rows() {
    let tty = Tty::new(100, 24);
    tty.ui.write("[reasoning]\nhidden thinking\n[answer]\nvisible answer\n");
    tty.ui.set_view_mode("normal");
    tty.frame();
    {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        st.selection = Some(selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout));
        st.selection_dragging = true;
    }
    tty.ui.set_transcript_overlay(true);
    assert!(tty.ui.state.lock().unwrap().selection.is_none());
    tty.frame();
    {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        st.selection = Some(selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout));
    }
    tty.ui.key(None, modified("t", true, false, false));
    assert!(tty.ui.state.lock().unwrap().selection.is_none());
    assert!(!tty.ui.state.lock().unwrap().selection_dragging);
}

#[tokio::test]
async fn native_full_height_modals_restore_compact_viewport_at_bottom_without_alt_screen() {
    for modal in ["transcript", "approval"] {
        let tty = Tty::with_mode(100, 24, false);
        tty.ui.write(&(0..40).map(|n| format!("native-{n}\n")).collect::<String>());
        tty.frame();
        let task = if modal == "transcript" {
            tty.ui.set_transcript_overlay(true);
            None
        } else {
            let ui = tty.ui.clone();
            Some(tokio::spawn(async move { ui.question("Approval: ", None, false, QuestionOpts { esc_cancels: true, ..Default::default() }).await }))
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        tty.frame();
        assert_eq!(tty.ui.state.lock().unwrap().last_frame_rows.len(), 24);
        tty.sink.0.lock().unwrap().clear();
        tty.key("escape");
        if let Some(task) = task {
            assert!(task.await.unwrap().is_err());
        }
        tty.frame();
        let written = tty.written();
        assert!(!written.contains("\x1b[?1049"), "no-alt-screen must apply to modal entry and exit");
        assert!(!written.contains("native-0\x1b[0m\r\n"), "committed history is not replayed");
        let height = tty.ui.state.lock().unwrap().last_frame_rows.len();
        assert!(height < 24);
        assert!(written.contains(&format!("\x1b[{}B", 24 - height)), "compact viewport must be restored at bottom: {written:?}");
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
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0, "idle Ctrl+C arms quit instead of exiting");
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
    assert!(frame.contains("Preparing write_file"));
    assert!(frame.contains("Explored"));
    assert!(regex::Regex::new(r"Read.*src/index\.ts").unwrap().is_match(&frame));
    assert!(regex::Regex::new(r"Search.*createModel in src").unwrap().is_match(&frame));
    assert!(frame.contains("Ran"));
    assert!(frame.contains("Edited"));
    assert!(regex::Regex::new(r"• .*Todo list:.* completed").unwrap().is_match(&frame), "{frame}");
    let lines: Vec<&str> = raw.split("\r\n").collect();
    let bullet = lines.iter().find(|l| l.contains("Todo list:")).unwrap();
    assert!(!bullet.contains(text::ERR));
    let removed = lines.iter().find(|l| crate::term::strip_vt(l).contains("const oldVal = 1;")).unwrap();
    assert!(removed.contains(text::ERR));
    assert!(removed.contains(text::Palette::DARK.del_bg), "removed lines sit on a red tint: {removed:?}");
}

#[tokio::test]
async fn failed_edit_does_not_claim_success_or_show_proposed_diff_as_applied() {
    let tty = Tty::new(100, 30);
    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "failed-edit", "name": "apply_patch", "state": "error", "meta": {
        "kind": "edit", "verb": "Edited", "summary": "src/subagents.rs", "added": 1, "removed": 1,
        "diff": "--- a/src/subagents.rs\n+++ b/src/subagents.rs\n@@\n-before_marker\n+after_marker",
        "error": "Patch hunk is ambiguous (matched 2 times)"
    } }));
    tty.ui.render_now();
    let frame = crate::term::strip_vt(&tty.ui.rendered_frame()).to_string();
    assert!(frame.contains("Failed to edit src/subagents.rs"), "{frame}");
    assert!(frame.contains("Patch hunk is ambiguous"), "{frame}");
    assert!(!frame.contains("Edited"), "{frame}");
    assert!(!frame.contains("after_marker"), "{frame}");
    assert!(!frame.contains("(+1 -1)"), "{frame}");
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
    for (cols, rows) in [(20, 10), (48, 12), (60, 22), (120, 40)] {
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
            assert!(frame.contains("│ a   │ bb  │"), "{frame}");
            assert!(frame.contains("│ ccc │ d   │"), "{frame}");
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
        st.ensure_body_layout();
        let incremental = tty.ui.render_styled_body_lines(&mut st, cols, true);
        let incremental_ends = st.body_source_ends.clone();
        let incremental_logical = st.body_layout.text.clone();
        let incremental_hits = (0..incremental.len()).flat_map(|row| [0, 1, 5, cols].map(|col| st.body_layout.hit(row, col))).collect::<Vec<_>>();
        let cached = st.body_cache.take();
        let full = tty.ui.render_styled_body_lines(&mut st, cols, true);
        assert_eq!(incremental, full, "step {step}");
        assert_eq!(incremental_ends, st.body_source_ends, "source map step {step}");
        assert_eq!(incremental_logical, st.body_layout.text, "logical text step {step}");
        let full_hits = (0..full.len()).flat_map(|row| [0, 1, 5, cols].map(|col| st.body_layout.hit(row, col))).collect::<Vec<_>>();
        assert_eq!(incremental_hits, full_hits, "logical hit map step {step}");
        assert_eq!(full.len(), st.body_source_ends.len());
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
async fn long_output_never_drifts_past_the_width_scrolls_with_pageup_and_pasted_image_paths_collapse() {
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
    tty.key("pageup");
    assert!(tty.frame().contains("Follow output"));
    tty.key("escape");
    assert!(!tty.frame().contains("Follow output"));
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
    // Composer card: rounded frame around the prompt, one quiet footer line below it.
    assert!(regex::Regex::new(r"^╭─+╮$").unwrap().is_match(&lines[n - 4]), "{:?}", lines[n - 4]);
    assert!(regex::Regex::new(r"^│ › Ask goon anything · / commands.*│$").unwrap().is_match(&lines[n - 3]), "{:?}", lines[n - 3]);
    assert!(regex::Regex::new(r"^╰─+╯$").unwrap().is_match(&lines[n - 2]), "{:?}", lines[n - 2]);
    assert!(lines[n - 1].starts_with("  ? for shortcuts"), "{:?}", lines[n - 1]);
    assert!(lines[n - 1].ends_with("0 tokens   test-model"), "{:?}", lines[n - 1]);
    assert_eq!(visible_length(&lines[n - 3]), visible_length(&lines[n - 4]), "frame edges line up");
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
    assert!(regex::Regex::new(r"^│ › can u play valorant\s*│$").unwrap().is_match(&lines[lines.len() - 3]), "{:?}", lines[lines.len() - 3]);
    tty.key("escape");
    tty.key("escape");
    assert_eq!(q2.await.unwrap().unwrap(), "/revert");
}

#[tokio::test]
async fn welcome_banner_is_plain_text_framed_as_a_card_and_view_mode_toggles_thinking() {
    let workspace = std::path::PathBuf::from("D:\\Th\u{01B0} mu\u{0323}c m\u{01A1}\u{0301}i (2)");
    let banner = crate::cli::welcome_banner("gpt-5.4", "openai", "https://api.ezaiapi.com/v1", Some("max"), &workspace, "0f40f011-0000-4000-8000-000000000000", "ask");
    let lines: Vec<&str> = banner.trim().split('\n').collect();
    assert_eq!(lines.len(), 5, "{banner}");
    assert!(regex::Regex::new(r"^◆ goon v\d+\.\d+\.\d+").unwrap().is_match(lines[0]), "{}", lines[0]);
    assert!(regex::Regex::new(r"model +gpt-5\.4 · effort max · mode ask").unwrap().is_match(&banner), "{banner}");
    assert!(banner.contains("gateway    api.ezaiapi.com/v1 · openai"));
    assert!(banner.contains("session 0f40f011"));
    // The text stays plain (pipes, NO_COLOR); only the TUI frames it as a card.
    assert!(!banner.contains(['╭', '╮', '╰', '╯', '│']));

    for (cols, boxed) in [(40, true), (28, false), (100, true)] {
        let tty = Tty::new(cols, 20);
        tty.ui.write(&banner);
        let frame = tty.frame();
        for row in rows(&frame) {
            assert!(visible_length(&row) <= cols, "{row:?}");
        }
        assert!(frame.contains("◆ goon v"));
        assert!(frame.contains("/help commands"), "{frame}");
        let lines = rows(&frame);
        let top = lines.iter().position(|l| l.starts_with('╭'));
        assert_eq!(top.is_some(), boxed, "{cols} cols:\n{frame}");
        if let Some(top) = top {
            assert!(lines[top + 1].starts_with("│ ◆ goon v"), "{frame}");
            let bottom = lines.iter().skip(top).position(|l| l.starts_with('╰')).unwrap() + top;
            let width = visible_length(&lines[top]);
            assert!(lines[top..=bottom].iter().all(|l| visible_length(l) == width), "card edges align:\n{frame}");
            // The card is decoration: selecting across it copies nothing from the frame.
            let mut st = tty.ui.state.lock().unwrap();
            st.ensure_body_layout();
            assert!(st.body_layout.text.iter().all(|t| !t.contains('│') && !t.contains('╭')));
        } else {
            assert!(frame.contains("model gpt-5.4"), "narrow banner collapses label padding:\n{frame}");
        }
        tty.ui.close();
    }
    let tty = Tty::new(40, 20);
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
    let body = &all[..all.len() - 4];
    let users: Vec<&String> = body.iter().filter(|l| l.starts_with(&format!("{}{}›", text::Palette::DARK.surface, Theme::BRAND_BOLD))).collect();
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
    assert!(!plain.contains("context left"));
    assert_eq!(plain.matches("test-model").count(), 1);
    tty.ui.event(&serde_json::json!({ "type": "usage", "input": 60000, "output": 8000, "total": 68000 }));
    tty.ui.event(&serde_json::json!({ "type": "subagent", "id": "kid", "state": "progress", "event": { "type": "usage", "input": 150000, "output": 1, "total": 150001 } }));
    assert!(crate::term::strip_vt(&footer(&tty)).contains("66% context left"));
    assert!(tty.frame().contains("218k tokens"));
    tty.ui.event(&serde_json::json!({ "type": "compaction", "count": 1 }));
    assert!(!crate::term::strip_vt(&footer(&tty)).contains("context left"));
    assert!(tty.frame().contains("◌ Context compacted #1"));
    tty.ui.event(&serde_json::json!({ "type": "usage", "input": 185000, "output": 0, "total": 185000 }));
    let raw = footer(&tty);
    assert!(crate::term::strip_vt(&raw).contains("7% context left"));
    assert!(raw.contains(&format!("{}7% context left", Theme::ERR)), "{raw:?}");
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
    assert!(regex::Regex::new(r"^│ › /rev\s*│$").unwrap().is_match(&lines[lines.len() - 3]), "{:?}", lines[lines.len() - 3]);
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
async fn narrow_tables_show_all_values_instead_of_clipping_the_right_columns() {
    let tty = Tty::new(32, 60);
    tty.ui.write("[answer]\n| Task | Status | Next step |\n|---|---|---|\n| Task 1 | Complete | Review results |\n| Task 2 | In progress | Finish implementation |\n");
    let frame = tty.frame();
    for word in ["Task 1", "Complete", "Review", "results", "Task 2", "In progress", "Finish", "implementation"] {
        assert!(frame.contains(word), "narrow table loses {word:?}:\n{frame}");
    }
}

#[tokio::test]
async fn selecting_a_record_label_highlights_only_its_visual_occurrence_after_reflow() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    for width in [24, 32] {
        let tty = Tty::new(width, 60);
        tty.ui.write("[answer]\n| Task | Status | Next step |\n|---|---|---|\n| Task 1 | SECRET_HIDDEN_VALUE | Review results |\n| Task 2 | Planned_long_status | Finish implementation |\n");
        tty.frame();
        let (column, row) = {
            let st = tty.ui.state.lock().unwrap();
            let index = st.transcript_view_rows.iter().rposition(|line| line.contains("Next step")).unwrap();
            let plain = crate::term::strip_vt(&st.transcript_view_rows[index]);
            (visible_length(plain.split("Next step").next().unwrap()) as u16, (st.transcript_screen_row + index) as u16)
        };
        for (kind, column) in [(MouseEventKind::Down(MouseButton::Left), column), (MouseEventKind::Up(MouseButton::Left), column + 9)] {
            tty.ui.handle_mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE });
        }
        let copy = tty.ui.selected_transcript_text().unwrap();
        assert!(copy.contains("Next step"));
        assert!(!copy.contains("Finish"));
        for columns in [width, 100, 24, 32, width] {
            *tty.size.lock().unwrap() = (columns, 60);
            tty.ui.resize();
            tty.frame();
            assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(copy.as_str()));
            let st = tty.ui.state.lock().unwrap();
            let labels: Vec<_> = st.last_frame_rows.iter().filter(|line| crate::term::strip_vt(line).contains("Next step")).collect();
            let highlighted = labels.iter().filter(|line| line.contains("\x1b[7m")).count();
            assert_eq!(highlighted, 1, "only the selected label occurrence may highlight at width {columns}: {labels:?}");
            if columns != 100 {
                assert!(labels.last().unwrap().contains("\x1b[7m"), "second record retains label ownership");
            }
        }
    }
}

#[tokio::test]
async fn record_label_drag_never_copies_cells_outside_the_visible_gesture() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    for (columns, reverse, shift_click) in [(24, false, false), (24, true, false), (32, false, false), (32, true, false), (24, false, true), (32, true, true)] {
        let tty = Tty::new(columns, 60);
        tty.ui.write("[answer]\n| Task | Status | Next step |\n|---|---|---|\n| Task 1 | SECRET_HIDDEN_VALUE | Review results |\n| Task 2 | Planned_long_status | Finish implementation |\n");
        tty.frame();
        let (start, end) = {
            let st = tty.ui.state.lock().unwrap();
            let row = st.transcript_view_rows.iter().position(|line| line.contains("Review results")).unwrap();
            let label = st.transcript_view_rows[..=row].iter().rposition(|line| line.contains("Next step")).unwrap();
            if columns == 32 {
                assert_eq!(label, row, "aligned records place label and value on the same row");
            }
            let at = |index: usize, text: &str, end: bool| {
                let plain = crate::term::strip_vt(&st.transcript_view_rows[index]);
                let col = visible_length(plain.split(text).next().unwrap()) + if end { visible_length(text) } else { 0 };
                (col as u16, (st.transcript_screen_row + index) as u16)
            };
            (at(label, "Next step", false), at(row, "Review results", true))
        };
        let (start, end) = if reverse { (end, start) } else { (start, end) };
        if shift_click {
            for (kind, (column, row), modifiers) in [
                (MouseEventKind::Down(MouseButton::Left), start, KeyModifiers::NONE),
                (MouseEventKind::Up(MouseButton::Left), end, KeyModifiers::NONE),
                (MouseEventKind::Down(MouseButton::Left), end, KeyModifiers::SHIFT),
            ] {
                tty.ui.handle_mouse(MouseEvent { kind, column, row, modifiers });
            }
        } else {
            for (kind, (column, row)) in [(MouseEventKind::Down(MouseButton::Left), start), (MouseEventKind::Up(MouseButton::Left), end)] {
                tty.ui.handle_mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE });
            }
        }
        let expected = "|  |  | Next step |\n|---|---|---|\n|  |  | Review results |";
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
        tty.frame();
        assert!(!tty.ui.rendered_frame().contains("\x1b[7mSECRET"), "unselected values cannot be highlighted");
        *tty.size.lock().unwrap() = (100, 60);
        tty.ui.resize();
        tty.frame();
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
    }
}

#[tokio::test]
async fn record_word_and_line_gestures_expand_visual_units_without_hidden_cells() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    for clicks in [2, 3] {
        let tty = Tty::new(24, 60);
        tty.ui.write("[answer]\n| Task | Status | Next step |\n|---|---|---|\n| Task 1 | SECRET_HIDDEN_VALUE | Review results |\n| Task 2 | Planned_long_status | Finish implementation |\n");
        tty.frame();
        let (label, value) = {
            let st = tty.ui.state.lock().unwrap();
            let value = st.transcript_view_rows.iter().position(|line| line.contains("Review results")).unwrap();
            let label = st.transcript_view_rows[..value].iter().rposition(|line| line.contains("Next step")).unwrap();
            let position = |index: usize, text: &str| {
                let plain = crate::term::strip_vt(&st.transcript_view_rows[index]);
                (visible_length(plain.split(text).next().unwrap()) as u16 + 1, (st.transcript_screen_row + index) as u16)
            };
            (position(label, "Next"), position(value, "Review"))
        };
        let mouse = |kind, (column, row), modifiers| tty.ui.handle_mouse(MouseEvent { kind, column, row, modifiers });
        for _ in 0..clicks {
            mouse(MouseEventKind::Down(MouseButton::Left), label, KeyModifiers::NONE);
            mouse(MouseEventKind::Up(MouseButton::Left), label, KeyModifiers::NONE);
        }
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(if clicks == 2 { "Next" } else { "Next step" }));
        mouse(MouseEventKind::Down(MouseButton::Left), value, KeyModifiers::SHIFT);
        let expected = if clicks == 2 { "|  |  | Next step |\n|---|---|---|\n|  |  | Review |" } else { "|  |  | Next step |\n|---|---|---|\n|  |  | Review results |" };
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
        mouse(MouseEventKind::Up(MouseButton::Left), value, KeyModifiers::SHIFT);
        *tty.size.lock().unwrap() = (100, 60);
        tty.ui.resize();
        tty.frame();
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
    }
}

#[tokio::test]
async fn adaptive_table_value_selection_keeps_logical_bytes_across_grid_and_records() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(32, 60);
    tty.ui.write("[answer]\n| Task | Status | Next step |\n|---|---|---|\n| Task 1 | Complete | Review results |\n| Task 2 | In progress | Finish implementation |\n");
    tty.frame();
    let (row, column) = {
        let st = tty.ui.state.lock().unwrap();
        let row = st.transcript_view_rows.iter().position(|line| line.contains("Review results")).unwrap();
        let plain = crate::term::strip_vt(&st.transcript_view_rows[row]);
        (st.transcript_screen_row + row, visible_length(plain.split("Review").next().unwrap()))
    };
    for (kind, column) in [(MouseEventKind::Down(MouseButton::Left), column), (MouseEventKind::Up(MouseButton::Left), column + 6)] {
        tty.ui.handle_mouse(MouseEvent { kind, column: column as u16, row: row as u16, modifiers: KeyModifiers::NONE });
    }
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("Review"));
    for columns in [100, 24, 80, 32] {
        *tty.size.lock().unwrap() = (columns, 60);
        tty.ui.resize();
        tty.frame();
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("Review"), "width {columns}");
    }
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn adaptive_table_full_copy_and_native_source_boundaries_survive_layout_changes() {
    let source = "[answer]\n| Task | Status | Next step |\n|---|---|---|\n| Task 1 | Complete | Review results |\n| Task 2 | In progress | Finish implementation |\n";
    let expected = "| Task | Status | Next step |\n|---|---|---|\n| Task 1 | Complete | Review results |\n| Task 2 | In progress | Finish implementation |";
    for columns in [12, 24, 32, 80, 120] {
        let tty = Tty::new(columns, 60);
        tty.ui.write(source);
        tty.frame();
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        let first = st.body_layout.text.iter().position(|line| line.contains("│ Task ")).unwrap();
        let last = st.body_layout.text.iter().position(|line| line.contains("Finish implementation")).unwrap();
        let mut selected = selection::Selection::new(selection::Point { line: first, offset: 0 }, &st.transcript, &st.body_layout);
        selected.end = selection::Point { line: last, offset: st.body_layout.text[last].len() };
        assert_eq!(selected.copy_request(false).unwrap().text, expected, "width {columns}");
        assert!(
            st.body_source_ends.iter().all(|end| *end >= st.transcript.find("| Task 2 | In progress | Finish implementation |").unwrap() + "| Task 2 | In progress | Finish implementation |".len()),
            "native commit cannot consume a table before all source rows, width {columns}: {:?}",
            st.body_source_ends
        );
        st.selection = Some(selected);
        drop(st);
        *tty.size.lock().unwrap() = (100, 60);
        tty.ui.resize();
        tty.frame();
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
    }
}

#[tokio::test]
async fn whole_table_copy_preserves_markdown_structure_alignment_and_empty_cells() {
    for columns in [24, 80, 120] {
        let tty = Tty::new(columns, 30);
        tty.ui.write("[answer]\n| A | B |\n|:---|---:|\n| left | right |\n|  |  |\n");
        tty.frame();
        let request = {
            let mut st = tty.ui.state.lock().unwrap();
            st.ensure_body_layout();
            let first = st.body_layout.text.iter().position(|line| line.contains("│ A")).unwrap();
            let last = st.body_layout.text.iter().rposition(|line| line.contains('│')).unwrap();
            let mut selected = selection::Selection::new(selection::Point { line: first, offset: 0 }, &st.transcript, &st.body_layout);
            selected.end = selection::Point { line: last, offset: st.body_layout.text[last].len() };
            selected.copy_request(false).unwrap()
        };
        assert_eq!(request.text, "| A | B |\n|:---|---:|\n| left | right |\n|  |  |", "width {columns}");
    }
}

#[tokio::test]
async fn table_copy_only_serializes_selected_cell_fragments_and_survives_reflow() {
    let tty = Tty::new(100, 30);
    tty.ui.write("[answer]\n| Label | Value |\n|---|---|\n| **alpha** | beta |\n| gamma | delta |\n");
    tty.frame();
    let install = |first: &str, from: usize, last: &str, to: usize| {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        let line = st.body_layout.text.iter().position(|line| line.contains(first)).unwrap();
        let offset = st.body_layout.text[line].find(first).unwrap() + from;
        let mut selected = selection::Selection::new(selection::Point { line, offset }, &st.transcript, &st.body_layout);
        let line = st.body_layout.text.iter().position(|line| line.contains(last)).unwrap();
        selected.end = selection::Point { line, offset: st.body_layout.text[line].find(last).unwrap() + to };
        st.selection = Some(selected);
    };
    install("alpha", 1, "alpha", 4);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("**lph**"));
    install("alpha", 0, "delta", 5);
    let expected = "|  |  |\n|---|---|\n| **alpha** | beta |\n| gamma | delta |";
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
    *tty.size.lock().unwrap() = (24, 24);
    tty.ui.resize();
    tty.frame();
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
    tty.ui.write("streaming updates must not change a pending selected table\n");
    tty.frame();
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
}

#[tokio::test]
async fn table_copy_keeps_literal_cells_plain_and_mixed_code_cells_formatted() {
    let tty = Tty::new(100, 30);
    tty.ui.write("[answer]\nBefore\n\n| `a\\|b` | B |\n|---|---|\n| **literal │ divider** | value |\n");
    tty.frame();
    let request = |start_text: &str, start_offset: usize, end_text: &str, end_offset: usize| {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        let line = st.body_layout.text.iter().position(|line| line.contains(start_text)).unwrap();
        let offset = st.body_layout.text[line].find(start_text).unwrap() + start_offset;
        let mut selected = selection::Selection::new(selection::Point { line, offset }, &st.transcript, &st.body_layout);
        let line = st.body_layout.text.iter().position(|line| line.contains(end_text)).unwrap();
        selected.end = selection::Point { line, offset: st.body_layout.text[line].find(end_text).unwrap() + end_offset };
        selected.copy_request(false).unwrap().text
    };
    assert_eq!(request("a|b", 0, "a|b", 3), "a|b");
    assert_eq!(request("a|b", 1, "a|b", 3), "|b");
    assert_eq!(request("Before", 0, "a|b", 3), "Before\n\n`a|b`");
    assert_eq!(request("literal │ divider", 0, "literal │ divider", "literal │ divider".len()), "**literal │ divider**");
    let separator = {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        let line = st.body_layout.text.iter().position(|line| line.contains('├')).unwrap();
        st.body_layout.text[line].to_string()
    };
    assert_eq!(request(&separator, 0, &separator, separator.len()), separator, "separator-only copy retains visible text");
}

#[tokio::test]
async fn table_copy_keeps_separate_tables_and_fenced_pipe_text_separate() {
    let tty = Tty::new(100, 40);
    tty.ui.write("[answer]\nBefore\n\n| A | B |\n|---|---|\n| one | two |\n\n```text\n| literal | code |\n```\n\n| C | D |\n|---|---|\n| three | four |\n\nAfter\n");
    tty.frame();
    let request = {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        let first = st.body_layout.text.iter().position(|line| line.contains("Before")).unwrap();
        let last = st.body_layout.text.iter().position(|line| line.contains("After")).unwrap();
        let mut selected = selection::Selection::new(selection::Point { line: first, offset: 0 }, &st.transcript, &st.body_layout);
        selected.end = selection::Point { line: last, offset: st.body_layout.text[last].len() };
        selected.copy_request(false).unwrap()
    };
    assert!(request.text.contains("| A | B |\n|---|---|\n| one | two |"), "{}", request.text);
    assert!(request.text.contains("| C | D |\n|---|---|\n| three | four |"), "{}", request.text);
    assert!(request.text.contains("| literal | code |"));
    assert_eq!(request.text.matches("|---|---|").count(), 2, "fenced pipe text must not gain a table separator");
}

#[tokio::test]
async fn escaped_table_pipes_remain_cell_content_and_copy_as_literal_text() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(100, 24);
    tty.ui.write("[answer]\n| Code | Value |\n|---|---|\n| `a\\|b` | right |\n| left\\|middle | tail |\n| : | -- |\n");
    tty.frame();
    let (row, column) = {
        let st = tty.ui.state.lock().unwrap();
        let row = st.transcript_view_rows.iter().position(|r| r.contains("a|b")).expect("escaped pipe must display literally inside code");
        for marker in ["a|b", "left|middle", ":"] {
            let row = st.transcript_view_rows.iter().find(|r| r.contains(marker)).expect("table cell content must remain visible");
            assert_eq!(crate::term::strip_vt(row).matches('│').count(), 3, "escaped content must not create an extra column: {row}");
            assert!(!row.contains("\\|"), "escape syntax is not displayed");
        }
        let column = visible_length(crate::term::strip_vt(&st.transcript_view_rows[row]).split("a|b").next().unwrap());
        (st.transcript_screen_row + row, column)
    };
    for (kind, column) in [(MouseEventKind::Down(MouseButton::Left), column), (MouseEventKind::Up(MouseButton::Left), column + 3)] {
        tty.ui.handle_mouse(MouseEvent { kind, column: column as u16, row: row as u16, modifiers: KeyModifiers::NONE });
    }
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("a|b"));
    *tty.size.lock().unwrap() = (60, 24);
    tty.ui.resize();
    tty.frame();
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("a|b"));
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
        tty.ui.record_turn_duration(3_734_000);
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
    assert!(tty.frame().contains("enter submit · esc cancel"));
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

#[tokio::test]
async fn turn_completion_duration_flushes_explore_styles_cleanly_and_rewinds_with_turn() {
    let tty = Tty::new(90, 24);
    tty.ui.mark_turn_start();
    tty.ui.write("\n> build the game\n");
    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "r1", "name": "read_file", "state": "done", "meta": { "kind": "read", "summary": "src/game.js" } }));
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "Game finished and verified.\n" }));
    tty.ui.record_turn_duration(134_200);
    tty.ui.render_now();
    let raw = tty.ui.rendered_frame();
    let plain = crate::term::strip_vt(&raw).to_string();
    let explored_pos = plain.find("○ Explored").expect("Explored cell shown");
    let answer_pos = plain.find("◆ Game finished and verified.").expect("Answer shown");
    let worked_pos = plain.find("✓ Worked for 2m 14s").expect("Worked duration shown");
    assert!(explored_pos < answer_pos && answer_pos < worked_pos, "{plain}");
    let worked_row = rows(&raw).into_iter().find(|l| crate::term::strip_vt(l).starts_with("✓ Worked for 2m 14s")).unwrap();
    assert!(worked_row.contains(&format!("{}✓{}", Theme::OK, Theme::RESET)), "{worked_row:?}");
    assert!(worked_row.contains(&format!("{}Worked for 2m 14s ─", Theme::FAINT)), "turn separator rule: {worked_row:?}");
    assert!(visible_length(&worked_row) <= 89, "{worked_row:?}");
    assert!(tty.ui.rewind_turns(1));
    assert!(!tty.frame().contains("Worked for 2m 14s"));
}

/// Converts text into realistic Windows Console `crossterm::event::Event` records:
/// - Every key press is paired with a `KeyEventKind::Release` event.
/// - `\r` becomes `KeyCode::Enter`, `\n` becomes `Ctrl+J`, `\t` becomes `KeyCode::Tab`.
/// - Uppercase / shifted ASCII (`~`, `{`, `}`) carry `SHIFT`; `@`, `[`, `]` carry `CONTROL | ALT` (AltGr).
fn windows_key_events_for_str(s: &str) -> Vec<crossterm::event::Event> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let mut out = Vec::with_capacity(s.len() * 2);
    for ch in s.chars() {
        let (code, mods) = match ch {
            '\x1b' => (KeyCode::Esc, KeyModifiers::empty()),
            '\r' => (KeyCode::Enter, KeyModifiers::empty()),
            '\n' => (KeyCode::Char('j'), KeyModifiers::CONTROL),
            '\t' => (KeyCode::Tab, KeyModifiers::empty()),
            '@' | '[' | ']' => (KeyCode::Char(ch), KeyModifiers::CONTROL | KeyModifiers::ALT),
            c if c.is_ascii_uppercase() || "~{}!#$%^&*()_+:\"<>?|".contains(c) => (KeyCode::Char(c), KeyModifiers::SHIFT),
            c => (KeyCode::Char(c), KeyModifiers::empty()),
        };
        out.push(Event::Key(KeyEvent::new_with_kind(code, mods, KeyEventKind::Press)));
        out.push(Event::Key(KeyEvent::new_with_kind(code, mods, KeyEventKind::Release)));
    }
    out
}

#[tokio::test]
async fn windows_bracketed_paste_key_stream_collapses_without_splitting_or_queueing() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(100, 28);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut lines = Vec::new();
    for i in 1..=40 {
        if i == 20 {
            lines.push(String::new());
        } else {
            lines.push(format!("dòng {i}: fn check_{i}(user_email: &str) {{ /* @tag[{i}] ~ok */ \tlet v = {i}; }}"));
        }
    }
    let pasted_crlf = lines.join("\r\n");
    let expected = lines.join("\n");

    let mut stream = vec![Event::FocusGained, Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Right), column: 4, row: 10, modifiers: KeyModifiers::empty() })];
    // Split `\x1b[200~` + 40-line body + `\x1b[201~` across 12 arbitrary chunks, including mid-CSI splits.
    stream.extend(windows_key_events_for_str("\x1b[200~"));
    stream.extend(windows_key_events_for_str(&pasted_crlf));
    stream.extend(windows_key_events_for_str("\x1b[201~"));

    let mut decoder = InputDecoder::default();
    // Feed `[FocusGained, Mouse, Esc(press), Esc(release), '['(press)]` first to split `\x1b[200~` mid-marker.
    decoder.feed_batch(&tty.ui, stream[..5].to_vec());
    assert!(decoder.has_pending_csi());
    assert!(!q.is_finished());
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);

    let rest = &stream[5..];
    let chunk_size = (rest.len() / 10).max(1);
    let chunks: Vec<&[Event]> = rest.chunks(chunk_size).collect();
    for (idx, chunk) in chunks.iter().enumerate() {
        let probe = decoder.probe_batch(chunk);
        if idx + 1 == chunks.len() {
            assert!(probe.ended_on_paste_end);
        }
        decoder.feed_batch(&tty.ui, chunk.to_vec());
    }
    assert!(!decoder.in_bracketed_paste());
    assert!(!q.is_finished(), "bracketed paste must never auto-submit premature chunks");
    assert!(tty.ui.state.lock().unwrap().queued_prompts.is_empty(), "bracketed paste must never queue trailing lines");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
    let frame = tty.frame();
    assert!(frame.contains("[Pasted text #1 +40 lines]"), "{frame}");

    // Simulated interactive user typing + Enter right after bracketed paste submits cleanly.
    let mut now = std::time::Instant::now();
    for ev in windows_key_events_for_str("hãy review kỹ") {
        decoder.feed_batch_at(&tty.ui, vec![ev], now, true);
        now += paste_burst::PasteBurst::recommended_char_flush_delay();
    }
    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))], now, true);
    let submitted = q.await.unwrap().unwrap();
    assert_eq!(submitted, format!("{expected}\nhãy review kỹ"));
    assert!(tty.ui.state.lock().unwrap().queued_prompts.is_empty());
}

#[tokio::test]
async fn windows_alt_bracket_paste_while_running_does_not_cancel_or_auto_queue() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(100, 24);
    tty.ui.set_status("Running");
    let mut decoder = InputDecoder::default();

    let lines25 = (1..=25).map(|i| format!("bước {i}: kiểm tra điều kiện biên #{i}")).collect::<Vec<_>>().join("\r\n");
    let mut events = vec![Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::ALT))];
    events.extend(windows_key_events_for_str("200~"));
    events.extend(windows_key_events_for_str(&lines25));
    events.push(Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::ALT)));
    events.extend(windows_key_events_for_str("201~"));

    for chunk in events.chunks(17) {
        decoder.feed_batch(&tty.ui, chunk.to_vec());
    }
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0, "CSI paste start must not cancel a running turn");
    assert!(tty.ui.state.lock().unwrap().queued_prompts.is_empty(), "pasting while running must stay in composer until Enter");
    assert!(tty.frame().contains("[Pasted text #1 +25 lines]"));

    // Explicit Enter queues the entire collapsed paste as a single item.
    decoder.feed_batch(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))]);
    assert_eq!(tty.ui.state.lock().unwrap().queued_prompts.iter().map(|q| q.text.clone()).collect::<Vec<_>>(), vec!["[Pasted text #1 +25 lines]".to_string()]);
}

#[tokio::test]
async fn non_bracketed_conpty_multiline_chunks_and_crlf_splits_coalesce_into_single_pill() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(100, 26);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut decoder = InputDecoder::default();
    let mut now = std::time::Instant::now();
    // Batch 1: short first line (< 150 chars) with FocusGained + Mouse + CRLF (Enter + Ctrl+J).
    let mut first_batch = vec![Event::FocusGained, Event::Mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column: 2, row: 2, modifiers: KeyModifiers::empty() })];
    first_batch.extend(windows_key_events_for_str("Hãy tối ưu hàm xử lý sự kiện sau:\r\n"));
    decoder.feed_batch_at(&tty.ui, first_batch, now, true);
    assert!(!q.is_finished(), "short first line of Windows CRLF paste must not auto-submit");

    // Batches 2..30: delivered line-by-line across 450ms total wall duration (> 250ms).
    let mut expected_lines = vec!["Hãy tối ưu hàm xử lý sự kiện sau:".to_string()];
    for i in 2..=30 {
        now += Duration::from_millis(15);
        let line = format!("    let item_{i} = map[@key_{i}].clone();");
        expected_lines.push(line.clone());
        decoder.feed_batch_at(&tty.ui, windows_key_events_for_str(&format!("{line}\r\n")), now, true);
        assert!(!q.is_finished(), "line {i} must stay buffered without submitting");
    }

    // Batch 31 & Batch 32: split CRLF right between `\r` and `\n` before the final closing brace!
    expected_lines.push("    Ok(())".into());
    expected_lines.push("}".into());
    now += Duration::from_millis(15);
    decoder.feed_batch_at(&tty.ui, windows_key_events_for_str("    Ok(())\r"), now, true);
    now += Duration::from_millis(15);
    decoder.feed_batch_at(&tty.ui, windows_key_events_for_str("\n}"), now, true);

    now += paste_burst::PasteBurst::recommended_active_flush_delay(true);
    assert!(tty.ui.flush_paste_burst_at(now, true));
    assert!(!q.is_finished());
    assert!(tty.ui.state.lock().unwrap().queued_prompts.is_empty());
    let frame = tty.frame();
    assert!(frame.contains("[Pasted text #1 +32 lines]"), "{frame}");

    // Single-character interactive typing after the suppression window stays outside the pill, and Enter submits intact.
    now += Duration::from_millis(150);
    for ch in "xong".chars() {
        decoder.feed_batch_at(&tty.ui, windows_key_events_for_str(&ch.to_string()), now, true);
        now += paste_burst::PasteBurst::recommended_char_flush_delay();
    }
    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))], now, true);
    let submitted = q.await.unwrap().unwrap();
    assert_eq!(submitted, format!("{}\nxong", expected_lines.join("\n")));
}

#[tokio::test]
async fn windows_codex_paste_burst_prevents_screenshot_split_inline_leak_and_autosubmit() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseEvent, MouseEventKind};
    let tty = Tty::new(110, 28);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut lines = Vec::with_capacity(28);
    lines.push("Nếu chạy theo cách truyền thống, hệ thống mutation testing phải biên dịch lại từng biến thể:".to_string());
    for i in 2..=13 {
        lines.push(if i == 7 { String::new() } else { format!("  - Bước {i}: kiểm tra AST node #{i} & ánh xạ bytecode\t[ok]") });
    }
    let middle_line = "Spawn process mới từ đầu cho từng mutant sẽ khiến thời gian chạy CI bùng nổ cấp số nhân (Combinatorial Explosion)";
    lines.push(middle_line.to_string());
    for i in 15..=28 {
        lines.push(format!("  - Ràng buộc {i}: giữ worker pool cô lập bộ nhớ mà không fork lại toàn bộ runtime"));
    }
    assert_eq!(lines.len(), 28);
    let expected = lines.join("\n");

    let mut decoder = InputDecoder::default();
    let t0 = std::time::Instant::now();
    let mut now = t0;

    // 1. First character `'N'` wakes `poll()` completely alone: must be held, NOT rendered inline!
    decoder.feed_batch_at(&tty.ui, windows_key_events_for_str("N"), now, true);
    let initial_frame = tty.frame();
    assert!(!initial_frame.contains("› N"), "first paste char must not leak inline before burst decision:\n{initial_frame}");

    // 2. Remainder of line 1 arrives 2ms later, promoting `'N'` into PasteBurst, followed by lines 2..13 across >250ms.
    now += Duration::from_millis(2);
    decoder.feed_batch_at(&tty.ui, windows_key_events_for_str(&format!("{}\r\n", &lines[0]["N".len()..])), now, true);
    for line in &lines[1..13] {
        now += Duration::from_millis(22);
        decoder.feed_batch_at(&tty.ui, windows_key_events_for_str(&format!("{line}\r\n")), now, true);
    }
    assert!(now.duration_since(t0) > Duration::from_millis(250), "burst spans >250ms without artificial wall-clock cut");

    // 3. Single-char `'S'`, 111-char middle fragment with non-char noise (Modifier, Null, Focus, Mouse), and single-char `')'`.
    now += Duration::from_millis(25);
    decoder.feed_batch_at(&tty.ui, windows_key_events_for_str("S"), now, true);
    now += Duration::from_millis(18);
    let mid_body = &middle_line[1..middle_line.len() - 1];
    let mut noisy_mid = vec![
        Event::FocusGained,
        Event::Key(KeyEvent::new_with_kind(KeyCode::Modifier(ModifierKeyCode::LeftShift), KeyModifiers::SHIFT, KeyEventKind::Press)),
        Event::Key(KeyEvent::new(KeyCode::Null, KeyModifiers::empty())),
        Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, column: 10, row: 5, modifiers: KeyModifiers::empty() }),
    ];
    noisy_mid.extend(windows_key_events_for_str(mid_body));
    decoder.feed_batch_at(&tty.ui, noisy_mid, now, true);
    now += Duration::from_millis(20);
    decoder.feed_batch_at(&tty.ui, windows_key_events_for_str(")\r\n"), now, true);

    // 4. Remaining 14 lines + standalone trailing Windows Terminal `KeyCode::Enter` (`\r` without `\n`).
    for (idx, line) in lines[14..].iter().enumerate() {
        now += Duration::from_millis(18);
        let chunk = if idx + 1 < lines[14..].len() { format!("{line}\r\n") } else { line.clone() };
        decoder.feed_batch_at(&tty.ui, windows_key_events_for_str(&chunk), now, true);
    }
    now += Duration::from_millis(25);
    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))], now, true);

    // 5. Flush after Windows active idle timeout (61ms): ONE pill `+28 lines`, zero inline leak, zero auto-submit!
    let flush_at = now + paste_burst::PasteBurst::recommended_active_flush_delay(true);
    assert!(tty.ui.flush_paste_burst_at(flush_at, true));
    assert!(!q.is_finished(), "trailing Enter at end of Windows paste must NEVER auto-submit");
    assert!(tty.ui.state.lock().unwrap().queued_prompts.is_empty(), "must never auto-queue");

    // 6. A slightly-late trailing Enter inside the 120ms suppression window is ALSO suppressed.
    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))], now + Duration::from_millis(95), true);
    assert!(!q.is_finished(), "late trailing Enter within 120ms suppress window must not auto-submit");

    let frame = tty.frame();
    assert!(frame.contains("› [Pasted text #1 +28 lines]"), "{frame}");
    assert!(!frame.contains("› N["), "leading 'N' must not leak outside pill:\n{frame}");
    assert!(!frame.contains("Combinatorial Explosion"), "middle line must not leak inline:\n{frame}");
    assert!(!frame.contains("[Pasted text #2"), "must not split into multiple paste pills:\n{frame}");

    // 7. Explicit user Enter after the suppression window submits the exact 28-line payload intact.
    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))], now + Duration::from_millis(250), true);
    let submitted = q.await.unwrap().unwrap();
    assert_eq!(submitted, expected);
}

#[tokio::test]
async fn paste_burst_tabs_ctrl_j_and_csi_u_shift_enter_never_complete_or_submit() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(100, 24);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut decoder = InputDecoder::default();
    let mut now = std::time::Instant::now();
    // Start with `/` + tab + `\x1b[13;2u` (Shift+Enter) + `Ctrl+J`: must buffer as paste, NOT autocomplete `/help` or submit!
    let mut burst = windows_key_events_for_str("/he\tfirst\x1b[13;2u\tsecond");
    burst.push(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)));
    burst.extend(windows_key_events_for_str("third"));
    decoder.feed_batch_at(&tty.ui, burst, now, true);
    now += paste_burst::PasteBurst::recommended_active_flush_delay(true);
    assert!(tty.ui.flush_paste_burst_at(now, true));
    assert!(!q.is_finished(), "Shift+Enter and Ctrl+J inside paste must never submit");
    assert!(tty.frame().contains("[Pasted text #1 +3 lines]"));

    // Outside a burst, standalone `\x1b[13;2u` and `Ctrl+J` also never submit.
    now += Duration::from_millis(200);
    decoder.feed_batch_at(&tty.ui, windows_key_events_for_str("\x1b[13;2u"), now, true);
    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))], now, true);
    assert!(!q.is_finished(), "standalone Shift+Enter / Ctrl+J must never submit");

    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))], now, true);
    let submitted = q.await.unwrap().unwrap();
    assert_eq!(submitted, "/he\tfirst\n\tsecond\nthird\n\n");
}

#[tokio::test]
async fn escape_returns_scrolled_running_view_to_latest_before_cancelling_agent() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    for windows in [false, true] {
        for status in ["Running", "Compacting", "Stopping"] {
            let tty = Tty::new(80, 24);
            tty.ui.write(&(0..80).map(|n| format!("history line {n}\n")).collect::<String>());
            tty.ui.set_status(status);
            tty.text("preserved follow-up draft");
            tty.frame();
            tty.key("pageup");
            assert!(tty.ui.state.lock().unwrap().scroll > 0);
            let mut decoder = InputDecoder::default();
            let escape = || Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            decoder.feed_batch_at(&tty.ui, vec![escape()], std::time::Instant::now(), windows);
            decoder.flush_timeout_at(&tty.ui, std::time::Instant::now(), windows);
            assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0, "{status}: returning to current output must consume Escape");
            assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
            assert_eq!(draft(&tty), "preserved follow-up draft");
            assert_eq!(tty.ui.state.lock().unwrap().status, status);
            // A distinct Escape at the live tail still intentionally stops the turn.
            decoder.feed_batch_at(&tty.ui, vec![escape()], std::time::Instant::now(), windows);
            decoder.flush_timeout_at(&tty.ui, std::time::Instant::now(), windows);
            assert_eq!(tty.cancelled.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn transcript_return_shortcuts_never_cancel_running_turn_or_leave_hidden_scroll() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    for alternate in [false, true] {
        for (code, modifiers, closes) in [(KeyCode::Esc, KeyModifiers::NONE, true), (KeyCode::Char('t'), KeyModifiers::CONTROL, true), (KeyCode::End, KeyModifiers::NONE, false)] {
            let tty = Tty::with_mode(80, 24, alternate);
            tty.ui.write(&(0..80).map(|n| format!("history line {n}\n")).collect::<String>());
            tty.ui.set_status("Running");
            tty.text("unsent draft");
            tty.ui.set_transcript_overlay(true);
            tty.frame();
            tty.key("pageup");
            assert!(tty.ui.state.lock().unwrap().scroll > 0);
            let mut decoder = InputDecoder::default();
            decoder.feed_batch(&tty.ui, vec![Event::Key(KeyEvent::new(code, modifiers))]);
            decoder.flush_timeout(&tty.ui);
            assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
            assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
            assert_eq!(tty.ui.is_transcript_overlay(), !closes);
            assert_eq!(draft(&tty), "unsent draft");
            assert_eq!(tty.ui.state.lock().unwrap().status, "Running");
        }
    }
    let tty = Tty::new(80, 24);
    tty.ui.write(&(0..80).map(|n| format!("history line {n}\n")).collect::<String>());
    tty.text("draft with caret");
    tty.key("home");
    tty.ui.set_status("Running");
    tty.frame();
    tty.key("pageup");
    let cursor = tty.ui.state.lock().unwrap().cursor;
    let mut decoder = InputDecoder::default();
    decoder.feed_batch(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL))]);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
    assert_eq!(tty.ui.state.lock().unwrap().cursor, cursor, "Ctrl+End navigates transcript, not draft");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn standalone_escape_timeout_and_short_pty_commands_preserve_interactive_semantics() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(80, 20);
    let mut decoder = InputDecoder::default();

    // 1. Standalone Esc + timeout cancels a Running turn.
    tty.ui.set_status("Running");
    decoder.feed_batch(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()))]);
    assert!(decoder.has_pending_csi());
    decoder.flush_timeout(&tty.ui);
    assert!(!decoder.has_pending_csi());
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 1);
    tty.ui.set_status("Ready");

    // 2. Unterminated `\x1b[200~` (e.g. dropped `\x1b[201~` over SSH) flushes cleanly on idle timeout,
    //    and partial `\x1b[2x` inside paste preserves `[2x` without prematurely ending paste mode.
    let ui = tty.ui.clone();
    let q1 = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    let broken = windows_key_events_for_str("\x1b[200~alpha\r\n\x1b[2x\r\ngamma");
    decoder.feed_batch(&tty.ui, broken);
    assert!(decoder.in_bracketed_paste());
    decoder.flush_timeout(&tty.ui);
    assert!(!decoder.in_bracketed_paste());
    assert!(tty.frame().contains("[Pasted text #1 +3 lines]"));
    decoder.feed_batch(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))]);
    assert_eq!(q1.await.unwrap().unwrap(), "alpha\n[2x\ngamma");

    // 3. Short single-line `\r`-terminated POSIX raw PTY writes (`terminal.py` / `terminal-mode-switch.py`, `is_windows = false`)
    //    auto-submit cleanly via the async PasteBurst tick, while multi-line Unix unbracketed bursts stay collapsed!
    for cmd in ["/status", "y"] {
        let ui = tty.ui.clone();
        let q2 = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let pty_cmd: Vec<Event> = format!("{cmd}\r")
            .chars()
            .map(|c| {
                let code = if c == '\r' { KeyCode::Enter } else { KeyCode::Char(c) };
                Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
            })
            .collect();
        decoder.feed_batch_at(&tty.ui, pty_cmd, std::time::Instant::now(), false);
        let got = tokio::time::timeout(Duration::from_millis(250), q2).await.expect("POSIX PTY command auto-submits").unwrap().unwrap();
        assert_eq!(got, cmd);
    }

    let ui = tty.ui.clone();
    let q3 = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    let unix_multiline: Vec<Event> = "line 1\rline 2\r"
        .chars()
        .map(|c| {
            let code = if c == '\r' { KeyCode::Enter } else { KeyCode::Char(c) };
            Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
        })
        .collect();
    let t_unix = std::time::Instant::now();
    decoder.feed_batch_at(&tty.ui, unix_multiline, t_unix, false);
    assert!(tty.ui.flush_paste_burst_at(t_unix + paste_burst::PasteBurst::recommended_active_flush_delay(false), false));
    assert!(!q3.is_finished(), "multi-line Unix unbracketed burst must collapse without submitting");
    assert!(tty.frame().contains("[Pasted text #1 +2 lines]"));
    decoder.feed_batch_at(&tty.ui, vec![Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()))], t_unix + Duration::from_millis(200), false);
    assert_eq!(q3.await.unwrap().unwrap(), "line 1\nline 2");
}

fn modified(name: &str, ctrl: bool, meta: bool, shift: bool) -> Key {
    Key { name: name.into(), ctrl, meta, shift, ..Default::default() }
}

fn draft(tty: &Tty) -> String {
    tty.ui.state.lock().unwrap().input.iter().collect()
}

#[tokio::test]
async fn multiline_composer_newlines_navigation_rendering_and_submission() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(60, 18);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.text("first line");
    let now = std::time::Instant::now();
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), now, true);
    tty.text("second line");
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL), now, true);
    tty.text("third line");
    assert!(!q.is_finished());
    let frame = tty.frame();
    for line in ["first line", "second line", "third line"] {
        assert!(frame.contains(line), "{frame}");
    }
    tty.key("home");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, "first line\nsecond line\n".chars().count());
    tty.key("up");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, "first line\n".chars().count());
    tty.key("end");
    tty.ui.key(None, modified("k", true, false, false));
    assert_eq!(draft(&tty), "first line\nsecond linethird line");
    tty.ui.key(None, modified("y", true, false, false));
    assert_eq!(draft(&tty), "first line\nsecond line\nthird line");
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap(), "first line\nsecond line\nthird line");
}

#[tokio::test]
async fn composer_graphemes_word_motion_kill_yank_and_draft_history() {
    let tty = Tty::new(70, 20);
    tty.text("a👩‍💻e\u{301}");
    tty.key("left");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, 4);
    tty.key("backspace");
    assert_eq!(draft(&tty), "ae\u{301}");
    tty.key("delete");
    assert_eq!(draft(&tty), "a");
    tty.ui.set_input("one two three", false);
    tty.ui.key(None, modified("left", true, false, false));
    assert_eq!(tty.ui.state.lock().unwrap().cursor, 8);
    tty.ui.key(None, modified("u", true, false, false));
    assert_eq!(draft(&tty), "three");
    tty.ui.key(None, modified("y", true, false, false));
    assert_eq!(draft(&tty), "one two three");
    {
        let mut st = tty.ui.state.lock().unwrap();
        st.history = vec!["older prompt".into(), "latest prompt".into()];
        st.history_index = st.history.len();
    }
    tty.ui.set_input("unfinished draft", false);
    tty.key("up");
    assert_eq!(draft(&tty), "unfinished draft", "ordinary history navigation must not replace a live draft");
    tty.ui.set_input("", false);
    tty.key("up");
    assert_eq!(draft(&tty), "latest prompt");
    tty.text(" edited");
    tty.key("up");
    assert_eq!(draft(&tty), "latest prompt edited", "editing a recall leaves history navigation");
    tty.ui.set_input("", false);
    tty.key("up");
    tty.key("up");
    assert_eq!(draft(&tty), "older prompt");
    tty.key("down");
    tty.key("down");
    assert_eq!(draft(&tty), "", "Down past newest exits history with an empty composer");
}

#[tokio::test]
async fn history_search_previews_accepts_without_sending_and_cancel_restores_draft() {
    let tty = Tty::new(90, 20);
    {
        let mut st = tty.ui.state.lock().unwrap();
        st.history = vec!["alpha older".into(), "beta".into(), "alpha latest".into()];
        st.history_index = st.history.len();
    }
    tty.ui.set_input("my draft", false);
    tty.ui.key(None, modified("r", true, false, false));
    tty.text("alpha");
    assert_eq!(draft(&tty), "alpha latest");
    tty.ui.key(None, modified("r", true, false, false));
    assert_eq!(draft(&tty), "alpha older");
    tty.key("escape");
    assert_eq!(draft(&tty), "my draft");
    tty.ui.key(None, modified("r", true, false, false));
    tty.ui.paste("no matching prompt");
    assert!(tty.frame().contains("no match"));
    tty.key("return");
    assert!(tty.ui.state.lock().unwrap().history_search.is_some());
    tty.ui.key(None, modified("c", true, false, false));
    assert_eq!(draft(&tty), "my draft");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
    tty.ui.key(None, modified("r", true, false, false));
    tty.text("beta");
    tty.key("return");
    assert_eq!(draft(&tty), "beta");
    assert!(!tty.ui.has_queued_prompts(), "accepting search only edits the draft");
}

#[tokio::test]
async fn questions_restore_draft_attachments_on_answer_escape_and_cancellation() {
    let tty = Tty::new(90, 20);
    let pasted = "draft payload\n".repeat(20);
    tty.ui.paste(&pasted);
    let original = draft(&tty);
    let original_pastes = tty.ui.state.lock().unwrap().pasted_texts.clone();
    for outcome in ["answer", "escape", "cancel"] {
        let cancel = CancellationToken::new();
        let ui = tty.ui.clone();
        let c = cancel.clone();
        let q = tokio::spawn(async move { ui.question("approve [y/N] ", Some(&c), false, QuestionOpts { esc_cancels: true, ..Default::default() }).await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        match outcome {
            "answer" => {
                tty.text("y");
                tty.key("return");
            }
            "escape" => tty.key("escape"),
            _ => cancel.cancel(),
        }
        let _ = q.await.unwrap();
        assert_eq!(draft(&tty), original);
        assert_eq!(tty.ui.state.lock().unwrap().pasted_texts, original_pastes);
        assert_eq!(tty.ui.state.lock().unwrap().label, "goon> ");
    }
}

#[tokio::test]
async fn escape_dismisses_slash_before_interrupt_and_ctrl_c_clears_without_exiting() {
    let tty = Tty::new(90, 20);
    tty.ui.set_status("Running");
    tty.text("/mod");
    tty.key("escape");
    assert_eq!(draft(&tty), "/mod");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
    assert!(Tui::slash_popup(&mut tty.ui.state.lock().unwrap()).0.is_empty());
    tty.key("escape");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 1);
    tty.ui.key(None, modified("c", true, false, false));
    assert_eq!(draft(&tty), "");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 1);
    tty.ui.key(None, modified("c", true, false, false));
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn enter_steers_but_tab_queues_and_fallback_never_loses_input() {
    let tty = Tty::new(90, 20);
    let steered = Arc::new(Mutex::new(vec![]));
    let received = steered.clone();
    tty.ui.set_on_steer(Arc::new(move |text| {
        received.lock().unwrap().push(text);
        true
    }));
    tty.ui.set_status("Running");
    tty.text("change direction now");
    tty.key("return");
    assert_eq!(*steered.lock().unwrap(), vec!["change direction now"]);
    assert!(!tty.ui.has_queued_prompts());
    tty.text("save this for later");
    tty.key("tab");
    assert_eq!(tty.ui.state.lock().unwrap().queued_prompts.iter().map(|q| q.text.clone()).collect::<Vec<_>>(), vec!["save this for later"]);
    tty.ui.set_on_steer(Arc::new(|_| false));
    tty.text("arrived as turn finished");
    tty.key("return");
    assert_eq!(tty.ui.state.lock().unwrap().queued_prompts.iter().map(|q| q.text.clone()).collect::<Vec<_>>(), vec!["save this for later", "arrived as turn finished"]);
}

#[tokio::test]
async fn escape_denies_approval_before_interrupting_running_work() {
    let tty = Tty::new(90, 20);
    tty.ui.set_status("Running");
    tty.text("keep my draft");
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("Allow command [y/N] ", None, false, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("escape");
    assert_eq!(q.await.unwrap().unwrap(), "");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
    assert_eq!(draft(&tty), "keep my draft");
    tty.key("escape");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_remote_race_question_restores_draft_and_does_not_strand_pending_input() {
    let tty = Tty::new(90, 20);
    tty.text("preserved draft");
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.ask_transient("\n▲ remote race approval\n", "[y/N] ", None, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(tty.ui.state.lock().unwrap().pending.is_some());
    q.abort();
    let _ = q.await;
    assert_eq!(draft(&tty), "preserved draft");
    assert!(tty.ui.state.lock().unwrap().pending.is_none());
    assert!(!tty.ui.transcript().contains("remote race approval"));
    let ui = tty.ui.clone();
    let next = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("return");
    assert_eq!(next.await.unwrap().unwrap(), "preserved draft");
}

#[tokio::test]
async fn mention_popup_is_fuzzy_cursor_local_selectable_and_never_submits_on_accept() {
    let tty = Tty::new(90, 24);
    tty.ui.set_workspace_files(vec!["src/app.rs".into(), "src/api.rs".into(), "docs/user guide.md".into()]);
    tty.ui.set_workspace_skills(vec![("review-tests".into(), "Review test coverage".into())]);
    tty.text("look @sr suffix");
    for _ in 0..7 {
        tty.key("left");
    }
    let frame = tty.frame();
    assert!(frame.contains("@src/app.rs") && frame.contains("@src/api.rs"), "{frame}");
    tty.key("down");
    tty.key("tab");
    assert_eq!(draft(&tty), "look @src/app.rs suffix");
    assert!(!tty.ui.has_queued_prompts());
    tty.ui.set_input("use $rev", false);
    assert!(tty.frame().contains("Review test coverage"));
    tty.key("return");
    assert_eq!(draft(&tty), "use $review-tests ");
    assert!(!tty.ui.has_queued_prompts());
    tty.ui.set_input("read @user", false);
    tty.key("tab");
    assert_eq!(draft(&tty), "read @\"docs/user guide.md\" ");
    tty.ui.set_status("Running");
    tty.ui.set_input("read @src", false);
    tty.key("escape");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
    assert!(Tui::mention_popup(&mut tty.ui.state.lock().unwrap()).is_none());
    tty.key("escape");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn quit_requires_same_chord_twice_and_other_activity_disarms_it() {
    let tty = Tty::new(90, 20);
    let commands = Arc::new(Mutex::new(vec![]));
    let output = commands.clone();
    tty.ui.set_on_immediate_command(Arc::new(move |cmd| {
        output.lock().unwrap().push(cmd);
        Box::pin(async {})
    }));
    tty.ui.key(None, modified("c", true, false, false));
    assert!(tty.frame().contains("Press ctrl+c again to quit"));
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
    tty.ui.key(None, modified("d", true, false, false));
    assert!(commands.lock().unwrap().is_empty(), "a different chord cannot confirm quit");
    tty.text("draft");
    tty.ui.key(None, modified("c", true, false, false));
    assert_eq!(draft(&tty), "");
    assert!(commands.lock().unwrap().is_empty());
    tty.ui.state.lock().unwrap().quit_armed.as_mut().unwrap().1 = std::time::Instant::now() - Duration::from_secs(1);
    tty.ui.key(None, modified("c", true, false, false));
    assert!(commands.lock().unwrap().is_empty(), "expired confirmation must re-arm");
    tty.ui.key(None, modified("c", true, false, false));
    assert_eq!(*commands.lock().unwrap(), vec!["/exit"]);
}

#[tokio::test]
async fn shortcut_help_is_reversible_and_normal_question_marks_still_type() {
    let tty = Tty::new(100, 24);
    tty.text("?");
    assert_eq!(draft(&tty), "");
    assert!(tty.frame().contains("ctrl+u/k/w/y"));
    tty.key("escape");
    assert!(!tty.frame().contains("Shortcuts ·"));
    tty.text("why?");
    assert_eq!(draft(&tty), "why?");
}

#[tokio::test]
async fn approval_choices_select_with_arrows_shortcuts_and_escape_and_preserve_draft() {
    let tty = Tty::new(80, 20);
    tty.ui.set_status("Running");
    tty.text("my unfinished draft");
    for method in ["arrow", "shortcut", "escape"] {
        let ui = tty.ui.clone();
        let q = tokio::spawn(async move {
            ui.ask_transient(
                "\nAllow bash?\n$ npm test\n",
                "Allow? ",
                None,
                QuestionOpts { choices: vec![("y".into(), "Yes, once".into()), ("p".into(), "Allow prefix".into()), ("n".into(), "No".into())], ..Default::default() },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(tty.frame().contains("Yes, once"));
        match method {
            "arrow" => {
                tty.key("down");
                tty.key("return");
            }
            "shortcut" => tty.text("y"),
            _ => tty.key("escape"),
        }
        assert_eq!(
            q.await.unwrap().unwrap(),
            match method {
                "arrow" => "p",
                "shortcut" => "y",
                _ => "n",
            }
        );
        assert_eq!(draft(&tty), "my unfinished draft");
        assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn persistent_history_callback_records_expanded_prompts_but_never_secret_questions() {
    let tty = Tty::new(90, 20);
    let recorded = Arc::new(Mutex::new(vec![]));
    let output = recorded.clone();
    tty.ui.set_prompt_history(vec!["from earlier session".into()], Arc::new(move |text| output.lock().unwrap().push(text)));
    tty.key("up");
    assert_eq!(draft(&tty), "from earlier session");
    tty.ui.set_input("", false);
    tty.ui.paste("first\nsecond\nthird");
    tty.key("tab");
    assert_eq!(*recorded.lock().unwrap(), vec!["first\nsecond\nthird"]);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("API key: ", None, false, QuestionOpts { mask: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.paste("private-secret-value");
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap(), "private-secret-value");
    assert_eq!(recorded.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn crossterm_choices_bypass_paste_bursts_and_ignore_bracketed_paste() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for is_windows in [false, true] {
        for (code, expected) in [(KeyCode::Char('p'), "p"), (KeyCode::Char('2'), "p"), (KeyCode::Enter, "y"), (KeyCode::Esc, "n")] {
            let tty = Tty::new(90, 24);
            tty.text("keep draft");
            let ui = tty.ui.clone();
            let q = tokio::spawn(async move {
                ui.question("Allow? ", None, false, QuestionOpts { choices: vec![("y".into(), "Once".into()), ("p".into(), "Prefix".into()), ("n".into(), "No".into())], ..Default::default() }).await
            });
            tokio::time::sleep(Duration::from_millis(5)).await;
            tty.ui.paste("y\n");
            assert!(!q.is_finished(), "bracketed paste must not approve a tool");
            assert_eq!(draft(&tty), "");
            tty.ui.handle_crossterm_key(KeyEvent::new(code, KeyModifiers::NONE), std::time::Instant::now(), is_windows);
            assert!(!tty.ui.state.lock().unwrap().paste_burst.is_active());
            assert_eq!(tokio::time::timeout(Duration::from_secs(1), q).await.unwrap().unwrap().unwrap(), expected);
            assert_eq!(draft(&tty), "keep draft");
            assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn crossterm_help_and_quit_use_shortcuts_not_delayed_paste_input() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for is_windows in [false, true] {
        let tty = Tty::new(100, 24);
        let idle_interrupts = Arc::new(AtomicUsize::new(0));
        let interrupts = idle_interrupts.clone();
        tty.ui.set_on_interrupt_idle(Arc::new(move || {
            interrupts.fetch_add(1, Ordering::SeqCst);
        }));
        let commands = Arc::new(Mutex::new(vec![]));
        let output = commands.clone();
        tty.ui.set_on_immediate_command(Arc::new(move |cmd| {
            output.lock().unwrap().push(cmd);
            Box::pin(async {})
        }));
        let key = |code, modifiers| tty.ui.handle_crossterm_key(KeyEvent::new(code, modifiers), std::time::Instant::now(), is_windows);
        key(KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert!(tty.frame().contains("Shortcuts ·"));
        assert_eq!(draft(&tty), "");
        key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!tty.frame().contains("Shortcuts ·"));
        key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(idle_interrupts.load(Ordering::SeqCst), 1);
        assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0, "idle interrupt must not exit");
        key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(tty.ui.state.lock().unwrap().quit_armed.is_none(), "even a pending paste character must disarm quit");
        tty.ui.flush_paste_burst_at(std::time::Instant::now() + Duration::from_millis(100), is_windows);
        key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(draft(&tty), "");
        assert!(commands.lock().unwrap().is_empty());
        key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(*commands.lock().unwrap(), vec!["/exit"]);
    }
}

#[tokio::test]
async fn local_history_retains_the_same_limit_as_persistent_history() {
    let tty = Tty::new(90, 24);
    tty.ui.set_prompt_history((0..1000).map(|n| format!("old {n}")).collect(), Arc::new(|_| {}));
    tty.text("new prompt");
    tty.key("tab");
    let st = tty.ui.state.lock().unwrap();
    assert_eq!(st.history.len(), 1000);
    assert_eq!(st.history.first().unwrap(), "old 1");
    assert_eq!(st.history.last().unwrap(), "new prompt");
}

#[tokio::test]
async fn quoted_file_completion_replaces_the_whole_token_and_keeps_suffix() {
    let tty = Tty::new(100, 24);
    tty.ui.set_workspace_files(vec!["docs/user guide.md".into(), "docs/user notes.md".into()]);
    for text in ["read @\"docs/user gu\" then summarize", "read @\"docs/user gu"] {
        tty.ui.set_input(text, false);
        tty.ui.state.lock().unwrap().cursor = "read @\"docs/user gu".chars().count();
        tty.key("tab");
        assert_eq!(draft(&tty), if text.ends_with("summarize") { "read @\"docs/user guide.md\" then summarize" } else { "read @\"docs/user guide.md\" " });
    }
}

#[tokio::test]
async fn modal_questions_keep_pending_draft_keys_out_of_secrets_and_restore_kill_state() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(100, 24);
    tty.text("draft ");
    tty.ui.state.lock().unwrap().kill_buffer = "saved kill buffer".into();
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), std::time::Instant::now(), true);
    let ui = tty.ui.clone();
    let q = tokio::spawn(async move { ui.question("Secret: ", None, false, QuestionOpts { mask: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(draft(&tty), "");
    assert!(!tty.ui.state.lock().unwrap().paste_burst.is_active());
    tty.text("?");
    assert_eq!(draft(&tty), "?", "masked prompts must treat shortcut discovery as literal input");
    tty.ui.paste("private-secret-value");
    tty.ui.key(None, modified("u", true, false, false));
    tty.ui.key(None, modified("y", true, false, false));
    tty.key("return");
    assert_eq!(q.await.unwrap().unwrap(), "?private-secret-value");
    let st = tty.ui.state.lock().unwrap();
    assert_eq!(st.input.iter().collect::<String>(), "draft x");
    assert_eq!(st.kill_buffer, "saved kill buffer");
    assert!(st.paste_archive.is_empty());
    assert!(st.history.is_empty());
}

#[tokio::test]
async fn queued_attachments_survive_archive_eviction_and_queue_edit_replaces_current_draft() {
    let tty = Tty::new(100, 24);
    tty.ui.paste("first\nsecond\nthird");
    tty.key("tab");
    {
        let mut st = tty.ui.state.lock().unwrap();
        st.paste_archive.clear();
        st.image_archive.clear();
    }
    tty.text("replaced draft");
    tty.ui.key(None, modified("up", false, true, false));
    assert!(!tty.ui.has_queued_prompts());
    assert!(draft(&tty).contains("[Pasted text #1"));
    tty.key("tab");
    let answer = tty.ui.question("goon> ", None, true, QuestionOpts::default()).await.unwrap();
    assert_eq!(answer, "first\nsecond\nthird");
}

#[tokio::test]
async fn interrupted_queue_merges_in_order_rebases_collisions_and_does_not_submit() {
    let tty = Tty::new(100, 24);
    for (image, content) in [("C:/first.png", "one\ntwo"), ("C:/second.png", "three\nfour")] {
        let mut st = tty.ui.state.lock().unwrap();
        st.input = chars_of("[Image #1] [Pasted text #1 +2 lines]");
        st.cursor = st.input.len();
        st.attached_images.insert("[Image #1]".into(), image.into());
        st.pasted_texts.insert("[Pasted text #1 +2 lines]".into(), content.into());
        let queued = composer::QueuedPrompt::capture(&st, &st.input.iter().collect::<String>());
        st.queued_prompts.push(queued);
        Tui::clear_composer(&mut st);
    }
    tty.text("unfinished draft");
    tty.ui.restore_queued_after_interrupt();
    assert!(!tty.ui.has_queued_prompts());
    {
        let mut st = tty.ui.state.lock().unwrap();
        assert_eq!(st.attached_images.len(), 2);
        assert_eq!(st.pasted_texts.len(), 2);
        let raw = st.input.iter().collect::<String>();
        assert_eq!(tty.ui.expand_placeholders(&mut st, &raw), "@\"C:/first.png\" one\ntwo\n@\"C:/second.png\" three\nfour\nunfinished draft");
    }
    let ui = tty.ui.clone();
    let question = tokio::spawn(async move { ui.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(!question.is_finished(), "interrupted follow-ups require explicit user submission");
    tty.key("return");
    assert!(question.await.unwrap().unwrap().ends_with("unfinished draft"));
}

#[tokio::test]
async fn switching_threads_restores_unsent_draft_cursor_queue_and_attachments_without_disk_history() {
    let tty = Tty::new(100, 24);
    tty.text("typed during startup");
    tty.ui.switch_input_session("a");
    assert_eq!(draft(&tty), "typed during startup");
    tty.ui.set_input("", false);
    tty.ui.paste("queued\nattachment");
    tty.key("tab");
    tty.text("thread a draft");
    tty.key("left");
    let original_cursor = tty.ui.state.lock().unwrap().cursor;
    tty.ui.switch_input_session("b");
    assert_eq!(draft(&tty), "");
    assert!(!tty.ui.has_queued_prompts());
    tty.text("thread b draft");
    tty.ui.switch_input_session("a");
    assert_eq!(draft(&tty), "thread a draft");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, original_cursor);
    assert!(tty.ui.has_queued_prompts());
    tty.ui.key(None, modified("up", false, true, false));
    tty.key("tab");
    assert_eq!(tty.ui.question("goon> ", None, true, QuestionOpts::default()).await.unwrap(), "queued\nattachment");
    tty.ui.switch_input_session("b");
    assert_eq!(draft(&tty), "thread b draft");
}

#[tokio::test]
async fn native_mode_preserves_shell_scrollback_commits_history_and_balances_terminal_modes() {
    let tty = Tty::with_mode(80, 24, false);
    assert!(!tty.written().contains("\x1b[?1049h"));
    for n in 0..40 {
        tty.ui.write(&format!("native history {n}\n"));
        tty.ui.render_now();
    }
    let output = tty.written();
    assert!(!output.contains("\x1b[H") && !output.contains("\x1b[2J"), "inline rendering must not clear shell output");
    assert!(output.contains("native history 0"));
    tty.key("pageup");
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
    tty.ui.set_transcript_overlay(true);
    assert!(tty.frame().contains("transcript"));
    tty.key("escape");
    tty.ui.close();
    assert!(!tty.written().contains("\x1b[?1049l"));
    assert!(tty.written().contains("\x1b[?2004l"));
}

#[tokio::test]
async fn user_question_choices_support_freeform_and_escape_cancels_without_selecting_default() {
    for method in ["arrows", "custom", "cancel"] {
        let tty = Tty::new(100, 24);
        tty.text("saved draft");
        let ui = tty.ui.clone();
        let q = tokio::spawn(async move {
            ui.question("Pick: ", None, false, QuestionOpts { choices: vec![("1".into(), "First".into()), ("2".into(), "Second".into())], allow_custom: true, esc_cancels: true, ..Default::default() })
                .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(tty.frame().contains("o other answer"));
        match method {
            "arrows" => {
                tty.key("down");
                tty.key("return");
            }
            "custom" => {
                tty.text("o");
                tty.ui.paste("my own answer");
                tty.key("return");
            }
            _ => tty.key("escape"),
        }
        let answer = q.await.unwrap();
        match method {
            "arrows" => assert_eq!(answer.unwrap(), "2"),
            "custom" => assert_eq!(answer.unwrap(), "my own answer"),
            _ => assert!(answer.unwrap_err().downcast_ref::<InputCancelled>().is_some()),
        }
        assert_eq!(draft(&tty), "saved draft");
    }
}

#[tokio::test]
async fn owned_transcript_wheel_scrolls_without_navigating_composer_history() {
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
    let tty = Tty::new(80, 20);
    tty.ui.write(&(0..80).map(|n| format!("history line {n}\n")).collect::<String>());
    tty.text("unfinished composer");
    let mut decoder = InputDecoder::default();
    let wheel = |kind| Event::Mouse(MouseEvent { kind, column: 4, row: 4, modifiers: KeyModifiers::NONE });
    tty.frame();
    decoder.feed_batch(&tty.ui, vec![wheel(MouseEventKind::ScrollUp)]);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 1);
    decoder.feed_batch(&tty.ui, vec![wheel(MouseEventKind::ScrollDown)]);
    tty.ui.set_transcript_overlay(true);
    tty.frame();
    assert!(tty.written().contains("\x1b[?1000h\x1b[?1006h"));
    decoder.feed_batch(&tty.ui, vec![wheel(MouseEventKind::ScrollUp)]);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 1);
    decoder.feed_batch(&tty.ui, vec![wheel(MouseEventKind::ScrollDown)]);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
    assert_eq!(draft(&tty), "unfinished composer");
    tty.key("escape");
    tty.frame();
    assert!(tty.ui.state.lock().unwrap().mouse_capture, "normal owned screen also captures mouse");
    let inline = Tty::with_mode(80, 20, false);
    inline.ui.write(&(0..80).map(|n| format!("history line {n}\n")).collect::<String>());
    inline.frame();
    decoder.feed_batch(&inline.ui, vec![wheel(MouseEventKind::ScrollUp)]);
    assert_eq!(inline.ui.state.lock().unwrap().scroll, 0);
    assert!(!inline.ui.state.lock().unwrap().mouse_capture, "inline mode retains terminal-native selection");
}

#[tokio::test]
async fn fractional_wheel_is_area_limited_and_follow_click_requires_matching_release_without_cancel() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    for overlay in [false, true] {
        let tty = Tty::new(100, 24);
        tty.ui.write(&(0..80).map(|n| format!("history line {n}\n")).collect::<String>());
        tty.ui.set_status("Running");
        tty.text("preserved draft");
        tty.ui.set_transcript_overlay(overlay);
        tty.ui.set_mouse_scroll_speed(0.1);
        tty.frame();
        let mouse = |kind, column, row| tty.ui.handle_mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE });
        for _ in 0..10 {
            mouse(MouseEventKind::ScrollUp, 5, if overlay { 0 } else { 23 });
        }
        assert_eq!(tty.ui.state.lock().unwrap().scroll, 0, "wheel outside transcript must not accumulate");
        for _ in 0..9 {
            mouse(MouseEventKind::ScrollUp, 5, 4);
        }
        assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
        mouse(MouseEventKind::ScrollUp, 5, 4);
        assert_eq!(tty.ui.state.lock().unwrap().scroll, 1);
        tty.ui.set_mouse_scroll_speed(0.75);
        mouse(MouseEventKind::ScrollUp, 5, 4);
        mouse(MouseEventKind::ScrollDown, 5, 4);
        assert_eq!(tty.ui.state.lock().unwrap().scroll, 1, "direction reversal drops old remainder");
        mouse(MouseEventKind::ScrollDown, 5, 4);
        assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
        tty.key("pageup");
        tty.frame();
        let follow_row = if overlay { 23 } else { tty.ui.state.lock().unwrap().transcript_view_rows.len() as u16 };
        let paused = tty.ui.state.lock().unwrap().scroll;
        mouse(MouseEventKind::Down(MouseButton::Left), 0, follow_row);
        mouse(MouseEventKind::Up(MouseButton::Left), 0, follow_row);
        assert_eq!(tty.ui.state.lock().unwrap().scroll, paused, "whole status row is not a button");
        mouse(MouseEventKind::Down(MouseButton::Left), 49, follow_row);
        assert_eq!(tty.ui.state.lock().unwrap().scroll, paused);
        mouse(MouseEventKind::Up(MouseButton::Left), 0, follow_row);
        assert_eq!(tty.ui.state.lock().unwrap().scroll, paused, "release outside cancels click");
        mouse(MouseEventKind::Up(MouseButton::Left), 49, follow_row);
        assert_eq!(tty.ui.state.lock().unwrap().scroll, paused, "release alone cannot jump");
        mouse(MouseEventKind::Down(MouseButton::Left), 49, follow_row);
        mouse(MouseEventKind::Up(MouseButton::Left), 49, follow_row);
        assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
        assert_eq!(tty.cancelled.load(Ordering::SeqCst), 0);
        assert_eq!(draft(&tty), "preserved draft");
        tty.ui.set_mouse_scroll_speed(f64::MAX);
        mouse(MouseEventKind::ScrollUp, 5, 4);
        let st = tty.ui.state.lock().unwrap();
        assert_eq!(st.scroll, st.max_scroll, "extreme valid speed saturates without arithmetic overflow");
    }
}

#[tokio::test]
async fn wheel_during_drag_keeps_selected_logical_text_on_release_and_keyboard_jump_drops_fraction() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(80, 24);
    tty.ui.write(&(0..80).map(|n| format!("history line {n}\n")).collect::<String>());
    tty.frame();
    let mouse = |kind, col, row| tty.ui.handle_mouse(MouseEvent { kind, column: col, row, modifiers: KeyModifiers::NONE });
    mouse(MouseEventKind::Down(MouseButton::Left), 0, 4);
    mouse(MouseEventKind::Drag(MouseButton::Left), 7, 4);
    let selected = tty.ui.selected_transcript_text().unwrap();
    mouse(MouseEventKind::ScrollUp, 7, 4);
    mouse(MouseEventKind::Up(MouseButton::Left), 7, 4);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(selected.as_str()), "wheel must not reinterpret the release row as another logical endpoint");
    tty.key("escape"); // clear selection first
    tty.ui.set_mouse_scroll_speed(0.75);
    mouse(MouseEventKind::ScrollUp, 0, 4); // remainder .75 while scrolled
    tty.key("escape"); // return to latest, discarding remainder
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
    mouse(MouseEventKind::ScrollUp, 0, 4);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 0, "Esc navigation must reset fractional momentum");
}

#[tokio::test]
async fn normal_owned_screen_selects_without_touching_composer_and_follows_output() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(100, 24);
    tty.ui.write(&(0..60).map(|n| format!("line {n} 界👩‍💻 text\n")).collect::<String>());
    tty.text("untouched draft");
    tty.frame();
    let mouse = |kind, column, row| tty.ui.handle_mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE });
    let (row, col) = {
        let st = tty.ui.state.lock().unwrap();
        let row = st.transcript_view_rows.iter().position(|line| line.contains("界")).unwrap();
        let plain = crate::term::strip_vt(&st.transcript_view_rows[row]);
        (row, visible_length(plain.split('界').next().unwrap()))
    };
    mouse(MouseEventKind::Down(MouseButton::Left), col as u16, row as u16);
    mouse(MouseEventKind::Drag(MouseButton::Left), (col + 4) as u16, row as u16);
    mouse(MouseEventKind::Up(MouseButton::Left), (col + 4) as u16, row as u16);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("界👩‍💻"));
    tty.frame();
    assert!(tty.ui.rendered_frame().contains("\x1b[7m"));
    tty.key("escape");
    assert!(tty.ui.selected_transcript_text().is_none());
    assert_eq!(draft(&tty), "untouched draft");
    mouse(MouseEventKind::ScrollUp, 0, 0);
    tty.frame();
    let follow_row = tty.ui.state.lock().unwrap().transcript_view_rows.len() as u16;
    assert!(tty.ui.state.lock().unwrap().scroll > 0);
    mouse(MouseEventKind::Down(MouseButton::Left), 49, follow_row);
    assert!(tty.ui.state.lock().unwrap().scroll > 0, "press alone cannot jump");
    mouse(MouseEventKind::Up(MouseButton::Left), 49, follow_row);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
    assert_eq!(draft(&tty), "untouched draft");
}

#[tokio::test]
async fn emoji_layout_matches_mouse_copy_columns_and_compact_draft_window() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(100, 24);
    tty.ui.write("a👩‍💻b 👍🏽 tail\n");
    tty.frame();
    let (row, col) = {
        let st = tty.ui.state.lock().unwrap();
        let row = st.transcript_view_rows.iter().position(|line| line.contains("👩‍💻")).unwrap();
        let plain = crate::term::strip_vt(&st.transcript_view_rows[row]);
        (row + st.transcript_screen_row, visible_length(plain.split("👩‍💻").next().unwrap()))
    };
    for (kind, column) in [(MouseEventKind::Down(MouseButton::Left), col), (MouseEventKind::Up(MouseButton::Left), col + 2)] {
        tty.ui.handle_mouse(MouseEvent { kind, column: column as u16, row: row as u16, modifiers: KeyModifiers::NONE });
    }
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("👩‍💻"));

    let compact = Tty::new(16, 10);
    compact.ui.set_input("prefixprefix👩‍💻", false);
    let frame = compact.frame();
    assert!(frame.contains("👩‍💻"), "compact input must keep the complete emoji: {frame:?}");
    assert!(frame.lines().all(|line| visible_length(line.trim_end_matches('\r')) < 16));
}

#[tokio::test]
async fn pointer_hit_testing_refreshes_scroll_and_snaps_wide_glyph_carets() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(100, 24);
    tty.ui.write(&(0..60).map(|n| format!("line {n:02} 界👩‍💻 tail\n")).collect::<String>());
    tty.text("keep draft");
    tty.frame();
    let mouse = |kind, column, row| tty.ui.handle_mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE });
    let (row, expected, wide_col) = {
        let st = tty.ui.state.lock().unwrap();
        let row = st.transcript_view_rows.iter().position(|line| line.contains("界")).unwrap();
        let plain = crate::term::strip_vt(&st.transcript_view_rows[row]);
        (row, plain[..7].to_string(), visible_length(plain.split('界').next().unwrap()))
    };
    mouse(MouseEventKind::ScrollUp, 0, row as u16);
    // No frame() between wheel and click: the hit test must apply pending scroll.
    mouse(MouseEventKind::Down(MouseButton::Left), 0, row as u16);
    mouse(MouseEventKind::Up(MouseButton::Left), 7, row as u16);
    let selected = tty.ui.selected_transcript_text().unwrap();
    assert_ne!(selected, expected);
    let current = {
        let st = tty.ui.state.lock().unwrap();
        crate::term::strip_vt(&st.transcript_view_rows[row]).to_string()
    };
    assert_eq!(selected, current[..7]);
    mouse(MouseEventKind::Down(MouseButton::Left), wide_col as u16, row as u16);
    mouse(MouseEventKind::Up(MouseButton::Left), (wide_col + 1) as u16, row as u16);
    assert!(tty.ui.selected_transcript_text().is_none(), "half a wide glyph is not a caret after it");
    mouse(MouseEventKind::Down(MouseButton::Left), wide_col as u16, row as u16);
    mouse(MouseEventKind::Up(MouseButton::Left), (wide_col + 2) as u16, row as u16);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("界"));
    tty.ui.resize(); // A duplicate resize notification cannot invalidate selection.
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("界"));
    tty.frame();
    mouse(MouseEventKind::Down(MouseButton::Left), 5, 22);
    assert!(tty.ui.selected_transcript_text().is_none(), "composer click releases transcript selection");
    assert!(!tty.ui.state.lock().unwrap().selection_dragging);
    assert_eq!(draft(&tty), "keep draft");

    {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        st.selection = Some(selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout));
        st.selection_dragging = true;
    }
    *tty.size.lock().unwrap() = (80, 20);
    tty.ui.resize();
    assert!(!tty.ui.state.lock().unwrap().selection_dragging, "aborted resize gesture cannot remain armed");
    {
        let mut st = tty.ui.state.lock().unwrap();
        st.ensure_body_layout();
        st.selection = Some(selection::Selection::new(selection::Point { line: 0, offset: 0 }, &st.transcript, &st.body_layout));
        st.selection_dragging = true;
    }
    tty.ui.write(&format!("{}\nretained tail\n", "x".repeat(TRANSCRIPT_MAX_CHARS)));
    let st = tty.ui.state.lock().unwrap();
    assert!(st.selection.is_some(), "immutable selected revision survives canonical history trimming");
}

#[tokio::test]
async fn logical_selection_survives_reflow_stream_replacement_and_trim_without_soft_wrap_newlines() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(54, 24);
    let logical = "alpha bold text with 👩‍💻 and a link (https://example.test/path) then omega";
    tty.ui.write("[answer]\nalpha **bold text** with 👩‍💻 and a [link](https://example.test/path) then omega\nsecond hard line\n");
    tty.frame();
    let (first, last, end_col) = {
        let st = tty.ui.state.lock().unwrap();
        let first = st.transcript_view_rows.iter().position(|row| row.contains("alpha")).unwrap();
        let last = st.transcript_view_rows.iter().position(|row| row.contains("omega")).unwrap();
        (first, last, visible_length(&st.transcript_view_rows[last]))
    };
    tty.ui.handle_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 0, row: first as u16, modifiers: KeyModifiers::NONE });
    tty.ui.handle_mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column: end_col as u16, row: last as u16, modifiers: KeyModifiers::NONE });
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(logical));
    {
        let mut st = tty.ui.state.lock().unwrap();
        st.transcript = "[answer]\nnew live revision with unrelated content\n".into();
    }
    for size in [(19, 12), (90, 30), (50, 18)] {
        *tty.size.lock().unwrap() = size;
        tty.ui.resize();
        let frame = tty.frame();
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(logical));
        assert!(tty.ui.rendered_frame().contains("\x1b[7m"), "selected endpoint remains visible: {frame:?}");
        assert!(!frame.contains("unrelated content"), "selection owns the displayed revision");
    }
    tty.ui.write(&format!("{}\nretained canonical tail\n", "x".repeat(TRANSCRIPT_MAX_CHARS)));
    tty.frame();
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(logical));
    tty.key("escape");
    assert!(tty.ui.selected_transcript_text().is_none());
    let frame = tty.frame();
    assert!(frame.contains("retained canonical tail"), "clearing selection rejoins advancing canonical history");
}

#[tokio::test]
async fn logical_copy_excludes_gutters_and_synthetic_fences_but_keeps_hard_blank_lines() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    for (source, expected, marker) in [
        ("[answer]\n- list words repeated enough to wrap into several rows of content\n", "list words repeated enough to wrap into several rows of content", "list words"),
        (
            "[answer]\n```rust\nlet long_identifier = \"a long string that wraps\";\n\nsecond_code_line();\n```\n",
            "let long_identifier = \"a long string that wraps\";\n\nsecond_code_line();",
            "let long",
        ),
        ("[answer]\n> quote words repeated enough to wrap into several rows of content\n", "quote words repeated enough to wrap into several rows of content", "quote words"),
        ("● Ran tools\n  └ 1. [é](http://x) and **bold** content\n", "1. é (http://x) and bold content", "é"),
    ] {
        let tty = Tty::new(30, 24);
        tty.ui.write(source);
        tty.frame();
        let (first, last, last_col) = {
            let st = tty.ui.state.lock().unwrap();
            let plain: Vec<String> = st.transcript_view_rows.iter().map(|row| crate::term::strip_vt(row).into_owned()).collect();
            let first = plain.iter().position(|row| row.contains(marker)).unwrap();
            let last = plain.iter().rposition(|row| row.contains("content") || row.contains("second_code_line")).unwrap();
            (first, last, visible_length(&st.transcript_view_rows[last]))
        };
        tty.ui.handle_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 0, row: first as u16, modifiers: KeyModifiers::NONE });
        tty.ui.handle_mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), column: last_col as u16, row: last as u16, modifiers: KeyModifiers::NONE });
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
        *tty.size.lock().unwrap() = (70, 24);
        tty.ui.resize();
        tty.frame();
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(expected));
    }
}

#[tokio::test]
async fn mouse_selection_paints_hard_newline_without_copying_padding() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    for overlay in [false, true] {
        let tty = Tty::new(40, 20);
        tty.ui.write("code\nnext\n");
        if overlay {
            tty.key("ctrl+t");
        }
        tty.frame();
        let (first, next) = {
            let st = tty.ui.state.lock().unwrap();
            (
                st.transcript_screen_row + st.transcript_view_rows.iter().position(|r| crate::term::strip_vt(r) == "code").unwrap(),
                st.transcript_screen_row + st.transcript_view_rows.iter().position(|r| crate::term::strip_vt(r) == "next").unwrap(),
            )
        };
        let mouse = |kind, column, row| tty.ui.handle_mouse(MouseEvent { kind, column, row: row as u16, modifiers: KeyModifiers::NONE });
        mouse(MouseEventKind::Down(MouseButton::Left), 0, first);
        mouse(MouseEventKind::Up(MouseButton::Left), 10, first);
        tty.frame();
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("code"));
        assert!(!tty.ui.rendered_frame().contains("code \x1b[0m"), "unselected newline has no padding highlight");
        mouse(MouseEventKind::Down(MouseButton::Left), 0, first);
        mouse(MouseEventKind::Up(MouseButton::Left), 0, next);
        tty.frame();
        assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("code\n"));
        assert!(tty.ui.rendered_frame().contains("code \x1b[0m"), "selected hard newline is visibly marked");
    }
}

#[tokio::test]
async fn clipped_table_projection_and_synthetic_mouse_release_keep_valid_anchors() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(40, 20);
    tty.ui.write("[answer]\n| [é](http://x) | **bold** and a very long clipped tail marker |\n|---|---|\n```rust\ncode body\n```\n");
    tty.frame();
    let (table, head, code) = {
        let st = tty.ui.state.lock().unwrap();
        (
            st.transcript_view_rows.iter().position(|line| line.contains("é")).unwrap(),
            st.transcript_view_rows.iter().position(|line| line.contains("rust")).unwrap(),
            st.transcript_view_rows.iter().position(|line| line.contains("code body")).unwrap(),
        )
    };
    let mouse = |kind, column, row| tty.ui.handle_mouse(MouseEvent { kind, column, row: row as u16, modifiers: KeyModifiers::NONE });
    let col = {
        let st = tty.ui.state.lock().unwrap();
        visible_length(crate::term::strip_vt(&st.transcript_view_rows[table]).split('é').next().unwrap())
    };
    mouse(MouseEventKind::Down(MouseButton::Left), col as u16, table);
    mouse(MouseEventKind::Up(MouseButton::Left), (col + 1) as u16, table);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("[é](<http://x>)"), "semantic copy preserves selected link styling without adding its unselected visible URL");
    tty.frame(); // highlight cannot slice a multibyte glyph at a raw-markup byte.
    mouse(MouseEventKind::Down(MouseButton::Left), 0, table);
    mouse(MouseEventKind::Up(MouseButton::Left), 39, table);
    let selected = tty.ui.selected_transcript_text().unwrap();
    assert!(selected.contains("é"));
    assert!(!selected.contains("clipped tail marker"), "narrow-table labels do not copy an unselected value offscreen");
    *tty.size.lock().unwrap() = (80, 24);
    tty.ui.resize();
    tty.frame();
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some(selected.as_str()), "grid/record transition retains selected logical bytes");
    tty.key("escape");
    // Re-establish small geometry so previously captured screen rows are valid.
    *tty.size.lock().unwrap() = (40, 20);
    tty.ui.resize();
    tty.frame();
    mouse(MouseEventKind::Down(MouseButton::Left), 4, code);
    mouse(MouseEventKind::Up(MouseButton::Left), 2, head);
    let st = tty.ui.state.lock().unwrap();
    assert!(!st.selection_dragging);
    assert!(st.selection.is_none(), "empty gesture released on a synthetic fence cannot freeze output");
}

#[tokio::test]
async fn local_history_owns_attachment_payloads_and_collapses_adjacent_identical_entries() {
    let tty = Tty::new(100, 24);
    tty.ui.paste("original\npaste payload");
    tty.key("tab");
    tty.ui.state.lock().unwrap().paste_archive.clear();
    tty.key("up");
    assert_eq!(tty.ui.state.lock().unwrap().pasted_texts.values().next().unwrap(), "original\npaste payload");
    tty.key("tab");
    assert_eq!(tty.ui.state.lock().unwrap().history.len(), 1, "unchanged resubmission is not a second local history entry");
}

#[tokio::test]
async fn fatal_recovery_expands_unsent_pastes_but_never_returns_secret_questions() {
    let tty = Tty::new(100, 24);
    tty.ui.paste("recover this\nexact draft");
    assert_eq!(tty.ui.unsent_draft_for_recovery().as_deref(), Some("recover this\nexact draft"));
    let ui = tty.ui.clone();
    let question = tokio::spawn(async move { ui.question("API key: ", None, false, QuestionOpts { mask: true, ..Default::default() }).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.paste("not-recoverable-secret");
    assert!(tty.ui.unsent_draft_for_recovery().is_none());
    question.abort();
    let _ = question.await;
    assert_eq!(tty.ui.unsent_draft_for_recovery().as_deref(), Some("recover this\nexact draft"));
}

#[tokio::test]
async fn question_notes_motion_preserves_commit_but_actual_edits_invalidate_it() {
    for motion in ["left", "right", "home", "end", "up", "down"] {
        let tty = Tty::new(100, 24);
        let ui = tty.ui.clone();
        let task = tokio::spawn(async move {
            ui.ask_user_questions(
                vec![serde_json::json!({"id":"first","question":"First?"}), serde_json::json!({"id":"last","question":"Last?","options":[{"label":"Yes"}]})],
                &CancellationToken::new(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        tty.ui.paste("committed notes");
        tty.key("return");
        tty.key("pageup");
        tty.key(motion);
        assert!(tty.ui.state.lock().unwrap().pending.as_ref().unwrap().form.as_ref().unwrap().questions[0].committed, "{motion} is not an edit");
        tty.key("pagedown");
        tty.key("return");
        let answers = task.await.unwrap().unwrap();
        assert_eq!(answers["first"], vec!["committed notes"]);
    }

    let tty = Tty::new(100, 24);
    let ui = tty.ui.clone();
    let task = tokio::spawn(async move {
        ui.ask_user_questions(vec![serde_json::json!({"id":"first","question":"First?"}), serde_json::json!({"id":"last","question":"Last?"})], &CancellationToken::new()).await
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.ui.paste("notes");
    tty.key("return");
    tty.key("pageup");
    tty.ui.key(None, modified("u", true, false, false));
    assert!(!tty.ui.state.lock().unwrap().pending.as_ref().unwrap().form.as_ref().unwrap().questions[0].committed, "kill-line edits invalidate commit");
    tty.key("escape");
    assert!(task.await.unwrap().is_err());
}

#[tokio::test]
async fn pasting_on_question_options_focuses_notes_and_keeps_them_visible() {
    use crossterm::event::Event;
    let tty = Tty::new(100, 24);
    let ui = tty.ui.clone();
    let task = tokio::spawn(async move { ui.ask_user_questions(vec![serde_json::json!({"id":"q","question":"Choose?","options":[{"label":"Yes"}]})], &CancellationToken::new()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    InputDecoder::default().feed_batch(&tty.ui, vec![Event::Paste("visible note".into())]);
    assert!(tty.ui.state.lock().unwrap().pending.as_ref().unwrap().form.as_ref().unwrap().questions[0].notes);
    assert!(tty.frame().contains("Notes"));
    tty.key("return");
    assert_eq!(task.await.unwrap().unwrap()["q"], vec!["Yes", "visible note"]);
}

#[tokio::test]
async fn terminal_focus_events_update_notification_state_without_touching_input() {
    use crossterm::event::Event;
    let tty = Tty::new(100, 24);
    tty.text("preserved input");
    let mut decoder = InputDecoder::default();
    assert!(tty.ui.is_focused());
    decoder.feed_batch(&tty.ui, vec![Event::FocusLost]);
    assert!(!tty.ui.is_focused());
    decoder.feed_batch(&tty.ui, vec![Event::FocusGained]);
    assert!(tty.ui.is_focused());
    assert_eq!(draft(&tty), "preserved input");
    tty.ui.close();
    assert!(tty.written().contains("\x1b[?1004h"));
    assert!(tty.written().contains("\x1b[?1004l"));
}

#[tokio::test]
async fn up_down_move_across_soft_wrapped_rows_with_grapheme_and_display_width() {
    let tty = Tty::new(13, 24); // ten display columns after the composer label/margin.
    tty.text("abcdefghijklmno");
    tty.key("up");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, 5);
    tty.key("down");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, 15);
    tty.ui.set_input("界界界界界abcd", false);
    tty.key("up");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, 2);
    tty.key("down");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, 9);
    tty.ui.set_input("", false);
    tty.text("abcdefgh");
    tty.ui.key(None, modified("j", true, false, false));
    tty.text("x");
    tty.ui.key(None, modified("j", true, false, false));
    tty.text("abcdefgh");
    tty.key("up");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, 10);
    tty.key("up");
    assert_eq!(tty.ui.state.lock().unwrap().cursor, 8, "vertical movement preserves preferred display column across short rows");
    tty.ui.set_input("a👩‍💻bcdefghij", false);
    tty.key("up");
    let st = tty.ui.state.lock().unwrap();
    let prefix = st.input[..st.cursor].iter().collect::<String>();
    assert!(unicode_segmentation::UnicodeSegmentation::graphemes("a👩‍💻bcdefghij", true)
        .scan(String::new(), |sum, g| {
            sum.push_str(g);
            Some(sum.clone())
        })
        .any(|boundary| boundary == prefix));
}

#[tokio::test]
async fn grouped_questions_preserve_each_selection_and_notes_until_atomic_submission() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let tty = Tty::new(100, 24);
    tty.text("original composer");
    let ui = tty.ui.clone();
    let task = tokio::spawn(async move {
        ui.ask_user_questions(
            vec![
                serde_json::json!({"id":"one","header":"First","question":"Choose first","options":[{"label":"A"},{"label":"B"}]}),
                serde_json::json!({"id":"two","header":"Second","question":"Choose second","options":[{"label":"C"},{"label":"D"}]}),
            ],
            &CancellationToken::new(),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    let key = |code| tty.ui.handle_crossterm_key(KeyEvent::new(code, KeyModifiers::NONE), std::time::Instant::now(), true);
    assert!(tty.frame().contains("Question 1/2"));
    key(KeyCode::Down);
    key(KeyCode::Tab);
    tty.ui.paste("first notes\nwith paste");
    key(KeyCode::PageDown);
    assert!(tty.frame().contains("Question 2/2"));
    key(KeyCode::Down);
    key(KeyCode::Down); // Other
    key(KeyCode::Enter);
    tty.ui.paste("custom second");
    key(KeyCode::PageUp);
    assert!(draft(&tty).contains("[Pasted text"));
    key(KeyCode::Enter); // commit first, advance
    assert_eq!(draft(&tty), "custom second");
    assert!(!task.is_finished());
    key(KeyCode::Enter);
    let answers = tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap().unwrap();
    assert_eq!(answers["one"], vec!["B", "first notes\nwith paste"]);
    assert_eq!(answers["two"], vec!["custom second"]);
    assert_eq!(draft(&tty), "original composer");
}

#[tokio::test]
async fn unanswered_confirmation_proceeds_only_after_confirmation_and_omits_uncommitted_drafts() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    for windows in [false, true] {
        let tty = Tty::new(100, 24);
        tty.text("saved composer");
        let ui = tty.ui.clone();
        let task = tokio::spawn(async move {
            ui.ask_user_questions(
                vec![serde_json::json!({"id":"skip","question":"Skipped?","options":[{"label":"Not an answer"}]}), serde_json::json!({"id":"last","question":"Last?"})],
                &CancellationToken::new(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        tty.key("tab");
        tty.ui.paste("uncommitted notes");
        tty.key("pagedown");
        tty.ui.paste("final notes");
        tty.key("return");
        assert!(tty.frame().contains("Submit with unanswered questions?"));
        assert!(!task.is_finished());
        let key = |code| tty.ui.handle_crossterm_key(KeyEvent::new(code, KeyModifiers::NONE), std::time::Instant::now(), windows);
        key(KeyCode::Char('2'));
        assert!(!task.is_finished(), "digits only select a confirmation row");
        assert!(tty.frame().contains("› 2. Go back"));
        key(KeyCode::Char('j')); // wrap to Proceed, even from a freeform composer.
        assert!(tty.frame().contains("› 1. Proceed"));
        key(KeyCode::Tab);
        key(KeyCode::PageUp);
        key(KeyCode::Char('x'));
        assert_eq!(draft(&tty), "final notes", "confirmation consumes unrelated composer keys");
        tty.ui.key(None, modified("d", true, false, false));
        assert!(!task.is_finished());
        key(KeyCode::Enter);
        let answers = task.await.unwrap().unwrap();
        assert!(answers["skip"].is_empty());
        assert_eq!(answers["last"], vec!["final notes"]);
        assert_eq!(draft(&tty), "saved composer");
    }
}

#[tokio::test]
async fn unanswered_confirmation_go_back_restores_first_skipped_draft_and_ctrl_c_cancels() {
    for back in ["escape", "backspace", "select"] {
        let tty = Tty::new(100, 24);
        tty.text("original");
        let ui = tty.ui.clone();
        let task = tokio::spawn(async move {
            ui.ask_user_questions(
                vec![serde_json::json!({"id":"first","question":"First?","options":[{"label":"A"}]}), serde_json::json!({"id":"last","question":"Last?","options":[{"label":"B"}]})],
                &CancellationToken::new(),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        tty.key("tab");
        tty.ui.paste("keep this draft");
        tty.key("pagedown");
        tty.key("return");
        assert!(tty.frame().contains("Submit with unanswered questions?"));
        if back == "select" {
            tty.key("up"); // wraps to Go back
            tty.key("return");
        } else {
            tty.key(back);
        }
        assert!(tty.frame().contains("Question 1/2"));
        assert_eq!(draft(&tty), "keep this draft");
        assert!(!task.is_finished());
        tty.key("pagedown");
        tty.key("return");
        tty.ui.key(None, modified("c", true, false, false));
        assert!(task.await.unwrap().is_err());
        assert_eq!(draft(&tty), "original");
    }
}

#[tokio::test]
async fn grouped_question_cancellation_and_dropped_future_restore_draft_without_history() {
    for cancel_by in ["escape", "token", "drop"] {
        let tty = Tty::new(100, 24);
        tty.text("preserve this");
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let ui = tty.ui.clone();
        let task = tokio::spawn(async move { ui.ask_user_questions(vec![serde_json::json!({"id":"q","question":"Question","options":[{"label":"A"}]})], &c).await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        tty.key("tab");
        tty.ui.paste("private notes");
        match cancel_by {
            "escape" => tty.key("escape"),
            "token" => cancel.cancel(),
            _ => task.abort(),
        }
        assert!(!matches!(task.await, Ok(Ok(_))));
        assert_eq!(draft(&tty), "preserve this");
        assert!(tty.ui.state.lock().unwrap().history.is_empty());
    }
}

#[tokio::test]
async fn grouped_question_layout_fits_small_terminals_and_freeform_empty_answers_are_allowed() {
    for (cols, rows) in [(12, 8), (30, 10), (80, 24)] {
        let tty = Tty::new(cols, rows);
        let ui = tty.ui.clone();
        let task = tokio::spawn(async move {
            ui.ask_user_questions(vec![serde_json::json!({"id":"q","question":"A long question with enough words to wrap over several rows on narrow terminals"})], &CancellationToken::new()).await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        tty.frame();
        {
            let st = tty.ui.state.lock().unwrap();
            assert!(st.last_frame_rows.len() <= rows, "{} rows emitted into {rows}", st.last_frame_rows.len());
            assert!(st.last_frame_rows.iter().all(|line| visible_length(line) < cols));
        }
        tty.key("return");
        assert!(!task.is_finished());
        tty.key("return"); // Explicitly confirm an empty freeform answer.
        assert!(task.await.unwrap().unwrap()["q"].is_empty());
    }
}

#[tokio::test]
async fn transcript_drag_selection_uses_display_columns_and_follow_click_returns_to_tail() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let tty = Tty::new(100, 24);
    tty.ui.write("hello 界👩‍💻 world\nsecond line\n");
    tty.ui.set_transcript_overlay(true);
    tty.frame();
    let row = {
        let st = tty.ui.state.lock().unwrap();
        st.transcript_view_rows.iter().position(|line| crate::term::strip_vt(line).contains("hello")).unwrap() + 1
    };
    let mouse = |kind, column, row| tty.ui.handle_mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE });
    let first_line = tty.ui.state.lock().unwrap().transcript_view_rows[row - 1].clone();
    let plain = crate::term::strip_vt(&first_line);
    let start = visible_length(plain.split("界").next().unwrap());
    mouse(MouseEventKind::Down(MouseButton::Left), start as u16, row as u16);
    mouse(MouseEventKind::Drag(MouseButton::Left), (start + 4) as u16, row as u16);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("界👩‍💻"));
    tty.frame();
    assert!(tty.ui.rendered_frame().contains("\x1b[7m"));
    mouse(MouseEventKind::ScrollUp, 0, row as u16);
    assert_eq!(tty.ui.selected_transcript_text().as_deref(), Some("界👩‍💻"), "wheel must not clear selection");
    tty.key("escape");
    assert!(tty.ui.is_transcript_overlay(), "first Escape clears selection before closing transcript");
    assert!(tty.ui.selected_transcript_text().is_none());
    tty.ui.write(&(0..60).map(|n| format!("line {n}\n")).collect::<String>());
    tty.frame();
    mouse(MouseEventKind::ScrollUp, 0, 4);
    assert!(tty.frame().contains("Follow output"));
    mouse(MouseEventKind::Down(MouseButton::Left), 49, 23);
    mouse(MouseEventKind::Up(MouseButton::Left), 49, 23);
    assert_eq!(tty.ui.state.lock().unwrap().scroll, 0);
    tty.ui.resize();
    assert!(tty.ui.selected_transcript_text().is_none());
}

#[tokio::test]
async fn clipped_question_options_cannot_be_submitted_until_fully_readable() {
    let tty = Tty::new(20, 8);
    let ui = tty.ui.clone();
    let task = tokio::spawn(async move {
        ui.ask_user_questions(vec![serde_json::json!({"id":"q","question":"Choose","options":[{"label":"A very long option that cannot be read completely in this tiny viewport", "description":"Important additional authorization details must remain visible"}]})], &CancellationToken::new()).await
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    tty.key("return");
    assert!(!task.is_finished());
    assert!(tty.frame().contains("Expand terminal"));
    *tty.size.lock().unwrap() = (120, 30);
    tty.ui.resize();
    tty.frame();
    tty.key("return");
    assert_eq!(task.await.unwrap().unwrap()["q"].len(), 1);
}

#[tokio::test]
async fn vietnamese_telex_ime_rewrite_echoes_immediately_and_submits_on_enter_while_multiline_paste_stays_protected() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Instant;
    let tty = Tty::new(80, 24);

    let ui1 = tty.ui.clone();
    let ask1 = tokio::spawn(async move { ui1.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut t = Instant::now();
    // 1. User types "tiee" on Windows, then Vietnamese Telex IME sends Backspace x 2 + "ếng" within 1ms!
    for ch in ['t', 'i', 'e', 'e'] {
        tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), t, true);
        t += Duration::from_millis(25);
    }
    tty.ui.flush_paste_burst_at(t + Duration::from_millis(70), true);
    t += Duration::from_millis(75);
    assert_eq!(draft(&tty), "tiee");

    // IME rewrite burst: Backspace x 2 + "ếng" at 1ms intervals
    for _ in 0..2 {
        tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), t, true);
        t += Duration::from_millis(1);
    }
    for ch in "ếng".chars() {
        tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), t, true);
        t += Duration::from_millis(1);
    }
    // Zero-lag assertion: "tiếng" is ALREADY in st.input without waiting 60ms!
    assert_eq!(draft(&tty), "tiếng");

    // Pressing Enter 5ms later submits immediately instead of inserting a newline!
    t += Duration::from_millis(5);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), t, true);
    assert_eq!(ask1.await.unwrap().unwrap(), "tiếng");

    // 2. Also verify "chào": type "chao", Backspace x 2 + "ào" + immediate Enter
    let ui2 = tty.ui.clone();
    let ask2 = tokio::spawn(async move { ui2.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    for ch in ['c', 'h', 'a', 'o'] {
        t += Duration::from_millis(20);
        tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), t, true);
    }
    t += Duration::from_millis(70);
    tty.ui.flush_paste_burst_at(t, true);
    for _ in 0..2 {
        t += Duration::from_millis(1);
        tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), t, true);
    }
    for ch in "ào".chars() {
        t += Duration::from_millis(1);
        tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), t, true);
    }
    assert_eq!(draft(&tty), "chào");
    t += Duration::from_millis(5);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), t, true);
    assert_eq!(ask2.await.unwrap().unwrap(), "chào");

    // 3. Unbracketed multi-line paste on Windows STILL coalesces and suppresses trailing Enter within 120ms!
    let ui3 = tty.ui.clone();
    let ask3 = tokio::spawn(async move { ui3.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    t += Duration::from_millis(200);
    for ch in "first line\r\nsecond line\r\nthird line\r\nfourth line".chars() {
        let code = match ch {
            '\r' | '\n' => KeyCode::Enter,
            c => KeyCode::Char(c),
        };
        tty.ui.handle_crossterm_key(KeyEvent::new(code, KeyModifiers::NONE), t, true);
        t += Duration::from_millis(1);
    }
    assert!(!ask3.is_finished(), "multi-line paste newlines must never auto-submit");
    let flush_at = t + Duration::from_millis(65);
    tty.ui.flush_paste_burst_at(flush_at, true);
    let composer_after_paste = draft(&tty);
    assert!(composer_after_paste.contains("[Pasted text #1"), "got {composer_after_paste:?}");
    ask3.abort();
}

#[tokio::test]
async fn status_notice_renders_while_busy_clears_retry_on_stream_and_beats_attachments_when_idle() {
    let tty = Tty::new(90, 24);
    tty.ui.set_status("Running");
    tty.ui.set_notice(Some("↻ HTTP 429 — retrying (1/3) in 2.0s"));
    let busy_frame = tty.frame();
    assert!(busy_frame.contains("↻ HTTP 429 — retrying (1/3) in 2.0s"), "busy frame must show retry notice:\n{busy_frame}");

    tty.ui.set_transcript_overlay(true);
    let overlay_frame = tty.frame();
    assert!(overlay_frame.contains("↻ HTTP 429"), "transcript overlay must show retry notice:\n{overlay_frame}");
    tty.ui.set_transcript_overlay(false);

    // Streaming delta clears retry notice automatically
    tty.ui.event(&serde_json::json!({ "type": "text", "text": "Recovered response" }));
    let recovered_frame = tty.frame();
    assert!(!recovered_frame.contains("↻ HTTP 429"), "streaming delta must clear retry notice:\n{recovered_frame}");

    // Non-retry transient notice while busy renders and expires cleanly
    tty.ui.set_notice(Some("Copied selection"));
    assert!(tty.frame().contains("Copied selection"));
    tty.ui.expire_busy_notice_for_test();
    assert!(!tty.frame().contains("Copied selection"));

    // Idle with attachment + notice: notice wins over "attached ... backspace removes"
    tty.ui.set_status("Ready");
    tty.ui.paste("alpha\nbeta\ngamma\ndelta\nepsilon");
    assert!(tty.frame().contains("attached"));
    tty.ui.set_notice(Some("Copied last answer"));
    let idle_notice_frame = tty.frame();
    assert!(idle_notice_frame.contains("Copied last answer"), "{idle_notice_frame}");
    assert!(!idle_notice_frame.contains("backspace removes"), "{idle_notice_frame}");
}

#[tokio::test]
async fn active_tool_elapsed_timer_renders_running_tool_seconds_and_clears_on_completion() {
    let tty = Tty::new(96, 24);
    tty.ui.set_status("Running");
    tty.ui.event(&serde_json::json!({
        "type": "tool",
        "id": "call_bash_1",
        "name": "bash",
        "state": "start",
        "meta": { "kind": "exec", "summary": "cargo test --locked" }
    }));
    tty.ui.backdate_active_tool_for_test("call_bash_1", 12_300);
    let f1 = tty.frame();
    assert!(f1.contains("Running bash (12s)"), "expected live tool timer in:\n{f1}");
    assert!(f1.contains("cargo test --locked"), "expected tool summary in:\n{f1}");

    tty.ui.event(&serde_json::json!({
        "type": "tool",
        "id": "call_read_2",
        "name": "read_file",
        "state": "start",
        "meta": { "kind": "read", "summary": "src/main.rs" }
    }));
    let f2 = tty.frame();
    assert!(f2.contains("(+1 more)"), "expected concurrent tool count in:\n{f2}");

    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "call_bash_1", "name": "bash", "state": "done", "meta": { "kind": "exec", "summary": "cargo test --locked" } }));
    tty.ui.event(&serde_json::json!({ "type": "tool", "id": "call_read_2", "name": "read_file", "state": "error", "meta": { "kind": "read", "summary": "src/main.rs" } }));
    assert!(tty.ui.state.lock().unwrap().active_tools.is_empty());
    let f3 = tty.frame();
    assert!(!f3.contains("Running bash ("), "{f3}");
}

#[tokio::test]
async fn emergency_terminal_restore_resets_dec_modes_and_guards_never_undo_panic_recovery() {
    let mut zero_buf = Vec::new();
    emergency_restore_terminal_to(&mut zero_buf, 0);
    assert!(zero_buf.is_empty(), "inactive terminal state must emit 0 bytes");

    let mut alt_buf = Vec::new();
    emergency_restore_terminal_to(&mut alt_buf, 0b111);
    let alt_seq = String::from_utf8(alt_buf).unwrap();
    assert!(
        alt_seq.contains("\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l\r\n"),
        "alt-screen emergency restore must match exact DEC reset suffix with trailing CRLF: {alt_seq:?}"
    );
    assert!(alt_seq.starts_with("\x1b[?2026l\x1b[0m"));
    assert!(alt_seq.ends_with("\r\n"));

    let mut inline_buf = Vec::new();
    emergency_restore_terminal_to(&mut inline_buf, 0b101);
    let inline_seq = String::from_utf8(inline_buf).unwrap();
    assert!(inline_seq.contains("\x1b[?1002l\x1b[?1004l\x1b[?2004l\x1b[?1007l\x1b[?1006l\x1b[?1000l\x1b[?25h\r\n"));
    assert!(!inline_seq.contains("\x1b[?1049l"), "inline mode must not emit ?1049l");

    let tty = Tty::new(80, 24);
    let ui_ref = tty.ui.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = ui_ref.state.lock().unwrap();
        panic!("simulated render panic while holding state lock");
    }));
    assert!(tty.ui.state.is_poisoned());
    tty.ui.restore_terminal();
    assert!(!tty.ui.is_closed());
    let raw_out = tty.written();
    assert!(raw_out.contains("\x1b[?2004l") && raw_out.contains("\x1b[?1049l"), "poisoned restore must still emit DEC reset sequences");

    // EditorGuard dropped during panic stack unwinding must NOT re-enable ?2004h or hide cursor (?25l)
    let ed_tty = Tty::new(80, 24);
    ed_tty.ui.state.lock().unwrap().editor_active = true;
    ed_tty.sink.0.lock().unwrap().clear();
    let ed_ui = ed_tty.ui.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = editor::EditorGuard(&ed_ui);
        panic!("simulated panic inside external editor handoff");
    }));
    let ed_out = ed_tty.written();
    assert!(ed_out.contains("\x1b[?25h\x1b[?1049l"), "EditorGuard panic unwind must restore terminal: {ed_out:?}");
    assert!(!ed_out.contains("\x1b[?25l") && !ed_out.contains("\x1b[?2004h"), "EditorGuard panic unwind must never re-arm TUI modes: {ed_out:?}");

    #[cfg(unix)]
    {
        let sus_tty = Tty::new(80, 24);
        sus_tty.ui.request_suspend();
        let sus_ui = sus_tty.ui.clone();
        let sus_sink = sus_tty.sink.0.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            sus_ui.suspend_with(|| -> std::io::Result<()> {
                sus_sink.lock().unwrap().clear();
                panic!("simulated panic while suspended");
            });
        }));
        let sus_out = sus_tty.written();
        assert!(
            !sus_out.contains("\x1b[?1049h") && !sus_out.contains("\x1b[?25l") && !sus_out.contains("\x1b[?2004h"),
            "ResumeGuard panic unwind must never re-enter alt screen or hide cursor: {sus_out:?}"
        );
    }
}

#[tokio::test]
async fn theme_modes_no_color_ansi16_and_light_propagate_across_transcript_overlays_and_tables() {
    let sgr_re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    let tty = Tty::new(84, 24);
    tty.ui.write(&format!(
        "╭─ ◆ goon v0.3.16 ─╮\n│  model   claude-sonnet-4-5 │\n╰──────────────────╯\n\n{USER_MARK}Explain `Palette` and **NO_COLOR** with [spec](https://no-color.org)\n\n[answer]\n# Heading\n| Mode | SGR |\n| --- | --- |\n| None | zero |\n\n```rust\nlet x = 42;\n```\n"
    ));
    for i in 0..20 {
        tty.ui.write(&format!("extra line {i}\n"));
    }

    // 1. ThemeMode::None (NO_COLOR): zero SGR sequences anywhere (normal, scrolled Follow, overlay, picker)
    tty.ui.set_theme_mode(ThemeMode::None);
    tty.frame();
    let normal_none = tty.ui.rendered_frame();
    assert!(!sgr_re.is_match(&normal_none), "ThemeMode::None leaked SGR in normal view:\n{normal_none:?}");

    tty.ui.state.lock().unwrap().scroll = 5;
    tty.frame();
    let scrolled_none = tty.ui.rendered_frame();
    assert!(scrolled_none.contains("Follow output"));
    assert!(!sgr_re.is_match(&scrolled_none), "ThemeMode::None leaked SGR in Follow pill:\n{scrolled_none:?}");
    tty.ui.state.lock().unwrap().scroll = 0;

    tty.ui.set_transcript_overlay(true);
    tty.frame();
    let overlay_none = tty.ui.rendered_frame();
    assert!(!sgr_re.is_match(&overlay_none), "ThemeMode::None leaked SGR in transcript overlay:\n{overlay_none:?}");
    tty.ui.set_transcript_overlay(false);

    // 2. ThemeMode::Ansi16: standard 16-color SGR present, zero 24-bit `38;2;` RGB sequences
    tty.ui.set_theme_mode(ThemeMode::Ansi16);
    tty.frame();
    let ansi16_frame = tty.ui.rendered_frame();
    assert!(sgr_re.is_match(&ansi16_frame));
    assert!(!ansi16_frame.contains("38;2;"), "Ansi16 must never emit 24-bit RGB:\n{ansi16_frame:?}");

    // 3. ThemeMode::Light: emits high-contrast Light palette RGB tokens
    tty.ui.set_theme_mode(ThemeMode::Light);
    tty.frame();
    let light_frame = tty.ui.rendered_frame();
    assert!(light_frame.contains("38;2;109;40;217m") || light_frame.contains("38;2;87;83;78m"), "Light theme must emit light palette tokens:\n{light_frame:?}");
    assert!(!light_frame.contains("38;2;167;139;250m"), "Light theme must not emit Dark brand pastel");
}

#[tokio::test]
async fn narrow_portrait_viewport_preserves_colors_clips_cleanly_and_shows_inline_slash_hint() {
    let sgr_re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    let tty = Tty::new(36, 18);
    tty.ui.set_mode("plan");
    tty.ui.write("\n[answer]\nShort **styled** answer on a 36-col mobile screen.\n");
    tty.text("/mo");
    tty.frame();
    let raw = tty.ui.rendered_frame();
    assert!(sgr_re.is_match(&raw), "36-col portrait terminal must preserve ANSI colors when NO_COLOR is unset:\n{raw:?}");
    for (idx, line) in raw.lines().enumerate() {
        let vis = visible_length(line);
        assert!(vis <= 35, "row {idx} exceeded safe_cols=35 (vis={vis}): {:?}", crate::term::strip_vt(line));
    }
    let plain = crate::term::strip_vt(&raw);
    assert!(plain.contains("› /model"), "compact viewport should show inline slash command hint:\n{plain}");
    assert!(tty.written().contains("\x1b[?25h"), "compact viewport must show cursor while typing");

    tty.ui.set_theme_mode(ThemeMode::None);
    tty.frame();
    let raw_no_color = tty.ui.rendered_frame();
    assert!(!sgr_re.is_match(&raw_no_color), "36-col viewport with ThemeMode::None must emit zero SGR:\n{raw_no_color:?}");
}

#[tokio::test]
async fn windows_conpty_long_first_line_multiline_and_single_line_pastes_coalesce_without_premature_submit() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Instant;

    let tty = Tty::new(90, 24);
    let ui1 = tty.ui.clone();
    let ask1 = tokio::spawn(async move { ui1.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let t0 = Instant::now();
    // 1. Multi-line ConPTY paste where Line 1 (150 chars) spans 0..110ms across 3 chunks,
    //    an async tick polls at t = 120ms, and \r\nLine 2\r\nLine 3\r\nLine 4 arrives at t = 122ms.
    for chunk_idx in 0..3 {
        let t_chunk = t0 + Duration::from_millis(chunk_idx * 55);
        for _ in 0..50 {
            tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), t_chunk, true);
        }
    }
    assert!(!tty.ui.flush_paste_burst_at(t0 + Duration::from_millis(120), true), "ConPTY paste must not hit 120ms wall cap");
    let t_rest = t0 + Duration::from_millis(122);
    for ch in "\r\nLine 2\r\nLine 3\r\nLine 4".chars() {
        let ev = match ch {
            '\r' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            '\n' => KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            c => KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
        };
        tty.ui.handle_crossterm_key(ev, t_rest, true);
    }
    assert!(!ask1.is_finished(), "Line 1 \\r at 122ms must never auto-submit mid-paste");
    assert!(tty.ui.flush_paste_burst_at(t_rest + Duration::from_millis(65), true));
    let d1 = draft(&tty);
    assert_eq!(d1, "[Pasted text #1 +4 lines] ", "entire payload must coalesce with zero inline prefix leak: {d1:?}");
    ask1.abort();
    let _ = ask1.await;

    // 2. 200-char single-line ConPTY paste spanning > 120ms + late trailing Enter within 120ms window
    tty.ui.set_input("", false);
    let ui2 = tty.ui.clone();
    let ask2 = tokio::spawn(async move { ui2.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let s0 = t_rest + Duration::from_millis(300);
    for (idx, ms) in [0u64, 50, 100, 150].into_iter().enumerate() {
        let at = s0 + Duration::from_millis(ms);
        let ch = char::from(b'a' + (idx as u8));
        for _ in 0..50 {
            tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), at, true);
        }
    }
    let last_at = s0 + Duration::from_millis(150);
    assert!(tty.ui.flush_paste_burst_at(last_at + Duration::from_millis(65), true));
    // Late trailing Enter at +70ms (< 120ms PASTE_ENTER_SUPPRESS_WINDOW) must NOT auto-submit!
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), last_at + Duration::from_millis(70), true);
    assert!(!ask2.is_finished(), "late trailing Enter within 120ms of large single-line paste must not auto-submit");
    assert!(draft(&tty).starts_with("[Pasted text #1"), "expected collapsed paste pill, got {:?}", draft(&tty));
    ask2.abort();
}

#[tokio::test]
async fn windows_human_rollover_enter_submits_tab_autocompletes_and_nfd_typing_normalizes_to_nfc() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Instant;

    let tty = Tty::new(80, 24);

    // 1. Traditional Telex "tiếng" where final "ng" rolls over in 5ms followed by Enter 30ms later on Windows
    let ui1 = tty.ui.clone();
    let ask1 = tokio::spawn(async move { ui1.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;

    let mut t = Instant::now();
    for ch in "tiế".chars() {
        tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), t, true);
        t += Duration::from_millis(25);
    }
    tty.ui.flush_paste_burst_at(t + Duration::from_millis(65), true);
    t += Duration::from_millis(70);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), t, true);
    t += Duration::from_millis(5);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), t, true);
    t += Duration::from_millis(30);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), t, true);
    assert_eq!(ask1.await.unwrap().unwrap(), "tiếng");

    // 2. "/mo" with 5ms rollover + Tab at +25ms autocompletes "/model " on Windows instead of literal '\t'
    t += Duration::from_millis(200);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), t, true);
    t += Duration::from_millis(25);
    tty.ui.flush_paste_burst_at(t, true);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), t, true);
    t += Duration::from_millis(5);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), t, true);
    t += Duration::from_millis(25);
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), t, true);
    assert_eq!(draft(&tty), "/model ");
    tty.ui.set_input("", false);

    // 3. Decomposed NFD combining sequence typed via terminal keystrokes normalizes to canonical 5-char NFC "tiếng"
    let ui2 = tty.ui.clone();
    let ask2 = tokio::spawn(async move { ui2.question("goon> ", None, true, QuestionOpts::default()).await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    t += Duration::from_millis(200);
    for ch in ['t', 'i', 'e', '\u{0302}', '\u{0301}', 'n', 'g'] {
        tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), t, true);
        t += Duration::from_millis(20);
    }
    tty.ui.flush_paste_burst_at(t + Duration::from_millis(65), true);
    assert_eq!(tty.ui.state.lock().unwrap().input.len(), 5, "composer input must be canonical 5-char NFC");
    tty.ui.handle_crossterm_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), t + Duration::from_millis(70), true);
    let submitted = ask2.await.unwrap().unwrap();
    assert_eq!(submitted, "tiếng");
    assert_eq!(submitted.chars().count(), 5);
}

#[tokio::test]
async fn busy_status_row_strips_osc52_dcs_apc_csi_newlines_and_tabs_across_all_event_channels() {
    let tty = Tty::new(96, 24);
    tty.ui.set_status("Running");
    tty.sink.0.lock().unwrap().clear();

    // 1. Malicious tool_stream preview & name containing OSC-52, DCS, APC, 8-bit C1, private CSI, \n, \t
    tty.ui.event(&serde_json::json!({
        "type": "tool_stream",
        "name": "read_file\n\t\x1b]52;c;bmFtZQ==\x07",
        "preview": "{\"path\":\"\x1b]52;c;cHduZWQ=\x07\x1bP1$r\x1b\\\x1b_Gi=1\x1b\\\u{9d}52;c;eA==\u{9c}\x1b[>4;2m\x1b[38:2::255:0:0mline1\n\tline2\"}"
    }));
    tty.frame();

    // 2. Malicious reasoning header/detail
    tty.ui.event(&serde_json::json!({
        "type": "reasoning",
        "text": "**Analyzing \x1b]52;c;cmVhc29u\x07\tstep**\nDetail with \x1bPbad\x1b\\ and \t tabs\n"
    }));
    tty.frame();

    // 3. Malicious active tool name/summary and multi-line HTTP 502 retry notice
    tty.ui.event(&serde_json::json!({
        "type": "tool",
        "id": "call_1",
        "name": "mcp_evil\n\t\x1b]52;c;dG9vbA==\x07",
        "state": "start",
        "meta": { "kind": "exec", "summary": "cmd\nwith\ttabs\x1b]52;c;c3Vt\x07" }
    }));
    tty.frame();

    tty.ui.set_notice(Some("↻ HTTP 502:\n<html>\tBad Gateway\x1b]52;c;cmV0cnk=\x07\r\nretrying in 2s"));
    tty.frame();
    assert_eq!(tty.ui.state.lock().unwrap().last_frame_rows.len(), 24, "multi-line retry notice must never break frame row count");

    let raw_pty = tty.written();
    for forbidden in ["\x1b]52", "\x1bP", "\x1b_", "\u{9d}", "\u{9b}", "\x1b[>4", "\x1b[38:2::", "\t"] {
        assert!(!raw_pty.contains(forbidden), "raw PTY output leaked {forbidden:?} in:\n{raw_pty:?}");
    }
}

#[tokio::test]
async fn busy_retry_notice_clears_immediately_on_esc_ctrl_c_and_stopped_and_notice_gen_bumps() {
    let tty = Tty::new(90, 24);
    tty.ui.set_status("Running");
    tty.ui.set_notice(Some("↻ HTTP 429 — Retrying in 4.0s"));
    assert!(tty.frame().contains("↻ HTTP 429"));

    // Esc while Running cancels AND clears lingering retry notice immediately
    tty.key("escape");
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 1);
    assert!(!tty.frame().contains("↻ HTTP 429"), "Esc cancel must clear retry notice immediately");

    // Ctrl+C while Connecting MCP cancels AND clears retry notice immediately
    tty.ui.set_status("Connecting MCP");
    tty.ui.set_notice(Some("↻ MCP handshake retrying..."));
    assert!(tty.frame().contains("↻ MCP handshake"));
    tty.ui.key(None, modified("c", true, false, false));
    assert_eq!(tty.cancelled.load(Ordering::SeqCst), 2);
    assert!(!tty.frame().contains("↻ MCP handshake"), "Ctrl+C cancel must clear retry notice immediately");

    // "stopped" event also clears retry notice and direct notice helpers bump notice_gen
    tty.ui.set_status("Running");
    tty.ui.set_notice(Some("↻ HTTP 503 — retrying"));
    let gen_before = tty.ui.state.lock().unwrap().notice_gen;
    tty.ui.event(&serde_json::json!({ "type": "stopped", "reason": "Interrupted" }));
    let gen_after = tty.ui.state.lock().unwrap().notice_gen;
    assert!(gen_after > gen_before);
    assert!(tty.ui.state.lock().unwrap().status_notice.is_none());
}

#[tokio::test]
async fn ultra_narrow_and_4_row_viewports_never_overflow_safe_cols_or_row_count_while_busy() {
    for mode in [ThemeMode::Dark, ThemeMode::None] {
        for (cols, rows) in [(6usize, 4usize), (8, 4), (4, 4), (36, 4)] {
            let tty = Tty::new(cols, rows);
            tty.ui.set_theme_mode(mode);
            tty.ui.set_status("Running");
            tty.ui.event(&serde_json::json!({
                "type": "tool",
                "id": "t1",
                "name": "bash",
                "state": "start",
                "meta": { "kind": "exec", "summary": "cargo test --locked" }
            }));
            for elapsed_ms in [0i64, 10_500, 134_000] {
                tty.ui.state.lock().unwrap().turn_started_at = Some(now() - elapsed_ms);
                tty.ui.backdate_active_tool_for_test("t1", elapsed_ms);
                tty.frame();
                let safe_cols = cols.max(4).saturating_sub(1).max(3);
                let frame_rows = tty.ui.state.lock().unwrap().last_frame_rows.clone();
                assert_eq!(frame_rows.len(), rows, "cols={cols} rows={rows} mode={mode:?} emitted {} rows", frame_rows.len());
                for (r, line) in frame_rows.iter().enumerate() {
                    let vis = visible_length(line);
                    assert!(vis <= safe_cols, "cols={cols} rows={rows} mode={mode:?} elapsed={elapsed_ms} row {r} vis={vis} > safe_cols={safe_cols}: {line:?}");
                }
            }
            tty.ui.set_notice(Some("↻ HTTP 429 — retrying (1/3) in 2.0s"));
            for overlay in [false, true] {
                tty.ui.set_transcript_overlay(overlay);
                tty.frame();
                let safe_cols = cols.max(4).saturating_sub(1).max(3);
                let frame_rows = tty.ui.state.lock().unwrap().last_frame_rows.clone();
                assert_eq!(frame_rows.len(), rows, "overlay={overlay} cols={cols} rows={rows} emitted {} rows", frame_rows.len());
                for (r, line) in frame_rows.iter().enumerate() {
                    let vis = visible_length(line);
                    assert!(vis <= safe_cols, "overlay={overlay} cols={cols} rows={rows} row {r} vis={vis} > safe_cols={safe_cols}: {line:?}");
                }
            }
        }
    }
}
