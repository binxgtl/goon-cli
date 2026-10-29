//! Small helpers that reproduce JavaScript/Node semantics the TypeScript implementation relied on
//! (number formatting, `localeCompare`, ISO timestamps, `String.prototype.slice` on code points),
//! so files written by the Rust port stay byte-compatible with the ones the TS build produced.

use serde_json::{Map, Number, Value};
use std::cmp::Ordering;

/// `JSON.stringify(n)` for a finite number: integral values print without a fraction.
pub fn num(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        if value < 0.0 {
            return Value::Number(Number::from(value as i64));
        }
        return Value::Number(Number::from(value as u64));
    }
    Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
}

/// Serde helper: serialize an `f64` the way `JSON.stringify` does (`0` not `0.0`).
pub mod js_f64 {
    use serde::Serializer;
    pub fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
            serializer.serialize_i64(*value as i64)
        } else {
            serializer.serialize_f64(*value)
        }
    }
}

/// Serde helper for `Option<f64>` with JS number formatting.
pub mod js_f64_opt {
    use serde::Serializer;
    pub fn serialize<S: Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => super::js_f64::serialize(v, serializer),
            None => serializer.serialize_none(),
        }
    }
}

/// Normalizes every float in a JSON value that is integral to an integer (JS number semantics).
pub fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(n) => {
            if n.is_f64() {
                if let Some(f) = n.as_f64() {
                    *value = num(f);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_numbers),
        Value::Object(map) => map.values_mut().for_each(normalize_numbers),
        _ => {}
    }
}

/// `JSON.stringify(value)` (compact) with JS number formatting.
pub fn stringify(value: &Value) -> String {
    let mut clone = value.clone();
    normalize_numbers(&mut clone);
    serde_json::to_string(&clone).unwrap_or_else(|_| "null".into())
}

/// `JSON.stringify(value, null, 2)`.
pub fn stringify_pretty(value: &Value) -> String {
    let mut clone = value.clone();
    normalize_numbers(&mut clone);
    serde_json::to_string_pretty(&clone).unwrap_or_else(|_| "null".into())
}

/// `JSON.stringify(str)` for a plain string.
pub fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// Approximation of ICU root collation used by `String.prototype.localeCompare` for the ASCII
/// identifiers, file names and ids this code sorts: case-insensitive first, lowercase before
/// uppercase on ties, then code point order.
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    let fold = |s: &str| -> Vec<char> { s.chars().flat_map(|c| c.to_lowercase()).collect() };
    let (fa, fb) = (fold(a), fold(b));
    // ICU ignores punctuation at the primary level only for some characters; keep it simple and
    // stable: compare folded strings, then lowercase-first, then raw.
    match compare_alnum_first(&fa, &fb) {
        Ordering::Equal => {}
        other => return other,
    }
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            let la = ca.is_lowercase();
            let lb = cb.is_lowercase();
            if la && !lb {
                return Ordering::Less;
            }
            if lb && !la {
                return Ordering::Greater;
            }
            return ca.cmp(&cb);
        }
    }
    a.len().cmp(&b.len())
}

/// ICU sorts punctuation/whitespace before digits before letters; ASCII code points already put
/// most punctuation before digits before letters except `_`, `[`..`` ` `` and `{`..`~`.
fn collation_key(c: char) -> (u8, u32) {
    if c.is_whitespace() {
        (0, c as u32)
    } else if c.is_alphanumeric() {
        if c.is_ascii_digit() {
            (2, c as u32)
        } else {
            (3, c as u32)
        }
    } else {
        (1, c as u32)
    }
}

fn compare_alnum_first(a: &[char], b: &[char]) -> Ordering {
    for (ca, cb) in a.iter().zip(b.iter()) {
        let ka = collation_key(*ca);
        let kb = collation_key(*cb);
        match ka.cmp(&kb) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    iso_from_millis(now_millis())
}

pub fn iso_from_millis(ms: i64) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(|| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(0).unwrap());
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `Date.now()`.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `Date.parse(iso)` for the ISO strings this program writes; `None` when unparsable.
pub fn parse_iso_millis(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|d| d.timestamp_millis())
}

