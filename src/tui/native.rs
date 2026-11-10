//! Native scrollback mode: commit stable history above a relative, bottom-anchored viewport.
//! No absolute home/clear-screen or partial scrolling regions: full-screen newline scrolling
//! also works in Windows Terminal, which does not retain partial DECSTBM rows in scrollback.

#[derive(Default)]
pub(super) struct NativeViewport {
    committed_source: String,
    pending: Vec<String>,
    previous: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    geometry_changed: bool,
    resize_up: Option<usize>,
    recovery_newlines: usize,
    modal: bool,
    modal_rows: usize,
}

impl NativeViewport {
    pub(super) fn commit_prefix(&mut self, lines: &mut Vec<String>, source_ends: &[usize], source: &str, tail: usize, protected: bool, busy: bool) {
        if protected {
            return;
        }
        debug_assert_eq!(lines.len(), source_ends.len());
        if !source.starts_with(&self.committed_source) {
            // Rewind or replacement: retain the common source watermark rather than
            // replaying history that is already in terminal scrollback. Explicit thread
            // switches/clear reset it. Width/style changes do not change this watermark.
            let common = source.chars().zip(self.committed_source.chars()).take_while(|(a, b)| a == b).map(|(c, _)| c.len_utf8()).sum();
            self.committed_source.truncate(common);
        }
        let committed = source_ends.partition_point(|end| *end <= self.committed_source.len());
        let mut commit_to = committed;
        if !busy {
            let target = lines.len().saturating_sub(tail);
            // Never commit half of a wrapped source line. Its live rows can then
            // reflow independently of immutable terminal scrollback after a resize.
            while commit_to < target {
                let end = source_ends[commit_to];
                let group_end = source_ends.partition_point(|offset| *offset <= end);
                if group_end > target || !source[..end].ends_with('\n') {
                    break;
                }
                commit_to = group_end;
            }
        }
        self.pending.extend_from_slice(&lines[committed..commit_to]);
        if commit_to > committed {
            self.committed_source = source[..source_ends[commit_to - 1]].to_string();
        }
        lines.drain(..commit_to);
    }

    pub(super) fn paint(&mut self, lines: &[String], cursor: &str) -> String {
        if !self.geometry_changed && self.pending.is_empty() && self.previous == lines && cursor.is_empty() {
            return String::new();
        }
        self.geometry_changed = false;
        let mut output = String::from("\x1b[?2026h\x1b[?25l\r");
        let up = self.resize_up.take().unwrap_or(self.cursor_row);
        if self.recovery_newlines > 0 {
            // Without a reliable origin, do not erase anything. Push the old frame
            // into scrollback and establish a fresh origin below it.
            output.push_str(&"\r\n".repeat(std::mem::take(&mut self.recovery_newlines)));
        } else {
            if up > 0 {
                output.push_str(&format!("\x1b[{up}A"));
            }
            output.push_str("\x1b[J");
        }
        for line in self.pending.drain(..) {
            output.push_str(&line);
            output.push_str("\x1b[0m\r\n");
        }
        output.push_str(&lines.iter().map(|line| format!("{line}\x1b[K")).collect::<Vec<_>>().join("\r\n"));
        let last_row = lines.len().saturating_sub(1);
        let desired = re!(r"\x1b\[(\d+);(\d+)H").captures(cursor).map(|c| (c[1].parse::<usize>().unwrap_or(1).saturating_sub(1).min(last_row), c[2].parse::<usize>().unwrap_or(1).max(1)));
        self.cursor_row = last_row;
        self.cursor_col = lines.last().map(|line| super::text::visible_length(line)).unwrap_or(0);
        if let Some((row, col)) = desired {
            if last_row > row {
                output.push_str(&format!("\x1b[{}A", last_row - row));
            }
            output.push('\r');
            if col > 1 {
                output.push_str(&format!("\x1b[{}C", col - 1));
            }
            output.push_str("\x1b[?25h");
            self.cursor_row = row;
            self.cursor_col = col - 1;
        }
        output.push_str("\x1b[?2026l");
        self.previous = lines.to_vec();
        output
    }

