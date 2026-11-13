//! Transcript parsing/styling and frame layout. Logical inline styles project before wrapping;
//! explicit content ranges keep copy/selection independent of gutters and soft wraps.

use super::text::{
    clip_line, format_compact_duration, format_inline_markdown_default, format_inline_markdown_with, format_token_count, slice_visible, style_box_line_with, styled_range_with, visible_length,
    wrap_terminal, Palette,
};
use super::{glyph, slash_description, Block, BodyCache, PickerState, Row, RowKind, State, Tui, SPINNER, USER_MARK};
use crate::session::format_goal_status_indicator;

type Seg = (String, &'static str);

fn seg(text: impl Into<String>, sgr: &'static str) -> Seg {
    (text.into(), sgr)
}

fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|') && line.trim_end().ends_with('|') && line.trim().chars().count() > 1
}

fn split_table_cells(trimmed: &str) -> Vec<String> {
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut backslashes = 0;
    for ch in inner.chars() {
        if ch == '|' {
            if backslashes % 2 == 1 {
                cell.pop(); // Markdown's escaped pipe is literal content, including in code spans.
                cell.push('|');
            } else {
                cells.push(cell.trim().to_string());
                cell.clear();
            }
        } else {
            cell.push(ch);
        }
        backslashes = if ch == '\\' { backslashes + 1 } else { 0 };
    }
    cells.push(cell.trim().to_string());
    cells
}

fn displayed_cell(cell: &str) -> String {
    crate::term::strip_vt(&format_inline_markdown_default(cell)).into_owned()
}

fn table_separator(trimmed: &str) -> bool {
    split_table_cells(trimmed).iter().all(|cell| re!(r"^:?-+:?$").is_match(cell))
}

/// Narrower terminals keep the unframed banner (a card would wrap every line).
const CARD_MIN_COLS: usize = 32;

fn is_banner_title(line: &str) -> bool {
    re!(r"^◆ goon v\S+\s*$").is_match(line)
}

/// `model      x · effort y` → `model x · effort y` for the unframed, narrow banner.
fn compact_banner_detail(detail: &str) -> String {
    re!(r"^(\S+)\s{2,}").replace(detail, "$1 ").into_owned()
}

fn frame_row(text: String, level: usize) -> Row {
    Row { kind: RowKind::Frame, pre: String::new(), text, glyph: None, sign: None, level, origin: None }
}

/// The welcome card (Claude Code / Codex style): a rounded frame around the title and the
/// session facts. Rows are decorative (`Frame`), so selection and copy skip them.
fn welcome_card(title: &str, details: &[&str], cols: usize) -> Vec<Row> {
    let infos: Vec<&str> = details.iter().copied().filter(|d| !d.starts_with("/help")).collect();
    let natural = infos.iter().map(|d| visible_length(d)).chain([visible_length(title)]).max().unwrap_or(0) + 4;
    let width = natural.max(48).min(cols).max(8);
    let inner = width - 4;
    let pad = |text: &str| format!("│ {text}{} │", " ".repeat(inner.saturating_sub(visible_length(text))));
    let mut rows = vec![frame_row(format!("╭{}╮", "─".repeat(width - 2)), 0), frame_row(pad(&slice_visible(title, inner)), 1)];
    if !infos.is_empty() {
        rows.push(frame_row(pad(""), 3));
    }
    for info in infos {
        let label = re!(r"^\S+\s{2,}").find(info).map(|m| visible_length(m.as_str())).filter(|w| *w <= inner / 2).unwrap_or(0);
        for (i, range) in super::text::wrap_ranges(info, inner, inner.saturating_sub(label).max(1), true).into_iter().enumerate() {
            let piece = if i == 0 { info[range].to_string() } else { format!("{}{}", " ".repeat(label), info[range].trim_start()) };
            rows.push(frame_row(pad(&slice_visible(&piece, inner)), 2));
        }
    }
    rows.push(frame_row(format!("╰{}╯", "─".repeat(width - 2)), 0));
    rows
}

/// `a · b · c` with dim separators and dim sub-labels (`effort`, `mode`, `session`).
fn paint_facts(text: &str, p: &Palette) -> String {
    let painted = re!(r"(^|· )(effort|mode|session|branch) ").replace_all(text, |c: &regex::Captures| format!("{}{}{}{} ", &c[1], p.dim, &c[2], p.reset));
    painted.replace(" · ", &format!(" {}·{} ", p.dim, p.reset))
}

fn style_frame(row: &Row, p: &Palette) -> String {
    let (Some(inner), true) = (row.text.strip_prefix("│ ").and_then(|t| t.strip_suffix(" │")), row.level > 0) else {
        return format!("{}{}{}", p.dim, row.text, p.reset);
    };
    let content = match row.level {
        1 => match re!(r"^◆ goon (v\S+)(\s*)$").captures(inner) {
            Some(c) => format!("{}{}{} {}goon{} {}{}{}{}", p.brand_bold, glyph::BRAND, p.reset, p.bold, p.reset, p.dim, &c[1], p.reset, &c[2]),
            None => inner.to_string(),
        },
        2 => match re!(r"^(\S+)(\s{2,})(.*)$").captures(inner) {
            Some(c) => format!("{}{}{}{}{}", p.dim, &c[1], p.reset, &c[2], paint_facts(&c[3], p)),
            None => paint_facts(inner, p),
        },
        _ => inner.to_string(),
    };
    format!("{}│{} {content} {}│{}", p.dim, p.reset, p.dim, p.reset)
}

/// Tip rows: the key of each `key description` item stays bright, the rest is dim.
fn style_tips(text: &str, p: &Palette) -> String {
    text.split(" · ")
        .map(|item| match item.split_once(' ') {
            Some((key, rest)) => format!("{}{key}{} {}{rest}{}", p.default_fg, p.reset, p.dim, p.reset),
            None => format!("{}{item}{}", p.dim, p.reset),
        })
        .collect::<Vec<_>>()
        .join(&format!(" {}·{} ", p.dim, p.reset))
}

fn plain_row(row: &Row, cols: usize) -> String {
    match row.kind {
        RowKind::Blank => String::new(),
        RowKind::CodeHead => slice_visible(&format!("  {}", row.text), cols),
        RowKind::Cell | RowKind::Child => slice_visible(&crate::term::strip_vt(&Tui::style_row(row, cols, &Palette::DARK)), cols),
        RowKind::Table => slice_visible(&format!("{}{}", row.pre, crate::term::strip_vt(&row.text)), cols),
        _ => slice_visible(&format!("{}{}", row.pre, crate::term::strip_vt(&row.text)), cols),
    }
}

fn cell_tone(glyph: &str, p: &Palette) -> &'static str {
    match glyph {
        "○" | "≡" | "↻" | "↺" => p.dim,
        "●" | "✓" => p.ok,
        "✗" => p.err,
        "±" => p.default_fg,
        "◈" | "◇" => p.brand,
        "▲" => p.warn,
        "◌" => p.dim,
        _ => p.dim,
    }
}

fn notice_tone(notice: &str, p: &Palette) -> &'static str {
    if Tui::is_retry_notice(notice) || notice.contains("Warning") {
        p.warn
    } else if notice.starts_with("Failed") || notice.contains("error") || notice.contains("Error") {
        p.err
    } else if notice.starts_with("Copied") || notice.starts_with('✓') {
        p.ok
    } else {
        p.dim
    }
}

impl Tui {
    pub(crate) fn format_active_tool_status(st: &State, max_cols: usize, now_ms: i64) -> Option<String> {
        let first = st.active_tools.values().next()?;
        let secs = ((now_ms - first.started_at_ms).max(0)) / 1000;
        let core = format!("Running {} ({secs}s)", first.name);
        let extra = st.active_tools.len().saturating_sub(1);
        let with_more = if extra > 0 { format!("{core} (+{extra} more)") } else { core };
        if !first.summary.is_empty() && first.summary != first.name {
            let base_len = visible_length(&with_more);
            if max_cols > base_len + 3 {
                let sum_clipped = slice_visible(&first.summary, max_cols - base_len - 3);
                if !sum_clipped.is_empty() {
                    return Some(format!("{with_more} · {sum_clipped}"));
                }
            }
        }
        Some(slice_visible(&with_more, max_cols.max(1)))
    }

