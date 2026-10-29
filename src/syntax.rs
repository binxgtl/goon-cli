//! Cheap, dependency-free syntax guard for the files the agent writes.
//!
//! Deliberately a heuristic, not a parser: it catches an edit that drops a closing brace without a
//! native parser. It must not reject valid code, so strings, comments, template literals and regex
//! literals are skipped.

use anyhow::{bail, Result};

const JS_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"];
const REGEX_PRECEDING_CHARS: &[char] = &['(', ',', '=', ':', '[', '!', '&', '|', '?', '{', '}', ';', '+', '-', '*', '%', '^', '~', '<', '>'];
const REGEX_PRECEDING_WORDS: &[&str] = &["return", "typeof", "instanceof", "in", "of", "new", "delete", "void", "throw", "case", "do", "else", "yield", "await"];

/// JSON files that legitimately allow comments/trailing commas (JSONC).
pub fn is_jsonc_path(path: &str) -> bool {
    let posix = path.replace('\\', "/");
    let base = posix.rsplit('/').next().unwrap_or("").to_string();
    let lower = base.to_lowercase();
    if (lower.starts_with("tsconfig") || lower.starts_with("jsconfig")) && lower.ends_with(".json") {
        return true;
    }
    if lower.ends_with(".jsonc") || lower == "devcontainer.json" {
        return true;
    }
    posix.starts_with(".vscode/") || posix.contains("/.vscode/")
}

fn extension_of(path: &str) -> String {
    let base = path.replace('\\', "/").rsplit('/').next().unwrap_or("").to_string();
    match base.rfind('.') {
        Some(dot) if dot > 0 => base[dot..].to_lowercase(),
        _ => String::new(),
    }
}

/// Validates `content` as if it were written to `path`; returns the first problem as an error.
pub fn validate_syntax(path: &str, content: &str) -> Result<()> {
    let ext = extension_of(path);
    if ext == ".json" {
        if is_jsonc_path(path) {
            return Ok(());
        }
        let text = content.strip_prefix('\u{feff}').unwrap_or(content);
        if let Err(error) = serde_json::from_str::<serde_json::Value>(text) {
            bail!("invalid JSON: {}", error);
        }
        return Ok(());
    }
    if !JS_EXTENSIONS.contains(&ext.as_str()) {
        return Ok(());
    }
    validate_brackets(content)
}

#[derive(Clone, Copy, PartialEq)]
enum Opener {
    Brace,
    Paren,
    Bracket,
    Template,
}

impl Opener {
    fn text(self) -> &'static str {
        match self {
            Opener::Brace => "{",
            Opener::Paren => "(",
            Opener::Bracket => "[",
            Opener::Template => "${",
        }
    }
    fn closer(self) -> char {
        match self {
            Opener::Brace | Opener::Template => '}',
            Opener::Paren => ')',
            Opener::Bracket => ']',
        }
    }
}

#[derive(PartialEq)]
enum State {
    Code,
    Template,
    Single,
    Double,
    Block,
    Line,
    Regex,
}

fn regex_allowed(last_char: Option<char>, last_word: &str) -> bool {
    match last_char {
        None => true,
        Some(c) if REGEX_PRECEDING_CHARS.contains(&c) => true,
        _ => REGEX_PRECEDING_WORDS.contains(&last_word),
    }
}

