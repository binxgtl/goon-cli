//! Transcript parsing/styling and frame layout. Every row is wrapped to the terminal width
//! before styling (prefixes included), so no emitted row can exceed it.

use super::text::{format_compact_duration, format_inline_markdown, format_inline_markdown_default, format_token_count, re, slice_ansi_line, slice_visible, style_box_line, visible_length, wrap_chunks, wrap_terminal, BOLD, BRAND, BRAND_BOLD, DEFAULT_FG, DIM, ERR, INFO, OK, RESET, WARN};
use super::{glyph, slash_description, BodyCache, Block, PickerState, Row, RowKind, State, Tui, SPINNER, USER_MARK};
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
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

fn displayed_cell(cell: &str) -> String {
    let c = re!(r"\*\*([^*]+)\*\*").replace_all(cell, "${1}");
    let c = re!(r"`([^`\n]+)`").replace_all(&c, "${1}");
    let c = re!(r"~~([^~]+)~~").replace_all(&c, "${1}");
    re!(r"\[([^\]]+)\]\(([^)]+)\)").replace_all(&c, "${1} (${2})").to_string()
}

fn table_separator(trimmed: &str) -> bool {
    re!(r"^\|[\s:|-]+\|$").is_match(trimmed)
}

fn plain_row(row: &Row, cols: usize) -> String {
    match row.kind {
        RowKind::Blank => String::new(),
        RowKind::CodeHead => slice_visible(&format!("  ─ {}", row.text), cols),
        _ => format!("{}{}", row.pre, row.text),
    }
}

fn cell_tone(glyph: &str) -> &'static str {
    match glyph {
        "○" | "≡" | "↻" | "↺" => DIM,
        "●" => OK,
        "✗" => ERR,
        "±" => DEFAULT_FG,
        "◈" | "◇" => BRAND,
        "▲" => WARN,
        "◌" => DIM,
        _ => DIM,
    }
}

impl Tui {
    pub(crate) fn render_styled_body_lines(&self, st: &mut State, columns: usize, rich: bool) -> Vec<String> {
        let cols = columns.max(4);
        let show_reasoning = st.view_mode != "normal" || st.in_transcript_overlay;
        let transcript = st.transcript.clone();
        let reuse = st.body_cache.as_ref().map(|c| c.cols == cols && c.rich == rich && c.show_reasoning == show_reasoning && transcript.len() >= c.source.len() && transcript.starts_with(&c.source)).unwrap_or(false);
        let cache = if reuse { st.body_cache.take() } else { None };
        let mut state = cache.as_ref().map(|c| c.state).unwrap_or_default();
        let mut rows: Vec<Row> = cache.as_ref().map(|c| c.rows.clone()).unwrap_or_default();
        let base_styled: Vec<String> = cache.as_ref().map(|c| c.styled.clone()).unwrap_or_default();
        let base_len = cache.as_ref().map(|c| c.source.len()).unwrap_or(0);
        let raw_lines: Vec<&str> = transcript[base_len..].split('\n').collect();

        // Checkpoint for the next frame: the last (possibly streaming) line, moved back past any
        // table rows because a growing table changes the widths of earlier rows.
        let mut checkpoint = raw_lines.len() - 1;
        while checkpoint > 0 && is_table_row(raw_lines[checkpoint - 1]) {
            checkpoint -= 1;
        }
        let mut checkpoint_rows = rows.len();
        let mut checkpoint_state = state;

        // Pre-scan contiguous Markdown table blocks so all rows of a table share column widths.
        let mut table_widths: std::collections::HashMap<usize, Vec<usize>> = Default::default();
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
            for k in i..j {
                table_widths.insert(k, widths.clone());
            }
            i = j;
        }