    pub(crate) fn render_styled_body_lines(&self, st: &mut State, columns: usize, rich: bool) -> Vec<String> {
        let cols = columns.max(4);
        let p = Palette::for_mode(st.theme_mode);
        let styled_mode = rich && p.is_styled();
        let show_reasoning = st.view_mode != "normal" || st.in_transcript_overlay;
        // Borrow the live transcript for the parse instead of copying it every frame; a selection
        // renders its own frozen revision.
        let selection_source = st.selection.as_ref().map(|s| s.source.to_string());
        let live = selection_source.is_none();
        let transcript: String = selection_source.unwrap_or_else(|| std::mem::take(&mut st.transcript));
        let reuse = st
            .body_cache
            .as_ref()
            .map(|c| {
                c.cols == cols
                    && c.rich == styled_mode
                    && c.theme_mode == st.theme_mode
                    && c.show_reasoning == show_reasoning
                    && transcript.len() >= c.source.len()
                    && transcript.starts_with(&c.source)
            })
            .unwrap_or(false);
        // The cache owns the rows/styled lines of the previous frame; reuse them in place (truncated
        // back to the checkpoint) instead of cloning the whole body every frame.
        let cache = if reuse { st.body_cache.take() } else { None };
        let mut state = cache.as_ref().map(|c| c.state).unwrap_or_default();
        let (mut cached_source, mut rows, base_styled, mut source_ends, cached) = match cache {
            Some(mut c) => {
                c.rows.truncate(c.checkpoint_rows);
                c.styled.truncate(c.checkpoint_rows);
                c.source_ends.truncate(c.checkpoint_rows);
                (c.source, c.rows, c.styled, c.source_ends, c.checkpoint_rows)
            }
            None => (String::new(), vec![], vec![], vec![], 0),
        };
        let base_len = cached_source.len();
        let mut source_offset = base_len;
        let mut previous_source_end = base_len;
        let raw_lines: Vec<&str> = transcript[base_len..].split('\n').collect();

        // Welcome cards: the title line plus its indented detail lines, framed together.
        let mut banner_ends: std::collections::HashMap<usize, usize> = Default::default();
        for (i, line) in raw_lines.iter().enumerate() {
            if is_banner_title(line) {
                let mut j = i + 1;
                while j < raw_lines.len() && raw_lines[j].starts_with("  ") && !raw_lines[j].trim().is_empty() {
                    j += 1;
                }
                banner_ends.insert(i, j - 1);
            }
        }

        // Checkpoint for the next frame: the last (possibly streaming) line, moved back past any
        // table rows because a growing table changes the widths of earlier rows.
        let mut checkpoint = raw_lines.len() - 1;
        while checkpoint > 0 && is_table_row(raw_lines[checkpoint - 1]) {
            checkpoint -= 1;
        }
        // A card still being written is re-framed from its title next frame.
        if let Some((start, _)) = banner_ends.iter().find(|(start, end)| **start < checkpoint && **end >= checkpoint) {
            checkpoint = *start;
        }
        let mut checkpoint_rows = rows.len();
        let mut checkpoint_state = state;

        // Pre-scan contiguous Markdown table blocks so all rows of a table share column widths.
        let mut table_widths: std::collections::HashMap<usize, Vec<usize>> = Default::default();
        let mut table_rows: std::collections::HashMap<usize, (std::sync::Arc<super::table_copy::Table>, usize)> = Default::default();
        let mut table_sources: std::collections::HashMap<usize, std::sync::Arc<Vec<Vec<String>>>> = Default::default();
        let mut table_ends = std::collections::HashMap::new();
        let mut i = 0;
        while i < raw_lines.len() {
            if !is_table_row(raw_lines[i]) {
                i += 1;
                continue;
            }
            let mut j = i;
            let mut widths: Vec<usize> = vec![];
            while j < raw_lines.len() && is_table_row(raw_lines[j]) {
                let trimmed = raw_lines[j].trim();
                if !table_separator(trimmed) {
                    for (k, c) in split_table_cells(trimmed).iter().enumerate() {
                        let w = visible_length(&displayed_cell(c));
                        if widths.len() <= k {
                            widths.resize(k + 1, 0);
                        }
                        widths[k] = widths[k].max(w);
                    }
                }
                j += 1;
            }
            let mut alignments = vec!["---".to_string(); widths.len()];
            for line in &raw_lines[i..j] {
                if table_separator(line.trim()) {
                    for (column, cell) in split_table_cells(line.trim()).iter().enumerate().take(alignments.len()) {
                        alignments[column] = format!("{}---{}", if cell.starts_with(':') { ":" } else { "" }, if cell.ends_with(':') { ":" } else { "" });
                    }
                    break;
                }
            }
            let table = std::sync::Arc::new(super::table_copy::Table { alignments });
            let sources: Vec<Vec<String>> = raw_lines[i..j].iter().filter(|line| !table_separator(line.trim())).map(|line| split_table_cells(line.trim())).collect();
            let sources = std::sync::Arc::new(sources);
            let mut row = 0;
            for (k, line) in raw_lines.iter().enumerate().take(j).skip(i) {
                table_widths.insert(k, widths.clone());
                table_rows.insert(k, (table.clone(), row));
                table_sources.insert(k, sources.clone());
                table_ends.insert(k, j - 1);
                if !table_separator(line.trim()) {
                    row += 1;
                }
            }
            i = j;
        }

        fn blank(rows: &mut Vec<Row>) {
            if rows.last().map(|r| r.kind != RowKind::Blank).unwrap_or(false) {
                rows.push(Row { kind: RowKind::Blank, pre: String::new(), text: String::new(), glyph: None, sign: None, level: 0, origin: None });
            }
        }
        let wrap_into = |rows: &mut Vec<Row>, kind: RowKind, text: &str, first: &str, cont: &str, glyph: Option<String>, sign: Option<char>, level: usize, words: bool| {
            let markdown = !matches!(kind, RowKind::Code | RowKind::Diff);
            let clean = crate::term::terminal_text(text).replace('\t', "  ");
            // Project markup before wrapping: delimiters and link URLs must not
            // change visibility or offset identity at a different terminal width.
            let painted = if markdown { format_inline_markdown_with(&clean, p.reset, &p) } else { clean.clone() };
            let logical: std::sync::Arc<str> = std::sync::Arc::from(crate::term::strip_vt(&painted).as_ref());
            let ranges = super::text::wrap_ranges(&logical, cols.saturating_sub(visible_length(first)), cols.saturating_sub(visible_length(cont)), words);
            for (i, range) in ranges.into_iter().enumerate() {
                let text = if styled_mode && markdown { styled_range_with(&painted, range.clone(), &p) } else { logical[range.clone()].to_string() };
                rows.push(Row {
                    kind,
                    pre: if i == 0 { first.to_string() } else { cont.to_string() },
                    text,
                    glyph: glyph.clone(),
                    sign,
                    level,
                    origin: Some(super::selection::Origin { text: logical.clone(), range, projected: markdown, table: None, fragments: None }),
                });
            }
        };
        let last_content_kind = |rows: &Vec<Row>| rows.iter().rev().find(|r| r.kind != RowKind::Blank).map(|r| r.kind);

        let push_markdown = |rows: &mut Vec<Row>, raw_line: &str, base: &str, lead: Option<&str>, para: RowKind, line_index: usize| {
            let first_pre = |pre: &str| match lead {
                Some(l) => format!("{l}{}", &pre[base.len().min(pre.len())..]),
                None => pre.to_string(),
            };
            if let Some(h) = re!(r"^\s*(#{1,4})\s+(.*)$").captures(raw_line) {
                wrap_into(rows, RowKind::Heading, &h[2], &first_pre(base), base, None, None, h[1].len(), true);
                return;
            }
            if re!(r"^\s*(?:-{3,}|\*{3,}|_{3,})\s*$").is_match(raw_line) {
                let n = 48;
                rows.push(Row { kind: RowKind::Rule, pre: first_pre(base), text: "─".repeat(n), glyph: None, sign: None, level: 0, origin: None });
                return;
            }
            if let Some(b) = re!(r"^(\s*)[-*+]\s+(.*)$").captures(raw_line) {
                let pre = format!("{base}{}• ", &b[1]);
                wrap_into(rows, para, &b[2], &first_pre(&pre), &" ".repeat(visible_length(&pre)), None, None, 0, true);
                return;
            }
            if let Some(n) = re!(r"^(\s*)(\d{1,3})[.)]\s+(.*)$").captures(raw_line) {
                let pre = format!("{base}{}{}. ", &n[1], &n[2]);
                wrap_into(rows, para, &n[3], &first_pre(&pre), &" ".repeat(visible_length(&pre)), None, None, 0, true);
                return;
            }
            if let Some(q) = re!(r"^\s*>\s?(.*)$").captures(raw_line) {
                let cont = format!("{base}▎ ");
                wrap_into(rows, RowKind::Quote, &q[1], &first_pre(&cont), &cont, None, None, 0, true);
                return;
            }
            if is_table_row(raw_line) {
                let widths = table_widths.get(&line_index).cloned().unwrap_or_default();
                let (table, _) = table_rows.get(&line_index).expect("pre-scanned table row");
                if table_ends[&line_index] == line_index {
                    rows.extend(super::table_view::render(&table_sources[&line_index], table, &widths, cols, base, &first_pre(base), &p));
                }
                return;
            }
            let indent_len = raw_line.len() - raw_line.trim_start().len();
            let indent = &raw_line[..indent_len];
            let both = format!("{base}{indent}");
            wrap_into(rows, para, &raw_line[indent_len..], &first_pre(&both), &both, None, None, 0, true);
        };

        for (line_index, raw_line) in raw_lines.iter().enumerate() {
            // All display rows emitted by a source line share its end offset. Delay
            // filling until the next iteration because parser branches can continue.
            source_ends.resize(rows.len(), previous_source_end);
            source_offset += raw_line.len() + usize::from(source_offset + raw_line.len() < transcript.len());
            previous_source_end = source_offset;
            let raw_line = *raw_line;
            if line_index == checkpoint {
                checkpoint_rows = rows.len();
                checkpoint_state = state;
            }
            let trimmed = raw_line.trim();
            if state.pending_gap && !trimmed.is_empty() {
                state.pending_gap = false;
                blank(&mut rows);
            }
            if raw_line == "[normal]" {
                state.block = Block::Normal;
                state.in_code = false;
                state.in_diff = false;
                state.answer_first = false;
                state.in_banner = false;
                continue;
            }
            if raw_line.starts_with("[reasoning") {
                state.block = Block::Reasoning;
                state.in_code = false;
                state.in_diff = false;
                if show_reasoning {
                    blank(&mut rows);
                    rows.push(Row { kind: RowKind::ReasoningHead, pre: String::new(), text: format!("{} Thinking", glyph::THINKING), glyph: None, sign: None, level: 0, origin: None });
                }
                continue;
            }
            if raw_line.starts_with("[answer]") {
                blank(&mut rows);
                state.block = Block::Answer;
                state.in_code = false;
                state.in_diff = false;
                state.answer_first = true;
                continue;
            }
            if let Some(rest) = raw_line.strip_prefix(USER_MARK) {
                if !state.last_was_user {
                    blank(&mut rows);
                }
                let first = if state.last_was_user { "  ".to_string() } else { format!("{} ", glyph::USER) };
                wrap_into(&mut rows, RowKind::User, rest, &first, "  ", None, None, 0, true);
                state.last_was_user = true;
                state.block = Block::Normal;
                state.in_code = false;
                state.in_diff = false;
                continue;
            }
            if state.last_was_user {
                state.last_was_user = false;
                // An empty source line right after the prompt is the gap itself.
                if !trimmed.is_empty() {
                    blank(&mut rows);
                }
            }
            if state.block == Block::Reasoning && !show_reasoning {
                continue;
            }
            if let Some(fence_label) = trimmed.strip_prefix("```") {
                if state.in_code {
                    state.in_code = false;
                    // Deferred: a following source blank line provides the gap instead.
                    state.pending_gap = true;
                } else {
                    state.in_code = true;
                    state.in_diff = false;
                    state.answer_first = false;
                    let label = fence_label.trim();
                    rows.push(Row {
                        kind: RowKind::CodeHead,
                        pre: String::new(),
                        text: if label.is_empty() { "code".into() } else { label.to_string() },
                        glyph: None,
                        sign: None,
                        level: 0,
                        origin: None,
                    });
                }
                continue;
            }
            if state.in_code {
                let lang = match rows.last() {
                    Some(r) if r.kind == RowKind::Code => r.glyph.clone(),
                    _ => rows.iter().rev().find(|r| r.kind == RowKind::CodeHead).map(|r| r.text.clone()),
                };
                wrap_into(&mut rows, RowKind::Code, raw_line.replace('\t', "  ").trim_end(), "    ", "    ", lang, None, 0, false);
                continue;
            }
            if trimmed.is_empty() {
                state.in_banner = false;
                if state.block == Block::Reasoning {
                    continue;
                }
                if state.block == Block::Answer && state.answer_first {
                    continue;
                }
                // This source blank is the gap a closed fence asked for; don't stack a second.
                state.pending_gap = false;
                rows.push(Row {
                    kind: RowKind::Blank,
                    pre: String::new(),
                    text: String::new(),
                    glyph: None,
                    sign: None,
                    level: 0,
                    origin: Some(super::selection::Origin { text: std::sync::Arc::from(""), range: 0..0, projected: false, table: None, fragments: None }),
                });
                continue;
            }
            if state.block == Block::Reasoning {
                wrap_into(&mut rows, RowKind::Reasoning, raw_line.trim_end(), "  ", "  ", None, None, 0, true);
                continue;
            }
            if state.block == Block::Answer {
                // A table is emitted at its final source line so native scrollback
                // cannot commit cells whose source rows have not been consumed yet.
                if table_ends.get(&line_index).is_some_and(|last| *last > line_index) {
                    continue;
                }
                let lead = if state.answer_first { Some(format!("{} ", glyph::ANSWER)) } else { None };
                state.answer_first = false;
                push_markdown(&mut rows, raw_line.trim_end(), "  ", lead.as_deref(), RowKind::Answer, line_index);
                continue;
            }
            // Normal block: tool cells, diffs, CLI output, banner.
            if is_banner_title(raw_line) {
                blank(&mut rows);
                let end = banner_ends.get(&line_index).copied().unwrap_or(line_index);
                let details: Vec<&str> = raw_lines[line_index + 1..=end].iter().map(|l| l.trim()).collect();
                if cols >= CARD_MIN_COLS {
                    rows.extend(welcome_card(trimmed, &details, cols));
                } else {
                    rows.push(Row { kind: RowKind::BannerTitle, pre: String::new(), text: trimmed.to_string(), glyph: None, sign: None, level: 0, origin: None });
                    for detail in details.iter().filter(|d| !d.starts_with("/help")) {
                        wrap_into(&mut rows, RowKind::Banner, &compact_banner_detail(detail), "  ", "    ", None, None, 0, true);
                    }
                }
                if let Some(tips) = details.iter().find(|d| d.starts_with("/help")) {
                    wrap_into(&mut rows, RowKind::Banner, tips, "  ", "  ", None, None, 0, true);
                }
                state.in_banner = true;
                continue;
            }
            if state.in_banner {
                if raw_line.starts_with("  ") {
                    continue;
                }
                state.in_banner = false;
            }
            if let Some(cell) = re!(r"^([○●✗±◈≡▲◌↻↺◇✓]) (\S.*)$").captures(raw_line) {
                blank(&mut rows);
                state.in_diff = &cell[1] == glyph::EDIT || re!(r"^(?:Edited|Added|Deleted|Wrote|Created|Updated)\b").is_match(&cell[2]);
                wrap_into(&mut rows, RowKind::Cell, cell[2].trim_end(), &format!("{} ", &cell[1]), "  ", Some(cell[1].to_string()), None, 0, true);
                continue;
            }
            if re!(r"^(?:--- |\+\+\+ |@@ )").is_match(raw_line) {
                state.in_diff = true;
                rows.push(Row { kind: RowKind::DiffMeta, pre: "  ".into(), text: raw_line.trim_end().to_string(), glyph: None, sign: None, level: 0, origin: None });
                continue;
            }
            if state.in_diff {
                let numbered = re!(r"^ {4}([ \d]{4}) ([+\- ])(.*)$").captures(raw_line);
                let unified = if numbered.is_none() { re!(r"^([+\- ])(.*)$").captures(raw_line) } else { None };
                if numbered.is_some() || unified.is_some() {
                    let (sign, code, pre) = match (&numbered, &unified) {
                        (Some(n), _) => (n[2].chars().next().unwrap(), n[3].replace('\t', "  ").trim_end().to_string(), format!("    {} {}", &n[1], &n[2])),
                        (_, Some(u)) => (u[1].chars().next().unwrap(), u[2].replace('\t', "  ").trim_end().to_string(), format!("  {}", &u[1])),
                        _ => unreachable!(),
                    };
                    let cont = " ".repeat(visible_length(&pre));
                    let path = match rows.last() {
                        Some(r) if r.kind == RowKind::Diff => r.glyph.clone(),
                        _ => rows
                            .iter()
                            .rev()
                            .find(|r| r.kind == RowKind::Cell || (r.kind == RowKind::DiffMeta && r.text.starts_with("+++ ")))
                            .and_then(|r| r.text.split_whitespace().nth(1).map(str::to_string)),
                    };
                    wrap_into(&mut rows, RowKind::Diff, &code, &pre, &cont, path, Some(sign), 0, false);
                    continue;
                }
                if re!(r"^\s*(?:⋮|… \+\d+ more|\.\.\. \(\+\d+ more)").is_match(raw_line) || raw_line.starts_with("\\ No newline") {
                    rows.push(Row { kind: RowKind::DiffMeta, pre: "    ".into(), text: trimmed.to_string(), glyph: None, sign: None, level: 0, origin: None });
                    continue;
                }
                state.in_diff = false;
            }
            let prev = last_content_kind(&rows);
            if matches!(prev, Some(RowKind::Cell) | Some(RowKind::Child)) && raw_line.starts_with("  ") {
                let te = raw_line.trim_end();
                let child = re!(r"^ {2}(└ | {2})?(.*)$").captures(te);
                let connector = match child.as_ref().and_then(|c| c.get(1)).map(|m| m.as_str()) {
                    Some("└ ") => "  └ ",
                    Some(_) => "    ",
                    None => "  ",
                };
                let body = child.as_ref().and_then(|c| c.get(2)).map(|m| m.as_str().to_string()).unwrap_or_else(|| trimmed.to_string());
                wrap_into(&mut rows, RowKind::Child, &body, connector, "    ", None, None, 0, true);
                continue;
            }
            if raw_line.starts_with(['╭', '│', '╰']) {
                rows.push(Row { kind: RowKind::Box, pre: String::new(), text: raw_line.trim_end().to_string(), glyph: None, sign: None, level: 0, origin: None });
                continue;
            }
            // `[Mode switched to: plan]`-style session notes read as quiet system lines.
            if let Some(note) = re!(r"^\[([A-Z◆][^\[\]]*)\]\s*$").captures(raw_line).filter(|c| !re!(r"^(?:Pasted text|Image) #").is_match(&c[1])) {
                blank(&mut rows);
                wrap_into(&mut rows, RowKind::Hint, note[1].trim(), "· ", "  ", None, None, 0, true);
                continue;
            }
            if raw_line.starts_with("Type /help") {
                wrap_into(&mut rows, RowKind::Hint, raw_line.trim_end(), "", "", None, None, 0, true);
                continue;
            }
            push_markdown(&mut rows, raw_line.trim_end(), "", None, RowKind::Normal, line_index);
        }

        source_ends.resize(rows.len(), previous_source_end);
        let mut styled = base_styled;
        styled.truncate(cached);
        for row in &rows[cached..] {
            styled.push(clip_line(&if styled_mode { Self::style_row(row, cols, &p) } else { plain_row(row, cols) }, cols, styled_mode));
        }
        // Remember everything before the checkpoint line for the next frame.
        let consumed: usize = raw_lines[..checkpoint].iter().map(|l| l.len() + 1).sum();
        cached_source.push_str(&transcript[base_len..base_len + consumed]);
        st.body_layout_dirty = true;
        st.body_source_ends.clone_from(&source_ends);
        let out = styled.clone();
        st.body_cache =
            Some(BodyCache { cols, rich: styled_mode, theme_mode: st.theme_mode, show_reasoning, source: cached_source, state: checkpoint_state, checkpoint_rows, rows, styled, source_ends });
        if live {
            st.transcript = transcript;
        }
        out
    }

