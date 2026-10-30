//! Width-aware terminal text helpers and the inline Markdown painter used by the TUI.

use crate::term::{strip_vt, terminal_text};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
pub const DEFAULT_FG: &str = "\x1b[39m";
pub const BRAND: &str = "\x1b[38;2;167;139;250m";
pub const BRAND_BOLD: &str = "\x1b[1m\x1b[38;2;167;139;250m";
pub const INFO: &str = "\x1b[38;2;125;211;252m";
pub const OK: &str = "\x1b[38;2;74;222;128m";
pub const ERR: &str = "\x1b[38;2;248;113;113m";
pub const WARN: &str = "\x1b[38;2;251;191;36m";

/// goon palette: terminal default colour plus bold/dim for chrome; one violet accent for goon's
/// voice; green/red/amber only where they carry status, sky only for code and commands.
pub struct Theme;
impl Theme {
    pub const RESET: &'static str = RESET;
    pub const BRAND: &'static str = BRAND;
    pub const BRAND_BOLD: &'static str = BRAND_BOLD;
    pub const THINKING: &'static str = DIM;
    pub const BULLET: &'static str = DIM;
    pub const TEXT: &'static str = DEFAULT_FG;
    pub const STRONG: &'static str = BOLD;
    pub const MUTED: &'static str = DIM;
    pub const FAINT: &'static str = DIM;
    pub const INFO: &'static str = INFO;
    pub const OK: &'static str = OK;
    pub const ERR: &'static str = ERR;
    pub const WARN: &'static str = WARN;
    pub const ADD_FG: &'static str = OK;
    pub const DEL_FG: &'static str = ERR;
}

fn slow_width(c: char) -> u8 {
    let s = c.to_string();
    if re!(r"\p{M}").is_match(&s) {
        return 0;
    }
    if re!(r"[\p{Extended_Pictographic}\x{1100}-\x{115f}\x{2e80}-\x{a4cf}\x{ac00}-\x{d7a3}\x{f900}-\x{faff}\x{fe10}-\x{fe6f}\x{ff01}-\x{ff60}]").is_match(&s) {
        return 2;
    }
    1
}

/// Column width per code point (0 for combining marks, 2 for wide/emoji, else 1), cached for the BMP.
pub fn width(c: char) -> usize {
    let code = c as u32;
    if code < 0x300 {
        return 1;
    }
    if code > 0xffff {
        return slow_width(c) as usize;
    }
    static CACHE: OnceLock<Vec<AtomicU8>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| (0..0x10000).map(|_| AtomicU8::new(0)).collect());
    let cached = cache[code as usize].load(Ordering::Relaxed);
    if cached != 0 {
        return (cached - 1) as usize;
    }
    let w = slow_width(c);
    cache[code as usize].store(w + 1, Ordering::Relaxed);
    w as usize
}

pub fn wrap_terminal(text: &str, columns: usize) -> Vec<String> {
    let max = columns.max(1);
    let mut lines = vec![String::new()];
    let mut used = 0;
    for c in terminal_text(text).replace('\t', "  ").chars() {
        if c == '\n' {
            lines.push(String::new());
            used = 0;
            continue;
        }
        let size = width(c);
        if used + size > max {
            lines.push(String::new());
            used = 0;
        }
        lines.last_mut().unwrap().push(c);
        used += size;
    }
    lines
}

pub fn visible_length(text: &str) -> usize {
    strip_vt(text).chars().map(width).sum()
}

