//! Terminal helpers: control-sequence stripping, TTY checks and line input for the plain
//! (non-TUI) interface.

use anyhow::{bail, Result};
use regex::Regex;
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;
use tokio::io::AsyncBufReadExt;
use tokio_util::sync::CancellationToken;

/// Raised when the user cancels an input prompt (Esc / Ctrl-C at a question).
#[derive(Debug, Clone)]
pub struct InputCancelled;

impl std::fmt::Display for InputCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancelled by user.")
    }
}

impl std::error::Error for InputCancelled {}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"[\x1B\x{9B}][\[\]()#;?]*",
            r"(?:(?:(?:(?:;[-a-zA-Z\d/#&.:=?%@~_]+)*",
            r"|[a-zA-Z\d]+(?:;[-a-zA-Z\d/#&.:=?%@~_]*)*)?",
            r"(?:\x07|\x1B\x5C|\x{9C}))",
            r"|(?:(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-ntqry=><~]))"
        ))
        .unwrap()
    })
}

/// `stripVTControlCharacters`: removes complete escape sequences only.
pub fn strip_vt(value: &str) -> std::borrow::Cow<'_, str> {
    // Fast path: most strings (plain text, single graphemes) carry no escape at all, and the
    // regex scan dominated render profiles when called per grapheme.
    if !value.contains(['\x1b', '\u{9b}']) {
        return std::borrow::Cow::Borrowed(value);
    }
    ansi_re().replace_all(value, "")
}

/// `stripVTControlCharacters` + NFC + removal of remaining control characters (keeps `\t`, `\n`).
pub fn terminal_text(value: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let stripped = ansi_re().replace_all(value, "");
    stripped.nfc().filter(|c| !matches!(*c as u32, 0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f)).collect()
}

pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// True for a real pipe or file on stdin (sockets and never-closing descriptors are ignored).
pub fn piped_stdin() -> bool {
    if stdin_is_tty() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let Ok(meta) = std::fs::metadata("/dev/stdin") else { return false };
        let ft = meta.file_type();
        ft.is_fifo() || ft.is_file()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

type Lines = tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>;

fn stdin_lines() -> &'static tokio::sync::Mutex<Option<Lines>> {
    static LINES: OnceLock<tokio::sync::Mutex<Option<Lines>>> = OnceLock::new();
    LINES.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// Reads one line from stdin after writing `label` to stderr. `Ok(None)` at end of input.
pub async fn read_line(label: &str, cancel: Option<&CancellationToken>) -> Result<Option<String>> {
    eprint!("{label}");
    let _ = std::io::stderr().flush();
    let mut guard = stdin_lines().lock().await;
    if guard.is_none() {
        *guard = Some(tokio::io::BufReader::new(tokio::io::stdin()).lines());
    }
    let lines = guard.as_mut().unwrap();
    let never = CancellationToken::new();
    let cancel = cancel.unwrap_or(&never);
    tokio::select! {
        line = lines.next_line() => Ok(line?.map(|l| l.strip_suffix('\r').map(str::to_string).unwrap_or(l))),
        _ = cancel.cancelled() => bail!(InputCancelled),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtyReadStep {
    Continue,
    Submit(String),
    Eof,
    Cancel,
}

/// Formats masked secret feedback (`•••• (12 chars)`) without revealing secret contents.
pub fn format_hidden_mask(char_count: usize) -> String {
    if char_count == 0 {
        return String::new();
    }
    let dots = "•".repeat(char_count.min(8));
    let unit = if char_count == 1 { "char" } else { "chars" };
    format!("{dots} ({char_count} {unit})")
}

/// Deletes the previous word or punctuation run in a single-line TTY input buffer (`Ctrl+W`).
pub fn delete_prev_word(buf: &mut String) {
    while buf.chars().last().is_some_and(char::is_whitespace) {
        buf.pop();
    }
    let is_sep = |c: char| c.is_whitespace() || matches!(c, '-' | '_' | '/' | ':' | '.');
    if buf.chars().last().is_some_and(|c| !c.is_whitespace() && is_sep(c)) {
        while buf.chars().last().is_some_and(|c| !c.is_whitespace() && is_sep(c)) {
            buf.pop();
        }
    } else {
        while buf.chars().last().is_some_and(|c| !is_sep(c)) {
            buf.pop();
        }
    }
}

fn pop_last_grapheme(buf: &mut String) {
    use unicode_segmentation::UnicodeSegmentation;
    if let Some((idx, _)) = buf.grapheme_indices(true).next_back() {
        buf.truncate(idx);
    }
}

/// Pure single-line TTY event reducer shared by `read_hidden` and `read_line_cancelable`.
pub fn step_tty_input(buf: &mut String, event: &crossterm::event::Event) -> TtyReadStep {
    step_tty_input_with(buf, event, cfg!(windows))
}

/// Platform-parameterized single-line TTY event reducer (preserves Windows `AltGr` `CONTROL | ALT`).
pub fn step_tty_input_with(buf: &mut String, event: &crossterm::event::Event, is_windows: bool) -> TtyReadStep {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            let super_meta = key.modifiers.intersects(KeyModifiers::SUPER | KeyModifiers::HYPER | KeyModifiers::META);
            let is_altgr = is_windows && ctrl && alt;
            let has_non_text_modifier = ((ctrl || alt) && !is_altgr) || super_meta;
            match key.code {
                KeyCode::Esc => TtyReadStep::Cancel,
                KeyCode::Enter => TtyReadStep::Submit(std::mem::take(buf)),
                KeyCode::Char('j' | 'J' | 'm' | 'M') if ctrl && !alt && !super_meta => TtyReadStep::Submit(std::mem::take(buf)),
                KeyCode::Char('\n' | '\r') if !alt && !super_meta => TtyReadStep::Submit(std::mem::take(buf)),
                KeyCode::Char('c' | 'C') if ctrl && !alt && !super_meta => TtyReadStep::Cancel,
                KeyCode::Char('\x03') => TtyReadStep::Cancel,
                KeyCode::Char('d' | 'D') if ctrl && !alt && !super_meta && buf.is_empty() => TtyReadStep::Eof,
                KeyCode::Char('\x04') if buf.is_empty() => TtyReadStep::Eof,
                KeyCode::Backspace | KeyCode::Delete | KeyCode::Char('\x08' | '\x7f') => {
                    pop_last_grapheme(buf);
                    TtyReadStep::Continue
                }
                KeyCode::Char('h' | 'H') if ctrl && !alt && !super_meta => {
                    pop_last_grapheme(buf);
                    TtyReadStep::Continue
                }
                KeyCode::Char('u' | 'U') if ctrl && !alt && !super_meta => {
                    buf.clear();
                    TtyReadStep::Continue
                }
                KeyCode::Char('\x15') => {
                    buf.clear();
                    TtyReadStep::Continue
                }
                KeyCode::Char('w' | 'W') if ctrl && !alt && !super_meta => {
                    delete_prev_word(buf);
                    TtyReadStep::Continue
                }
                KeyCode::Char('\x17') => {
                    delete_prev_word(buf);
                    TtyReadStep::Continue
                }
                KeyCode::Char(c) if !has_non_text_modifier && !c.is_control() => {
                    buf.push(c);
                    TtyReadStep::Continue
                }
                _ => TtyReadStep::Continue,
            }
        }
        Event::Paste(text) => {
            for ch in text.chars() {
                if !matches!(ch, '\r' | '\n') && !ch.is_control() {
                    buf.push(ch);
                }
            }
            TtyReadStep::Continue
        }
        _ => TtyReadStep::Continue,
    }
}

struct RawModeGuard {
    bracketed_paste: bool,
}

impl RawModeGuard {
    fn enter(bracketed_paste: bool) -> Result<Self> {
        crate::tui::install_panic_hook();
        crossterm::terminal::enable_raw_mode()?;
        crate::tui::mark_raw_mode_enabled(bracketed_paste);
        if bracketed_paste {
            eprint!("\x1b[?2004h");
            let _ = std::io::stderr().flush();
        }
        Ok(Self { bracketed_paste })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.bracketed_paste {
            eprint!("\x1b[?2004l");
            let _ = std::io::stderr().flush();
        }
        let _ = crossterm::terminal::disable_raw_mode();
        crate::tui::mark_raw_mode_disabled(self.bracketed_paste);
    }
}

fn stderr_supports_line_redraw() -> bool {
    stderr_is_tty() && std::env::var("TERM").map(|v| !v.eq_ignore_ascii_case("dumb")).unwrap_or(true)
}

fn split_prompt_prefix(label: &str) -> (Option<&str>, &str) {
    match label.rsplit_once('\n') {
        Some((prefix, tail)) => (Some(prefix), tail),
        None => (None, label),
    }
}

fn is_bare_esc_press(event: &crossterm::event::Event) -> bool {
    matches!(
        event,
        crossterm::event::Event::Key(k)
            if k.code == crossterm::event::KeyCode::Esc
                && k.modifiers.is_empty()
                && k.kind != crossterm::event::KeyEventKind::Release
    )
}

fn drain_split_escape_sequence() -> Result<bool> {
    if !crossterm::event::poll(std::time::Duration::from_millis(15))? {
        return Ok(false);
    }
    let _ = crossterm::event::read()?;
    for _ in 0..8 {
        if !crossterm::event::poll(std::time::Duration::from_millis(2))? {
            break;
        }
        let _ = crossterm::event::read()?;
    }
    Ok(true)
}

/// Reads one line on a TTY with live `Esc` / `Ctrl+C` cancellation; falls back to `read_line` when non-TTY or `TERM=dumb`.
pub async fn read_line_cancelable(label: &str, cancel: Option<&CancellationToken>) -> Result<Option<String>> {
    if !stdin_is_tty() || std::env::var("TERM").map(|v| v.eq_ignore_ascii_case("dumb")).unwrap_or(false) {
        return read_line(label, cancel).await;
    }
    let (prefix, line_label_ref) = split_prompt_prefix(label);
    if let Some(prefix) = prefix {
        eprintln!("{prefix}");
    }
    eprint!("{line_label_ref}");
    let _ = std::io::stderr().flush();
    let line_label = line_label_ref.to_string();
    let token = cancel.cloned().unwrap_or_default();
    let redraw = stderr_supports_line_redraw();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        let _guard = RawModeGuard::enter(redraw)?;
        let mut buf = String::new();
        let mut shown = String::new();
        loop {
            if token.is_cancelled() {
                eprint!("\r\n");
                let _ = std::io::stderr().flush();
                break Err(InputCancelled.into());
            }
            if !crossterm::event::poll(std::time::Duration::from_millis(40))? {
                continue;
            }
            let event = crossterm::event::read()?;
            if is_bare_esc_press(&event) && drain_split_escape_sequence()? {
                continue;
            }
            match step_tty_input(&mut buf, &event) {
                TtyReadStep::Continue => {
                    if redraw && buf != shown {
                        shown.clone_from(&buf);
                        eprint!("\r\x1b[2K{line_label}{shown}");
                        let _ = std::io::stderr().flush();
                    }
                }
                TtyReadStep::Submit(out) => {
                    if redraw {
                        eprint!("\r\x1b[2K{line_label}{out}\r\n");
                    } else {
                        eprint!("\r\n");
                    }
                    let _ = std::io::stderr().flush();
                    break Ok(Some(out));
                }
                TtyReadStep::Eof => {
                    eprint!("\r\n");
                    let _ = std::io::stderr().flush();
                    break Ok(None);
                }
                TtyReadStep::Cancel => {
                    eprint!("\r\n");
                    let _ = std::io::stderr().flush();
                    break Err(InputCancelled.into());
                }
            }
        }
    })
    .await?
}