    fn style_row(row: &Row, cols: usize, p: &Palette) -> String {
        let inline = |text: &str, base: &str| {
            if row.origin.as_ref().is_some_and(|o| o.projected) {
                text.to_string()
            } else {
                format_inline_markdown_with(text, &format!("{}{base}", p.reset), p)
            }
        };
        let md = |text: &str, base: &str| format!("{base}{}{}", inline(text, base), p.reset);
        let prefix = |pre: &str, tone: &str| if pre.trim().is_empty() { pre.to_string() } else { format!("{tone}{pre}{}", p.reset) };
        let answer_pre = |pre: &str| {
            if let Some(rest) = pre.strip_prefix(glyph::ANSWER) {
                format!("{}{}{}{}", p.brand_bold, glyph::ANSWER, p.reset, prefix(rest, p.dim))
            } else {
                prefix(pre, p.dim)
            }
        };
        match row.kind {
            RowKind::Blank => String::new(),
            RowKind::User => {
                let pre = if row.pre.trim().is_empty() { row.pre.clone() } else { format!("{}{}{}", p.brand_bold, row.pre, p.reset) };
                if p.surface.is_empty() {
                    return format!("{pre}{}{}{}", p.bold, inline(&row.text, p.bold), p.reset);
                }
                // Codex-style prompt band: the tint survives inline styles and fills the row.
                let body = format!("{pre}{}{}", p.default_fg, inline(&row.text, p.default_fg));
                let used = visible_length(&row.pre) + visible_length(&row.text);
                let tinted = body.replace(p.reset, &format!("{}{}", p.reset, p.surface));
                format!("{}{tinted}{}{}", p.surface, " ".repeat(cols.saturating_sub(used)), p.reset)
            }
            RowKind::Answer => format!("{}{}", answer_pre(&row.pre), md(&row.text, p.default_fg)),
            RowKind::Normal => format!("{}{}", prefix(&row.pre, p.dim), inline(&row.text, p.reset)),
            RowKind::Heading => {
                let tone = match row.level {
                    1 => format!("{}{}", p.brand_bold, p.underline),
                    2 => p.brand_bold.to_string(),
                    _ => p.bold.to_string(),
                };
                format!("{}{}", answer_pre(&row.pre), md(&row.text, &tone))
            }
            RowKind::Quote => {
                let pre = if let Some(rest) = row.pre.strip_prefix(glyph::ANSWER) {
                    format!("{}{}{}{}{rest}{}", p.brand_bold, glyph::ANSWER, p.reset, p.dim, p.reset)
                } else {
                    format!("{}{}{}", p.dim, row.pre, p.reset)
                };
                format!("{pre}{}", md(&row.text, &format!("{}{}", p.dim, p.italic)))
            }
            RowKind::Rule => format!("{}{}{}{}", answer_pre(&row.pre), p.dim, row.text, p.reset),
            RowKind::Table => {
                if row.text.starts_with('├') {
                    return format!("{}{}{}{}", answer_pre(&row.pre), p.dim, row.text, p.reset);
                }
                format!("{}{}", answer_pre(&row.pre), row.text)
            }
            RowKind::ReasoningHead => format!("{}{}{}{}", p.dim, p.italic, row.text, p.reset),
            RowKind::Reasoning => format!("{}{}", row.pre, md(&row.text, &format!("{}{}", p.dim, p.italic))),
            RowKind::CodeHead => format!("  {}╭─ {}{}{}{}", p.dim, p.reset, p.info, slice_visible(&row.text, cols.saturating_sub(5).max(1)), p.reset),
            RowKind::Code => format!("  {}│{} {}{}", p.dim, p.reset, super::highlight::code(&row.text, row.glyph.as_deref().unwrap_or(""), p), p.reset),
            RowKind::Cell => Self::style_cell(row, cols, p),
            RowKind::Child => Self::style_child(row, p),
            RowKind::Diff => {
                let tone = match row.sign {
                    Some('+') => p.ok,
                    Some('-') => p.err,
                    _ => p.dim,
                };
                let num_len = row.pre.char_indices().find(|(_, c)| !(c.is_whitespace() || c.is_ascii_digit())).map(|(i, _)| i).unwrap_or(row.pre.len());
                let num_end = {
                    // `^(\s*\d*)`: leading spaces then digits.
                    let spaces = row.pre.len() - row.pre.trim_start().len();
                    let digits = row.pre[spaces..].chars().take_while(|c| c.is_ascii_digit()).count();
                    (spaces + digits).min(num_len)
                };
                let bg = match row.sign {
                    Some('+') => p.add_bg,
                    Some('-') => p.del_bg,
                    _ => "",
                };
                if bg.is_empty() {
                    return format!("{}{}{}{tone}{}{}{}", p.dim, &row.pre[..num_end], p.reset, &row.pre[num_end..], row.text, p.reset);
                }
                // Claude Code style: tinted rows, a coloured sign, readable syntax-coloured code.
                let code = super::highlight::generic(&row.text, row.glyph.as_deref().unwrap_or(""), p);
                let fill = " ".repeat(cols.saturating_sub(visible_length(&row.pre) + visible_length(&row.text)));
                format!("{}{}{}{bg}{tone}{}{}{code}{fill}{}", p.dim, &row.pre[..num_end], p.reset, &row.pre[num_end..], p.default_fg, p.reset)
            }
            RowKind::DiffMeta => format!("{}{}{}{}", row.pre, if row.text.starts_with("@@") { p.info } else { p.dim }, row.text, p.reset),
            RowKind::Box => style_box_line_with(&row.text, p),
            RowKind::Hint => format!("{}{}{}{}", p.dim, row.pre, row.text, p.reset),
            RowKind::Frame => style_frame(row, p),
            RowKind::BannerTitle => {
                let version = re!(r"^◆ goon (v\S+)").captures(&row.text).map(|c| c[1].to_string()).unwrap_or_default();
                format!("{}{}{} {}goon{} {}{version}{}", p.brand_bold, glyph::BRAND, p.reset, p.bold, p.reset, p.dim, p.reset)
            }
            RowKind::Banner => {
                if row.text.contains(" · ") && re!(r"(^|· )(/help|@|\$|!|shift\+tab) ").is_match(&row.text) {
                    return format!("{}{}", row.pre, style_tips(&row.text, p));
                }
                let painted = re!(r"^(model|gateway|workspace) ").replace(&row.text, format!("{}${{1}}{} ", p.dim, p.reset).as_str());
                format!("{}{}", row.pre, paint_facts(&painted, p))
            }
        }
    }

