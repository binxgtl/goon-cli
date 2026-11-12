//! Width-aware terminal text helpers and the inline Markdown painter used by the TUI.

use crate::term::{strip_vt, terminal_text};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
    Ansi16,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub mode: ThemeMode,
    pub reset: &'static str,
    pub dim: &'static str,
    pub bold: &'static str,
    pub bold_off: &'static str,
    pub italic: &'static str,
    pub underline: &'static str,
    pub underline_off: &'static str,
    pub strike: &'static str,
    pub strike_off: &'static str,
    pub reverse: &'static str,
    pub reverse_bold: &'static str,
    pub default_fg: &'static str,
    pub brand: &'static str,
    pub brand_bold: &'static str,
    pub info: &'static str,
    pub ok: &'static str,
    pub err: &'static str,
    pub warn: &'static str,
}

impl Palette {
    pub const DARK: Self = Self {
        mode: ThemeMode::Dark,
        reset: RESET,
        dim: DIM,
        bold: BOLD,
        bold_off: "\x1b[22m",
        italic: "\x1b[3m",
        underline: "\x1b[4m",
        underline_off: "\x1b[24m",
        strike: "\x1b[9m",
        strike_off: "\x1b[29m",
        reverse: "\x1b[7m",
        reverse_bold: "\x1b[7;1m",
        default_fg: DEFAULT_FG,
        brand: BRAND,
        brand_bold: BRAND_BOLD,
        info: INFO,
        ok: OK,
        err: ERR,
        warn: WARN,
    };

    /// High-contrast WCAG AA (>= 4.5:1 on `#FFFFFF` and `#FDF6E3` Solarized Light) semantic tokens.
    pub const LIGHT: Self = Self {
        mode: ThemeMode::Light,
        reset: RESET,
        dim: "\x1b[38;2;87;83;78m",
        bold: BOLD,
        bold_off: "\x1b[22m",
        italic: "\x1b[3m",
        underline: "\x1b[4m",
        underline_off: "\x1b[24m",
        strike: "\x1b[9m",
        strike_off: "\x1b[29m",
        reverse: "\x1b[7m",
        reverse_bold: "\x1b[7;1m",
        default_fg: DEFAULT_FG,
        brand: "\x1b[38;2;109;40;217m",
        brand_bold: "\x1b[1m\x1b[38;2;109;40;217m",
        info: "\x1b[38;2;3;105;161m",
        ok: "\x1b[38;2;21;128;61m",
        err: "\x1b[38;2;185;28;28m",
        warn: "\x1b[38;2;180;83;9m",
    };

    /// Standard 16-color SGR palette (`31`..`36` + `1`/`2`) without `38;2;R;G;B` sequences.
    pub const ANSI16: Self = Self {
        mode: ThemeMode::Ansi16,
        reset: RESET,
        dim: DIM,
        bold: BOLD,
        bold_off: "\x1b[22m",
        italic: "\x1b[3m",
        underline: "\x1b[4m",
        underline_off: "\x1b[24m",
        strike: "\x1b[9m",
        strike_off: "\x1b[29m",
        reverse: "\x1b[7m",
        reverse_bold: "\x1b[7;1m",
        default_fg: DEFAULT_FG,
        brand: "\x1b[35m",
        brand_bold: "\x1b[1;35m",
        info: "\x1b[36m",
        ok: "\x1b[32m",
        err: "\x1b[31m",
        warn: "\x1b[33m",
    };

    /// Pure plain-text palette (`NO_COLOR`, `CLICOLOR=0`, `FORCE_COLOR=0`, `TERM=dumb`, `GOON_THEME=none`).
    pub const NONE: Self = Self {
        mode: ThemeMode::None,
        reset: "",
        dim: "",
        bold: "",
        bold_off: "",
        italic: "",
        underline: "",
        underline_off: "",
        strike: "",
        strike_off: "",
        reverse: "",
        reverse_bold: "",
        default_fg: "",
        brand: "",
        brand_bold: "",
        info: "",
        ok: "",
        err: "",
        warn: "",
    };

    pub const fn for_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::DARK,
            ThemeMode::Light => Self::LIGHT,
            ThemeMode::Ansi16 => Self::ANSI16,
            ThemeMode::None => Self::NONE,
        }
    }

    pub const fn is_styled(&self) -> bool {
        !matches!(self.mode, ThemeMode::None)
    }
}