        fn blank(rows: &mut Vec<Row>) {
            if rows.last().map(|r| r.kind != RowKind::Blank).unwrap_or(false) {
                rows.push(Row { kind: RowKind::Blank, pre: String::new(), text: String::new(), glyph: None, sign: None, level: 0 });
            }
        }
        let wrap_into = |rows: &mut Vec<Row>, kind: RowKind, text: &str, first: &str, cont: &str, glyph: Option<String>, sign: Option<char>, level: usize, words: bool| {
            let chunks = wrap_chunks(text, cols.saturating_sub(visible_length(first)), cols.saturating_sub(visible_length(cont)), words);
            for (i, chunk) in chunks.into_iter().enumerate() {
                rows.push(Row { kind, pre: if i == 0 { first.to_string() } else { cont.to_string() }, text: chunk, glyph: glyph.clone(), sign, level });
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
                let n = cols.saturating_sub(base.chars().count() + 2).clamp(1, 48);
                rows.push(Row { kind: RowKind::Rule, pre: first_pre(base), text: "─".repeat(n), glyph: None, sign: None, level: 0 });
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
                let trimmed = raw_line.trim();
                let widths = table_widths.get(&line_index).cloned().unwrap_or_default();
                let avail = cols.saturating_sub(base.chars().count()).max(1);
                if table_separator(trimmed) {
                    let grid = format!("├{}┤", widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join("┼"));
                    rows.push(Row { kind: RowKind::Table, pre: first_pre(base), text: slice_visible(&grid, avail), glyph: None, sign: None, level: 0 });
                } else {
                    let cells: Vec<String> = split_table_cells(trimmed).iter().enumerate().map(|(i, c)| format!("{c}{}", " ".repeat(widths.get(i).copied().unwrap_or(0).saturating_sub(visible_length(&displayed_cell(c)))))).collect();
                    rows.push(Row { kind: RowKind::Table, pre: first_pre(base), text: slice_visible(&format!("│ {} │", cells.join(" │ ")), avail), glyph: None, sign: None, level: 0 });
                }
                return;
            }
            let indent_len = raw_line.len() - raw_line.trim_start().len();
            let indent = &raw_line[..indent_len];
            let both = format!("{base}{indent}");
            wrap_into(rows, para, &raw_line[indent_len..], &first_pre(&both), &both, None, None, 0, true);
        };

