//! Cheap, dependency-free syntax colouring for fenced code and diff bodies, plus the animated
//! "working" shimmer. Output only changes the foreground colour (never resets), so a caller's
//! background tint survives across tokens.

use super::text::Palette;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Comments {
    Slash,
    Hash,
    Dash,
    SlashAndHash,
}

#[derive(Clone, Copy, Debug)]
struct Lang {
    comments: Comments,
    /// `'x'` is a string (false for Rust lifetimes / Lisp quotes, which only close on a short char).
    single_quote_strings: bool,
    backtick_strings: bool,
    /// `$VAR` / `${VAR}` interpolation (shells).
    dollar_vars: bool,
    /// Whitespace-separated keyword list.
    keywords: &'static str,
}

const C_LIKE: &str = "fn let mut const static pub use mod struct enum impl trait where for while loop if else match return break continue in as async await move ref self \
    Self super crate type dyn unsafe extern true false null nil undefined function var class extends implements interface new this import from export \
    default try catch finally throw throws package func go defer chan select case switch do public private protected void int bool boolean string char \
    float double long short byte final override virtual namespace using template typename sizeof typeof instanceof of yield readonly declare keyof \
    satisfies union goto when val object data sealed open internal companion init deinit guard protocol extension inout";
const PYTHON: &str = "def class return if elif else for while in not and or is import from as with try except finally raise pass break continue lambda yield global nonlocal \
    assert del async await True False None self match case";
const SHELL: &str = "if then else elif fi for while until do done case esac in function return export local readonly source set unset shift exit true false sudo cd echo \
    printf eval exec trap";
const RUBY: &str = "def end class module if elsif else unless while until for in do return yield begin rescue ensure raise self nil true false require require_relative \
    attr_accessor attr_reader include extend then case when and or not lambda proc";
const SQL: &str = "select from where insert into values update set delete create table drop alter add index join left right inner outer on group by order having limit \
    offset as and or not null is in primary key foreign references default distinct union all case when then else end SELECT FROM WHERE INSERT INTO VALUES \
    UPDATE SET DELETE CREATE TABLE DROP ALTER ADD INDEX JOIN LEFT RIGHT INNER OUTER ON GROUP BY ORDER HAVING LIMIT OFFSET AS AND OR NOT NULL IS IN PRIMARY \
    KEY FOREIGN REFERENCES DEFAULT DISTINCT UNION ALL CASE WHEN THEN ELSE END";
const LUA: &str = "local function end if then else elseif for while do repeat until return break in and or not nil true false";
const DATA: &str = "true false null yes no on off";

fn lang_for(label: &str) -> Option<Lang> {
    let label = label.trim().to_ascii_lowercase();
    let name = label.split(|c: char| c.is_whitespace() || c == ',' || c == '{').next().unwrap_or("");
    let c_like = |backtick: bool, single: bool| Lang { comments: Comments::Slash, single_quote_strings: single, backtick_strings: backtick, dollar_vars: false, keywords: C_LIKE };
    Some(match name {
        "rust" | "rs" => c_like(false, false),
        "js" | "javascript" | "jsx" | "mjs" | "cjs" | "ts" | "typescript" | "tsx" | "mts" | "cts" | "vue" | "svelte" => c_like(true, true),
        "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "cs" | "csharp" | "java" | "kotlin" | "kt" | "kts" | "swift" | "go" | "golang" | "dart" | "scala" | "zig" | "groovy" | "gradle" | "proto"
        | "glsl" | "hlsl" | "wgsl" | "solidity" | "sol" | "objc" | "objective-c" | "v" => c_like(false, true),
        "php" => Lang { comments: Comments::SlashAndHash, single_quote_strings: true, backtick_strings: false, dollar_vars: true, keywords: C_LIKE },
        "py" | "python" | "python3" | "pyi" | "gdscript" => Lang { comments: Comments::Hash, single_quote_strings: true, backtick_strings: false, dollar_vars: false, keywords: PYTHON },
        "sh" | "bash" | "zsh" | "shell" | "fish" | "ksh" | "console" | "shellsession" | "termux" | "dockerfile" | "docker" | "make" | "makefile" | "ps1" | "powershell" | "pwsh" | "bat" | "cmd" => {
            Lang { comments: Comments::Hash, single_quote_strings: true, backtick_strings: true, dollar_vars: true, keywords: SHELL }
        }
        "rb" | "ruby" | "crystal" | "cr" | "elixir" | "ex" | "exs" => Lang { comments: Comments::Hash, single_quote_strings: true, backtick_strings: false, dollar_vars: false, keywords: RUBY },
        "sql" | "psql" | "mysql" | "sqlite" | "pgsql" => Lang { comments: Comments::Dash, single_quote_strings: true, backtick_strings: true, dollar_vars: false, keywords: SQL },
        "lua" | "luau" | "haskell" | "hs" | "elm" => Lang { comments: Comments::Dash, single_quote_strings: true, backtick_strings: false, dollar_vars: false, keywords: LUA },
        "json" | "jsonc" | "json5" => Lang { comments: Comments::Slash, single_quote_strings: false, backtick_strings: false, dollar_vars: false, keywords: DATA },
        "yaml" | "yml" | "toml" | "ini" | "conf" | "cfg" | "env" | "dotenv" | "properties" | "nix" | "hcl" | "tf" | "terraform" => {
            Lang { comments: Comments::Hash, single_quote_strings: true, backtick_strings: false, dollar_vars: true, keywords: DATA }
        }
        "css" | "scss" | "sass" | "less" => Lang { comments: Comments::Slash, single_quote_strings: true, backtick_strings: false, dollar_vars: true, keywords: "" },
        _ => return None,
    })
}