fn validate_brackets(content: &str) -> Result<()> {
    let chars: Vec<char> = content.chars().collect();
    let mut stack: Vec<(Opener, usize)> = vec![];
    let mut line = 1usize;
    let mut i = 0usize;
    let mut state = State::Code;
    let mut last_char: Option<char> = None;
    let mut last_word = String::new();
    while i < chars.len() {
        let ch = chars[i];
        match state {
            State::Line => {
                if ch == '\n' {
                    line += 1;
                    state = State::Code;
                }
                i += 1;
                continue;
            }
            State::Block => {
                if ch == '\n' {
                    line += 1;
                } else if ch == '*' && chars.get(i + 1) == Some(&'/') {
                    state = State::Code;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            State::Single | State::Double => {
                if ch == '\\' {
                    if chars.get(i + 1) == Some(&'\n') {
                        line += 1;
                    }
                    i += 2;
                    continue;
                }
                if ch == '\n' {
                    // An unescaped newline ends a plain string literal in JS. Ending the string here
                    // keeps a stray apostrophe (JSX text like `Don't`) from swallowing the file.
                    line += 1;
                    state = State::Code;
                    last_char = Some('a');
                    i += 1;
                    continue;
                }
                if (state == State::Single && ch == '\'') || (state == State::Double && ch == '"') {
                    state = State::Code;
                }
                i += 1;
                continue;
            }
            State::Template => {
                if ch == '\\' {
                    if chars.get(i + 1) == Some(&'\n') {
                        line += 1;
                    }
                    i += 2;
                    continue;
                }
                if ch == '\n' {
                    line += 1;
                }
                if ch == '`' {
                    state = State::Code;
                    last_char = Some('`');
                    last_word.clear();
                    i += 1;
                    continue;
                }
                if ch == '$' && chars.get(i + 1) == Some(&'{') {
                    stack.push((Opener::Template, line));
                    state = State::Code;
                    last_char = None;
                    last_word.clear();
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            State::Regex => {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == '[' {
                    i += 1;
                    while i < chars.len() {
                        if chars[i] == '\\' {
                            i += 2;
                            continue;
                        }
                        if chars[i] == ']' {
                            i += 1;
                            break;
                        }
                        if chars[i] == '\n' {
                            line += 1;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                if ch == '/' {
                    state = State::Code;
                    last_char = Some('a');
                    last_word.clear();
                    i += 1;
                    continue;
                }
                if ch == '\n' {
                    line += 1;
                    state = State::Code;
                    last_char = Some('a');
                    i += 1;
                    continue;
                }
                i += 1;
                continue;
            }
            State::Code => {}
        }
        if ch == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if ch == '/' && chars.get(i + 1) == Some(&'/') {
            state = State::Line;
            i += 2;
            continue;
        }
        if ch == '/' && chars.get(i + 1) == Some(&'*') {
            state = State::Block;
            i += 2;
            continue;
        }
        if ch == '/' {
            if regex_allowed(last_char, &last_word) {
                state = State::Regex;
                i += 1;
                continue;
            }
            last_char = Some('/');
            last_word.clear();
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            state = if ch == '"' { State::Double } else { State::Single };
            last_char = Some(ch);
            last_word.clear();
            i += 1;
            continue;
        }
        if ch == '`' {
            state = State::Template;
            last_char = Some('`');
            last_word.clear();
            i += 1;
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' {
            let mut end = i + 1;
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_' || chars[end] == '$') {
                end += 1;
            }
            last_word = chars[i..end].iter().collect();
            last_char = Some('a');
            i = end;
            continue;
        }
        if ch.is_ascii_digit() {
            let mut end = i + 1;
            while end < chars.len() && (chars[end].is_ascii_hexdigit() || "._xXoObB+-".contains(chars[end])) {
                end += 1;
            }
            last_char = Some('0');
            last_word.clear();
            i = end;
            continue;
        }
        if ch == '{' || ch == '(' || ch == '[' {
            let opener = match ch {
                '{' => Opener::Brace,
                '(' => Opener::Paren,
                _ => Opener::Bracket,
            };
            stack.push((opener, line));
            last_char = Some(ch);
            last_word.clear();
            i += 1;
            continue;
        }
        if ch == '}' || ch == ')' || ch == ']' {
            let top = stack.last().copied();
            if ch == '}' && matches!(top, Some((Opener::Template, _))) {
                stack.pop();
                state = State::Template;
                last_char = Some('}');
                last_word.clear();
                i += 1;
                continue;
            }
            let expected = match ch {
                '}' => Opener::Brace,
                ')' => Opener::Paren,
                _ => Opener::Bracket,
            };
            let Some((opener, opened_at)) = top else {
                bail!("Unexpected '{ch}' at line {line}.");
            };
            if opener != expected {
                bail!("Mismatched '{ch}' at line {line}; expected '{}' to close '{}' from line {opened_at}.", opener.closer(), opener.text());
            }
            stack.pop();
            last_char = Some(ch);
            last_word.clear();
            i += 1;
            continue;
        }
        if !ch.is_whitespace() {
            last_char = Some(ch);
            last_word.clear();
        }
        i += 1;
    }
    if let Some((opener, opened_at)) = stack.last() {
        if *opener == Opener::Template {
            bail!("Unclosed template expression at line {opened_at}; expected '}}'.");
        }
        bail!("Unclosed '{}' at line {opened_at}; expected '{}'.", opener.text(), opener.closer());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_code() {
        for code in [
            "const a = { b: [1, 2], c: (x) => x };",
            "const re = /[(]/; const s = '}'; // }\n/* ( */",
            "const t = `a ${ { x: 1 }.x } b`;",
            "if (a) { return /x/.test(b) }",
            "const jsx = <p>Don't stop</p>;\nfunction f() { return 1; }",
        ] {
            validate_syntax("a.ts", code).unwrap();
        }
    }

    #[test]
    fn rejects_unbalanced() {
        assert!(validate_syntax("a.ts", "function f() {").unwrap_err().to_string().contains("Unclosed '{' at line 1"));
        assert!(validate_syntax("a.js", "a)").is_err());
        assert!(validate_syntax("a.json", "{").unwrap_err().to_string().contains("invalid JSON"));
        validate_syntax("tsconfig.json", "{ // c\n }").unwrap();
        validate_syntax("a.txt", "{").unwrap();
    }
}