    fn style_cell(row: &Row, cols: usize, p: &Palette) -> String {
        let glyph = row.glyph.clone().unwrap_or_else(|| "•".into());
        let tone = cell_tone(&glyph, p);
        let with_durations = |text: &str, base: &str| {
            let t = re!(r"\((?:\d+h )?(?:\d+m )?\d+(?:\.\d+)?s\)").replace_all(text, |c: &regex::Captures| format!("{}{}{}{base}", p.dim, &c[0], p.reset));
            re!(r"\(\+(\d+) -(\d+)\)")
                .replace_all(&t, |c: &regex::Captures| format!("{}({}+{}{}{} {}-{}{}{}{}){}{base}", p.dim, p.reset, p.ok, &c[1], p.reset, p.err, &c[2], p.reset, p.dim, "", p.reset))
                .to_string()
        };
        if row.pre.trim().is_empty() {
            return format!("{}{}{}{}", row.pre, p.dim, with_durations(&row.text, p.dim), p.reset);
        }
        if glyph == glyph::WORKED {
            // Turn separator: `✓ Worked for 3.1s ──────────`.
            let fill = cols.saturating_sub(visible_length(&row.pre) + visible_length(&row.text) + 1);
            let rule = if fill >= 3 { format!(" {}", "─".repeat(fill)) } else { String::new() };
            return format!("{}{glyph}{} {}{}{rule}{}", p.ok, p.reset, p.dim, row.text, p.reset);
        }
        let (verb, rest) = match re!(r"^(\S+)(.*)$").captures(&row.text) {
            Some(c) => (c[1].to_string(), c[2].to_string()),
            None => (row.text.clone(), String::new()),
        };
        let glyph_out = format!("{tone}{}{glyph}{} ", p.bold, p.reset);
        if glyph == glyph::ALLOW {
            return format!("{glyph_out}{}{}{}{}", p.warn, p.bold, row.text, p.reset);
        }
        if glyph == glyph::COMPACT {
            return format!("{glyph_out}{}{}{}", p.dim, row.text, p.reset);
        }
        if glyph == glyph::PROMPT {
            return format!("{glyph_out}{}{}{}", p.brand_bold, row.text, p.reset);
        }
        let verb_tone = if glyph == glyph::FAILED { format!("{}{}", p.err, p.bold) } else { p.bold.to_string() };
        let rest_base = if glyph == glyph::RAN {
            p.info
        } else if glyph == glyph::EDIT {
            p.default_fg
        } else {
            p.dim
        };
        format!("{glyph_out}{verb_tone}{verb}{}{rest_base}{}{}", p.reset, with_durations(&rest, rest_base), p.reset)
    }

    fn style_child(row: &Row, p: &Palette) -> String {
        let pre = if row.pre.contains('└') { format!("{}{}{}", p.dim, row.pre, p.reset) } else { row.pre.clone() };
        let text = &row.text;
        if let Some(c) = re!(r"^(Read|List|Search)\b(.*)$").captures(text) {
            return format!("{pre}{}{}{}{}{}{}", p.info, &c[1], p.reset, p.default_fg, &c[2], p.reset);
        }
        if let Some(rest) = text.strip_prefix("✓ ") {
            return format!("{pre}{}✓{} {}{rest}{}", p.ok, p.reset, p.dim, p.reset);
        }
        if let Some(rest) = text.strip_prefix("▸ ") {
            return format!("{pre}{}▸ {rest}{}", p.brand_bold, p.reset);
        }
        if let Some(rest) = text.strip_prefix("○ ") {
            return format!("{pre}{}○{} {}{rest}{}", p.dim, p.reset, p.default_fg, p.reset);
        }
        if let Some(rest) = text.strip_prefix("$ ") {
            return format!("{pre}{}${} {}{}{rest}{}", p.dim, p.reset, p.info, p.bold, p.reset);
        }
        if re!(r"^… \+\d+").is_match(text) {
            return format!("{pre}{}{text}{}", p.dim, p.reset);
        }
        if let Some(c) = re!(r"^(\d{1,2})\. (.*)$").captures(text) {
            let body = if row.origin.as_ref().is_some_and(|o| o.projected) { c[2].to_string() } else { format_inline_markdown_with(&c[2], p.reset, p) };
            return format!("{pre}{}{}.{} {body}", p.brand_bold, &c[1], p.reset);
        }
        format!("{pre}{}{text}{}", p.dim, p.reset)
    }

    /// Row-level differential flush: unchanged rows are never rewritten after the first frame.
    fn flush_screen_lines(&self, st: &mut State, lines: Vec<String>, columns: usize, rows: usize, cursor: String) {
        let styled = Palette::for_mode(st.theme_mode).is_styled();
        if let Some(native) = st.native.as_mut() {
            let cursor_y = st.native_resize_cursor.take().filter(|(c, r, _)| *c == columns && *r == rows).map(|(_, _, y)| y);
            if st.last_cols > 0 && (columns != st.last_cols || rows != st.last_rows) {
                native.resize(st.last_cols, columns, rows, cursor_y);
            }
            let modal = st.in_transcript_overlay || st.picker.is_some() || (st.pending.is_some() && !st.remember);
            let mut lines = lines;
            let cursor = if modal {
                // Short questions also own a full-height frame; adjust the local caret.
                let padding = rows.saturating_sub(lines.len());
                lines.splice(0..0, std::iter::repeat_n(String::new(), padding));
                re!(r"\x1b\[(\d+);(\d+)H").replace_all(&cursor, |c: &regex::Captures| format!("\x1b[{};{}H", c[1].parse::<usize>().unwrap_or(1) + padding, &c[2])).into_owned()
            } else {
                cursor
            };
            let mut output = native.transition_modal_with(modal, rows, lines.len(), styled);
            output.push_str(&native.paint_with(&lines, &cursor, styled));
            st.last_cols = columns;
            st.last_rows = rows;
            st.last_frame_rows = lines;
            st.last_cursor_seq = cursor;
            st.needs_full_clear = false;
            self.write_raw(&output);
            return;
        }
        if st.needs_full_clear || columns != st.last_cols || rows != st.last_rows || st.last_frame_rows.len() != lines.len() {
            st.needs_full_clear = false;
            st.last_cols = columns;
            st.last_rows = rows;
            st.last_cursor_seq = cursor.clone();
            let body = lines.iter().map(|l| format!("{l}\x1b[K")).collect::<Vec<_>>().join("\r\n");
            let frame = format!("\x1b[?2026h\x1b[?25l\x1b[H\x1b[2J{body}{cursor}\x1b[?2026l");
            st.last_frame_rows = lines;
            self.write_raw(&frame);
            return;
        }
        let reset_prefix = if styled { "\x1b[0m" } else { "" };
        let mut patches = String::new();
        // Streaming output usually just moves the body up a few rows. Let the terminal scroll
        // that region (DECSTBM + SU) and repaint only the rows that are new, instead of
        // rewriting every visible row each frame: far less output for the terminal to parse
        // and draw, which is what CPU-rendered terminals struggle with.
        if let Some((region, shift)) = scroll_shift(&st.last_frame_rows, &lines) {
            // Line feeds at the region's bottom margin: the most widely supported way to scroll.
            patches.push_str(&format!("\x1b[0m\x1b[1;{region}r\x1b[{region};1H{}\x1b[r", "\n".repeat(shift)));
            st.last_frame_rows[..region].rotate_left(shift);
            for row in &mut st.last_frame_rows[region - shift..region] {
                row.clear();
            }
        }
        for (i, line) in lines.iter().enumerate() {
            if *line != st.last_frame_rows[i] {
                patches.push_str(&format!("\x1b[{};1H{reset_prefix}{line}\x1b[K", i + 1));
                st.last_frame_rows[i] = line.clone();
            }
        }
        if patches.is_empty() && cursor == st.last_cursor_seq {
            return;
        }
        st.last_cursor_seq = cursor.clone();
        self.write_raw(&format!("\x1b[?2026h\x1b[?25l{patches}{cursor}\x1b[?2026l"));
    }

    /// One full-width line: left segments clip last; right groups drop (front first) when narrow.
    fn band(left: &[Seg], right: Vec<Vec<Seg>>, cols: usize, p: &Palette) -> String {
        let sep: Seg = seg("   ", "");
        let flatten = |groups: &[Vec<Seg>]| -> Vec<Seg> {
            let mut out = vec![];
            for (i, g) in groups.iter().enumerate() {
                if i > 0 {
                    out.push(sep.clone());
                }
                out.extend(g.iter().cloned());
            }
            out
        };
        let plain_width = |segs: &[Seg]| visible_length(&segs.iter().map(|s| s.0.as_str()).collect::<String>());
        let left_min = plain_width(left).min((cols as f64 * 0.6).floor() as usize);
        let mut groups: Vec<Vec<Seg>> = right.into_iter().filter(|g| !g.is_empty()).collect();
        while !groups.is_empty() && left_min + plain_width(&flatten(&groups)) + 2 > cols {
            groups.remove(0);
        }
        let right_segs = flatten(&groups);
        let right_width = plain_width(&right_segs);
        let left_avail = cols.saturating_sub(right_width + if right_width > 0 { 2 } else { 0 });
        let paint = |text: &str, sgr: &str| if p.is_styled() && !sgr.is_empty() { format!("{sgr}{text}{}", p.reset) } else { text.to_string() };
        let mut used = 0;
        let mut left_out = String::new();
        for (text, sgr) in left {
            let room = left_avail.saturating_sub(used);
            if room == 0 {
                break;
            }
            let piece = if visible_length(text) > room { slice_visible(text, room) } else { text.clone() };
            left_out.push_str(&paint(&piece, sgr));
            used += visible_length(&piece);
        }
        let gap = " ".repeat(cols.saturating_sub(used + right_width));
        let right_out: String = right_segs.iter().map(|(t, s)| paint(t, s)).collect();
        if right_out.is_empty() {
            left_out
        } else {
            format!("{left_out}{gap}{right_out}")
        }
    }