/// Resolves terminal color/theme capability from standard environment variables (`NO_COLOR`,
/// `FORCE_COLOR`, `CLICOLOR`, `TERM`, `COLORTERM`, `COLORFGBG`, `GOON_THEME`).
pub fn resolve_theme_mode(get_env: impl Fn(&str) -> Option<String>) -> ThemeMode {
    let goon_theme = get_env("GOON_THEME").unwrap_or_default().trim().to_ascii_lowercase();
    if matches!(goon_theme.as_str(), "none" | "mono" | "off" | "no-color" | "nocolor") {
        return ThemeMode::None;
    }
    if get_env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return ThemeMode::None;
    }
    let force_raw = get_env("FORCE_COLOR").unwrap_or_default().trim().to_ascii_lowercase();
    if matches!(force_raw.as_str(), "0" | "false") {
        return ThemeMode::None;
    }
    let forced_ansi16 = force_raw == "1";
    let forced = forced_ansi16 || matches!(force_raw.as_str(), "2" | "3" | "true");

    if !forced {
        let clicolor_force = get_env("CLICOLOR_FORCE").unwrap_or_default();
        let clicolor_forced = !clicolor_force.trim().is_empty() && clicolor_force.trim() != "0";
        if !clicolor_forced && get_env("CLICOLOR").is_some_and(|v| v.trim() == "0") {
            return ThemeMode::None;
        }
        if get_env("TERM").is_some_and(|t| t.trim().eq_ignore_ascii_case("dumb")) {
            return ThemeMode::None;
        }
    }

    match goon_theme.as_str() {
        "ansi" | "ansi16" | "16" => return ThemeMode::Ansi16,
        "light" => return if forced_ansi16 { ThemeMode::Ansi16 } else { ThemeMode::Light },
        "dark" => return if forced_ansi16 { ThemeMode::Ansi16 } else { ThemeMode::Dark },
        _ => {}
    }

    if forced_ansi16 {
        return ThemeMode::Ansi16;
    }

    let colorterm = get_env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    let has_truecolor_hint = colorterm.contains("truecolor") || colorterm.contains("24bit");
    let term = get_env("TERM").unwrap_or_default().trim().to_ascii_lowercase();
    let is_legacy_16 = matches!(term.as_str(), "linux" | "ansi" | "vt100" | "vt102" | "vt220" | "cons25" | "rxvt" | "eterm-color") || term.ends_with("-16color");
    if !has_truecolor_hint && !forced && is_legacy_16 {
        return ThemeMode::Ansi16;
    }

    if let Some(fgbg) = get_env("COLORFGBG") {
        if let Some(bg_part) = fgbg.rsplit(';').next() {
            if let Ok(bg) = bg_part.trim().parse::<u8>() {
                if matches!(bg, 7 | 15 | 230 | 231 | 249..=255) {
                    return ThemeMode::Light;
                }
            }
        }
    }

    ThemeMode::Dark
}

pub fn detect_env_theme_mode() -> ThemeMode {
    resolve_theme_mode(|k| std::env::var(k).ok())
}

/// Width of a standalone code point. Layout must measure complete graphemes instead.
pub fn width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

pub fn wrap_terminal(text: &str, columns: usize) -> Vec<String> {
    let max = columns.max(1);
    let mut lines = vec![String::new()];
    let mut used = 0;
    for grapheme in terminal_text(text).replace('\t', "  ").graphemes(true) {
        if grapheme == "\n" {
            lines.push(String::new());
            used = 0;
            continue;
        }
        let size = UnicodeWidthStr::width(grapheme);
        if used + size > max && !lines.last().unwrap().is_empty() {
            lines.push(String::new());
            used = 0;
        }
        lines.last_mut().unwrap().push_str(grapheme);
        used += size;
    }
    lines
}

pub fn visible_length(text: &str) -> usize {
    UnicodeWidthStr::width(strip_vt(text).as_ref())
}