/// Reads a line without echo on a TTY (API keys); falls back to a plain line otherwise.
pub async fn read_hidden(label: &str) -> Result<Option<String>> {
    if !stdin_is_tty() {
        return read_line(label, None).await;
    }
    let (prefix, line_label_ref) = split_prompt_prefix(label);
    if let Some(prefix) = prefix {
        eprintln!("{prefix}");
    }
    eprint!("{line_label_ref}");
    let _ = std::io::stderr().flush();
    let line_label = line_label_ref.to_string();
    let redraw = stderr_supports_line_redraw();
    tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        use unicode_segmentation::UnicodeSegmentation;
        let _guard = RawModeGuard::enter(redraw)?;
        let mut out = String::new();
        let mut last_count = 0usize;
        loop {
            match crossterm::event::read() {
                Ok(event) => {
                    if is_bare_esc_press(&event) && drain_split_escape_sequence()? {
                        continue;
                    }
                    match step_tty_input(&mut out, &event) {
                        TtyReadStep::Continue => {
                            let count = out.graphemes(true).count();
                            if redraw && count != last_count {
                                last_count = count;
                                let mask = format_hidden_mask(count);
                                eprint!("\r\x1b[2K{line_label}{mask}");
                                let _ = std::io::stderr().flush();
                            }
                        }
                        TtyReadStep::Submit(secret) => {
                            if redraw {
                                eprint!("\r\x1b[2K{line_label}\r\n");
                            } else {
                                eprint!("\r\n");
                            }
                            let _ = std::io::stderr().flush();
                            break Ok(Some(secret));
                        }
                        TtyReadStep::Eof => {
                            if redraw {
                                eprint!("\r\x1b[2K{line_label}\r\n");
                            } else {
                                eprint!("\r\n");
                            }
                            let _ = std::io::stderr().flush();
                            break Ok(None);
                        }
                        TtyReadStep::Cancel => {
                            if redraw {
                                eprint!("\r\x1b[2K{line_label}\r\n");
                            } else {
                                eprint!("\r\n");
                            }
                            let _ = std::io::stderr().flush();
                            break Err(InputCancelled.into());
                        }
                    }
                }
                Err(e) => {
                    eprint!("\r\n");
                    let _ = std::io::stderr().flush();
                    break Err(e.into());
                }
            }
        }
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    #[test]
    fn strips_escape_sequences() {
        assert_eq!(terminal_text("\x1b[31mred\x1b[0m\x07 ok\tx\n"), "red ok\tx\n");
        assert_eq!(terminal_text("\x1b]0;title\x07after"), "after");
        assert_eq!(terminal_text("e\u{301}"), "é");
    }

    #[test]
    fn hidden_input_ignores_ctrl_alt_super_and_release_events() {
        let mut buf = String::from("sk-");
        for ev in [
            Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT)),
            Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER)),
            Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::META)),
            Event::Key(KeyEvent::new(KeyCode::Char('\x16'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new_with_kind(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Release)),
        ] {
            assert_eq!(step_tty_input(&mut buf, &ev), TtyReadStep::Continue);
        }
        assert_eq!(buf, "sk-");
    }

    #[test]
    fn hidden_input_supports_shift_unicode_paste_backspace_ctrl_u_and_ctrl_w() {
        let mut buf = String::new();
        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT))), TtyReadStep::Continue);
        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))), TtyReadStep::Continue);
        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE))), TtyReadStep::Continue);
        assert_eq!(step_tty_input(&mut buf, &Event::Paste("ant-api03-xyz\r\n\x07".into())), TtyReadStep::Continue);
        assert_eq!(buf, "Sk-ant-api03-xyz");

        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))), TtyReadStep::Continue);
        assert_eq!(buf, "Sk-ant-api03-");

        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))), TtyReadStep::Continue);
        assert_eq!(buf, "Sk-ant-api03");

        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL))), TtyReadStep::Continue);
        assert_eq!(buf, "Sk-ant-api0");

        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))), TtyReadStep::Continue);
        assert_eq!(buf, "");
    }

    #[test]
    fn hidden_input_cancel_eof_and_submit_variants() {
        let mut buf = String::from("partial");
        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))), TtyReadStep::Cancel);
        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))), TtyReadStep::Cancel);
        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))), TtyReadStep::Continue);
        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))), TtyReadStep::Submit("partial".into()));
        assert!(buf.is_empty());
        assert_eq!(step_tty_input(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))), TtyReadStep::Eof);

        let mut cr_buf = String::from("key2");
        assert_eq!(step_tty_input(&mut cr_buf, &Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))), TtyReadStep::Submit("key2".into()));
    }

    #[test]
    fn hidden_mask_formatting_caps_dots_and_pluralizes_count() {
        assert_eq!(format_hidden_mask(0), "");
        assert_eq!(format_hidden_mask(1), "• (1 char)");
        assert_eq!(format_hidden_mask(4), "•••• (4 chars)");
        assert_eq!(format_hidden_mask(8), "•••••••• (8 chars)");
        assert_eq!(format_hidden_mask(24), "•••••••• (24 chars)");
    }

    #[test]
    fn split_prompt_prefix_distinguishes_leading_newline_from_no_newline() {
        assert_eq!(split_prompt_prefix("API Key: "), (None, "API Key: "));
        assert_eq!(split_prompt_prefix("\nSelect provider: "), (Some(""), "Select provider: "));
        assert_eq!(split_prompt_prefix("Header\nPrompt: "), (Some("Header"), "Prompt: "));
    }

    #[test]
    fn windows_altgr_inserts_printable_symbols_and_backspace_deletes_full_graphemes() {
        let mut buf = String::from("http://");
        for ch in ['[', ':', ':', '1', ']', '@', '~'] {
            let ev = Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL | KeyModifiers::ALT));
            assert_eq!(step_tty_input_with(&mut buf, &ev, true), TtyReadStep::Continue);
        }
        assert_eq!(buf, "http://[::1]@~");

        // On Unix (is_windows == false), CONTROL | ALT is not AltGr and must be ignored
        let ev_unix = Event::Key(KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert_eq!(step_tty_input_with(&mut buf, &ev_unix, false), TtyReadStep::Continue);
        assert_eq!(buf, "http://[::1]@~");

        // Ctrl+V, Alt+V, and Super+V remain ignored even when is_windows == true
        for mods in [KeyModifiers::CONTROL, KeyModifiers::ALT, KeyModifiers::SUPER] {
            assert_eq!(step_tty_input_with(&mut buf, &Event::Key(KeyEvent::new(KeyCode::Char('v'), mods)), true), TtyReadStep::Continue);
        }
        assert_eq!(buf, "http://[::1]@~");

        // NFD combining sequence ("ae\u{0301}") and ZWJ emoji delete atomically on Backspace / Ctrl+H
        let mut gbuf = String::from("ae\u{0301}👩‍💻");
        assert_eq!(step_tty_input_with(&mut gbuf, &Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)), true), TtyReadStep::Continue);
        assert_eq!(gbuf, "ae\u{0301}");
        assert_eq!(step_tty_input_with(&mut gbuf, &Event::Key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)), true), TtyReadStep::Continue);
        assert_eq!(gbuf, "a");
    }
}