    fn context_group(st: &State, p: &Palette) -> Vec<Seg> {
        let (Some(used), true) = (st.context_used, st.context_window > 0) else { return vec![] };
        let pct = ((used as f64 / st.context_window as f64) * 100.0).round().clamp(0.0, 100.0) as u64;
        let tone = if pct >= 90 {
            p.err
        } else if pct >= 70 {
            p.warn
        } else {
            p.dim
        };
        vec![seg(format!("{}% context left", 100 - pct), tone)]
    }

    fn picker_lines(state: &mut PickerState, cols: usize, max_rows: usize, p: &Palette) -> Vec<String> {
        let mut lines = vec![];
        let tabs_text = (0..state.tabs.len())
            .map(|i| {
                let count = Tui::picker_matches(state, i).len();
                let label = format!("{} {count}", state.tabs[i].label);
                if i == state.tab {
                    format!("{}{label}{}", p.brand_bold, p.reset)
                } else {
                    format!("{}{label}{}", p.dim, p.reset)
                }
            })
            .collect::<Vec<_>>()
            .join("   ");
        lines.push(clip_line(&format!("{}{}{}   {tabs_text}", p.bold, state.title, p.reset), cols, p.is_styled()));
        let matches = Tui::picker_matches(state, state.tab);
        let list_rows = max_rows.saturating_sub(3).max(1);
        if matches.is_empty() {
            let empty = if !state.filter.is_empty() {
                format!("No match for \"{}\"", state.filter)
            } else {
                state.tabs.get(state.tab).and_then(|t| t.empty.clone()).unwrap_or_else(|| "Nothing here yet".into())
            };
            lines.push(format!("{}  {}{}", p.dim, slice_visible(&empty, cols.saturating_sub(2).max(1)), p.reset));
        } else {
            state.index = state.index.min(matches.len() - 1);
            let start = (state.index + 1).saturating_sub(list_rows).min(matches.len().saturating_sub(list_rows));
            for (i, item) in matches.iter().enumerate().skip(start).take(list_rows) {
                let selected = i == state.index;
                let meta = item.meta.as_ref().map(|m| slice_visible(&crate::term::terminal_text(m), (cols as f64 * 0.45).floor() as usize)).unwrap_or_default();
                let meta_cols = visible_length(&meta);
                let label_room = cols.saturating_sub(2 + meta_cols + if meta_cols > 0 { 3 } else { 0 }).max(4);
                let mut label = crate::term::terminal_text(&item.label).split_whitespace().collect::<Vec<_>>().join(" ");
                if visible_length(&label) > label_room {
                    label = format!("{}…", slice_visible(&label, label_room - 1));
                }
                let gap = " ".repeat((if meta_cols > 0 { 3 } else { 0 }).max(cols.saturating_sub(2 + visible_length(&label) + meta_cols)));
                lines.push(if selected {
                    format!("{}› {}{}{label}{}{gap}{}{meta}{}", p.brand_bold, p.reset, p.bold, p.reset, p.dim, p.reset)
                } else {
                    format!("  {label}{gap}{}{meta}{}", p.dim, p.reset)
                });
            }
            if let Some(detail) = matches.get(state.index).and_then(|m| m.detail.clone()) {
                lines.push(format!("{}  {}{}", p.dim, slice_visible(&crate::term::terminal_text(&detail), cols.saturating_sub(2).max(1)), p.reset));
            }
        }
        lines
    }

    /// The visible composer slice around the cursor.
    fn slice_input_window(st: &State, avail: usize) -> (String, String) {
        let max_before = avail.saturating_sub(1);
        let chars: Vec<char> = if st.masked { st.input.iter().map(|_| '•').collect() } else { st.input.clone() };
        use unicode_segmentation::UnicodeSegmentation;
        let cursor = st.cursor.min(chars.len());
        let prefix: String = chars[..cursor].iter().collect();
        let mut start = prefix.len();
        let mut used = 0;
        for (index, grapheme) in prefix.grapheme_indices(true).rev() {
            let w = visible_length(grapheme);
            if grapheme == "\n" || used + w > max_before {
                break;
            }
            used += w;
            start = index;
        }
        let before = prefix[start..].to_string();
        let after: String = chars[cursor..].iter().collect();
        let after = slice_visible(&after, avail);
        let shown = wrap_terminal(&format!("{before}{after}"), avail).into_iter().next().unwrap_or_default();
        (before, shown)
    }

    pub(crate) fn render(&self, st: &mut State) {
        if st.closed || st.editor_active || st.input_yielded || st.suspend_requested {
            return;
        }
        self.sync_mouse_mode(st);
        if st.picker.is_some() || (st.pending.is_some() && !st.remember) || st.scroll == 0 {
            st.follow_control = super::pointer::Follow::default();
        }
        let (columns, rows) = self.size();
        let columns = columns.max(4);
        let rows = rows.max(4);
        let safe_cols = columns.saturating_sub(1).max(3);
        let clip = |s: &str| wrap_terminal(s, safe_cols).into_iter().next().unwrap_or_default();
        let current_input: String = st.input.iter().collect();
        let busy = matches!(st.status.as_str(), "Running" | "Connecting MCP" | "Compacting");
        let indicator = if busy { SPINNER[st.spin_tick % SPINNER.len()] } else { "●" };
        let p = Palette::for_mode(st.theme_mode);
        let color_enabled = p.is_styled();
        let compact_chrome = columns < 48 || rows < 12;
        let asking = st.label != "goon> ";
        let title = st.title.clone();
        let parts: Vec<&str> = title.split(" · ").collect();
        let model_name = if parts[0].is_empty() { title.clone() } else { parts[0].to_string() };
        let session_label = if parts.len() > 1 { parts[1..].join(" · ") } else { "session".into() };
        let now_ms = crate::jsutil::now_millis();
        let elapsed = st.turn_started_at.map(|t| format_compact_duration(now_ms - t)).unwrap_or_default();

        let mut all = self.render_styled_body_lines(st, safe_cols, color_enabled);
        if let Some(native) = st.native.as_mut() {
            // Streaming Markdown and running tool cells can restyle previous lines. Only
            // commit finalized turns; the live tail remains reversible until the run is idle.
            native.commit_prefix(
                &mut all,
                &st.body_source_ends,
                &st.transcript,
                rows.saturating_sub(8).clamp(1, 8),
                st.in_transcript_overlay || st.picker.is_some() || (st.pending.is_some() && !st.remember),
                busy,
            );
        }
        if st.scroll > 0 && st.last_body_total_lines > 0 && all.len() > st.last_body_total_lines {
            st.scroll += all.len() - st.last_body_total_lines;
        }
        st.last_body_total_lines = all.len();
        if columns != st.last_cols || rows != st.last_rows {
            if st.selection.is_some() {
                st.ensure_body_layout();
            }
            // Keep the selected endpoint in view instead of reinterpreting the
            // old row offset after rewrap. Body-height clamping happens below.
            if let Some(row) = st.selection.as_ref().and_then(|s| st.body_layout.row_for(s.end)) {
                st.scroll = all.len().saturating_sub(row + 1);
            }
        }

        let active_secs = st.turn_started_at.map(|t| (now_ms - t) as f64 / 1000.0).unwrap_or(0.0);
        let goal_indicator = st.goal.as_ref().map(|g| format_goal_status_indicator(g, active_secs)).unwrap_or_default();
        let goal_tone: &'static str = match st.goal.as_ref().map(|g| g.status.as_str()) {
            Some("active") => p.brand,
            Some("complete") => p.ok,
            Some("paused") => p.dim,
            _ => p.warn,
        };
        let goal_group: Vec<Seg> = if goal_indicator.is_empty() { vec![] } else { vec![seg(goal_indicator.clone(), goal_tone)] };
        let todo_group: Vec<Seg> = if st.todo_label.is_empty() { vec![] } else { vec![seg(format!("todo {}", st.todo_label), p.dim)] };
        let mut token_group: Vec<Seg> = vec![seg(format!("{} tokens", format_token_count(st.tokens)), p.dim)];
        if st.cost_usd > 0.0 {
            token_group.push(seg(format!(" · ${}", crate::jsutil::to_fixed(st.cost_usd, 4)), p.dim));
        }

        // A picker (e.g. /resume) replaces the status/popup rows and turns the prompt into its filter.
        if st.picker.is_some() {
            let max_picker_rows = rows.saturating_sub(6).clamp(4, 18);
            let picker_lines = Self::picker_lines(st.picker.as_mut().unwrap(), safe_cols, max_picker_rows, &p);
            let height = rows.saturating_sub(picker_lines.len() + 3).max(1);
            st.max_scroll = all.len().saturating_sub(height);
            st.scroll = 0;
            let mut body: Vec<String> = all[all.len().saturating_sub(height)..].to_vec();
            while body.len() < height {
                body.push(String::new());
            }
            let picker = st.picker.as_ref().unwrap();
            let filter = slice_visible(&picker.filter, safe_cols.saturating_sub(3).max(1));
            let prompt_line = format!("{}› {}{}", p.brand_bold, p.reset, if filter.is_empty() { format!("{}type to filter{}", p.dim, p.reset) } else { filter.clone() });
            let hints = format!("←→ tabs   ↑↓ select   enter {}   esc cancel", picker.action);
            let footer = if color_enabled { format!("{}{}{}", p.dim, clip(&hints), p.reset) } else { clip(&hints) };
            let mut lines = body;
            lines.extend(picker_lines);
            lines.push(format!("{}{}{}", p.dim, "─".repeat(safe_cols), p.reset));
            lines.push(prompt_line);
            lines.push(footer);
            let cursor = format!("\x1b[{};{}H\x1b[?25h", lines.len() - 1, safe_cols.min(3 + visible_length(&filter)));
            self.flush_screen_lines(st, lines, columns, rows, cursor);
            return;
        }

