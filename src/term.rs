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

/// Reads a line without echo on a TTY (API keys); falls back to a plain line otherwise.
pub async fn read_hidden(label: &str) -> Result<Option<String>> {
    if !stdin_is_tty() {
        return read_line(label, None).await;
    }
    eprint!("{label}");
    let _ = std::io::stderr().flush();
    let result = tokio::task::spawn_blocking(|| -> Result<Option<String>> {
        use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
        crossterm::terminal::enable_raw_mode()?;
        let mut out = String::new();
        let outcome = loop {
            match read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter => break Ok(Some(out)),
                    KeyCode::Backspace => {
                        out.pop();
                    }
                    KeyCode::Esc => break Err(InputCancelled.into()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break Err(InputCancelled.into()),
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) && out.is_empty() => break Ok(None),
                    KeyCode::Char(c) => out.push(c),
                    _ => {}
                },
                Ok(Event::Paste(text)) => out.push_str(&text),
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            }
        };
        let _ = crossterm::terminal::disable_raw_mode();
        eprintln!();
        outcome
    })
    .await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escape_sequences() {
        assert_eq!(terminal_text("\x1b[31mred\x1b[0m\x07 ok\tx\n"), "red ok\tx\n");
        assert_eq!(terminal_text("\x1b]0;title\x07after"), "after");
        assert_eq!(terminal_text("e\u{301}"), "é");
    }
}