/// Whether a fence label asks for unified-diff colouring.
pub fn is_diff_lang(label: &str) -> bool {
    matches!(label.trim().to_ascii_lowercase().as_str(), "diff" | "patch" | "udiff")
}

/// Colours one row of fenced code. Unknown or plain-text labels are returned unchanged.
pub fn code(line: &str, label: &str, p: &Palette) -> String {
    if !p.is_styled() || line.is_empty() {
        return line.to_string();
    }
    if is_diff_lang(label) {
        let tone = if line.starts_with("+++") || line.starts_with("---") {
            p.bold
        } else if line.starts_with('+') {
            p.ok
        } else if line.starts_with('-') {
            p.err
        } else if line.starts_with("@@") {
            p.info
        } else {
            return line.to_string();
        };
        return format!("{tone}{line}{}{}", p.default_fg, p.bold_off);
    }
    let Some(lang) = lang_for(label) else { return line.to_string() };
    paint(line, &lang, p)
}

/// Generic colouring for diff bodies whose language is unknown: strings, numbers and comments only.
pub fn generic(line: &str, path_hint: &str, p: &Palette) -> String {
    if !p.is_styled() || line.is_empty() {
        return line.to_string();
    }
    let ext = path_hint.rsplit('.').next().unwrap_or("");
    match lang_for(ext) {
        Some(lang) => paint(line, &lang, p),
        None => line.to_string(),
    }
}

fn paint(line: &str, lang: &Lang, p: &Palette) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len() + 32);
    let mut i = 0;
    let mut plain_start = 0;
    let flush = |out: &mut String, from: usize, to: usize| out.push_str(&line[from..to]);
    let emit = |out: &mut String, plain_start: &mut usize, start: usize, end: usize, tone: &str, italic: bool| {
        flush(out, *plain_start, start);
        out.push_str(tone);
        if italic {
            out.push_str(p.italic);
        }
        out.push_str(&line[start..end]);
        out.push_str(p.default_fg);
        if italic {
            out.push_str("\x1b[23m");
        }
        if tone == p.dim && p.dim == "\x1b[2m" {
            out.push_str(p.bold_off);
        }
        *plain_start = end;
    };
    let mut prev_word_char = false;
    while i < bytes.len() {
        let c = bytes[i];
        let rest = &line[i..];
        let comment = match lang.comments {
            Comments::Slash => rest.starts_with("//") || rest.starts_with("/*"),
            Comments::Hash => c == b'#' && !prev_word_char && !rest.starts_with("#!") && !rest.starts_with("#{") || rest.starts_with("#!") && i == 0,
            Comments::Dash => rest.starts_with("--"),
            Comments::SlashAndHash => rest.starts_with("//") || rest.starts_with("/*") || (c == b'#' && !prev_word_char),
        };
        if comment {
            emit(&mut out, &mut plain_start, i, line.len(), p.dim, true);
            break;
        }
        let quote = c == b'"' || (c == b'\'' && lang.single_quote_strings) || (c == b'`' && lang.backtick_strings);
        let rust_char = c == b'\'' && !lang.single_quote_strings && short_char_literal(rest);
        if quote || rust_char {
            let end = string_end(line, i, c);
            emit(&mut out, &mut plain_start, i, end, p.ok, false);
            i = end;
            prev_word_char = false;
            continue;
        }
        if lang.dollar_vars && c == b'$' {
            let end = var_end(line, i);
            if end > i + 1 {
                emit(&mut out, &mut plain_start, i, end, p.info, false);
                i = end;
                prev_word_char = true;
                continue;
            }
        }
        if c.is_ascii_digit() && !prev_word_char {
            let mut end = i + 1;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || (bytes[end] == b'.' && bytes.get(end + 1).is_some_and(u8::is_ascii_digit))) {
                end += 1;
            }
            emit(&mut out, &mut plain_start, i, end, p.warn, false);
            i = end;
            prev_word_char = true;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let mut end = i + 1;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let word = &line[i..end];
            let preceded_by_dot = i > 0 && bytes[i - 1] == b'.';
            if !prev_word_char && !preceded_by_dot && lang.keywords.split_whitespace().any(|k| k == word) {
                emit(&mut out, &mut plain_start, i, end, p.brand, false);
            } else {
                let type_name = !prev_word_char && word.len() > 1 && word.as_bytes()[0].is_ascii_uppercase() && word.bytes().any(|b| b.is_ascii_lowercase());
                let call = bytes.get(end) == Some(&b'(') || (bytes.get(end) == Some(&b'!') && bytes.get(end + 1) == Some(&b'('));
                if type_name || call {
                    emit(&mut out, &mut plain_start, i, end, p.info, false);
                }
            }
            i = end;
            prev_word_char = true;
            continue;
        }
        prev_word_char = c >= 0x80;
        i += 1;
        // Stay on a char boundary for multi-byte text.
        while i < bytes.len() && !line.is_char_boundary(i) {
            i += 1;
        }
    }
    flush(&mut out, plain_start, line.len());
    out
}