    /// --no-alt-screen also applies to overlays. Full-height modals move their
    /// origin to row zero; restore a compact viewport against the bottom on exit.
    pub(super) fn transition_modal(&mut self, modal: bool, rows: usize, next_rows: usize) -> String {
        if modal == self.modal {
            if modal {
                self.modal_rows = rows;
            }
            return String::new();
        }
        self.modal = modal;
        self.geometry_changed = true;
        if modal {
            self.modal_rows = rows;
            if self.recovery_newlines > 0 {
                return String::new();
            }
            // Grow using full-screen newlines before painting; never rely on
            // overflowing modal content to establish its row-zero origin.
            let below = self.previous.len().saturating_sub(self.cursor_row + 1);
            let mut output = String::from("\x1b[?2026h\x1b[?25l\r");
            if below > 0 {
                output.push_str(&format!("\x1b[{below}B"));
            }
            output.push_str(&"\r\n".repeat(rows.saturating_sub(self.previous.len())));
            output.push_str(&format!("\x1b[{}A\x1b[J", rows.saturating_sub(1).max(1)));
            self.cursor_row = 0;
            self.cursor_col = 0;
            self.previous.clear();
            self.resize_up = None;
            output.push_str("\x1b[?2026l");
            return output;
        }
        if self.recovery_newlines > 0 {
            // A concurrent resize made the origin uncertain. Keep the non-erasing
            // recovery selected by resize(), rather than clearing from a guess.
            return String::new();
        }
        let up = self.resize_up.take().unwrap_or(self.cursor_row).min(rows.saturating_sub(1));
        let mut output = String::from("\x1b[?2026h\x1b[?25l\r");
        if up > 0 {
            output.push_str(&format!("\x1b[{up}A"));
        }
        output.push_str("\x1b[J");
        // Flush newly finalized history at the owned top before anchoring the
        // compact frame. Inserting it after CUD would scroll that anchor away.
        let had_pending = !self.pending.is_empty();
        for line in self.pending.drain(..) {
            output.push_str(&line);
            output.push_str("\x1b[0m\r\n");
        }
        if had_pending {
            // A watermark means these rows must actually reach scrollback, not
            // remain above the composer only until the next modal erases them.
            output.push_str(&"\r\n".repeat(rows.saturating_sub(1)));
        }
        if next_rows < rows || had_pending {
            output.push_str(&format!("\x1b[{}A", rows.saturating_sub(1).max(1)));
        }
        let down = self.modal_rows.min(rows).saturating_sub(next_rows.min(rows));
        if down > 0 {
            output.push_str(&format!("\x1b[{down}B"));
        }
        output.push_str("\x1b[?2026l");
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.previous.clear();
        self.recovery_newlines = 0;
        output
    }

    pub(super) fn reset(&mut self) {
        self.committed_source.clear();
        self.pending.clear();
    }

    /// Rebase only when hard-row reflow is unambiguous. Terminals disagree on emoji
    /// widths/reflow; a missing probe or ambiguous row must never erase shell output.
    pub(super) fn resize(&mut self, old_columns: usize, columns: usize, rows: usize, cursor_y: Option<usize>) {
        let columns = columns.max(1);
        let reliable_rows = self.previous.iter().all(|line| {
            let plain = crate::term::strip_vt(line);
            plain.is_ascii() && plain.len() < old_columns && plain.bytes().all(|b| b.is_ascii_graphic() || b == b' ')
        });
        if let Some(y) = cursor_y.filter(|y| *y < rows && (old_columns == columns || reliable_rows)) {
            let reflowed_row = if old_columns == columns {
                self.cursor_row
            } else {
                let before_cursor: usize = self.previous.iter().take(self.cursor_row).map(|line| reflow_position(line, usize::MAX, columns).0 + 1).sum();
                let cursor_line = self.previous.get(self.cursor_row).map(String::as_str).unwrap_or_default();
                before_cursor + reflow_position(cursor_line, self.cursor_col, columns).0
            };
            self.resize_up = Some(reflowed_row.min(y));
        } else {
            // First reach the bottom even from row zero, then scroll a complete
            // screen so no stale rows remain below the uncertain old caret.
            self.recovery_newlines = rows.max(1).saturating_mul(2).saturating_sub(1);
        }
        self.geometry_changed = true;
    }

    pub(super) fn trim_source_prefix(&mut self, bytes: usize) {
        let remove = bytes.min(self.committed_source.len());
        self.committed_source.drain(..remove);
    }

    pub(super) fn finish(&mut self) -> String {
        let below = self.previous.len().saturating_sub(self.cursor_row + 1);
        self.cursor_row = self.previous.len().saturating_sub(1);
        if below > 0 {
            format!("\x1b[{below}B\r\n")
        } else {
            "\r\n".into()
        }
    }
}

