//! Composer editing primitives, kept separate from terminal decoding and rendering.
//!
//! Like Codex's textarea/history state machines, movement and deletion respect graphemes,
//! line editing keeps a single kill buffer, and history previews retain the original draft.
//! Search accepts a preview without submitting it; Escape/Ctrl+C restores the draft.

use super::{chars_of, State, Tui};
use indexmap::IndexMap;
use std::sync::Weak;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone)]
pub(super) struct Draft {
    input: Vec<char>,
    cursor: usize,
    images: IndexMap<String, String>,
    pastes: IndexMap<String, String>,
    next_image: usize,
    next_paste: usize,
    reverted: bool,
    vertical_goal_col: Option<usize>,
}

/// A queued prompt owns its attachments; bounded history archives are not durable ownership.
#[derive(Clone, PartialEq)]
pub(super) struct QueuedPrompt {
    pub text: String,
    images: IndexMap<String, String>,
    pastes: IndexMap<String, String>,
}

impl QueuedPrompt {
    pub(super) fn capture(st: &State, text: &str) -> Self {
        let images = st.image_archive.iter().chain(st.attached_images.iter()).filter(|(token, _)| text.contains(token.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        let pastes = st.paste_archive.iter().chain(st.pasted_texts.iter()).filter(|(token, _)| text.contains(token.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        Self { text: text.to_string(), images, pastes }
    }

    pub(super) fn expand(&self) -> String {
        let text = re!(r"\[Image #\d+\]").replace_all(&self.text, |c: &regex::Captures| self.images.get(&c[0]).map(|path| format!("@\"{path}\"")).unwrap_or_else(|| c[0].to_string()));
        let text = re!(r"(\[Pasted text #\d+[^\]]*\]) $").replace(&text, "${1}");
        re!(r"(\[Pasted text #\d+[^\]]*\])( ?)")
            .replace_all(&text, |c: &regex::Captures| match self.pastes.get(&c[1]) {
                Some(content) if !c[2].is_empty() && content.contains('\n') && !content.ends_with('\n') => format!("{content}\n"),
                Some(content) => format!("{content}{}", &c[2]),
                None => c[0].to_string(),
            })
            .into_owned()
    }

    fn restore(self, st: &mut State) {
        st.input = chars_of(&self.text);
        st.cursor = st.input.len();
        st.vertical_goal_col = None;
        st.attached_images = self.images;
        st.pasted_texts = self.pastes;
        st.input_restored_from_revert = false;
        for (token, path) in st.attached_images.clone() {
            if let Some(c) = re!(r"^\[Image #(\d+)").captures(&token) {
                st.next_image_idx = st.next_image_idx.max(c[1].parse::<usize>().unwrap_or(0) + 1);
            }
            Tui::record_image_archive(st, &token, &path);
        }
        for (token, text) in st.pasted_texts.clone() {
            if let Some(c) = re!(r"^\[Pasted text #(\d+)").captures(&token) {
                st.next_paste_idx = st.next_paste_idx.max(c[1].parse::<usize>().unwrap_or(0) + 1);
            }
            Tui::record_paste_archive(st, &token, &text);
        }
        st.paste_burst.clear_after_explicit_paste();
        Tui::clear_paste_burst(st);
    }
}

impl Draft {
    pub(super) fn capture(st: &State) -> Self {
        Self {
            input: st.input.clone(),
            cursor: st.cursor,
            images: st.attached_images.clone(),
            pastes: st.pasted_texts.clone(),
            next_image: st.next_image_idx,
            next_paste: st.next_paste_idx,
            reverted: st.input_restored_from_revert,
            vertical_goal_col: st.vertical_goal_col,
        }
    }

    pub(super) fn restore(self, st: &mut State) {
        st.input = self.input;
        st.cursor = self.cursor;
        st.attached_images = self.images;
        st.pasted_texts = self.pastes;
        st.next_image_idx = self.next_image;
        st.next_paste_idx = self.next_paste;
        st.input_restored_from_revert = self.reverted;
        st.vertical_goal_col = self.vertical_goal_col;
        st.paste_burst.clear_after_explicit_paste();
        Tui::clear_paste_burst(st);
    }
}

#[derive(Clone)]
pub(super) struct HistorySearch {
    pub query: String,
    pub matched: Option<usize>,
    saved: Draft,
}

/// Unsent input stays in memory with its thread, never in the global history file.
pub(super) struct ThreadInput {
    draft: Draft,
    queued: Vec<QueuedPrompt>,
    kill_buffer: String,
    image_archive: IndexMap<String, String>,
    paste_archive: IndexMap<String, String>,
    recent_expanded: Vec<(String, String)>,
    history_index: usize,
    history_draft: Option<Draft>,
    history_search: Option<HistorySearch>,
}

pub(super) struct QuestionSnapshot {
    pub draft: Draft,
    pub label: String,
    pub remember: bool,
    pub masked: bool,
    pub esc_cancels: bool,
    pub esc_hint: String,
    pub history_index: usize,
    pub history_draft: Option<Draft>,
    pub history_search: Option<HistorySearch>,
    pub kill_buffer: String,
    pub image_archive: IndexMap<String, String>,
    pub paste_archive: IndexMap<String, String>,
    pub recent_expanded: Vec<(String, String)>,
}

/// select! can drop a terminal question when a remote approval wins. Restore on drop too.
pub(super) struct QuestionGuard {
    pub tui: Weak<Tui>,
    pub id: u64,
    pub saved: Option<QuestionSnapshot>,
}

impl Drop for QuestionGuard {
    fn drop(&mut self) {
        let Some(tui) = self.tui.upgrade() else { return };
        let mut st = tui.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.pending.as_ref().map(|p| p.id != self.id).unwrap_or(false) {
            return;
        }
        st.pending = None;
        if let Some(saved) = self.saved.take() {
            saved.draft.restore(&mut st);
            st.label = saved.label;
            st.remember = saved.remember;
            st.masked = saved.masked;
            st.esc_cancels = saved.esc_cancels;
            st.esc_hint = saved.esc_hint;
            st.history_index = saved.history_index;
            st.history_draft = saved.history_draft;
            st.history_search = saved.history_search;
            st.kill_buffer = saved.kill_buffer;
            st.image_archive = saved.image_archive;
            st.paste_archive = saved.paste_archive;
            st.recent_expanded = saved.recent_expanded;
        }
        if !std::thread::panicking() {
            tui.schedule(&mut st, 0);
        }
    }
}

pub(super) struct TransientGuard {
    pub tui: Weak<Tui>,
    pub appended: String,
}

impl Drop for TransientGuard {
    fn drop(&mut self) {
        let Some(tui) = self.tui.upgrade() else { return };
        let mut st = tui.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = st.transcript.rfind(&self.appended) {
            st.transcript.replace_range(pos..pos + self.appended.len(), "");
        }
        st.body_cache = None;
        if !std::thread::panicking() {
            tui.schedule(&mut st, 0);
        }
    }
}

pub(super) fn previous_grapheme(input: &[char], cursor: usize) -> usize {
    let text: String = input[..cursor].iter().collect();
    text.graphemes(true).next_back().map(|g| cursor - g.chars().count()).unwrap_or(0)
}

pub(super) fn next_grapheme(input: &[char], cursor: usize) -> usize {
    let text: String = input[cursor..].iter().collect();
    text.graphemes(true).next().map(|g| cursor + g.chars().count()).unwrap_or(input.len())
}

pub(super) fn line_start(st: &State) -> usize {
    st.input[..st.cursor].iter().rposition(|c| *c == '\n').map(|i| i + 1).unwrap_or(0)
}

pub(super) fn line_end(st: &State) -> usize {
    st.input[st.cursor..].iter().position(|c| *c == '\n').map(|i| st.cursor + i).unwrap_or(st.input.len())
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub(super) fn word_left(st: &State) -> usize {
    let mut at = st.cursor;
    while at > 0 && st.input[at - 1].is_whitespace() {
        at -= 1;
    }
    if at > 0 {
        let word = word_char(st.input[at - 1]);
        while at > 0 && !st.input[at - 1].is_whitespace() && word_char(st.input[at - 1]) == word {
            at -= 1;
        }
    }
    at
}

pub(super) fn word_right(st: &State) -> usize {
    let mut at = st.cursor;
    while at < st.input.len() && st.input[at].is_whitespace() {
        at += 1;
    }
    if at < st.input.len() {
        let word = word_char(st.input[at]);
        while at < st.input.len() && !st.input[at].is_whitespace() && word_char(st.input[at]) == word {
            at += 1;
        }
    }
    at
}

/// Never leave a caret or a deletion endpoint inside an atomic paste/image element.
pub(super) fn snap_atomic(st: &State, target: usize, forward: bool) -> usize {
    let text: String = st.input.iter().collect();
    for m in re!(r"\[(?:Image #\d+|Pasted text #\d+[^\]]*)\]").find_iter(&text) {
        let start = text[..m.start()].chars().count();
        let end = start + m.as_str().chars().count();
        if target > start && target < end {
            return if forward { end } else { start };
        }
    }
    target
}

impl Tui {
    pub(super) fn move_vertical(st: &mut State, older: bool, cols: usize) -> bool {
        let text: String = st.input.iter().collect();
        let mut positions = Vec::new();
        let (mut row, mut col, mut offset) = (0usize, 0usize, 0usize);
        for grapheme in text.graphemes(true) {
            let width = super::visible_length(if st.masked { "•" } else { grapheme }).min(cols);
            if grapheme != "\n" && col + width > cols {
                row += 1;
                col = 0;
            }
            positions.push((offset, row, col));
            if grapheme == "\n" {
                row += 1;
                col = 0;
            } else {
                col += width;
            }
            offset += grapheme.chars().count();
        }
        if col >= cols {
            row += 1;
            col = 0;
        }
        positions.push((offset, row, col));
        let Some((_, current_row, current_col)) = positions.iter().find(|(offset, _, _)| *offset == st.cursor).copied() else { return false };
        let target_row = if older { current_row.checked_sub(1) } else { Some(current_row + 1) };
        let Some(target_row) = target_row else { return false };
        let goal_col = *st.vertical_goal_col.get_or_insert(current_col);
        let target = positions.iter().filter(|(_, row, _)| *row == target_row).min_by_key(|(_, _, col)| col.abs_diff(goal_col));
        if let Some((offset, _, _)) = target {
            st.cursor = snap_atomic(st, *offset, !older);
            true
        } else {
            false
        }
    }

    /// Recover only ordinary composer input after a fatal session error, never a modal secret.
    /// Callers display this after restoring terminal modes; no draft is persisted to disk.
    pub fn unsent_draft_for_recovery(&self) -> Option<String> {
        let mut st = self.state.lock().unwrap();
        if st.masked || (st.pending.is_some() && !st.remember) {
            return None;
        }
        match st.paste_burst.flush_before_modified_input() {
            super::paste_burst::FlushResult::Typed(ch) => Self::insert_chars(&mut st, &[ch]),
            super::paste_burst::FlushResult::Paste(text) if st.paste_burst.take_typed_burst_flag() => {
                Self::insert_chars(&mut st, &super::chars_of(&crate::term::terminal_text(&text).replace('\n', " ")))
            }
            super::paste_burst::FlushResult::Paste(text) => self.handle_paste(&mut st, &text, true),
            super::paste_burst::FlushResult::None => {}
        }
        let text = QueuedPrompt::capture(&st, &st.input.iter().collect::<String>()).expand();
        (!text.is_empty()).then(|| crate::term::terminal_text(&text))
    }

    pub fn switch_input_session(&self, session_id: &str) {
        let mut st = self.state.lock().unwrap();
        if st.input_session.as_deref() == Some(session_id) || (st.pending.is_some() && !st.remember) {
            return;
        }
        if st.input_session.is_none() {
            // Input may arrive while startup is still building the harness.
            st.input_session = Some(session_id.to_string());
            return;
        }
        match st.paste_burst.flush_before_modified_input() {
            super::paste_burst::FlushResult::Typed(ch) => Self::insert_chars(&mut st, &[ch]),
            super::paste_burst::FlushResult::Paste(text) if st.paste_burst.take_typed_burst_flag() => {
                Self::insert_chars(&mut st, &super::chars_of(&crate::term::terminal_text(&text).replace('\n', " ")))
            }
            super::paste_burst::FlushResult::Paste(text) => self.handle_paste(&mut st, &text, true),
            super::paste_burst::FlushResult::None => {}
        }
        if let Some(previous) = st.input_session.replace(session_id.to_string()) {
            let snapshot = ThreadInput {
                draft: Draft::capture(&st),
                queued: std::mem::take(&mut st.queued_prompts),
                kill_buffer: std::mem::take(&mut st.kill_buffer),
                image_archive: std::mem::take(&mut st.image_archive),
                paste_archive: std::mem::take(&mut st.paste_archive),
                recent_expanded: std::mem::take(&mut st.recent_expanded),
                history_index: st.history_index,
                history_draft: st.history_draft.take(),
                history_search: st.history_search.take(),
            };
            st.thread_inputs.insert(previous, snapshot);
        }
        Self::clear_composer(&mut st);
        st.queued_prompts.clear();
        st.kill_buffer.clear();
        st.image_archive.clear();
        st.paste_archive.clear();
        st.recent_expanded.clear();
        st.next_image_idx = 1;
        st.next_paste_idx = 1;
        st.history_index = st.history.len();
        st.history_draft = None;
        st.history_search = None;
        if let Some(saved) = st.thread_inputs.shift_remove(session_id) {
            saved.draft.restore(&mut st);
            st.queued_prompts = saved.queued;
            st.kill_buffer = saved.kill_buffer;
            st.image_archive = saved.image_archive;
            st.paste_archive = saved.paste_archive;
            st.recent_expanded = saved.recent_expanded;
            st.history_index = saved.history_index.min(st.history.len());
            st.history_draft = saved.history_draft;
            st.history_search = saved.history_search;
        }
        st.dismissed_mention = None;
        st.dismissed_slash = None;
        st.quit_armed = None;
        st.shortcuts_open = false;
        self.schedule(&mut st, 0);
    }

    /// Codex restores queued input after interrupt, never silently runs the next prompt.
    /// Rebase attachment tokens while merging independent drafts with colliding IDs.
    pub fn restore_queued_after_interrupt(&self) {
        let mut st = self.state.lock().unwrap();
        if st.queued_prompts.is_empty() || (st.pending.is_some() && !st.remember) {
            return;
        }
        match st.paste_burst.flush_before_modified_input() {
            super::paste_burst::FlushResult::Typed(ch) => Self::insert_chars(&mut st, &[ch]),
            super::paste_burst::FlushResult::Paste(text) if st.paste_burst.take_typed_burst_flag() => {
                Self::insert_chars(&mut st, &super::chars_of(&crate::term::terminal_text(&text).replace('\n', " ")))
            }
            super::paste_burst::FlushResult::Paste(text) => self.handle_paste(&mut st, &text, true),
            super::paste_burst::FlushResult::None => {}
        }
        let current = QueuedPrompt::capture(&st, &st.input.iter().collect::<String>());
        let mut prompts = std::mem::take(&mut st.queued_prompts);
        if !current.text.is_empty() {
            prompts.push(current);
        }
        let mut combined = QueuedPrompt { text: String::new(), images: IndexMap::new(), pastes: IndexMap::new() };
        for prompt in prompts {
            let text = re!(r"\[(?:Image #\d+|Pasted text #\d+[^\]]*)\]")
                .replace_all(&prompt.text, |c: &regex::Captures| {
                    if let Some(path) = prompt.images.get(&c[0]) {
                        let token = format!("[Image #{}]", st.next_image_idx);
                        st.next_image_idx += 1;
                        combined.images.insert(token.clone(), path.clone());
                        token
                    } else if let Some(content) = prompt.pastes.get(&c[0]) {
                        let suffix = re!(r"^\[Pasted text #\d+").replace(&c[0], "");
                        let token = format!("[Pasted text #{}{suffix}", st.next_paste_idx);
                        st.next_paste_idx += 1;
                        combined.pastes.insert(token.clone(), content.clone());
                        token
                    } else {
                        c[0].to_string()
                    }
                })
                .into_owned();
            if !combined.text.is_empty() {
                combined.text.push('\n');
            }
            combined.text.push_str(text.trim_end());
        }
        combined.restore(&mut st);
        st.history_draft = None;
        st.history_search = None;
        st.history_index = st.history.len();
        st.set_raw_notice("Queued messages restored — edit and press Enter to send");
        self.schedule(&mut st, 0);
    }

    pub(super) fn edit_latest_queued(&self, st: &mut State) {
        let Some(prompt) = st.queued_prompts.pop() else { return };
        prompt.restore(st);
        st.history_index = st.history.len();
        st.history_draft = None;
        st.history_search = None;
        self.schedule(st, 0);
    }

    /// The same shortcut must be pressed twice inside Codex's one-second quit window.
    pub(super) fn quit_shortcut(&self, st: &mut State, chord: &str, actions: &mut Vec<super::Action>) -> bool {
        let now = std::time::Instant::now();
        if st.quit_armed.as_ref().map(|(key, until)| key == chord && now < *until).unwrap_or(false) {
            st.quit_armed = None;
            actions.push(super::Action::Quit);
            return true;
        }
        st.quit_armed = Some((chord.into(), now + std::time::Duration::from_secs(1)));
        self.schedule(st, 0);
        let me = self.me.clone();
        self.handle.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if let Some(tui) = me.upgrade() {
                let mut st = tui.state.lock().unwrap();
                if st.quit_armed.as_ref().map(|(_, until)| std::time::Instant::now() >= *until).unwrap_or(false) {
                    st.quit_armed = None;
                    tui.schedule(&mut st, 0);
                }
            }
        });
        false
    }

    pub(super) fn insert_newline(st: &mut State) {
        // Auto-added separator after an element is not an extra logical line.
        if st.cursor > 0 && st.input[st.cursor - 1] == ' ' && Self::atomic_before(st).is_some() {
            st.cursor -= 1;
            st.input.remove(st.cursor);
        }
        Self::insert_chars(st, &['\n']);
        st.input_restored_from_revert = false;
    }

    pub(super) fn kill_range(st: &mut State, start: usize, end: usize) {
        let start = snap_atomic(st, start, false);
        let end = snap_atomic(st, end, true);
        if start >= end {
            return;
        }
        st.kill_buffer = st.input.drain(start..end).collect();
        st.cursor = start;
        st.input_restored_from_revert = false;
        // Keep archive ownership: yank/history can rehydrate these exact elements.
        let text: String = st.input.iter().collect();
        st.attached_images.retain(|token, _| text.contains(token));
        st.pasted_texts.retain(|token, _| text.contains(token));
    }

    pub(super) fn remember_prompt(st: &mut State, raw: &str) {
        let raw = raw.trim_end();
        let payload = QueuedPrompt::capture(st, raw);
        st.history_payloads.resize(st.history.len(), None);
        if st.history.last().map(String::as_str) != Some(raw) || st.history_payloads.last().and_then(Option::as_ref) != Some(&payload) {
            st.history.push(raw.to_string());
            st.history_payloads.push(Some(payload));
        }
        if st.history.len() > crate::history::MAX_ENTRIES {
            let remove = st.history.len() - crate::history::MAX_ENTRIES;
            st.history.drain(..remove);
            st.history_payloads.drain(..remove);
        }
        st.history_index = st.history.len();
        st.history_draft = None;
        st.history_search = None;
    }

    pub(super) fn navigate_history(&self, st: &mut State, older: bool) {
        if st.history.is_empty() || (!older && st.history_index == st.history.len()) {
            return;
        }
        let text = st.input.iter().collect::<String>();
        if !text.is_empty() && (st.history.get(st.history_index) != Some(&text) || (st.cursor != 0 && st.cursor != st.input.len())) {
            return;
        }
        st.history_index = if older { st.history_index.saturating_sub(1) } else { (st.history_index + 1).min(st.history.len()) };
        if st.history_index == st.history.len() {
            Self::clear_composer(st);
            st.history_draft = None;
        } else {
            self.restore_history_entry(st, st.history_index);
        }
        st.scroll = 0;
    }

    fn restore_history_entry(&self, st: &mut State, index: usize) {
        if let Some(payload) = st.history_payloads.get(index).and_then(Option::as_ref).cloned() {
            payload.restore(st);
        } else {
            st.input = chars_of(&st.history[index]);
            st.cursor = st.input.len();
            st.vertical_goal_col = None;
            st.input_restored_from_revert = false;
            self.rehydrate_placeholders(st);
        }
    }

    pub(super) fn search_history(&self, st: &mut State, older: bool, restart: bool) {
        let Some(search) = st.history_search.as_mut() else { return };
        let query = search.query.to_lowercase();
        let previous = search.matched.map(|i| st.history[i].clone());
        let start = if restart { st.history.len() } else { search.matched.unwrap_or(st.history.len()) };
        let matched = if older {
            (0..start).rev().find(|i| st.history[*i].to_lowercase().contains(&query) && (restart || Some(&st.history[*i]) != previous.as_ref()))
        } else {
            (start.saturating_add(1)..st.history.len()).find(|i| st.history[*i].to_lowercase().contains(&query) && Some(&st.history[*i]) != previous.as_ref())
        };
        if matched.is_some() || restart {
            search.matched = matched;
        }
        if let Some(index) = search.matched {
            self.restore_history_entry(st, index);
        }
        st.scroll = 0;
    }

    pub(super) fn begin_history_search(&self, st: &mut State) {
        st.history_search = Some(HistorySearch { query: String::new(), matched: None, saved: Draft::capture(st) });
        self.search_history(st, true, true);
    }

    pub(super) fn cancel_history_search(st: &mut State) {
        if let Some(search) = st.history_search.take() {
            search.saved.restore(st);
        }
    }
}