pub fn slice_visible(text: &str, max_cols: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let normalized: String = text.nfc().collect();
    for grapheme in normalized.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if used + w > max_cols {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    out
}

/// Clips a line to `cols` visible columns, omitting trailing SGR reset when styling is disabled.
pub fn clip_line(line: &str, cols: usize, styled: bool) -> String {
    if !styled {
        return slice_visible(&strip_vt(line), cols);
    }
    slice_ansi_line(line, cols)
}

/// Clips a line containing SGR sequences to `cols` visible columns, keeping the styles.
pub fn slice_ansi_line(line: &str, cols: usize) -> String {
    if visible_length(line) <= cols {
        return line.to_string();
    }
    let mut out = String::new();
    // Determine a plain-text grapheme boundary first, even if an SGR sequence
    // occurs inside that grapheme. Then replay styles up to that byte boundary.
    let plain = strip_vt(line);
    let mut used = 0;
    let mut remaining = 0;
    for grapheme in plain.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > cols {
            break;
        }
        used += width;
        remaining += grapheme.len();
    }
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
        let take = remaining.min(part.len());
        out.push_str(&part[..take]);
        remaining -= take;
        if take < part.len() {
            out.push_str(RESET);
            return out;
        }
    }
    out
}

pub fn pad_end_visible(text: &str, cols: usize) -> String {
    let sliced = slice_visible(text, cols);
    let vis = visible_length(&sliced);
    format!("{sliced}{}", " ".repeat(cols.saturating_sub(vis)))
}

/// Palette-aware inline Markdown (**bold**, `code`, [label](url), ~~strike~~) and attachment pills.
pub fn format_inline_markdown_with(raw: &str, base: &str, p: &Palette) -> String {
    if !p.is_styled() {
        let mut out = re!(r"`([^`\n]+)`").replace_all(raw, "${1}").to_string();
        out = re!(r"\[([^\]]+)\]\(([^)]+)\)").replace_all(&out, "${1} (${2})").to_string();
        out = re!(r"\*\*([^*]+)\*\*").replace_all(&out, "${1}").to_string();
        out = re!(r"~~([^~]+)~~").replace_all(&out, "${1}").to_string();
        return out;
    }
    let mut out = re!(r"(\[(?:Image #\d+|Pasted text #\d+)[^\]]*\])").replace_all(raw, format!("{}${{1}}{base}", p.brand_bold).as_str()).to_string();
    out = re!(r"`([^`\n]+)`").replace_all(&out, format!("{}${{1}}{base}", p.info).as_str()).to_string();
    out = re!(r"\[([^\]]+)\]\(([^)]+)\)").replace_all(&out, format!("{}{}${{1}}{}{} (${{2}}){base}", p.underline, p.info, p.underline_off, p.dim).as_str()).to_string();
    out = re!(r"\*\*([^*]+)\*\*").replace_all(&out, format!("{}${{1}}{}{base}", p.bold, p.bold_off).as_str()).to_string();
    out = re!(r"~~([^~]+)~~").replace_all(&out, format!("{}${{1}}{}{base}", p.strike, p.strike_off).as_str()).to_string();
    out
}

/// Inline Markdown (**bold**, `code`, [label](url), ~~strike~~) and attachment pills → ANSI.
pub fn format_inline_markdown(raw: &str, base: &str) -> String {
    format_inline_markdown_with(raw, base, &Palette::DARK)
}

pub fn format_inline_markdown_default(raw: &str) -> String {
    format_inline_markdown(raw, RESET)
}

/// Wraps `text` into chunks fitting `first` columns on the first row and `cont` after. With
/// `words`, breaks at the last space when the carried tail still fits; otherwise hard-wraps.
pub fn wrap_chunks(text: &str, first: usize, cont: usize, words: bool) -> Vec<String> {
    let clean = terminal_text(text).replace('\t', "  ");
    wrap_ranges(&clean, first, cont, words).into_iter().map(|range| clean[range].to_string()).collect()
}

/// Wrapping provenance in the sanitized input, including gaps for elided spaces.
pub(super) fn wrap_ranges(clean: &str, first: usize, cont: usize, words: bool) -> Vec<std::ops::Range<usize>> {
    let cont_limit = cont.max(1);
    let mut out = vec![];
    let mut avail = first.max(1);
    let mut start = 0;
    let mut used = 0;
    for (index, grapheme) in clean.grapheme_indices(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if used + w > avail && index > start {
            if words && grapheme == " " {
                out.push(start..index);
                start = index + 1;
                used = 0;
                avail = cont_limit;
                continue;
            }
            let cut = if words { clean[start..index].rfind(' ').map(|i| start + i) } else { None };
            match cut.filter(|i| *i > start) {
                Some(i) if visible_length(&clean[i + 1..index]) + w <= cont_limit => {
                    out.push(start..i);
                    start = i + 1;
                }
                _ => {
                    out.push(start..index);
                    start = index;
                }
            }
            used = visible_length(&clean[start..index]);
            avail = cont_limit;
        }
        used += w;
    }
    out.push(start..clean.len());
    out
}