        for (line_index, raw_line) in raw_lines.iter().enumerate() {
            let raw_line = *raw_line;
            if line_index == checkpoint {
                checkpoint_rows = rows.len();
                checkpoint_state = state;
            }
            let trimmed = raw_line.trim();
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
                    rows.push(Row { kind: RowKind::ReasoningHead, pre: String::new(), text: format!("{} Thinking", glyph::THINKING), glyph: None, sign: None, level: 0 });
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
                blank(&mut rows);
            }
            if state.block == Block::Reasoning && !show_reasoning {
                continue;
            }
            if trimmed.starts_with("```") {
                if state.in_code {
                    state.in_code = false;
                    blank(&mut rows);
                } else {
                    state.in_code = true;
                    state.in_diff = false;
                    state.answer_first = false;
                    let label = trimmed[3..].trim();
                    rows.push(Row { kind: RowKind::CodeHead, pre: String::new(), text: if label.is_empty() { "code".into() } else { label.to_string() }, glyph: None, sign: None, level: 0 });
                }
                continue;
            }
            if state.in_code {
                wrap_into(&mut rows, RowKind::Code, raw_line.replace('\t', "  ").trim_end(), "    ", "    ", None, None, 0, false);
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
                blank(&mut rows);
                continue;
            }
            if state.block == Block::Reasoning {
                wrap_into(&mut rows, RowKind::Reasoning, raw_line.trim_end(), "  ", "  ", None, None, 0, true);
                continue;
            }
            if state.block == Block::Answer {
                let lead = if state.answer_first { Some(format!("{} ", glyph::ANSWER)) } else { None };
                state.answer_first = false;
                push_markdown(&mut rows, raw_line.trim_end(), "  ", lead.as_deref(), RowKind::Answer, line_index);
                continue;
            }
            // Normal block: tool cells, diffs, CLI output, banner.
            if re!(r"^◆ goon v\S+\s*$").is_match(raw_line) {
                blank(&mut rows);
                rows.push(Row { kind: RowKind::BannerTitle, pre: String::new(), text: trimmed.to_string(), glyph: None, sign: None, level: 0 });
                state.in_banner = true;
                continue;
            }
            if state.in_banner {
                if raw_line.starts_with("  ") {
                    wrap_into(&mut rows, RowKind::Banner, trimmed, "  ", "    ", None, None, 0, true);
                    continue;
                }
                state.in_banner = false;
            }
            if let Some(cell) = re!(r"^([○●✗±◈≡▲◌↻↺◇]) (\S.*)$").captures(raw_line) {
                blank(&mut rows);
                state.in_diff = &cell[1] == glyph::EDIT || re!(r"^(?:Edited|Added|Deleted|Wrote|Created|Updated)\b").is_match(&cell[2]);
                wrap_into(&mut rows, RowKind::Cell, cell[2].trim_end(), &format!("{} ", &cell[1]), "  ", Some(cell[1].to_string()), None, 0, true);
                continue;
            }
            if re!(r"^(?:--- |\+\+\+ |@@ )").is_match(raw_line) {
                state.in_diff = true;
                rows.push(Row { kind: RowKind::DiffMeta, pre: "  ".into(), text: slice_visible(raw_line.trim_end(), cols.saturating_sub(2).max(1)), glyph: None, sign: None, level: 0 });
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
                    wrap_into(&mut rows, RowKind::Diff, &code, &pre, &cont, None, Some(sign), 0, false);
                    continue;
                }
                if re!(r"^\s*(?:⋮|… \+\d+ more|\.\.\. \(\+\d+ more)").is_match(raw_line) || raw_line.starts_with("\\ No newline") {
                    rows.push(Row { kind: RowKind::DiffMeta, pre: "    ".into(), text: slice_visible(trimmed, cols.saturating_sub(4).max(1)), glyph: None, sign: None, level: 0 });
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
                rows.push(Row { kind: RowKind::Box, pre: String::new(), text: slice_visible(raw_line.trim_end(), cols), glyph: None, sign: None, level: 0 });
                continue;
            }
            if raw_line.starts_with("Type /help") {
                wrap_into(&mut rows, RowKind::Hint, raw_line.trim_end(), "", "", None, None, 0, true);
                continue;
            }
            push_markdown(&mut rows, raw_line.trim_end(), "", None, RowKind::Normal, line_index);
        }

        let cached = cache.as_ref().map(|c| c.rows.len()).unwrap_or(0);
        let mut styled: Vec<String> = base_styled[..cached.min(base_styled.len())].to_vec();
        for row in &rows[cached..] {
            styled.push(if rich { Self::style_row(row, cols) } else { plain_row(row, cols) });
        }
        // Remember everything before the checkpoint line for the next frame.
        let consumed: usize = raw_lines[..checkpoint].iter().map(|l| l.len() + 1).sum();
        st.body_cache = Some(BodyCache {
            cols,
            rich,
            show_reasoning,
            source: transcript[..base_len + consumed].to_string(),
            state: checkpoint_state,
            rows: rows[..checkpoint_rows].to_vec(),
            styled: styled[..checkpoint_rows].to_vec(),
        });
        styled
    }

    fn style_row(row: &Row, cols: usize) -> String {
        let md = |text: &str, base: &str| format!("{base}{}{RESET}", format_inline_markdown(text, &format!("{RESET}{base}")));
        let prefix = |pre: &str, tone: &str| if pre.trim().is_empty() { pre.to_string() } else { format!("{tone}{pre}{RESET}") };
        let answer_pre = |pre: &str| {
            if let Some(rest) = pre.strip_prefix(glyph::ANSWER) {
                format!("{BRAND_BOLD}{}{RESET}{}", glyph::ANSWER, prefix(rest, DIM))
            } else {
                prefix(pre, DIM)
            }
        };
        match row.kind {
            RowKind::Blank => String::new(),
            RowKind::User => {
                let pre = if row.pre.trim().is_empty() { row.pre.clone() } else { format!("{BRAND_BOLD}{}{RESET}", row.pre) };
                format!("{pre}{BOLD}{}{RESET}", format_inline_markdown(&row.text, &format!("{RESET}{BOLD}")))
            }
            RowKind::Answer => format!("{}{}", answer_pre(&row.pre), md(&row.text, DEFAULT_FG)),
            RowKind::Normal => format!("{}{}", prefix(&row.pre, DIM), format_inline_markdown_default(&row.text)),
            RowKind::Heading => {
                let tone = if row.level == 1 { format!("{BOLD}\x1b[4m") } else { BOLD.to_string() };
                format!("{}{}", answer_pre(&row.pre), md(&row.text, &tone))
            }
            RowKind::Quote => {
                let pre = if let Some(rest) = row.pre.strip_prefix(glyph::ANSWER) { format!("{BRAND_BOLD}{}{RESET}{DIM}{rest}{RESET}", glyph::ANSWER) } else { format!("{DIM}{}{RESET}", row.pre) };
                format!("{pre}{}", md(&row.text, &format!("{DIM}\x1b[3m")))
            }
            RowKind::Rule => format!("{}{DIM}{}{RESET}", answer_pre(&row.pre), row.text),
            RowKind::Table => {
                if row.text.starts_with('├') {
                    return format!("{}{DIM}{}{RESET}", answer_pre(&row.pre), row.text);
                }
                format!("{}{}", answer_pre(&row.pre), row.text.split('│').map(format_inline_markdown_default).collect::<Vec<_>>().join(&format!("{DIM}│{RESET}")))
            }
            RowKind::ReasoningHead => format!("{DIM}\x1b[3m{}{RESET}", row.text),
            RowKind::Reasoning => format!("{}{}", row.pre, md(&row.text, &format!("{DIM}\x1b[3m"))),
            RowKind::CodeHead => format!("  {DIM}{}{RESET}", slice_visible(&row.text, cols.saturating_sub(2).max(1))),
            RowKind::Code => format!("  {DIM}│{RESET} {}", row.text),
            RowKind::Cell => Self::style_cell(row),
            RowKind::Child => Self::style_child(row),
            RowKind::Diff => {
                let tone = match row.sign {
                    Some('+') => OK,
                    Some('-') => ERR,
                    _ => DIM,
                };
                let num_len = row.pre.char_indices().find(|(_, c)| !(c.is_whitespace() || c.is_ascii_digit())).map(|(i, _)| i).unwrap_or(row.pre.len());
                let num_end = {
                    // `^(\s*\d*)`: leading spaces then digits.
                    let spaces = row.pre.len() - row.pre.trim_start().len();
                    let digits = row.pre[spaces..].chars().take_while(|c| c.is_ascii_digit()).count();
                    (spaces + digits).min(num_len)
                };
                format!("{DIM}{}{RESET}{tone}{}{}{RESET}", &row.pre[..num_end], &row.pre[num_end..], row.text)
            }
            RowKind::DiffMeta => format!("{}{}{}{RESET}", row.pre, if row.text.starts_with("@@") { INFO } else { DIM }, row.text),
            RowKind::Box => style_box_line(&row.text),
            RowKind::Hint => format!("{DIM}{}{RESET}", row.text),
            RowKind::BannerTitle => {
                let version = re!(r"^◆ goon (v\S+)").captures(&row.text).map(|c| c[1].to_string()).unwrap_or_default();
                format!("{BRAND_BOLD}{}{RESET} {BOLD}goon{RESET} {DIM}{version}{RESET}", glyph::BRAND)
            }
            RowKind::Banner => {
                if re!(r"^/help\b").is_match(&row.text) {
                    let painted = re!(r"(/help|shift\+tab|(?:^|\s)[@$!]) ").replace_all(&row.text, |c: &regex::Captures| {
                        let tok = &c[1];
                        if tok == "/help" || tok == "shift+tab" {
                            format!("{RESET}{tok}{DIM} ")
                        } else {
                            format!("{RESET}{tok}{DIM} ")
                        }
                    });
                    let painted = re!(r"(shift\+tab)$").replace_all(&painted, format!("{RESET}${{1}}{DIM}").as_str());
                    return format!("{}{DIM}{painted}{RESET}", row.pre);
                }
                let painted = re!(r"(^|\s)(model|effort|mode|gateway|workspace|session|branch) ").replace_all(&row.text, format!("${{1}}{DIM}${{2}}{RESET} ").as_str());
                format!("{}{painted}", row.pre)
            }
        }
    }

    fn style_cell(row: &Row) -> String {
        let glyph = row.glyph.clone().unwrap_or_else(|| "•".into());
        let tone = cell_tone(&glyph);
        let with_durations = |text: &str, base: &str| {
            let t = re!(r"\((?:\d+m )?\d+s\)").replace_all(text, |c: &regex::Captures| format!("{DIM}{}{RESET}{base}", &c[0]));
            re!(r"\(\+(\d+) -(\d+)\)").replace_all(&t, |c: &regex::Captures| format!("{DIM}({RESET}{OK}+{}{RESET} {ERR}-{}{RESET}{DIM}){RESET}{base}", &c[1], &c[2])).to_string()
        };
        if row.pre.trim().is_empty() {
            return format!("{}{DIM}{}{RESET}", row.pre, with_durations(&row.text, DIM));
        }
        let (verb, rest) = match re!(r"^(\S+)(.*)$").captures(&row.text) {
            Some(c) => (c[1].to_string(), c[2].to_string()),
            None => (row.text.clone(), String::new()),
        };
        let glyph_out = format!("{tone}\x1b[1m{glyph}{RESET} ");
        if glyph == glyph::ALLOW {
            return format!("{glyph_out}{WARN}\x1b[1m{}{RESET}", row.text);
        }
        if glyph == glyph::COMPACT {
            return format!("{glyph_out}{DIM}{}{RESET}", row.text);
        }
        if glyph == glyph::PROMPT {
            return format!("{glyph_out}{BRAND_BOLD}{}{RESET}", row.text);
        }
        let verb_tone = if glyph == glyph::FAILED { format!("{ERR}\x1b[1m") } else { BOLD.to_string() };
        let rest_base = if glyph == glyph::RAN { INFO } else if glyph == glyph::EDIT { DEFAULT_FG } else { DIM };
        format!("{glyph_out}{verb_tone}{verb}{RESET}{rest_base}{}{RESET}", with_durations(&rest, rest_base))
    }

    fn style_child(row: &Row) -> String {
        let pre = if row.pre.contains('└') { format!("{DIM}{}{RESET}", row.pre) } else { row.pre.clone() };
        let text = &row.text;
        if let Some(c) = re!(r"^(Read|List|Search)\b(.*)$").captures(text) {
            return format!("{pre}{INFO}{}{RESET}{DEFAULT_FG}{}{RESET}", &c[1], &c[2]);
        }
        if let Some(rest) = text.strip_prefix("✓ ") {
            return format!("{pre}{OK}✓{RESET} {DIM}{rest}{RESET}");
        }
        if let Some(rest) = text.strip_prefix("▸ ") {
            return format!("{pre}{BRAND_BOLD}▸ {rest}{RESET}");
        }
        if let Some(rest) = text.strip_prefix("○ ") {
            return format!("{pre}{DIM}○{RESET} {DEFAULT_FG}{rest}{RESET}");
        }
        if let Some(rest) = text.strip_prefix("$ ") {
            return format!("{pre}{DIM}${RESET} {INFO}\x1b[1m{rest}{RESET}");
        }
        if re!(r"^… \+\d+").is_match(text) {
            return format!("{pre}{DIM}{text}{RESET}");
        }
        if let Some(c) = re!(r"^(\d{1,2})\. (.*)$").captures(text) {
            return format!("{pre}{BRAND_BOLD}{}.{RESET} {}", &c[1], format_inline_markdown_default(&c[2]));
        }
        format!("{pre}{DIM}{text}{RESET}")
    }

    /// Row-level differential flush: unchanged rows are never rewritten after the first frame.
    fn flush_screen_lines(&self, st: &mut State, lines: Vec<String>, columns: usize, rows: usize, cursor: String) {
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
        let mut patches = String::new();
        for (i, line) in lines.iter().enumerate() {
            if *line != st.last_frame_rows[i] {
                patches.push_str(&format!("\x1b[{};1H\x1b[0m{line}\x1b[K", i + 1));
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
    fn band(left: &[Seg], right: Vec<Vec<Seg>>, cols: usize, rich: bool) -> String {
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
        let paint = |text: &str, sgr: &str| if rich && !sgr.is_empty() { format!("{sgr}{text}{RESET}") } else { text.to_string() };
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

    fn context_group(st: &State) -> Vec<Seg> {
        let (Some(used), true) = (st.context_used, st.context_window > 0) else { return vec![] };
        let pct = ((used as f64 / st.context_window as f64) * 100.0).round().clamp(0.0, 100.0) as u64;
        let tone = if pct >= 90 { ERR } else if pct >= 70 { WARN } else { DIM };
        vec![seg(format!("ctx {pct}%"), tone)]
    }

    fn picker_lines(state: &mut PickerState, cols: usize, max_rows: usize) -> Vec<String> {
        let mut lines = vec![];
        let tabs_text = (0..state.tabs.len())
            .map(|i| {
                let count = Tui::picker_matches(state, i).len();
                let label = format!("{} {count}", state.tabs[i].label);
                if i == state.tab {
                    format!("{BRAND_BOLD}{label}{RESET}")
                } else {
                    format!("{DIM}{label}{RESET}")
                }
            })
            .collect::<Vec<_>>()
            .join("   ");
        lines.push(slice_ansi_line(&format!("{BOLD}{}{RESET}   {tabs_text}", state.title), cols));
        let matches = Tui::picker_matches(state, state.tab);
        let list_rows = max_rows.saturating_sub(3).max(1);
        if matches.is_empty() {
            let empty = if !state.filter.is_empty() { format!("No match for \"{}\"", state.filter) } else { state.tabs.get(state.tab).and_then(|t| t.empty.clone()).unwrap_or_else(|| "Nothing here yet".into()) };
            lines.push(format!("{DIM}  {}{RESET}", slice_visible(&empty, cols.saturating_sub(2).max(1))));
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
                lines.push(if selected { format!("{BRAND_BOLD}› {RESET}{BOLD}{label}{RESET}{gap}{DIM}{meta}{RESET}") } else { format!("  {label}{gap}{DIM}{meta}{RESET}") });
            }
            if let Some(detail) = matches.get(state.index).and_then(|m| m.detail.clone()) {
                lines.push(format!("{DIM}  {}{RESET}", slice_visible(&crate::term::terminal_text(&detail), cols.saturating_sub(2).max(1))));
            }
        }
        lines
    }

    /// The visible composer slice around the cursor.
    fn slice_input_window(st: &State, avail: usize) -> (String, String) {
        let max_before = avail.saturating_sub(1);
        let chars: Vec<char> = if st.masked { st.input.iter().map(|_| '•').collect() } else { st.input.clone() };
        let mut start = st.cursor.min(chars.len());
        let mut used = 0;
        while start > 0 {
            let w = super::text::width(chars[start - 1]);
            if used + w > max_before {
                break;
            }
            used += w;
            start -= 1;
        }
        let cursor = st.cursor.min(chars.len());
        let before: String = chars[start..cursor].iter().collect();
        let after: String = chars[cursor..(cursor + avail + 4).min(chars.len())].iter().collect();
        let shown = wrap_terminal(&format!("{before}{after}"), avail).into_iter().next().unwrap_or_default();
        (before, shown)
    }

    pub(crate) fn render(&self, st: &mut State) {
        if st.closed {
            return;
        }
        let (columns, rows) = self.size();
        let columns = columns.max(4);
        let rows = rows.max(8);
        let safe_cols = columns.saturating_sub(1).max(3);
        let clip = |s: &str| wrap_terminal(s, safe_cols).into_iter().next().unwrap_or_default();
        let current_input: String = st.input.iter().collect();
        let busy = matches!(st.status.as_str(), "Running" | "Connecting MCP" | "Compacting");
        let indicator = if busy { SPINNER[st.spin_tick % SPINNER.len()] } else { "●" };
        let rich = columns >= 48 && rows >= 12;
        let asking = st.label != "goon> ";
        let title = st.title.clone();
        let parts: Vec<&str> = title.split(" · ").collect();
        let model_name = if parts[0].is_empty() { title.clone() } else { parts[0].to_string() };
        let session_label = if parts.len() > 1 { parts[1..].join(" · ") } else { "session".into() };
        let elapsed = st.turn_started_at.map(|t| format_compact_duration(crate::jsutil::now_millis() - t)).unwrap_or_default();

        let all = self.render_styled_body_lines(st, safe_cols, rich);
        if st.scroll > 0 && st.last_body_total_lines > 0 && all.len() > st.last_body_total_lines {
            st.scroll += all.len() - st.last_body_total_lines;
        }
        st.last_body_total_lines = all.len();

        let active_secs = st.turn_started_at.map(|t| (crate::jsutil::now_millis() - t) as f64 / 1000.0).unwrap_or(0.0);
        let goal_indicator = st.goal.as_ref().map(|g| format_goal_status_indicator(g, active_secs)).unwrap_or_default();
        let goal_tone: &'static str = match st.goal.as_ref().map(|g| g.status.as_str()) {
            Some("active") => BRAND,
            Some("complete") => OK,
            Some("paused") => DIM,
            _ => WARN,
        };
        let goal_group: Vec<Seg> = if goal_indicator.is_empty() { vec![] } else { vec![seg(goal_indicator.clone(), goal_tone)] };
        let todo_group: Vec<Seg> = if st.todo_label.is_empty() { vec![] } else { vec![seg(format!("todo {}", st.todo_label), DIM)] };
        let mut token_group: Vec<Seg> = vec![seg(format!("{} tokens", format_token_count(st.tokens)), DIM)];
        if st.cost_usd > 0.0 {
            token_group.push(seg(format!(" · ${}", crate::jsutil::to_fixed(st.cost_usd, 4)), DIM));
        }

        // A picker (e.g. /resume) replaces the status/popup rows and turns the prompt into its filter.
        if st.picker.is_some() {
            let max_picker_rows = rows.saturating_sub(6).clamp(4, 18);
            let picker_lines = Self::picker_lines(st.picker.as_mut().unwrap(), safe_cols, max_picker_rows);
            let height = rows.saturating_sub(picker_lines.len() + 3).max(1);
            st.max_scroll = all.len().saturating_sub(height);
            st.scroll = 0;
            let mut body: Vec<String> = all[all.len().saturating_sub(height)..].to_vec();
            while body.len() < height {
                body.push(String::new());
            }
            let p = st.picker.as_ref().unwrap();
            let filter = slice_visible(&p.filter, safe_cols.saturating_sub(3).max(1));
            let prompt_line = format!("{BRAND_BOLD}› {RESET}{}", if filter.is_empty() { format!("{DIM}type to filter{RESET}") } else { filter.clone() });
            let hints = format!("←→ tabs   ↑↓ select   enter {}   esc cancel", p.action);
            let footer = if rich { format!("{DIM}{}{RESET}", clip(&hints)) } else { clip(&hints) };
            let mut lines = body;
            lines.extend(picker_lines);
            lines.push(format!("{DIM}{}{RESET}", "─".repeat(safe_cols)));
            lines.push(prompt_line);
            lines.push(footer);
            let cursor = format!("\x1b[{};{}H\x1b[?25h", lines.len() - 1, safe_cols.min(3 + visible_length(&filter)));
            self.flush_screen_lines(st, lines, columns, rows, cursor);
            return;
        }

        // Full-screen transcript (Ctrl+T or /transcript).
        if st.in_transcript_overlay {
            let header = Self::band(&[seg(format!("{} ", glyph::BRAND), BRAND_BOLD), seg("transcript", BOLD), seg(format!("   {session_label}"), DIM)], vec![goal_group.clone(), todo_group.clone()], safe_cols, rich);
            let height = rows.saturating_sub(2).max(2);
            st.max_scroll = all.len().saturating_sub(height);
            st.scroll = st.scroll.min(st.max_scroll);
            let end = all.len() - st.scroll;
            let mut body: Vec<String> = all[end.saturating_sub(height)..end].to_vec();
            while body.len() < height {
                body.push(String::new());
            }
            let mut live: Vec<Seg> = vec![];
            if busy {
                live = vec![seg(format!("{indicator} "), BRAND), seg("working", DEFAULT_FG), seg(format!("  {elapsed}   "), DIM)];
            }
            live.push(seg(format!("line {} of {}", end.max(1), all.len().max(1)), DIM));
            let footer = Self::band(&live, vec![vec![seg("↑↓ scroll   esc back", DIM)]], safe_cols, rich);
            let mut lines = vec![header];
            lines.extend(body);
            lines.push(footer);
            self.flush_screen_lines(st, lines, columns, rows, String::new());
            return;
        }

        // Live status row: only says something when there is something to say.
        let mut status_line = String::new();
        if st.scroll > 0 {
            let text = clip(&format!("↑ {} lines above the latest output   End or Esc returns", st.scroll));
            status_line = if rich { format!("{DIM}{text}{RESET}") } else { text };
        } else if let Some(prime) = &st.esc_primed {
            let text = clip(match prime {
                super::EscPrime::Backtrack => "Press Esc again to rewind and edit the previous prompt",
                super::EscPrime::Clear => "Press Esc again to clear the input",
            });
            status_line = if rich { format!("{WARN}{text}{RESET}") } else { text };
        } else if busy {
            let working = if st.status == "Running" { "Working".to_string() } else { st.status.clone() };
            let tail = format!("  {elapsed}   esc to interrupt");
            status_line = if rich { format!("{BRAND}{indicator}{RESET} {working}{DIM}{}{RESET}", clip(&tail)) } else { clip(&format!("{indicator} {working}{tail}")) };
        } else if !st.attached_images.is_empty() || !st.pasted_texts.is_empty() {
            let mut items: Vec<String> = st.attached_images.iter().map(|(k, p)| format!("{k} {}", super::text::basename(p))).collect();
            items.extend(st.pasted_texts.keys().cloned());
            let text = clip(&format!("attached  {}   backspace removes", items.join("  ")));
            status_line = if rich { format!("{DIM}{text}{RESET}") } else { text };
        } else if let Some(notice) = &st.status_notice {
            let text = clip(notice);
            status_line = if rich { format!("{DIM}{text}{RESET}") } else { text };
        } else if st.goal.as_ref().map(|g| g.status != "complete").unwrap_or(false) {
            let text = clip(&format!("{} {goal_indicator}   {}", glyph::BRAND, st.goal.as_ref().unwrap().objective));
            status_line = if rich { format!("{goal_tone}{text}{RESET}") } else { text };
        } else if !(current_input.starts_with('/') && !current_input.chars().any(char::is_whitespace)) {
            let values: Vec<String> = st.tool_states.values().cloned().collect();
            let text = clip(&values[values.len().saturating_sub(2)..].join("   "));
            status_line = if rich && !text.is_empty() { format!("{DIM}{text}{RESET}") } else { text };
        }

        if !rich {
            // Compact fallback for tiny (<48x12) terminals.
            let footer = clip(&format!("{}  {} tokens  ctrl+c cancel", st.status, crate::jsutil::js_string(&crate::jsutil::num(st.tokens))));
            let height = rows.saturating_sub(3).max(2);
            st.max_scroll = all.len().saturating_sub(height);
            st.scroll = st.scroll.min(st.max_scroll);
            let end = all.len() - st.scroll;
            let mut body: Vec<String> = all[end.saturating_sub(height)..end].to_vec();
            while body.len() < height {
                body.push(String::new());
            }
            let label = clip(&st.label);
            let label_cols = visible_length(&label);
            let (before, shown) = Self::slice_input_window(st, safe_cols.saturating_sub(label_cols).max(1));
            let prompt = wrap_terminal(&format!("{label}{shown}"), safe_cols).into_iter().next().unwrap_or_default();
            let cursor_col = columns.min(label_cols + visible_length(&before) + 1);
            let mut lines = body;
            lines.push(status_line);
            lines.push(prompt);
            lines.push(footer);
            let cursor = if st.pending.is_some() { format!("\x1b[{};{cursor_col}H\x1b[?25h", lines.len() - 1) } else { String::new() };
            self.flush_screen_lines(st, lines, columns, rows, cursor);
            return;
        }

        let queue_lines: Vec<String> = st.queued_prompts[st.queued_prompts.len().saturating_sub(3)..].iter().map(|q| format!("{DIM}{}{RESET}", clip(&format!("  queued  {}   alt+↑ edit", q.split_whitespace().collect::<Vec<_>>().join(" "))))).collect();
        let fixed_rows = 1 + queue_lines.len() + 3;
        let mut popup_lines: Vec<String> = vec![];
        let (matches, selected) = Self::slash_popup(st);
        if !matches.is_empty() {
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
                popup_lines.push(if is_sel { format!("{BRAND_BOLD}{head}{RESET}{tail}") } else { format!("{head}{DIM}{tail}{RESET}") });
            }
            if matches.len() > shown_count && room > 0 {
                popup_lines.push(format!("{DIM}{}{RESET}", clip(&format!("  {} of {}   ↑↓ select   tab accept", selected + 1, matches.len()))));
            }
        }
        let chrome = fixed_rows + popup_lines.len();
        let height = rows.saturating_sub(chrome).max(2);
        st.max_scroll = all.len().saturating_sub(height);
        st.scroll = st.scroll.min(st.max_scroll);
        let end = all.len() - st.scroll;
        let mut body: Vec<String> = all[end.saturating_sub(height)..end].to_vec();
        while body.len() < height {
            body.push(String::new());
        }

        // Composer: a rule (amber while goon waits for an answer), the prompt and a footer.
        let awaiting = asking && st.pending.is_some();
        let rule = format!("{}{}{RESET}", if awaiting { WARN } else { DIM }, "─".repeat(safe_cols));
        let prompt_label = if asking { clip(&st.label) } else { "› ".to_string() };
        let label_cols = visible_length(&prompt_label);
        let input_cols = safe_cols.saturating_sub(label_cols).max(1);
        let (before, shown) = Self::slice_input_window(st, input_cols);
        let empty = st.input.is_empty() && !asking;
        let placeholder_text = if busy { "Type to queue a follow-up; /commands run now" } else { "Ask anything   / commands   @ files   $ skills   ! shell" };
        let placeholder = if empty { wrap_terminal(placeholder_text, input_cols).into_iter().next().unwrap_or_default() } else { String::new() };
        let label_styled = if asking { format!("{WARN}{BOLD}{prompt_label}{RESET}") } else { format!("{BRAND_BOLD}{prompt_label}{RESET}") };
        let content = if !placeholder.is_empty() { format!("{DIM}{placeholder}{RESET}") } else if st.masked { shown.clone() } else { format_inline_markdown_default(&shown) };
        let prompt_line = format!("{label_styled}{content}");
        let display_mode = if st.mode_label == "auto" { "auto-approve".to_string() } else { st.mode_label.clone() };
        // Auto-approve is the one risky state, so it is the one mode shown in a warning colour.
        let mode_tone = if display_mode == "auto-approve" { WARN } else if display_mode == "plan" { BRAND } else { DIM };
        let mut left: Vec<Seg> = vec![seg(display_mode, mode_tone), seg(format!("   {model_name}"), DIM), seg(format!("   effort {}", st.effort_label), DIM)];
        if st.view_mode == "transcript" {
            left.push(seg("   transcript view", DIM));
        }
        let hint: Vec<Seg> = if awaiting { vec![seg(format!("enter submit   esc {}", st.esc_hint), DIM)] } else { vec![] };
        let place: Vec<Seg> = if st.workspace_name.is_empty() { vec![] } else { vec![seg(if st.branch.is_empty() { st.workspace_name.clone() } else { format!("{} ({})", st.workspace_name, st.branch) }, DIM)] };
        let footer = Self::band(&left, vec![hint, place, goal_group, todo_group, Self::context_group(st), token_group], safe_cols, rich);
        let mut lines = body;
        lines.push(status_line);
        lines.extend(popup_lines);
        lines.extend(queue_lines);
        lines.push(rule);
        lines.push(prompt_line);
        lines.push(footer);
        let prompt_row = lines.len() - 1;
        let cursor_col = safe_cols.min(label_cols + visible_length(&before) + 1);
        let cursor = if st.pending.is_some() || !st.input.is_empty() { format!("\x1b[{prompt_row};{cursor_col}H\x1b[?25h") } else { String::new() };
        self.flush_screen_lines(st, lines, columns, rows, cursor);
    }
}