/// Hard rows may soft-wrap after narrowing. Count graphemes, not width/columns:
/// a two-cell glyph cannot occupy the last single cell of a physical row.
fn reflow_position(line: &str, until_col: usize, columns: usize) -> (usize, usize) {
    use unicode_segmentation::UnicodeSegmentation;
    let plain = crate::term::strip_vt(line);
    let (mut row, mut col, mut consumed) = (0, 0, 0);
    for grapheme in plain.graphemes(true) {
        if consumed >= until_col {
            break;
        }
        let width = super::text::visible_length(grapheme).min(columns);
        if col + width > columns {
            row += 1;
            col = 0;
        }
        col += width;
        consumed += width;
    }
    (row, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Model physical rows and full-screen newline scrolling, not just escape
    // substrings: ED can otherwise erase a supposedly committed history row.
    struct Terminal {
        screen: Vec<String>,
        scrollback: Vec<String>,
        row: usize,
        col: usize,
    }

    impl Terminal {
        fn new(rows: usize) -> Self {
            Self { screen: vec![String::new(); rows], scrollback: vec![], row: 0, col: 0 }
        }

        fn apply(&mut self, output: &str) {
            let mut chars = output.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '\x1b' => {
                        assert_eq!(chars.next(), Some('['));
                        let mut parameter = String::new();
                        let command = loop {
                            let ch = chars.next().expect("complete CSI");
                            if ('@'..='~').contains(&ch) {
                                break ch;
                            }
                            parameter.push(ch);
                        };
                        let count = parameter.parse::<usize>().unwrap_or(1).max(1);
                        match command {
                            'A' => self.row = self.row.saturating_sub(count),
                            'B' => self.row = (self.row + count).min(self.screen.len() - 1),
                            'C' => self.col += count,
                            'J' => {
                                self.screen[self.row].truncate(self.col);
                                for line in &mut self.screen[self.row + 1..] {
                                    line.clear();
                                }
                            }
                            'K' => self.screen[self.row].truncate(self.col),
                            'm' | 'h' | 'l' => {}
                            _ => panic!("unmodeled CSI: {parameter}{command}"),
                        }
                    }
                    '\r' => self.col = 0,
                    '\n' => {
                        if self.row + 1 == self.screen.len() {
                            self.scrollback.push(self.screen.remove(0));
                            self.screen.push(String::new());
                        } else {
                            self.row += 1;
                        }
                    }
                    ch => {
                        assert!(ch.is_ascii(), "model uses single-column ASCII fixtures");
                        let line = &mut self.screen[self.row];
                        if line.len() < self.col {
                            line.push_str(&" ".repeat(self.col - line.len()));
                        }
                        if self.col < line.len() {
                            line.replace_range(self.col..self.col + 1, &ch.to_string());
                        } else {
                            line.push(ch);
                        }
                        self.col += 1;
                    }
                }
            }
        }
    }

    #[test]
    fn modal_pending_history_reaches_scrollback_before_reopening_overlay() {
        for pending_count in [0, 1, 5, 30] {
            for next_rows in [6, 24] {
                for caret in [false, true] {
                    let mut terminal = Terminal::new(24);
                    terminal.apply("shell output\r\n");
                    let mut viewport = NativeViewport::default();
                    terminal.apply(&viewport.paint(&vec!["live".into(); 4], ""));
                    terminal.apply(&viewport.transition_modal(true, 24, 24));
                    terminal.apply(&viewport.paint(&vec!["modal".into(); 24], if caret { "\x1b[10;2H" } else { "" }));
                    let history = (0..pending_count).map(|i| format!("finalized {i}")).collect::<Vec<_>>();
                    viewport.pending.clone_from(&history);
                    terminal.apply(&viewport.transition_modal(false, 24, next_rows));
                    terminal.apply(&viewport.paint(&vec!["compact".into(); next_rows], if caret { "\x1b[3;2H" } else { "" }));
                    assert_eq!(terminal.row, if caret { 24 - next_rows + 2 } else { 23 }, "compact frame must remain bottom anchored");
                    for line in &history {
                        assert_eq!(terminal.scrollback.iter().filter(|row| *row == line).count(), 1, "history must be committed, not merely visible above the frame");
                    }
                    terminal.apply(&viewport.transition_modal(true, 24, 24));
                    terminal.apply(&viewport.paint(&vec!["reopened modal".into(); 24], ""));
                    for line in &history {
                        assert_eq!(terminal.scrollback.iter().filter(|row| *row == line).count(), 1);
                    }
                    assert!(terminal.scrollback.iter().any(|row| row == "shell output"));
                }
            }
        }
    }

    #[test]
    fn stable_prefix_is_written_once_and_cursor_moves_are_relative() {
        let mut viewport = NativeViewport::default();
        let all = (0..15).map(|n| format!("line {n}")).collect::<Vec<_>>();
        let source = format!("{}\n", all.join("\n"));
        let ends = source.match_indices('\n').map(|(i, _)| i + 1).collect::<Vec<_>>();
        let mut first = all.clone();
        viewport.commit_prefix(&mut first, &ends, &source, 3, false, false);
        assert_eq!(first, all[12..]);
        let painted = viewport.paint(&first, "\x1b[2;3H\x1b[?25h");
        assert!(painted.contains("line 0\x1b[0m\r\n"));
        assert!(!painted.contains("\x1b[H") && !painted.contains("\x1b[2J") && !painted.contains("\x1b[2;3H"));
        let mut second = all.clone();
        viewport.commit_prefix(&mut second, &ends, &source, 3, false, false);
        assert_eq!(second, first);
        let painted = viewport.paint(&second, "\x1b[2;3H\x1b[?25h");
        assert!(!painted.contains("line 0\x1b[0m"));
        let mut overlay = all.clone();
        viewport.commit_prefix(&mut overlay, &ends, &source, 3, true, false);
        assert_eq!(overlay, all, "overlay must include already committed history");
        let mut streaming = all.clone();
        streaming.push("new streaming line".into());
        let streaming_source = format!("{source}new streaming line");
        let mut streaming_ends = ends.clone();
        streaming_ends.push(streaming_source.len());
        viewport.commit_prefix(&mut streaming, &streaming_ends, &streaming_source, 3, false, true);
        assert_eq!(streaming, vec!["line 12", "line 13", "line 14", "new streaming line"]);
    }

    #[test]
    fn resize_and_style_changes_do_not_replay_committed_source() {
        let mut viewport = NativeViewport::default();
        let source = "old long source line\nlive source line\n";
        let old_end = source.find('\n').unwrap() + 1;
        let mut wide = vec!["old wide".into(), "live wide".into()];
        viewport.commit_prefix(&mut wide, &[old_end, source.len()], source, 1, false, false);
        assert!(viewport.paint(&wide, "").contains("old wide\x1b[0m\r\n"));

        let mut narrow = vec!["old narrow 1".into(), "old narrow 2".into(), "live narrow 1".into(), "live narrow 2".into()];
        viewport.commit_prefix(&mut narrow, &[old_end, old_end, source.len(), source.len()], source, 1, false, false);
        assert_eq!(narrow, vec!["live narrow 1", "live narrow 2"], "a wrapped source line stays entirely live");
        let painted = viewport.paint(&narrow, "");
        assert!(!painted.contains("old narrow"));
        assert!(viewport.pending.is_empty());

        let mut restyled = vec!["old restyled".into(), "live restyled".into()];
        viewport.commit_prefix(&mut restyled, &[old_end, source.len()], source, 1, false, true);
        assert_eq!(restyled, vec!["live restyled"]);
        let rewind = "old long source line\n";
        let mut rewound = vec!["old rewound".into()];
        viewport.commit_prefix(&mut rewound, &[rewind.len()], rewind, 1, false, false);
        assert!(rewound.is_empty(), "rewind must not replay an unchanged committed prefix");
    }

    #[test]
    fn unterminated_source_line_is_never_committed() {
        let mut viewport = NativeViewport::default();
        let mut lines = vec!["unfinished wrapped 1".into(), "unfinished wrapped 2".into()];
        viewport.commit_prefix(&mut lines, &[10, 10], "unfinished", 0, false, false);
        assert_eq!(lines.len(), 2);
        assert!(viewport.committed_source.is_empty());
    }

    #[test]
    fn front_trim_rebases_watermark_without_replaying_retained_history() {
        let mut viewport = NativeViewport::default();
        let source = "first\nsecond\nlive\n";
        let ends = [6, 13, 18];
        let mut lines = vec!["first".into(), "second".into(), "live".into()];
        viewport.commit_prefix(&mut lines, &ends, source, 1, false, false);
        viewport.paint(&lines, "");
        viewport.trim_source_prefix(6);
        let source = &source[6..];
        let mut lines = vec!["second restyled".into(), "live".into()];
        viewport.commit_prefix(&mut lines, &[7, 12], source, 1, false, false);
        assert_eq!(lines, vec!["live"]);
        assert!(viewport.pending.is_empty());
    }

    #[test]
    fn resize_rebases_cursor_for_wrapped_rows_and_preserves_shell_above_viewport() {
        let mut viewport = NativeViewport::default();
        viewport.paint(&["123456789".into(), "abcdefghi".into(), "draft".into(), "footer".into()], "\x1b[3;4H");
        viewport.resize(20, 5, 12, Some(9));
        assert_eq!(viewport.resize_up, Some(4), "two old rows now occupy four physical rows");
        let frame = viewport.paint(&["new live".into(), "draft".into(), "footer".into()], "\x1b[2;4H");
        assert!(frame.starts_with("\x1b[?2026h\x1b[?25l\r\x1b[4A\x1b[J"));
        assert!(!frame.contains("\x1b[H") && !frame.contains("\x1b[2J") && !frame.contains("\x1b[3J"));
        assert_eq!(viewport.cursor_row, 1);
    }

    #[test]
    fn resize_clamps_to_measured_cursor_when_old_origin_is_offscreen() {
        let mut viewport = NativeViewport::default();
        viewport.paint(&["long source row".into(), "second row".into(), "draft".into()], "\x1b[3;4H");
        viewport.resize(20, 4, 4, Some(2));
        assert_eq!(viewport.cursor_row, 2);
        let frame = viewport.paint(&["new".into(), "draft".into(), "end".into()], "");
        assert!(frame.starts_with("\x1b[?2026h\x1b[?25l\r\x1b[2A\x1b[J"));
        viewport.resize(4, 80, 4, None);
        let recovered = viewport.paint(&["new".into(), "draft".into()], "");
        assert!(!recovered.contains("\x1b[J"), "failed position probes must never erase shell output");
        assert!(recovered.starts_with("\x1b[?2026h\x1b[?25l\r\r\n\r\n\r\n\r\n"));
    }

    #[test]
    fn resize_repaints_identical_content_and_handles_wide_grapheme_wrap() {
        assert_eq!(reflow_position("ab界界界", usize::MAX, 3), (3, 2));
        assert_eq!(reflow_position("abc", usize::MAX, 3), (0, 3), "exact-width row stays in delayed-wrap state");
        let mut viewport = NativeViewport::default();
        let lines = vec!["same".into(), "frame".into()];
        viewport.paint(&lines, "");
        assert!(viewport.paint(&lines, "").is_empty());
        viewport.resize(80, 40, 12, Some(1));
        assert!(!viewport.paint(&lines, "").is_empty());
    }

    #[test]
    fn modal_entry_grows_before_paint_and_exit_commits_before_bottom_anchor() {
        let mut viewport = NativeViewport::default();
        viewport.paint(&vec!["live".into(); 4], "\x1b[3;2H");
        let enter = viewport.transition_modal(true, 24, 24);
        assert!(enter.contains(&"\r\n".repeat(20)));
        assert!(!enter.contains("\x1b[?1049") && !enter.contains("\x1b[H") && !enter.contains("\x1b[2J"));
        viewport.paint(&vec!["modal".into(); 24], "");
        viewport.pending.push("new finalized history".into());
        let exit = viewport.transition_modal(false, 24, 6);
        assert!(exit.find("new finalized history").unwrap() < exit.find("\x1b[18B").unwrap());
        assert!(viewport.pending.is_empty());
        assert!(!viewport.paint(&vec!["live".into(); 6], "").contains("new finalized history"));
    }

    #[test]
    fn uncertain_resize_modal_exit_preserves_non_erasing_recovery() {
        let mut viewport = NativeViewport::default();
        viewport.transition_modal(true, 24, 24);
        viewport.paint(&vec!["modal".into(); 24], "");
        viewport.resize(80, 40, 12, None);
        assert!(viewport.transition_modal(false, 12, 4).is_empty());
        assert!(viewport.recovery_newlines > 0);
        assert!(!viewport.paint(&vec!["live".into(); 4], "").contains("\x1b[J"));
    }

    #[test]
    fn ambiguous_unicode_resize_uses_non_destructive_recovery() {
        let mut viewport = NativeViewport::default();
        viewport.paint(&["👩‍💻 Unicode".into(), "draft".into()], "\x1b[2;2H");
        viewport.resize(80, 20, 12, Some(8));
        assert_eq!(viewport.cursor_row, 1, "logical frame cursor must not become a physical-row estimate");
        assert_eq!(viewport.finish(), "\r\n");
        let recovered = viewport.paint(&["new".into(), "draft".into()], "");
        assert!(!recovered.contains("\x1b[J"));
        assert!(!recovered.contains("\x1b[8A"));
    }
}