/// Palette-aware slice of styled text by bytes in its plain projection, replaying prior SGR state.
pub(super) fn styled_range_with(text: &str, range: std::ops::Range<usize>, p: &Palette) -> String {
    if !p.is_styled() {
        let plain = strip_vt(text);
        let start = range.start.min(plain.len());
        let end = range.end.min(plain.len()).max(start);
        return plain[start..end].to_string();
    }
    let mut output = String::new();
    let mut offset = 0;
    let mut last = 0;
    let mut saw_sgr = false;
    for sgr in re!(r"\x1b\[[0-9;]*m").find_iter(text) {
        let part = &text[last..sgr.start()];
        let start = range.start.saturating_sub(offset).min(part.len());
        let end = range.end.saturating_sub(offset).min(part.len());
        if start < end {
            output.push_str(&part[start..end]);
        }
        offset += part.len();
        if offset >= range.end {
            if saw_sgr {
                output.push_str(p.reset);
            }
            return output;
        }
        output.push_str(sgr.as_str());
        saw_sgr = true;
        last = sgr.end();
    }
    let part = &text[last..];
    let start = range.start.saturating_sub(offset).min(part.len());
    let end = range.end.saturating_sub(offset).min(part.len());
    if start < end {
        output.push_str(&part[start..end]);
    }
    if saw_sgr {
        output.push_str(p.reset);
    }
    output
}

#[allow(dead_code)]
/// Slice styled text by bytes in its plain projection, replaying prior SGR state.
pub(super) fn styled_range(text: &str, range: std::ops::Range<usize>) -> String {
    styled_range_with(text, range, &Palette::DARK)
}

pub fn format_token_count(n: f64) -> String {
    if !n.is_finite() || n < 1000.0 {
        // `String(Math.max(0, Math.round(n || 0)))`: NaN and -Infinity print 0, Infinity stays.
        let n = if n.is_nan() { 0.0 } else { n };
        return crate::jsutil::number_to_string(crate::jsutil::js_round(n).max(0.0));
    }
    let trim = |s: String| s.strip_suffix(".0").map(str::to_string).unwrap_or(s);
    if n < 1_000_000.0 {
        return format!("{}k", trim(crate::jsutil::to_fixed(n / 1000.0, if n < 10_00.0 * 10.0 { 1 } else { 0 })));
    }
    format!("{}M", trim(crate::jsutil::to_fixed(n / 1_000_000.0, 1)))
}

pub fn format_compact_duration(ms: i64) -> String {
    let total = (ms / 1000).max(0);
    if total < 60 {
        return format!("{total}s");
    }
    if total < 3600 {
        return format!("{}m {:02}s", total / 60, total % 60);
    }
    format!("{}h {:02}m {:02}s", total / 3600, (total % 3600) / 60, total % 60)
}