/// True for strings accepted by zod's `z.string().datetime()` (UTC `Z` suffix, optional fraction).
pub fn is_iso_datetime(text: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?Z$").unwrap())
        .is_match(text)
        && parse_iso_millis(text).is_some()
}

/// Number of UTF-16 code units (`string.length` in JS).
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `text.slice(0, n)` counted in characters (code points).
pub fn take_chars(text: &str, n: usize) -> &str {
    match text.char_indices().nth(n) {
        Some((idx, _)) => &text[..idx],
        None => text,
    }
}

/// `text.slice(-n)` counted in characters.
pub fn tail_chars(text: &str, n: usize) -> &str {
    let count = text.chars().count();
    if n >= count {
        return text;
    }
    let skip = count - n;
    match text.char_indices().nth(skip) {
        Some((idx, _)) => &text[idx..],
        None => "",
    }
}

/// `text.slice(start, end)` in characters (both bounds clamped).
pub fn slice_chars(text: &str, start: usize, end: usize) -> &str {
    if start >= end {
        return "";
    }
    let mut begin_byte = text.len();
    let mut end_byte = text.len();
    for (i, (idx, _)) in text.char_indices().enumerate() {
        if i == start {
            begin_byte = idx;
        }
        if i == end {
            end_byte = idx;
            break;
        }
    }
    if begin_byte > end_byte {
        return "";
    }
    &text[begin_byte..end_byte]
}

pub fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// `Object.entries(value)` sorted with `localeCompare`, recursively (stable identity of a value).
pub fn stable_stringify(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(stable_stringify).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| locale_cmp(a, b));
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", quote(k), stable_stringify(&map[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        other => stringify(other),
    }
}

/// Recursively sorts object keys (`canonicalize` in the TS harness).
pub fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| locale_cmp(a, b));
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), canonicalize(&map[k]));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `process.platform` value for this build.
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "android" => "android",
        "freebsd" => "freebsd",
        "openbsd" => "openbsd",
        _ => "linux",
    }
}

pub fn is_windows() -> bool {
    cfg!(windows)
}

/// `os.homedir()`: HOME (POSIX) / USERPROFILE (Windows), falling back to the other.
pub fn home_dir() -> std::path::PathBuf {
    let candidates: [&str; 2] = if cfg!(windows) { ["USERPROFILE", "HOME"] } else { ["HOME", "USERPROFILE"] };
    for name in candidates {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                return std::path::PathBuf::from(value);
            }
        }
    }
    std::path::PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" })
}

/// Truthy check for JS-style optional booleans in JSON (`value === true`).
pub fn is_true(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true)))
}

pub fn as_str<'a>(value: Option<&'a Value>) -> Option<&'a str> {
    value.and_then(Value::as_str)
}

/// Number formatting like `Number.prototype.toFixed(digits)`.
pub fn to_fixed(value: f64, digits: usize) -> String {
    format!("{:.*}", digits, value)
}

/// `Math.round` (half away from zero for positives, toward +inf for .5 negatives like JS).
pub fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_format_like_js() {
        assert_eq!(stringify(&json!({"a": 0.0, "b": 0.15, "c": 3.0})), r#"{"a":0,"b":0.15,"c":3}"#);
    }

    #[test]
    fn locale_compare_orders_case_insensitively() {
        let mut names = vec!["b", "B", "a", "A", "_x", "1"];
        names.sort_by(|a, b| locale_cmp(a, b));
        assert_eq!(names, vec!["_x", "1", "a", "A", "b", "B"]);
    }

    #[test]
    fn iso_format() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00.000Z");
        assert!(is_iso_datetime("2026-10-29T10:22:57.123Z"));
        assert!(!is_iso_datetime("2026-10-29 10:22"));
    }

    #[test]
    fn char_slicing() {
        assert_eq!(take_chars("héllo", 2), "hé");
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(slice_chars("abcdef", 1, 3), "bc");
    }
}