fn short_char_literal(rest: &str) -> bool {
    let mut chars = rest.chars().skip(1);
    match chars.next() {
        Some('\\') => rest.chars().skip(2).take(10).any(|c| c == '\''),
        Some(_) => chars.next() == Some('\''),
        None => false,
    }
}

fn string_end(line: &str, start: usize, quote: u8) -> usize {
    let bytes = line.as_bytes();
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    let mut end = bytes.len().min(i);
    while end < bytes.len() && !line.is_char_boundary(end) {
        end += 1;
    }
    end
}

fn var_end(line: &str, start: usize) -> usize {
    let bytes = line.as_bytes();
    if bytes.get(start + 1) == Some(&b'{') {
        return line[start..].find('}').map(|i| start + i + 1).unwrap_or(start + 1);
    }
    let mut end = start + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if end == start + 1 && bytes.get(end).is_some_and(|b| matches!(b, b'?' | b'@' | b'#' | b'*' | b'$' | b'!')) {
        end += 1;
    }
    end
}

/// A soft highlight that sweeps across `text` (truecolor palettes only), Codex-style.
pub fn shimmer(text: &str, tick: usize, p: &Palette) -> String {
    let Some((base, hi)) = p.shimmer else {
        return text.to_string();
    };
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    if graphemes.is_empty() {
        return String::new();
    }
    const HALF: f64 = 4.0;
    let period = graphemes.len() + (HALF as usize) * 2 + 6;
    let center = (tick % period) as f64 - HALF;
    let mut out = String::with_capacity(text.len() * 20);
    let mut last = (0u8, 0u8, 0u8, false);
    for (i, g) in graphemes.iter().enumerate() {
        let d = (i as f64 - center).abs();
        let t = if d < HALF { 0.5 * (1.0 + (std::f64::consts::PI * d / HALF).cos()) } else { 0.0 };
        let mix = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
        let rgb = (mix(base.0, hi.0), mix(base.1, hi.1), mix(base.2, hi.2), true);
        if rgb != last {
            out.push_str(&format!("\x1b[38;2;{};{};{}m", rgb.0, rgb.1, rgb.2));
            last = rgb;
        }
        out.push_str(g);
    }
    out.push_str(p.default_fg);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::strip_vt;

    #[test]
    fn highlighting_keeps_text_and_only_changes_foreground() {
        let p = Palette::DARK;
        for (label, line) in [
            ("rust", "fn parse(input: &str) -> Result<i32, ParseError> { let n = 'x'; // done"),
            ("rust", "fn f<'a>(s: &'a str) -> &'a str { s }"),
            ("ts", "const x = `hi ${name}`; // c"),
            ("bash", "echo \"$HOME\" && ls -la # list"),
            ("python", "def f(x): return 'a' + str(42)  # note"),
            ("json", "{\"a\": [1, 2.5, true, null]}"),
            ("diff", "+added line"),
            ("sql", "SELECT * FROM users WHERE id = 1 -- c"),
            ("rust", "let s = \"unterminated"),
            ("rust", "let v = \"tiếng Việt\"; // chú thích"),
        ] {
            let painted = code(line, label, &p);
            assert_eq!(strip_vt(&painted), line, "{label}: {painted:?}");
            assert!(!painted.contains("\x1b[0m"), "must not reset background: {painted:?}");
        }
        assert_eq!(code("plain text", "text", &p), "plain text");
        assert_eq!(code("fn x()", "rust", &Palette::NONE), "fn x()");
        assert!(code("fn x()", "rust", &p).contains(p.brand));
    }

    #[test]
    fn shimmer_preserves_text_and_moves() {
        let p = Palette::DARK;
        let a = shimmer("Working", 3, &p);
        let b = shimmer("Working", 6, &p);
        assert_eq!(strip_vt(&a), "Working");
        assert_ne!(a, b);
        assert_eq!(shimmer("Working", 3, &Palette::ANSI16), "Working");
    }
}