pub fn format_turn_duration(ms: i64) -> String {
    let clamped = ms.max(0);
    if clamped < 10_000 {
        let tenths = ((clamped as f64) / 100.0).round().max(1.0) as i64;
        if tenths >= 100 {
            return "10s".into();
        }
        if tenths % 10 == 0 {
            return format!("{}s", tenths / 10);
        }
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }
    format_compact_duration(clamped)
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

/// Palette-aware welcome-banner / panel box rows.
pub fn style_box_line_with(line: &str, p: &Palette) -> String {
    if !p.is_styled() {
        return line.to_string();
    }
    if line.starts_with('╭') || line.starts_with('╰') {
        let inner = re!(r"[^─╭╮╰╯]+").replace_all(line, format!("{}{}${{0}}{}{}", p.reset, p.brand_bold, p.reset, p.dim).as_str());
        return format!("{}{inner}{}", p.dim, p.reset);
    }
    let Some(m) = re!(r"^│(.*?)(│?)$").captures(line) else { return line.to_string() };
    let content = m.get(1).map(|x| x.as_str()).unwrap_or("");
    let close = if m.get(2).map(|x| !x.as_str().is_empty()).unwrap_or(false) { format!("{}│{}", p.dim, p.reset) } else { String::new() };
    let styled = if let Some(t) = re!(r"^(\s*)(◆ goon)(\s+)(v\S+)(\s+)(.*?)(\s*)$").captures(content) {
        format!("{}{}{}{}{}{}{}{}{}{}{}{}{}", &t[1], p.brand_bold, &t[2], p.reset, &t[3], p.dim, &t[4], p.reset, &t[5], p.dim, &t[6], p.reset, &t[7])
    } else if let Some(f) = re!(r"^(\s{2,4})([a-z]+)(\s{2,})(.*)$").captures(content) {
        format!("{}{}{}{}{}{}{}{}", &f[1], p.dim, &f[2], p.reset, &f[3], p.default_fg, &f[4], p.reset)
    } else {
        let tokens = re!(r"(/help|shift\+tab|[@$!] )").replace_all(content, |c: &regex::Captures| {
            let tok = &c[1];
            if tok.ends_with(' ') && tok.len() == 2 {
                format!("{}{}{}{}{} ", p.reset, p.info, &tok[..1], p.reset, p.dim)
            } else {
                format!("{}{}{tok}{}{}", p.reset, p.info, p.reset, p.dim)
            }
        });
        format!("{}{tokens}{}", p.dim, p.reset)
    };
    format!("{}│{}{styled}{close}", p.dim, p.reset)
}

/// Paints welcome-banner / panel box rows: faint frame, brand mark, faint labels, bright values.
pub fn style_box_line(line: &str) -> String {
    style_box_line_with(line, &Palette::DARK)
}

pub fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.get(k).cloned()
    }

    fn parse_rgb(sgr: &str) -> (u8, u8, u8) {
        let c = re!(r"38;2;(\d+);(\d+);(\d+)m").captures(sgr).expect("24-bit RGB SGR");
        (c[1].parse().unwrap(), c[2].parse().unwrap(), c[3].parse().unwrap())
    }

    fn rel_luminance((r, g, b): (u8, u8, u8)) -> f64 {
        let chan = |v: u8| {
            let s = v as f64 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * chan(r) + 0.7152 * chan(g) + 0.0722 * chan(b)
    }

    fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        let (la, lb) = (rel_luminance(a), rel_luminance(b));
        let (lighter, darker) = if la >= lb { (la, lb) } else { (lb, la) };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn theme_mode_precedence_respects_no_color_force_color_term_and_colorfgbg() {
        assert_eq!(resolve_theme_mode(env_from(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor"), ("FORCE_COLOR", "3")])), ThemeMode::None);
        assert_eq!(resolve_theme_mode(env_from(&[("NO_COLOR", ""), ("TERM", "xterm-256color")])), ThemeMode::Dark);
        assert_eq!(resolve_theme_mode(env_from(&[("CLICOLOR", "0"), ("TERM", "xterm-256color")])), ThemeMode::None);
        assert_eq!(resolve_theme_mode(env_from(&[("CLICOLOR", "0"), ("CLICOLOR_FORCE", "1"), ("TERM", "xterm-256color")])), ThemeMode::Dark);
        assert_eq!(resolve_theme_mode(env_from(&[("FORCE_COLOR", "0"), ("TERM", "xterm-256color")])), ThemeMode::None);
        assert_eq!(resolve_theme_mode(env_from(&[("FORCE_COLOR", "1"), ("TERM", "xterm-256color")])), ThemeMode::Ansi16);
        assert_eq!(resolve_theme_mode(env_from(&[("TERM", "dumb")])), ThemeMode::None);
        assert_eq!(resolve_theme_mode(env_from(&[("TERM", "dumb"), ("FORCE_COLOR", "2")])), ThemeMode::Dark);
        assert_eq!(resolve_theme_mode(env_from(&[("TERM", "linux")])), ThemeMode::Ansi16);
        assert_eq!(resolve_theme_mode(env_from(&[("TERM", "vt100")])), ThemeMode::Ansi16);
        assert_eq!(resolve_theme_mode(env_from(&[("TERM", "xterm-16color")])), ThemeMode::Ansi16);
        assert_eq!(resolve_theme_mode(env_from(&[("TERM", "linux"), ("COLORTERM", "truecolor")])), ThemeMode::Dark);
        for capable in ["", "xterm", "xterm-256color", "screen", "tmux", "tmux-256color"] {
            assert_eq!(resolve_theme_mode(env_from(&[("TERM", capable)])), ThemeMode::Dark, "{capable:?}");
        }
        assert_eq!(resolve_theme_mode(env_from(&[("COLORFGBG", "0;15")])), ThemeMode::Light);
        assert_eq!(resolve_theme_mode(env_from(&[("COLORFGBG", "12;default;7")])), ThemeMode::Light);
        assert_eq!(resolve_theme_mode(env_from(&[("COLORFGBG", "15;0")])), ThemeMode::Dark);
        assert_eq!(resolve_theme_mode(env_from(&[("COLORFGBG", "15;234")])), ThemeMode::Dark);
        assert_eq!(resolve_theme_mode(env_from(&[("COLORFGBG", "0;15"), ("GOON_THEME", "dark")])), ThemeMode::Dark);
        assert_eq!(resolve_theme_mode(env_from(&[("GOON_THEME", "light")])), ThemeMode::Light);
        assert_eq!(resolve_theme_mode(env_from(&[("GOON_THEME", "ansi16")])), ThemeMode::Ansi16);
        assert_eq!(resolve_theme_mode(env_from(&[("GOON_THEME", "none")])), ThemeMode::None);
    }

    #[test]
    fn light_palette_meets_wcag_aa_contrast_and_ansi16_none_avoid_disallowed_sgr() {
        for bg in [(255, 255, 255), (253, 246, 227)] {
            for (name, token) in
                [("dim", Palette::LIGHT.dim), ("brand", Palette::LIGHT.brand), ("info", Palette::LIGHT.info), ("ok", Palette::LIGHT.ok), ("err", Palette::LIGHT.err), ("warn", Palette::LIGHT.warn)]
            {
                let ratio = contrast_ratio(parse_rgb(token), bg);
                assert!(ratio >= 4.5, "{name} on {bg:?} had contrast {ratio:.2} < 4.5");
            }
        }
        for token in [Palette::ANSI16.brand, Palette::ANSI16.brand_bold, Palette::ANSI16.info, Palette::ANSI16.ok, Palette::ANSI16.err, Palette::ANSI16.warn] {
            assert!(!token.contains("38;2;"), "Ansi16 must not emit 24-bit RGB: {token:?}");
        }
        let plain_md = format_inline_markdown_with("**bold** `code` [x](https://e.co) ~~old~~ [Image #1]", "", &Palette::NONE);
        assert!(!plain_md.contains("\x1b["), "{plain_md:?}");
        assert_eq!(plain_md, "bold code x (https://e.co) old [Image #1]");
    }

    #[test]
    fn emoji_and_combining_graphemes_are_measured_wrapped_and_clipped_atomically() {
        for grapheme in ["👩‍💻", "👨‍👩‍👧‍👦", "👍🏽", "🇺🇸", "1️⃣", "❤️"] {
            assert_eq!(visible_length(grapheme), 2, "{grapheme}");
            assert_eq!(wrap_terminal(&format!("a{grapheme}b"), 3), vec![format!("a{grapheme}"), "b".into()]);
            assert_eq!(wrap_chunks(&format!("a{grapheme}b"), 3, 3, false), vec![format!("a{grapheme}"), "b".into()]);
            assert_eq!(slice_visible(&format!("a{grapheme}b"), 2), "a");
            assert_eq!(slice_visible(&format!("a{grapheme}b"), 3), format!("a{grapheme}"));
            let styled = format!("\x1b[1ma{grapheme}b\x1b[0m");
            assert_eq!(strip_vt(&slice_ansi_line(&styled, 2)), "a");
            assert_eq!(strip_vt(&slice_ansi_line(&styled, 3)), format!("a{grapheme}"));
        }
        assert_eq!(visible_length("e\u{301}"), 1);
        assert_eq!(wrap_terminal("ae\u{301}b", 2), vec!["aé", "b"]);
        assert_eq!(wrap_terminal("界", 1), vec!["界"], "oversized grapheme must not create an empty leading row");
        assert_eq!(strip_vt(&slice_ansi_line("👩\x1b[1m‍💻x", 2)), "👩‍💻", "styles inside an emoji cannot split it");
    }

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
        assert_eq!(width('✓'), 1);
        assert_eq!(format_turn_duration(0), "0.1s");
        assert_eq!(format_turn_duration(420), "0.4s");
        assert_eq!(format_turn_duration(1000), "1s");
        assert_eq!(format_turn_duration(3480), "3.5s");
        assert_eq!(format_turn_duration(9960), "10s");
        assert_eq!(format_turn_duration(18_400), "18s");
        assert_eq!(format_turn_duration(134_200), "2m 14s");
        assert_eq!(format_turn_duration(3_734_000), "1h 02m 14s");
        assert_eq!(format_compact_duration(3_734_000), "1h 02m 14s");
    }
}