pub fn slice_visible(text: &str, max_cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.nfc() {
        let w = width(c);
        if used + w > max_cols {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

/// Clips a line containing SGR sequences to `cols` visible columns, keeping the styles.
pub fn slice_ansi_line(line: &str, cols: usize) -> String {
    if visible_length(line) <= cols {
        return line.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let sgr = re!(r"\x1b\[[0-9;]*m");
    let mut last = 0;
    let mut pieces: Vec<(bool, &str)> = vec![];
    for m in sgr.find_iter(line) {
        pieces.push((false, &line[last..m.start()]));
        pieces.push((true, m.as_str()));
        last = m.end();
    }
    pieces.push((false, &line[last..]));
    for (is_sgr, part) in pieces {
        if is_sgr {
            out.push_str(part);
            continue;
        }
        for c in part.chars() {
            let w = width(c);
            if used + w > cols {
                out.push_str(RESET);
                return out;
            }
            out.push(c);
            used += w;
        }
    }
    out
}

pub fn pad_end_visible(text: &str, cols: usize) -> String {
    let sliced = slice_visible(text, cols);
    let vis = visible_length(&sliced);
    format!("{sliced}{}", " ".repeat(cols.saturating_sub(vis)))
}

/// Inline Markdown (**bold**, `code`, [label](url), ~~strike~~) and attachment pills → ANSI.
pub fn format_inline_markdown(raw: &str, base: &str) -> String {
    let mut out = re!(r"(\[(?:Image #\d+|Pasted text #\d+)[^\]]*\])").replace_all(raw, format!("{BRAND_BOLD}${{1}}{base}").as_str()).to_string();
    out = re!(r"`([^`\n]+)`").replace_all(&out, format!("{INFO}${{1}}{base}").as_str()).to_string();
    out = re!(r"\[([^\]]+)\]\(([^)]+)\)").replace_all(&out, format!("\x1b[4m{INFO}${{1}}\x1b[24m{DIM} (${{2}}){base}").as_str()).to_string();
    out = re!(r"\*\*([^*]+)\*\*").replace_all(&out, format!("\x1b[1m${{1}}\x1b[22m{base}").as_str()).to_string();
    out = re!(r"~~([^~]+)~~").replace_all(&out, format!("\x1b[9m${{1}}\x1b[29m{base}").as_str()).to_string();
    out
}

pub fn format_inline_markdown_default(raw: &str) -> String {
    format_inline_markdown(raw, RESET)
}

/// Wraps `text` into chunks fitting `first` columns on the first row and `cont` after. With
/// `words`, breaks at the last space when the carried tail still fits; otherwise hard-wraps.
pub fn wrap_chunks(text: &str, first: usize, cont: usize, words: bool) -> Vec<String> {
    let clean = terminal_text(text).replace('\t', "  ");
    let cont_limit = cont.max(1);
    let mut out = vec![];
    let mut avail = first.max(1);
    let mut cur = String::new();
    let mut used = 0;
    for c in clean.chars() {
        let w = width(c);
        if used + w > avail && !cur.is_empty() {
            if words && c == ' ' {
                out.push(std::mem::take(&mut cur));
                used = 0;
                avail = cont_limit;
                continue;
            }
            let cut = if words { cur.rfind(' ') } else { None };
            match cut.filter(|i| *i > 0) {
                Some(i) if visible_length(&cur[i + 1..]) + w <= cont_limit => {
                    let tail = cur[i + 1..].to_string();
                    cur.truncate(i);
                    out.push(std::mem::replace(&mut cur, tail));
                }
                _ => out.push(std::mem::take(&mut cur)),
            }
            used = visible_length(&cur);
            avail = cont_limit;
        }
        cur.push(c);
        used += w;
    }
    out.push(cur);
    out
}

pub fn format_token_count(n: f64) -> String {
    if !n.is_finite() || n < 1000.0 {
        // `String(Math.max(0, Math.round(n || 0)))`: NaN and -Infinity print 0, Infinity stays.
        let n = if n.is_nan() { 0.0 } else { n };
        return crate::jsutil::number_to_string(crate::jsutil::js_round(n).max(0.0));
    }
    let trim = |s: String| s.strip_suffix(".0").map(str::to_string).unwrap_or(s);
    if n < 1_000_000.0 {
        return format!("{}k", trim(crate::jsutil::to_fixed(n / 1000.0, if n < 10_000.0 { 1 } else { 0 })));
    }
    format!("{}M", trim(crate::jsutil::to_fixed(n / 1_000_000.0, 1)))
}

pub fn format_compact_duration(ms: i64) -> String {
    let total = (ms / 1000).max(0);
    if total < 60 {
        return format!("{total}s");
    }
    format!("{}m {:02}s", total / 60, total % 60)
}

pub fn extract_reasoning_summary(buffer: &str) -> (Option<String>, Option<String>) {
    let lines: Vec<&str> = re!(r"\r?\n").split(buffer).map(str::trim).filter(|l| !l.is_empty()).collect();
    let mut header = None;
    let mut detail = None;
    for line in lines.iter().rev() {
        if line.starts_with("<!--") {
            continue;
        }
        let bold = re!(r"^\*\*([^*]{3,80})\*\*").captures(line).or_else(|| re!(r"^#{1,3}\s+(.{3,80})$").captures(line));
        match bold {
            Some(c) if header.is_none() => header = Some(c[1].trim().to_string()),
            _ => {
                if detail.is_none() {
                    let d = re!(r"\*\*([^*]+)\*\*").replace_all(line, "${1}");
                    let d = re!(r"`([^`]+)`").replace_all(&d, "${1}");
                    detail = Some(crate::jsutil::slice_chars(&d, 0, 120).to_string());
                }
            }
        }
        if header.is_some() && detail.is_some() {
            break;
        }
    }
    (header, detail)
}

pub fn format_compact_diff(diff: &str, max_lines: usize) -> Vec<String> {
    let mut out = vec![];
    let mut old_line: i64 = 1;
    let mut new_line: i64 = 1;
    let mut shown = 0;
    let mut omitted: u64 = 0;
    for line in re!(r"\r?\n").split(diff) {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        if let Some(c) = re!(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@").captures(line) {
            if shown > 0 && shown < max_lines {
                out.push("      ⋮".to_string());
            }
            old_line = c[1].parse::<i64>().unwrap_or(1).max(1);
            new_line = c[2].parse::<i64>().unwrap_or(1).max(1);
            continue;
        }
        if let Some(c) = re!(r"^\.\.\. \([+-](\d+) more (?:added|removed) lines\)$").captures(line) {
            omitted += c[1].parse::<u64>().unwrap_or(0);
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            if shown < max_lines {
                out.push(format!("    {:>4} +{rest}", new_line));
                shown += 1;
            } else {
                omitted += 1;
            }
            new_line += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            if shown < max_lines {
                out.push(format!("    {:>4} -{rest}", old_line));
                shown += 1;
            } else {
                omitted += 1;
            }
            old_line += 1;
        } else if let Some(rest) = line.strip_prefix(' ') {
            if shown < max_lines {
                out.push(format!("    {:>4}  {rest}", new_line));
                shown += 1;
            }
            old_line += 1;
            new_line += 1;
        }
    }
    if omitted > 0 {
        out.push(format!("    … +{omitted} more diff lines"));
    }
    out
}

/// Paints welcome-banner / panel box rows: faint frame, brand mark, faint labels, bright values.
pub fn style_box_line(line: &str) -> String {
    if line.starts_with('╭') || line.starts_with('╰') {
        let inner = re!(r"[^─╭╮╰╯]+").replace_all(line, format!("{RESET}{BRAND_BOLD}${{0}}{RESET}{DIM}").as_str());
        return format!("{DIM}{inner}{RESET}");
    }
    let Some(m) = re!(r"^│(.*?)(│?)$").captures(line) else { return line.to_string() };
    let content = m.get(1).map(|x| x.as_str()).unwrap_or("");
    let close = if m.get(2).map(|x| !x.as_str().is_empty()).unwrap_or(false) { format!("{DIM}│{RESET}") } else { String::new() };
    let styled = if let Some(t) = re!(r"^(\s*)(◆ goon)(\s+)(v\S+)(\s+)(.*?)(\s*)$").captures(content) {
        format!("{}{BRAND_BOLD}{}{RESET}{}{DIM}{}{RESET}{}{DIM}{}{RESET}{}", &t[1], &t[2], &t[3], &t[4], &t[5], &t[6], &t[7])
    } else if let Some(f) = re!(r"^(\s{2,4})([a-z]+)(\s{2,})(.*)$").captures(content) {
        format!("{}{DIM}{}{RESET}{}{DEFAULT_FG}{}{RESET}", &f[1], &f[2], &f[3], &f[4])
    } else {
        let tokens = re!(r"(/help|shift\+tab|[@$!] )").replace_all(content, |c: &regex::Captures| {
            let tok = &c[1];
            if tok.ends_with(' ') && tok.len() == 2 {
                format!("{RESET}{INFO}{}{RESET}{DIM} ", &tok[..1])
            } else {
                format!("{RESET}{INFO}{tok}{RESET}{DIM}")
            }
        });
        format!("{DIM}{tokens}{RESET}")
    };
    format!("{DIM}│{RESET}{styled}{close}")
}

pub fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_and_wrapping() {
        assert_eq!(wrap_terminal("ab界c", 4), vec!["ab界", "c"]);
        assert_eq!(terminal_text("safe\x1b[2Jtext\x1b]52;c;c2VjcmV0\x07end"), "safetextend");
        assert_eq!(visible_length("\x1b[1mab\x1b[0m界"), 4);
        assert_eq!(format_token_count(999.0), "999");
        assert_eq!(format_token_count(1500.0), "1.5k");
        assert_eq!(format_token_count(10_000.0), "10k");
        assert_eq!(format_token_count(2_000_000.0), "2M");
        assert_eq!(wrap_chunks("hello world foo", 8, 8, true), vec!["hello", "world", "foo"]);
        assert_eq!(slice_ansi_line("\x1b[1mabcdef\x1b[0m", 3), "\x1b[1mabc\x1b[0m");
    }
}