        if let Some(form) = st.pending.as_ref().and_then(|pending| pending.form.as_ref()) {
            // Question card (Codex style): anchored above the footer with the transcript still
            // visible above it, instead of a bare form at the top of an empty screen.
            let (inner, budget, framed) = Self::form_card_size(columns, rows);
            let tone = p.brand;
            let card_row = |content: String| -> String {
                if framed {
                    let fill = " ".repeat(inner.saturating_sub(visible_length(&content)));
                    format!("{tone}│{} {content}{fill} {tone}│{}", p.reset, p.reset)
                } else {
                    content
                }
            };
            let card_top = |title: &str| -> String {
                if framed {
                    let title = slice_visible(title, inner.saturating_sub(1).max(1));
                    let dashes = "─".repeat(safe_cols.saturating_sub(3 + visible_length(&title) + 1));
                    format!("{tone}╭─{}{}{title}{}{tone}{dashes}╮{}", p.reset, p.bold, p.reset, p.reset)
                } else {
                    format!("{}{}{}", p.warn, clip(title.trim()), p.reset)
                }
            };
            let card_bottom = || format!("{tone}╰{}╯{}", "─".repeat(safe_cols.saturating_sub(2)), p.reset);
            let mut card: Vec<String> = vec![];
            let footer_text: String;
            let mut cursor_at: Option<(usize, usize)> = None;
            if let Some(selected) = form.confirm {
                let unanswered = form.questions.iter().filter(|q| !q.committed).count();
                let noun = if unanswered == 1 { "question" } else { "questions" };
                card.push(card_top(" Submit with unanswered questions? "));
                card.push(card_row(format!("{}{}{}", p.dim, slice_visible(&format!("{unanswered} unanswered {noun}"), inner), p.reset)));
                for (index, label) in ["Proceed", "Go back"].iter().enumerate() {
                    let marker = if index == selected { "›" } else { " " };
                    let tone = if index == selected { p.brand_bold } else { p.default_fg };
                    card.push(card_row(format!("{tone}{}{}", slice_visible(&format!("{marker} {}. {label}", index + 1), inner), p.reset)));
                    let description = if index == 0 { format!("Submit with {unanswered} unanswered {noun}") } else { "Return to the first unanswered question".into() };
                    card.push(card_row(format!("{}{}{}", p.dim, slice_visible(&format!("     {description}"), inner), p.reset)));
                }
                if framed {
                    card.push(card_bottom());
                }
                footer_text = "  Press enter to confirm or esc to go back".into();
            } else {
                let question = &form.questions[form.index];
                card.push(card_top(&format!(" Question {}/{} · {} ", form.index + 1, form.questions.len(), question.header)));
                let (prompt, options, editor_rows, option_room) = Self::form_layout(question, inner, budget);
                card.extend(prompt.into_iter().map(|line| card_row(format!("{}{line}{}", p.bold, p.reset))));
                let mut start = question.selected;
                let mut used = options[question.selected].len();
                while start > 0 && used + options[start - 1].len() <= option_room {
                    start -= 1;
                    used += options[start].len();
                }
                let mut available = option_room;
                for (index, option) in options.iter().enumerate().skip(start) {
                    for row in option.iter().take(available) {
                        card.push(card_row(format!("{}{row}{}", if index == question.selected { p.brand_bold } else { p.default_fg }, p.reset)));
                    }
                    available = available.saturating_sub(option.len());
                    if available == 0 {
                        break;
                    }
                }
                let notes = question.notes || question.options.is_empty();
                card.push(card_row(format!("{}{}{}", p.dim, slice_visible(if notes { "Notes · tab options" } else { "tab notes · ↑↓ select · ←→ questions" }, inner), p.reset)));
                let (editor, row, col) = Self::composer_lines(st, inner.saturating_sub(2).max(1), editor_rows);
                let editor_start = card.len();
                for line in editor {
                    card.push(card_row(format!("{}›{} {line}", if notes { p.brand_bold } else { p.dim }, p.reset)));
                }
                if framed {
                    card.push(card_bottom());
                }
                if notes {
                    cursor_at = Some((editor_start + row, col + 3 + if framed { 2 } else { 0 }));
                }
                footer_text = st.status_notice.clone().unwrap_or_else(|| "  enter next/submit · pgup/pgdn questions · esc cancel".into());
            }
            let footer = format!("{}{}{}", p.dim, clip(&footer_text), p.reset);
            let height = rows.saturating_sub(card.len() + 1);
            let mut lines: Vec<String> = all[all.len().saturating_sub(height)..].to_vec();
            while lines.len() < height {
                lines.push(String::new());
            }
            let card_start = lines.len();
            lines.extend(card);
            lines.push(footer);
            let cursor = match cursor_at {
                Some((row, col)) => format!("\x1b[{};{}H\x1b[?25h", card_start + row + 1, col.min(columns)),
                None => "\x1b[?25l".into(),
            };
            self.flush_screen_lines(st, lines, columns, rows, cursor);
            return;
        }

        if let Some(pending) = st.pending.as_ref().filter(|pending| !pending.choices.is_empty() && !pending.custom) {
            // Claude Code style choice card: a framed list under the request it answers.
            let framed = safe_cols >= 12;
            let count = pending.choices.len().min(rows.saturating_sub(if framed { 6 } else { 5 }).max(1));
            let start = (pending.selected + 1).saturating_sub(count);
            let height = rows.saturating_sub(count + if framed { 4 } else { 3 }).max(1);
            let mut lines: Vec<String> = all[all.len().saturating_sub(height)..].to_vec();
            while lines.len() < height {
                lines.push(String::new());
            }
            let tone = if pending.allow_custom { p.brand } else { p.warn };
            let inner = if framed { safe_cols - 4 } else { safe_cols };
            if framed {
                lines.push(format!("{tone}╭{}╮{}", "─".repeat(safe_cols - 2), p.reset));
            } else {
                lines.push(format!("{tone}{}{}", "─".repeat(safe_cols), p.reset));
            }
            for (index, (_, label)) in pending.choices.iter().enumerate().skip(start).take(count) {
                let selected = index == pending.selected;
                let marker = if selected { "›" } else { " " };
                let text = slice_visible(&format!("{marker} {}. {label}", index + 1), inner);
                let fill = " ".repeat(inner.saturating_sub(visible_length(&text)));
                let body = if selected { format!("{}{text}{}", p.brand_bold, p.reset) } else { format!("{}{text}{}", p.default_fg, p.reset) };
                lines.push(if framed { format!("{tone}│{} {body}{fill} {tone}│{}", p.reset, p.reset) } else { body });
            }
            if framed {
                lines.push(format!("{tone}╰{}╯{}", "─".repeat(safe_cols - 2), p.reset));
            }
            let hint = clip(if pending.allow_custom { "  ↑↓ select · enter confirm · o other answer · esc cancel" } else { "  ↑↓ select · enter confirm · esc deny" });
            lines.push(if color_enabled { format!("{}{hint}{}", p.dim, p.reset) } else { hint });
            if !framed {
                lines.push(String::new());
            }
            self.flush_screen_lines(st, lines, columns, rows, "\x1b[?25l".into());
            return;
        }

        // Full-screen transcript (Ctrl+T or /transcript).
        if st.in_transcript_overlay {
            let header = Self::band(
                &[seg(format!("{} ", glyph::BRAND), p.brand_bold), seg("transcript", p.bold), seg(format!("   {session_label}"), p.dim)],
                vec![goal_group.clone(), todo_group.clone()],
                safe_cols,
                &p,
            );
            let height = rows.saturating_sub(2).max(1);
            st.max_scroll = all.len().saturating_sub(height);
            st.scroll = st.scroll.min(st.max_scroll);
            let end = all.len() - st.scroll;
            let mut body: Vec<String> = all[end.saturating_sub(height)..end].to_vec();
            Self::select_body(st, &mut body, end.saturating_sub(height), 1, safe_cols);
            while body.len() < height {
                body.push(String::new());
            }
            let mut live: Vec<Seg> = vec![];
            if busy {
                if let Some(notice) = &st.status_notice {
                    let clean_notice = super::text::sanitize_single_line(notice);
                    let tone = notice_tone(&clean_notice, &p);
                    live = vec![seg(format!("{indicator} "), p.brand), seg(slice_visible(&clean_notice, safe_cols.saturating_sub(24).max(8)), tone), seg(format!("  {elapsed}   "), p.dim)];
                } else if let Some(active_tool) = Self::format_active_tool_status(st, safe_cols.saturating_sub(24).max(12), now_ms) {
                    live = vec![seg(format!("{indicator} "), p.brand), seg(active_tool, p.default_fg), seg(format!("  {elapsed}   "), p.dim)];
                } else {
                    live = vec![seg(format!("{indicator} "), p.brand), seg("working", p.default_fg), seg(format!("  {elapsed}   "), p.dim)];
                }
            }
            live.push(seg(format!("line {} of {}", end.max(1), all.len().max(1)), p.dim));
            let footer = Self::band(
                &live,
                vec![vec![seg(
                    if st.selection.is_some() {
                        "ctrl+o copy selection · esc clear"
                    } else if st.scroll > 0 {
                        "↓ Follow output · End   ctrl+o copy selection"
                    } else {
                        "↑↓ scroll   esc back   drag select"
                    },
                    p.dim,
                )]],
                safe_cols,
                &p,
            );
            let follow = st.scroll > 0 && st.selection.is_none() && st.history_search.is_none();
            let footer = st.follow_control.render(safe_cols, rows.saturating_sub(1), follow, &p).unwrap_or(footer);
            let mut lines = vec![header];
            lines.extend(body);
            lines.push(footer);
            self.flush_screen_lines(st, lines, columns, rows, String::new());
            return;
        }

