//! Cursor-local file/skill completion. Popup selection edits a token, never submits a prompt.
//! Escape dismisses the current query until that token changes; surrounding draft text survives.

use super::{chars_of, State, Tui};
use std::ops::Range;

/// Quoted file mentions are one editable token, even with the caret after a space.
fn token_at_cursor(input: &[char], cursor: usize) -> Option<Range<usize>> {
    let mut start = 0;
    while start < input.len() {
        if input[start].is_whitespace() {
            start += 1;
            continue;
        }
        let mut end = start;
        let mut quoted = false;
        while end < input.len() {
            if input[end] == '"' {
                quoted = !quoted;
            } else if input[end].is_whitespace() && !quoted {
                break;
            }
            end += 1;
        }
        if cursor > start && cursor <= end {
            return Some(start..end);
        }
        start = end;
    }
    None
}

pub(super) struct MentionPopup {
    pub range: Range<usize>,
    pub sigil: char,
    pub signature: String,
    pub items: Vec<(String, String)>,
    pub selected: usize,
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if candidate.starts_with(&query) {
        return Some(0);
    }
    if candidate.rsplit(['/', '\\']).next().unwrap_or("").starts_with(&query) {
        return Some(1);
    }
    if let Some(at) = candidate.find(&query) {
        return Some(2 + at);
    }
    let mut chars = candidate.chars();
    let mut gaps = 0;
    for ch in query.chars() {
        loop {
            match chars.next() {
                Some(found) if found == ch => break,
                Some(_) => gaps += 1,
                None => return None,
            }
        }
    }
    Some(100 + gaps)
}

impl Tui {
    pub fn set_workspace_skills(&self, skills: Vec<(String, String)>) {
        let mut st = self.state.lock().unwrap();
        st.workspace_skills = skills;
        self.schedule(&mut st, 0);
    }

    pub(super) fn mention_popup(st: &mut State) -> Option<MentionPopup> {
        if st.masked || st.history_search.is_some() || st.picker.is_some() || (st.pending.is_some() && !st.remember) {
            return None;
        }
        let cursor = st.cursor.min(st.input.len());
        let range = token_at_cursor(&st.input, cursor)?;
        let start = range.start;
        let sigil = *st.input.get(start)?;
        if !matches!(sigil, '@' | '$') || cursor <= start {
            return None;
        }
        let end = range.end;
        let token: String = st.input[start..end].iter().collect();
        let query: String = st.input[start + 1..cursor].iter().collect();
        let signature = format!("{start}:{token}:{query}");
        if st.dismissed_mention.as_ref() == Some(&signature) {
            return None;
        }
        st.dismissed_mention = None;
        if st.mention_for != signature {
            st.mention_for = signature.clone();
            st.popup_index = 0;
        }
        let candidates: Vec<(String, String)> = if sigil == '@' { st.workspace_files.iter().map(|p| (p.clone(), String::new())).collect() } else { st.workspace_skills.clone() };
        let mut matches: Vec<_> = candidates.into_iter().filter_map(|(name, description)| fuzzy_score(query.trim_matches('"'), &name).map(|score| (score, name, description))).collect();
        matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let items: Vec<_> = matches.into_iter().take(100).map(|(_, name, desc)| (name, desc)).collect();
        if items.is_empty() {
            return None;
        }
        let selected = st.popup_index.min(items.len() - 1);
        Some(MentionPopup { range: start..end, sigil, signature, items, selected })
    }

    pub(super) fn accept_mention(&self, st: &mut State, popup: MentionPopup) {
        let (name, _) = &popup.items[popup.selected];
        let name = if popup.sigil == '@' && name.chars().any(char::is_whitespace) { format!("\"{name}\"") } else { name.clone() };
        let replacement = chars_of(&format!("{}{name}", popup.sigil));
        let end = popup.range.start + replacement.len();
        st.input.splice(popup.range, replacement);
        st.cursor = end;
        if st.input.get(end) == Some(&' ') {
            st.cursor += 1;
        } else {
            Self::insert_chars(st, &[' ']);
        }
        st.input_restored_from_revert = false;
        st.mention_for.clear();
        st.popup_index = 0;
        // The accepted value to the left of a separator must not immediately reopen.
        st.dismissed_mention = Some(popup.signature);
        self.schedule(st, 0);
    }
}