        // Live status row: only says something when there is something to say.
        let mut status_line = String::new();
        if let Some((key, _)) = st.quit_armed.as_ref().filter(|(_, until)| std::time::Instant::now() < *until) {
            let text = clip(&format!("Press ctrl+{key} again to quit"));
            status_line = if color_enabled { format!("{}{text}{}", p.warn, p.reset) } else { text };
        } else if let Some(search) = &st.history_search {
            let result = if search.matched.is_some() { "enter accept" } else { "no match" };
            let text = clip(&format!("history search: {}   {result}   esc cancel", search.query));
            status_line = if color_enabled { format!("{}{text}{}", p.dim, p.reset) } else { text };
        } else if st.scroll > 0 {
            let text = clip(&format!("↑ {} lines above the latest output   Ctrl+End or Esc returns", st.scroll));
            status_line = if color_enabled { format!("{}{text}{}", p.dim, p.reset) } else { text };
        } else if let Some(prime) = &st.esc_primed {
            let text = clip(match prime {
                super::EscPrime::Backtrack => "Press Esc again to rewind and edit the previous prompt",
                super::EscPrime::Clear => "Press Esc again to clear the input",
            });
            status_line = if color_enabled { format!("{}{text}{}", p.warn, p.reset) } else { text };
        } else if busy {
            if let Some(notice) = &st.status_notice {
                let clean_notice = super::text::sanitize_single_line(notice);
                let full_tail = format!(" ({elapsed} · esc to interrupt)");
                let short_tail = if elapsed.is_empty() { String::new() } else { format!(" ({elapsed})") };
                let tail = if 2 + visible_length(&clean_notice) + visible_length(&full_tail) <= safe_cols {
                    full_tail
                } else if 2 + visible_length(&clean_notice) + visible_length(&short_tail) <= safe_cols {
                    short_tail
                } else {
                    String::new()
                };
                let shown_notice = slice_visible(&clean_notice, safe_cols.saturating_sub(2 + visible_length(&tail)).max(1));
                let tone = notice_tone(&clean_notice, &p);
                let raw_status =
                    if color_enabled { format!("{}{indicator}{} {tone}{shown_notice}{}{}{tail}{}", p.brand, p.reset, p.reset, p.dim, p.reset) } else { format!("{indicator} {shown_notice}{tail}") };
                status_line = clip_line(&raw_status, safe_cols, color_enabled);
            } else {
                let tool_running = !st.active_tools.is_empty();
                // A running tool already shows its own timer; the tail then only offers esc.
                let candidate_tail = if safe_cols >= 48 {
                    if tool_running {
                        " · esc to interrupt".to_string()
                    } else {
                        format!(" ({elapsed} · esc to interrupt)")
                    }
                } else if elapsed.is_empty() || tool_running {
                    String::new()
                } else {
                    format!(" ({elapsed})")
                };
                let candidate_avail = safe_cols.saturating_sub(2 + visible_length(&candidate_tail)).max(1);
                let resolve_working = |avail: usize| -> (String, bool) {
                    match Self::format_active_tool_status(st, avail, now_ms) {
                        Some(tool) => (tool, false),
                        None => {
                            let raw = if st.status == "Running" {
                                st.reasoning_header.as_deref().map(super::text::sanitize_single_line).filter(|h| !h.is_empty()).unwrap_or_else(|| "Working".to_string())
                            } else {
                                super::text::sanitize_single_line(&st.status)
                            };
                            let raw = if raw.ends_with(['…', '.']) { raw } else { format!("{raw}…") };
                            (slice_visible(&raw, avail), true)
                        }
                    }
                };
                let (mut working, mut thinking) = resolve_working(candidate_avail);
                let tail = if 2 + visible_length(&working) + visible_length(&candidate_tail) <= safe_cols {
                    candidate_tail
                } else {
                    (working, thinking) = resolve_working(safe_cols.saturating_sub(2).max(1));
                    String::new()
                };
                // Claude Code / Codex: a breathing star and a soft shimmer across the verb.
                let painted = if thinking && visible_length(&working) <= 64 { super::highlight::shimmer(&working, st.spin_tick, &p) } else { working.clone() };
                let raw_status = if color_enabled { format!("{}{indicator}{} {painted}{}{}{tail}{}", p.brand, p.reset, p.reset, p.dim, p.reset) } else { format!("{indicator} {working}{tail}") };
                status_line = clip_line(&raw_status, safe_cols, color_enabled);
            }
        } else if let Some(notice) = &st.status_notice {
            let clean_notice = super::text::sanitize_single_line(notice);
            let text = clip(&clean_notice);
            let tone = notice_tone(&clean_notice, &p);
            status_line = if color_enabled { format!("{tone}{text}{}", p.reset) } else { text };
        } else if !st.attached_images.is_empty() || !st.pasted_texts.is_empty() {
            let mut items: Vec<String> = st.attached_images.iter().map(|(k, path)| format!("{k} {}", super::text::basename(path))).collect();
            items.extend(st.pasted_texts.keys().cloned());
            let text = clip(&format!("attached  {}   backspace removes", items.join("  ")));
            status_line = if color_enabled { format!("{}{text}{}", p.dim, p.reset) } else { text };
        } else if st.goal.as_ref().map(|g| g.status != "complete").unwrap_or(false) {
            let text = clip(&format!("{} {goal_indicator}   {}", glyph::BRAND, st.goal.as_ref().unwrap().objective));
            status_line = if color_enabled { format!("{goal_tone}{text}{}", p.reset) } else { text };
        } else if !(current_input.starts_with('/') && !current_input.chars().any(char::is_whitespace)) {
            let values: Vec<String> = st.tool_states.values().cloned().collect();
            let text = clip(&values[values.len().saturating_sub(2)..].join("   "));
            status_line = if color_enabled && !text.is_empty() { format!("{}{text}{}", p.dim, p.reset) } else { text };
        }

        // Mode badge (Claude Code style): quiet in `ask`, loud when approvals are relaxed.
        let (mode_badge, mode_tone): (String, &'static str) = match st.mode_label.as_str() {
            "auto" => ("» auto-approve on".into(), p.warn),
            "yolo" => ("▲ yolo (unrestricted)".into(), p.err),
            "plan" => ("◇ plan mode".into(), p.brand),
            "read-only" | "readonly" => ("◌ read-only".into(), p.info),
            other => (other.to_string(), p.dim),
        };
        let quiet_mode = matches!(st.mode_label.as_str(), "ask" | "");
        let model_group: Vec<Seg> = {
            let effort = st.effort_label.trim();
            let text = if effort.is_empty() || effort == "default" { model_name.clone() } else { format!("{model_name} · {effort}") };
            if text.trim().is_empty() {
                vec![]
            } else {
                vec![seg(text, p.dim)]
            }
        };

        if compact_chrome {
            // Compact layout for narrow/short (<48x12) mobile/split viewports: no frame, the
            // prompt sits on a tinted band so it still reads as the input area.
            let (slash_matches, slash_sel) = Self::slash_popup(st);
            if status_line.is_empty() {
                if let Some(cmd) = slash_matches.get(slash_sel) {
                    let hint = clip(&format!("› {cmd}  {}", slash_description(cmd)));
                    status_line = if color_enabled { format!("{}{hint}{}", p.brand_bold, p.reset) } else { hint };
                } else if let Some(popup) = Self::mention_popup(st) {
                    if let Some((name, desc)) = popup.items.get(popup.selected) {
                        let hint = clip(&format!("› {}{name}  {desc}", popup.sigil));
                        status_line = if color_enabled { format!("{}{hint}{}", p.brand_bold, p.reset) } else { hint };
                    }
                }
            }
            let left_compact = vec![seg(mode_badge.clone(), mode_tone)];
            let footer = Self::band(&left_compact, vec![token_group, model_group, Self::context_group(st, &p)], safe_cols, &p);
            let height = rows.saturating_sub(3).max(1);
            st.max_scroll = all.len().saturating_sub(height);
            st.scroll = st.scroll.min(st.max_scroll);
            let end = all.len() - st.scroll;
            let mut body: Vec<String> = all[end.saturating_sub(height)..end].to_vec();
            Self::select_body(st, &mut body, end.saturating_sub(height), 0, safe_cols);
            while st.native.is_none() && body.len() < height {
                body.push(String::new());
            }
            let follow = st.scroll > 0 && st.selection.is_none() && st.history_search.is_none() && st.native.is_none();
            status_line = st.follow_control.render(safe_cols, body.len(), follow, &p).unwrap_or(status_line);
            let label = if asking { clip(&st.label) } else { "› ".to_string() };
            let label_cols = visible_length(&label);
            let (before, shown) = Self::slice_input_window(st, safe_cols.saturating_sub(label_cols).max(1));
            let shown_fit = slice_visible(&shown, safe_cols.saturating_sub(label_cols));
            let label_styled = if color_enabled {
                if asking {
                    format!("{}{}{label}{}", p.warn, p.bold, p.reset)
                } else {
                    format!("{}{label}{}", p.brand_bold, p.reset)
                }
            } else {
                label
            };
            let content = if st.input.is_empty() && !asking {
                let hint = slice_visible(if busy { "steer or queue" } else { "Ask goon anything" }, safe_cols.saturating_sub(label_cols));
                if color_enabled {
                    format!("{}{hint}{}", p.dim, p.reset)
                } else {
                    String::new()
                }
            } else {
                shown_fit
            };
            let prompt = clip_line(&format!("{label_styled}{content}"), safe_cols, color_enabled);
            let prompt = if p.surface.is_empty() || !color_enabled {
                prompt
            } else {
                let fill = " ".repeat(safe_cols.saturating_sub(visible_length(&prompt)));
                format!("{}{}{fill}{}", p.surface, prompt.replace(p.reset, &format!("{}{}", p.reset, p.surface)), p.reset)
            };
            let cursor_col = columns.min(label_cols + visible_length(&before) + 1);
            let mut lines = body;
            lines.push(status_line);
            lines.push(prompt);
            lines.push(footer);
            let cursor = if st.pending.is_some() || !st.input.is_empty() { format!("\x1b[{};{cursor_col}H\x1b[?25h", lines.len() - 1) } else { String::new() };
            self.flush_screen_lines(st, lines, columns, rows, cursor);
            return;
        }

        let queue_lines: Vec<String> = st.queued_prompts[st.queued_prompts.len().saturating_sub(3)..]
            .iter()
            .map(|q| format!("{}{}{}", p.dim, clip(&format!("  ↳ queued  {}   alt+↑ edit", q.text.split_whitespace().collect::<Vec<_>>().join(" "))), p.reset))
            .collect();
        // Composer card (Claude Code style): `╭─╮ │ › draft │ ╰─╯`, then one footer line.
        let inner_cols = safe_cols.saturating_sub(4).max(1);
        let prompt_label = if asking { slice_visible(&st.label, inner_cols.saturating_sub(1).max(1)) } else { "› ".to_string() };
        let label_cols = visible_length(&prompt_label);
        let input_cols = inner_cols.saturating_sub(label_cols).max(1);
        let (editor_lines, editor_cursor_row, editor_cursor_col) = Self::composer_lines(st, input_cols, rows.saturating_sub(8 + queue_lines.len()).clamp(1, 8));
        let fixed_rows = 1 + queue_lines.len() + 3 + editor_lines.len();
        let mut popup_lines: Vec<String> = vec![];
        let (matches, selected) = Self::slash_popup(st);
        if st.shortcuts_open {
            let pairs: [(&str, &str); 13] = [
                ("enter", "send · steer while working"),
                ("tab", "queue a message while working"),
                ("shift+enter", "newline (also ctrl+j)"),
                ("esc", "interrupt · esc esc rewinds the last prompt"),
                ("shift+tab", "cycle ask → auto → plan → read-only"),
                ("alt+. / alt+,", "raise / lower reasoning effort"),
                ("ctrl+r / ctrl+s", "search prompt history · ↑↓ recall"),
                ("ctrl+a / ctrl+e", "line start / end · alt+b/f by word"),
                ("ctrl+u/k/w/y", "cut line / to end / word · paste cut"),
                ("ctrl+g", "edit the draft in $EDITOR"),
                ("ctrl+t", "full transcript · drag to select, ctrl+o copy"),
                ("/ @ $ !", "commands · files · skills · shell"),
                ("ctrl+c ×2", "quit"),
            ];
            let room = rows.saturating_sub(fixed_rows + 2);
            if room > 0 {
                popup_lines.push(format!("{}{}{}", p.bold, clip("  Shortcuts · ? or esc to close"), p.reset));
            }
            let key_cols = pairs.iter().map(|(k, _)| visible_length(k)).max().unwrap_or(0) + 2;
            for (key, what) in pairs.into_iter().take(room.saturating_sub(1)) {
                let plain = clip(&format!("  {key:<key_cols$}{what}"));
                let split = (2 + key_cols).min(plain.len());
                let split = (split..=plain.len()).find(|i| plain.is_char_boundary(*i)).unwrap_or(plain.len());
                let (head, tail) = plain.split_at(split);
                popup_lines.push(if color_enabled { format!("{}{head}{}{}{tail}{}", p.info, p.reset, p.dim, p.reset) } else { plain.clone() });
            }
        } else if !matches.is_empty() {
            let room = rows.saturating_sub(fixed_rows + 3).min(8);
            let shown_count = if matches.len() > room { room.saturating_sub(1) } else { matches.len() };
            let start = (selected + 1).saturating_sub(shown_count).min(matches.len().saturating_sub(shown_count));
            let windowed = &matches[start..(start + shown_count).min(matches.len())];
            let name_cols = windowed.iter().map(|c| c.len()).max().unwrap_or(0).max(8) + 2;
            for (offset, cmd) in windowed.iter().enumerate() {
                let is_sel = start + offset == selected;
                let marker = if is_sel { "› " } else { "  " };
                let plain = clip(&format!("{marker}{cmd:<name_cols$}{}", slash_description(cmd)));
                let name_end = (2 + name_cols).min(plain.len());
                let (head, tail) = plain.split_at(plain.char_indices().map(|(i, _)| i).chain(std::iter::once(plain.len())).find(|i| *i >= name_end).unwrap_or(plain.len()));
                popup_lines.push(if is_sel { format!("{}{head}{}{}{tail}{}", p.brand_bold, p.reset, p.default_fg, p.reset) } else { format!("{}{head}{}{}{tail}{}", p.info, p.reset, p.dim, p.reset) });
            }
            if matches.len() > shown_count && room > 0 {
                popup_lines.push(format!("{}{}{}", p.dim, clip(&format!("  {} of {}   ↑↓ select   tab accept", selected + 1, matches.len())), p.reset));
            }
        } else if let Some(popup) = Self::mention_popup(st) {
            let room = rows.saturating_sub(fixed_rows + 3).min(8);
            let count = popup.items.len().min(room.saturating_sub(1));
            let start = (popup.selected + 1).saturating_sub(count);
            for (index, (name, description)) in popup.items.iter().enumerate().skip(start).take(count) {
                let selected = index == popup.selected;
                let marker = if selected { "›" } else { " " };
                let label = clip(&format!("{marker} {}{name}  {description}", popup.sigil));
                popup_lines.push(format!("{}{label}{}", if selected { p.brand_bold } else { p.dim }, p.reset));
            }
            if room > 0 {
                popup_lines.push(format!("{}{}{}", p.dim, clip(&format!("  {} of {}   ↑↓ select   tab/enter accept   esc dismiss", popup.selected + 1, popup.items.len())), p.reset));
            }
        }
        let chrome = fixed_rows + popup_lines.len();
        let height = rows.saturating_sub(chrome).max(2);
        st.max_scroll = all.len().saturating_sub(height);
        st.scroll = st.scroll.min(st.max_scroll);
        let end = all.len() - st.scroll;
        let mut body: Vec<String> = all[end.saturating_sub(height)..end].to_vec();
        Self::select_body(st, &mut body, end.saturating_sub(height), 0, safe_cols);
        while st.native.is_none() && body.len() < height {
            body.push(String::new());
        }
        let follow = st.scroll > 0 && st.selection.is_none() && st.history_search.is_none() && st.native.is_none();
        status_line = st.follow_control.render(safe_cols, body.len(), follow, &p).unwrap_or(status_line);

        // The frame is amber while goon waits for an answer and sky for a `!` shell command.
        let awaiting = asking && st.pending.is_some();
        let shell = !asking && current_input.starts_with('!');
        let border = if awaiting {
            p.warn
        } else if shell {
            p.info
        } else {
            p.dim
        };
        let width = inner_cols + 4;
        let top = format!("{border}╭{}╮{}", "─".repeat(width.saturating_sub(2)), p.reset);
        let bottom = format!("{border}╰{}╯{}", "─".repeat(width.saturating_sub(2)), p.reset);
        let empty = st.input.is_empty() && !asking;
        // The longest hint that fits whole; never cut an item in half.
        let placeholder_options: &[&str] = if busy {
            &["Enter to steer · Tab to queue · Esc to interrupt", "Enter steer · Tab queue · Esc stop", "Enter steer · Tab queue", "steer or queue"]
        } else {
            &["Ask goon anything · / commands · @ files · ! shell", "Ask goon anything · / commands · @ files", "Ask goon anything · / commands", "Ask goon anything", "Ask"]
        };
        let placeholder_text = placeholder_options.iter().find(|o| visible_length(o) <= input_cols).copied().unwrap_or("");
        let placeholder = if empty { placeholder_text.to_string() } else { String::new() };
        let label_styled = if asking {
            format!("{}{}{prompt_label}{}", p.warn, p.bold, p.reset)
        } else if shell {
            format!("{}{prompt_label}{}", p.info, p.reset)
        } else {
            format!("{}{prompt_label}{}", p.brand_bold, p.reset)
        };
        let prompt_lines: Vec<String> = editor_lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let label = if i == 0 { label_styled.clone() } else { " ".repeat(label_cols) };
                let content = if empty { format!("{}{placeholder}{}", p.dim, p.reset) } else { line.clone() };
                let used = label_cols + if empty { visible_length(&placeholder) } else { visible_length(line) };
                let fill = " ".repeat(inner_cols.saturating_sub(used));
                format!("{border}│{} {label}{content}{fill} {border}│{}", p.reset, p.reset)
            })
            .collect();
        let mut left: Vec<Seg> = if awaiting {
            vec![seg(format!("  enter submit · esc {}", st.esc_hint), p.dim)]
        } else if !quiet_mode {
            vec![seg(format!("  {mode_badge}"), mode_tone), seg(" · shift+tab to cycle", p.dim)]
        } else if busy {
            vec![seg("  enter steer · tab queue", p.dim)]
        } else {
            vec![seg("  ? for shortcuts", p.dim)]
        };
        if st.view_mode == "transcript" {
            left.push(seg(" · transcript view", p.dim));
        }
        let place: Vec<Seg> =
            if st.workspace_name.is_empty() { vec![] } else { vec![seg(if st.branch.is_empty() { st.workspace_name.clone() } else { format!("{} ({})", st.workspace_name, st.branch) }, p.dim)] };
        let footer = Self::band(&left, vec![place, todo_group, goal_group, token_group, model_group, Self::context_group(st, &p)], safe_cols, &p);
        let mut lines = body;
        lines.push(status_line);
        lines.extend(popup_lines);
        lines.extend(queue_lines);
        lines.push(top);
        let prompt_start = lines.len() + 1;
        lines.extend(prompt_lines);
        lines.push(bottom);
        lines.push(footer);
        let prompt_row = prompt_start + editor_cursor_row;
        let cursor_col = safe_cols.min(2 + label_cols + editor_cursor_col + 1);
        let cursor = if st.pending.is_some() || !st.input.is_empty() { format!("\x1b[{prompt_row};{cursor_col}H\x1b[?25h") } else { String::new() };
        self.flush_screen_lines(st, lines, columns, rows, cursor);
    }

    /// Wrap the actual editable draft, not markdown, and keep the caret's row in view.
    fn composer_lines(st: &State, cols: usize, max_rows: usize) -> (Vec<String>, usize, usize) {
        use unicode_segmentation::UnicodeSegmentation;
        let text: String = st.input.iter().collect();
        let mut lines = vec![String::new()];
        let (mut col, mut offset) = (0, 0);
        let (mut cursor_row, mut cursor_col) = (0, 0);
        for g in text.graphemes(true) {
            let rendered = if st.masked { "•" } else { g };
            let width = visible_length(rendered).min(cols);
            if g != "\n" && col + width > cols {
                lines.push(String::new());
                col = 0;
            }
            if offset == st.cursor {
                cursor_row = lines.len() - 1;
                cursor_col = col;
            }
            if g == "\n" {
                lines.push(String::new());
                col = 0;
            } else {
                lines.last_mut().unwrap().push_str(rendered);
                col += width;
            }
            offset += g.chars().count();
        }
        if st.cursor == st.input.len() {
            if col >= cols {
                lines.push(String::new());
                col = 0;
            }
            cursor_row = lines.len() - 1;
            cursor_col = col;
        }
        let start = (cursor_row + 1).saturating_sub(max_rows);
        (lines.into_iter().skip(start).take(max_rows).collect(), cursor_row - start, cursor_col)
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;

    #[test]
    fn escaped_pipe_parsing_respects_backslash_parity_and_empty_cells() {
        assert_eq!(split_table_cells(r"| `a\|b` | café |"), ["`a|b`", "café"]);
        assert_eq!(split_table_cells(r"| a\\| b |"), [r"a\\", "b"]);
        assert_eq!(split_table_cells(r"| a\\\|b |"), [r"a\\|b"]);
        assert_eq!(split_table_cells("| | value | |"), ["", "value", ""]);
        assert_eq!(split_table_cells("||"), [""]);
        assert!(table_separator("|:--|--:|:--:|"));
        assert!(table_separator("| - | - |"));
        assert!(!table_separator("| : | -- |"), "a colon alone is content, not a delimiter cell");
        assert!(!table_separator("| | -- |"), "empty delimiter cell is invalid");
        assert!(!table_separator(r"| --\|-- | -- |"), "literal pipes cannot join delimiter columns");
    }
}

/// Detects a frame whose top region is the previous frame's top region moved up by `shift` rows.
/// Returns `(region_rows, shift)` when scrolling saves a meaningful amount of repainting.
fn scroll_shift(previous: &[String], next: &[String]) -> Option<(usize, usize)> {
    let height = previous.len().min(next.len());
    if height < 8 {
        return None;
    }
    let mut best: Option<(usize, usize)> = None;
    for shift in 1..=(height / 2).min(24) {
        // Rows that survive the scroll: next[i] == previous[i + shift], counted from the top.
        let kept = (0..height - shift).take_while(|&i| next[i] == previous[i + shift]).count();
        // Only worth it when most of the region moves, and the row already in place doesn't.
        if kept >= 6 && kept * 2 >= height - shift && kept > best.map_or(0, |(region, s)| region - s) && next[0] != previous[0] {
            best = Some((kept + shift, shift));
        }
    }
    best
}

#[cfg(test)]
mod scroll_tests {
    use super::scroll_shift;

    fn frame(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_a_body_that_moved_up_and_leaves_static_frames_alone() {
        let body: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
        let mut prev = body[0..20].to_vec();
        prev.extend(frame(&["status", "╭──╮", "│ › │", "╰──╯", "footer"]));
        let mut next = body[3..23].to_vec();
        next.extend(frame(&["status 2", "╭──╮", "│ › │", "╰──╯", "footer"]));
        assert_eq!(scroll_shift(&prev, &next), Some((20, 3)));
        assert_eq!(scroll_shift(&prev, &prev), None, "nothing moved");
        let mut edited = prev.clone();
        edited[5] = "changed".into();
        assert_eq!(scroll_shift(&prev, &edited), None, "an in-place edit is a patch, not a scroll");
        assert_eq!(scroll_shift(&prev[..5], &next[..5]), None, "tiny screens repaint");
    }
}
