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

/// `Number.prototype.toString()` for a finite or non-finite number (shortest round-trip digits,
/// decimal notation for `1e-7 <= |x| < 1e21`, exponential with an explicit sign otherwise).
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if value == 0.0 {
        return "0".into();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // `{:e}` prints the shortest round-trip digits: "d.ddde<exp>".
    let sci = format!("{:e}", value.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exp: i32 = exp.parse().unwrap_or(0);
    let k = digits.len() as i32;
    let n = exp + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let e_sign = if e < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{digits}e{e_sign}{}", e.abs())
        } else {
            format!("{}.{}e{e_sign}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    format!("{sign}{body}")
}

/// serde_json formatter whose floats print like `JSON.stringify` (ryu switches to exponent
/// notation earlier than JavaScript does, e.g. `1e-6` vs `0.000001`).
struct JsNumbers<F>(F);

impl<F: serde_json::ser::Formatter> serde_json::ser::Formatter for JsNumbers<F> {
    fn write_f64<W: ?Sized + std::io::Write>(&mut self, writer: &mut W, value: f64) -> std::io::Result<()> {
        writer.write_all(number_to_string(value).as_bytes())
    }
    fn begin_array<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.begin_array(w)
    }
    fn end_array<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_array(w)
    }
    fn begin_array_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W, first: bool) -> std::io::Result<()> {
        self.0.begin_array_value(w, first)
    }
    fn end_array_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_array_value(w)
    }
    fn begin_object<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.begin_object(w)
    }
    fn end_object<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_object(w)
    }
    fn begin_object_key<W: ?Sized + std::io::Write>(&mut self, w: &mut W, first: bool) -> std::io::Result<()> {
        self.0.begin_object_key(w, first)
    }
    fn end_object_key<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_object_key(w)
    }
    fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.begin_object_value(w)
    }
    fn end_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.0.end_object_value(w)
    }
}

fn write_js<F: serde_json::ser::Formatter>(value: &Value, formatter: F) -> String {
    use serde::Serialize;
    let mut clone = value.clone();
    normalize_numbers(&mut clone);
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, JsNumbers(formatter));
    if clone.serialize(&mut ser).is_err() {
        return "null".into();
    }
    String::from_utf8(out).unwrap_or_else(|_| "null".into())
}

/// `JSON.stringify(value)` (compact) with JS number formatting.
pub fn stringify(value: &Value) -> String {
    write_js(value, serde_json::ser::CompactFormatter)
}

/// `JSON.stringify(value, null, 2)`.
pub fn stringify_pretty(value: &Value) -> String {
    write_js(value, serde_json::ser::PrettyFormatter::with_indent(b"  "))
}

/// `JSON.parse(text)`: the parsed value, or V8's `SyntaxError` message for invalid input
/// (`Unexpected token 'x', "x = 1" is not valid JSON`, `... at position 7 (line 1 column 8)`).
pub fn parse_json(text: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => Ok(value),
        Err(error) => Err(json_syntax_error(text).unwrap_or_else(|| error.to_string())),
    }
}

/// V8's `JSON.parse` error message for `text`, or `None` when V8 would accept it.
pub fn json_syntax_error(text: &str) -> Option<String> {
    V8Json::new(text).check()
}

struct V8Json {
    src: Vec<u16>,
    pos: usize,
}

const MAX_CONTEXT: usize = 10;

impl V8Json {
    fn new(text: &str) -> Self {
        V8Json { src: text.encode_utf16().collect(), pos: 0 }
    }

    fn peek(&self) -> Option<u16> {
        self.src.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(0x20 | 0x09 | 0x0a | 0x0d)) {
            self.pos += 1;
        }
    }

    fn location(&self) -> (usize, usize) {
        let mut line = 1;
        let mut last_break = 0;
        let mut i = 0;
        while i < self.pos {
            if self.src[i] == 0x0d && i + 1 < self.pos && self.src[i + 1] == 0x0a {
                i += 1;
            }
            if self.src[i] == 0x0d || self.src[i] == 0x0a {
                line += 1;
                last_break = i + 1;
            }
            i += 1;
        }
        (line, 1 + i - last_break)
    }

    fn at(&self, template: &str) -> String {
        let (line, column) = self.location();
        format!("{template} at position {} (line {line} column {column})", self.pos)
    }

    /// `ReportUnexpectedToken(token)` without an explicit message.
    fn unexpected(&self) -> String {
        let Some(c) = self.peek() else { return "Unexpected end of JSON input".into() };
        if c == u16::from(b'"') {
            return self.at("Unexpected string in JSON");
        }
        if c == u16::from(b'-') || (u16::from(b'0')..=u16::from(b'9')).contains(&c) {
            return self.at("Unexpected number in JSON");
        }
        let whole = String::from_utf16_lossy(&self.src);
        if matches!(whole.as_str(), "[object Object]" | "undefined" | "Infinity" | "NaN") {
            return format!("\"{whole}\" is not valid JSON");
        }
        let token = String::from_utf16_lossy(&[c]);
        let len = self.src.len();
        if len <= MAX_CONTEXT * 2 {
            return format!("Unexpected token '{token}', \"{whole}\" is not valid JSON");
        }
        let slice = |a: usize, b: usize| String::from_utf16_lossy(&self.src[a..b.min(len)]);
        if self.pos < MAX_CONTEXT {
            format!("Unexpected token '{token}', \"{}\"... is not valid JSON", slice(0, self.pos + MAX_CONTEXT))
        } else if self.pos < len - MAX_CONTEXT {
            format!("Unexpected token '{token}', ...\"{}\"... is not valid JSON", slice(self.pos - MAX_CONTEXT, self.pos + MAX_CONTEXT))
        } else {
            format!("Unexpected token '{token}', ...\"{}\" is not valid JSON", slice(self.pos - MAX_CONTEXT, len))
        }
    }

    fn check(mut self) -> Option<String> {
        self.document().err()
    }

    fn document(&mut self) -> Result<(), String> {
        enum Frame {
            Object,
            Array,
        }
        let mut stack: Vec<Frame> = vec![];
        'value: loop {
            self.skip_ws();
            match self.peek() {
                Some(0x22) => self.string()?,
                Some(c) if c == u16::from(b'-') || (u16::from(b'0')..=u16::from(b'9')).contains(&c) => self.number()?,
                Some(0x7b) => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(0x7d) {
                        self.pos += 1;
                    } else {
                        self.key("Expected property name or '}'")?;
                        stack.push(Frame::Object);
                        continue 'value;
                    }
                }
                Some(0x5b) => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(0x5d) {
                        self.pos += 1;
                    } else {
                        stack.push(Frame::Array);
                        continue 'value;
                    }
                }
                Some(0x74) => self.literal("true")?,
                Some(0x66) => self.literal("false")?,
                Some(0x6e) => self.literal("null")?,
                _ => return Err(self.unexpected()),
            }
            // A value is complete: unwind finished containers.
            loop {
                self.skip_ws();
                match stack.last() {
                    None => {
                        return match self.peek() {
                            None => Ok(()),
                            Some(_) => Err(self.at("Unexpected non-whitespace character after JSON")),
                        };
                    }
                    Some(Frame::Object) => {
                        if self.peek() == Some(0x2c) {
                            self.pos += 1;
                            self.skip_ws();
                            self.key("Expected double-quoted property name")?;
                            continue 'value;
                        }
                        if self.peek() != Some(0x7d) {
                            return Err(self.at("Expected ',' or '}' after property value in JSON"));
                        }
                        self.pos += 1;
                        stack.pop();
                    }
                    Some(Frame::Array) => {
                        if self.peek() == Some(0x2c) {
                            self.pos += 1;
                            continue 'value;
                        }
                        if self.peek() != Some(0x5d) {
                            return Err(self.at("Expected ',' or ']' after array element in JSON"));
                        }
                        self.pos += 1;
                        stack.pop();
                    }
                }
            }
        }
    }

    /// A property key and its colon (whitespace before the key already skipped).
    fn key(&mut self, expected: &str) -> Result<(), String> {
        if self.peek() != Some(0x22) {
            return Err(self.at(&format!("{expected} in JSON")));
        }
        self.string()?;
        self.skip_ws();
        if self.peek() != Some(0x3a) {
            return Err(self.at("Expected ':' after property name in JSON"));
        }
        self.pos += 1;
        Ok(())
    }

    /// `ScanJsonString`, starting at the opening quote.
    fn string(&mut self) -> Result<(), String> {
        self.pos += 1;
        loop {
            let Some(c) = self.peek() else { return Err(self.at("Unterminated string in JSON")) };
            match c {
                0x22 => {
                    self.pos += 1;
                    return Ok(());
                }
                0x5c => {
                    self.pos += 1;
                    let Some(e) = self.peek() else { return Err("Unexpected end of JSON input".into()) };
                    if e > 0xff {
                        return Err(self.unexpected());
                    }
                    match e as u8 {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            for _ in 0..4 {
                                self.pos += 1;
                                if !self.peek().is_some_and(|h| (h as u32) < 0x80 && (h as u8).is_ascii_hexdigit()) {
                                    return Err(self.at("Bad Unicode escape in JSON"));
                                }
                            }
                        }
                        _ => return Err(self.at("Bad escaped character in JSON")),
                    }
                    self.pos += 1;
                }
                c if c < 0x20 => return Err(self.at("Bad control character in string literal in JSON")),
                _ => self.pos += 1,
            }
        }
    }

    fn digit(&self) -> bool {
        self.peek().is_some_and(|c| (u16::from(b'0')..=u16::from(b'9')).contains(&c))
    }

    fn number_part(&self) -> bool {
        self.peek().is_some_and(|c| c < 0x80 && matches!(c as u8, b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'))
    }

    /// `ParseJsonNumber`.
    fn number(&mut self) -> Result<(), String> {
        if self.peek() == Some(u16::from(b'-')) {
            self.pos += 1;
        }
        if self.peek() == Some(u16::from(b'0')) {
            self.pos += 1;
            if self.number_part() && self.digit() {
                return Err(self.at("Unexpected number in JSON"));
            }
        } else {
            if !self.digit() {
                return Err(self.at("No number after minus sign in JSON"));
            }
            while self.digit() {
                self.pos += 1;
            }
        }
        if self.peek() == Some(u16::from(b'.')) {
            self.pos += 1;
            if !self.digit() {
                return Err(self.at("Unterminated fractional number in JSON"));
            }
            while self.digit() {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(0x65 | 0x45)) {
            self.pos += 1;
            if matches!(self.peek(), Some(0x2b | 0x2d)) {
                self.pos += 1;
            }
            if !self.digit() {
                return Err(self.at("Exponent part is missing a number in JSON"));
            }
            while self.digit() {
                self.pos += 1;
            }
        }
        Ok(())
    }

    /// `ScanLiteral`: the first character already matched.
    fn literal(&mut self, word: &str) -> Result<(), String> {
        let expected: Vec<u16> = word.encode_utf16().collect();
        self.pos += 1;
        for &w in &expected[1..] {
            match self.peek() {
                None => return Err("Unexpected end of JSON input".into()),
                Some(c) if c == w => self.pos += 1,
                Some(_) => return Err(self.unexpected()),
            }
        }
        Ok(())
    }
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

/// `Number.prototype.toFixed(digits)`: exact decimal rounding with ties going up (Rust's `{:.N}`
/// rounds exact ties to even, so `(1.25).toFixed(1)` would print `1.2` instead of `1.3`).
pub fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() {
        return number_to_string(value);
    }
    if value.abs() >= 1e21 {
        return number_to_string(value);
    }
    let negative = value < 0.0;
    let x = value.abs();
    let body = fixed_digits(x, digits).unwrap_or_else(|| format!("{:.*}", digits, x));
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// Exact `toFixed` for `0 <= x < 1e21` using the binary expansion `m * 2^e`.
fn fixed_digits(x: f64, digits: usize) -> Option<String> {
    if digits > 20 {
        return None;
    }
    let bits = x.to_bits();
    let exp_bits = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (mantissa, exp) = if exp_bits == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp_bits - 1075) };
    let scale = 10u128.pow(digits as u32);
    let n: u128 = if exp >= 0 {
        // Integral already; `x < 1e21` keeps `mantissa << exp` below 2^70.
        ((mantissa as u128) << exp) * scale
    } else {
        let shift = (-exp) as u32;
        let scaled = (mantissa as u128) * scale; // < 2^53 * 10^20 < 2^120
        if shift >= 127 {
            0
        } else {
            let whole = scaled >> shift;
            let rem = scaled & ((1u128 << shift) - 1);
            let half = 1u128 << (shift - 1);
            if rem >= half {
                whole + 1
            } else {
                whole
            }
        }
    };
    let text = n.to_string();
    if digits == 0 {
        return Some(text);
    }
    let padded = if text.len() <= digits { format!("{}{text}", "0".repeat(digits + 1 - text.len())) } else { text };
    let split = padded.len() - digits;
    Some(format!("{}.{}", &padded[..split], &padded[split..]))
}

/// `Math.round`: the nearest integer, ties toward +Infinity.
pub fn js_round(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
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
    fn numbers_print_like_number_to_string() {
        for (value, text) in [(0.000001, "0.000001"), (1e-7, "1e-7"), (1e21, "1e+21"), (1.5e20, "150000000000000000000"), (0.1 + 0.2, "0.30000000000000004"), (-2.5e-8, "-2.5e-8"), (123.0, "123"), (1.0e16, "10000000000000000")] {
            assert_eq!(number_to_string(value), text);
        }
        assert_eq!(stringify(&json!({"a": 0.000001, "b": [1.5e20]})), r#"{"a":0.000001,"b":[150000000000000000000]}"#);
        assert_eq!(stringify_pretty(&json!({"a": [1, {"b": 0.5}], "c": {}})), "{\n  \"a\": [\n    1,\n    {\n      \"b\": 0.5\n    }\n  ],\n  \"c\": {}\n}");
    }

    #[test]
    fn to_fixed_matches_javascript() {
        for (value, digits, text) in [(1.25, 1, "1.3"), (1.005, 2, "1.00"), (2.5, 0, "3"), (-1.25, 1, "-1.3"), (0.0, 2, "0.00"), (-0.0001, 2, "-0.00"), (1234.5678, 4, "1234.5678"), (0.000123, 4, "0.0001"), (1e21, 2, "1e+21"), (f64::NAN, 2, "NaN"), (12.345, 2, "12.35"), (8.345, 2, "8.35"), (1.45, 1, "1.4")] {
            assert_eq!(to_fixed(value, digits), text, "{value}.toFixed({digits})");
        }
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(0.49999999999999994), 0.0);
    }

    #[test]
    fn json_errors_match_v8() {
        let cases = [
            ("", "Unexpected end of JSON input"),
            ("{", "Expected property name or '}' in JSON at position 1 (line 1 column 2)"),
            ("{\"a\":1,}", "Expected double-quoted property name in JSON at position 7 (line 1 column 8)"),
            ("// c\n{\"a\":1}", "Unexpected token '/', \"// c\n{\"a\":1}\" is not valid JSON"),
            ("[1", "Expected ',' or ']' after array element in JSON at position 2 (line 1 column 3)"),
            ("[1,]", "Unexpected token ']', \"[1,]\" is not valid JSON"),
            ("{\"a\" 1}", "Expected ':' after property name in JSON at position 5 (line 1 column 6)"),
            ("{\"a\":1 \"b\":2}", "Expected ',' or '}' after property value in JSON at position 7 (line 1 column 8)"),
            ("{} x", "Unexpected non-whitespace character after JSON at position 3 (line 1 column 4)"),
            ("\"abc", "Unterminated string in JSON at position 4 (line 1 column 5)"),
            ("\"a\\x\"", "Bad escaped character in JSON at position 3 (line 1 column 4)"),
            ("01", "Unexpected number in JSON at position 1 (line 1 column 2)"),
            ("-", "No number after minus sign in JSON at position 1 (line 1 column 2)"),
            ("1.", "Unterminated fractional number in JSON at position 2 (line 1 column 3)"),
            ("1e", "Exponent part is missing a number in JSON at position 2 (line 1 column 3)"),
            ("tru", "Unexpected end of JSON input"),
            ("undefined", "\"undefined\" is not valid JSON"),
            ("{\n  \"a\": 1,\n  oops: 2\n}", "Expected double-quoted property name in JSON at position 14 (line 3 column 3)"),
            ("{\"key\": \"value\", \"other\": nope}", "Unexpected token 'o', ...\"\"other\": nope}\" is not valid JSON"),
            ("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "Unexpected token 'x', \"xxxxxxxxxx\"... is not valid JSON"),
            ("\"a\\u12g4\"", "Bad Unicode escape in JSON at position 6 (line 1 column 7)"),
            ("\"a\u{1}\"", "Bad control character in string literal in JSON at position 2 (line 1 column 3)"),
            ("[\"\\中\"]", "Unexpected token '中', \"[\"\\中\"]\" is not valid JSON"),
            ("{\"a\":1}\u{a0}", "Unexpected non-whitespace character after JSON at position 7 (line 1 column 8)"),
            ("  \n\r\n  }", "Unexpected token '}', \"  \n\r\n  }\" is not valid JSON"),
            ("\"\\", "Unexpected end of JSON input"),
        ];
        for (text, message) in cases {
            assert_eq!(json_syntax_error(text).as_deref(), Some(message), "{text:?}");
        }
        assert_eq!(json_syntax_error(r#"{"a":[1,2.5e-3,{"b":null}],"c":"\u00e9"}"#), None);
        assert_eq!(parse_json("{\"a\":1,}").unwrap_err(), "Expected double-quoted property name in JSON at position 7 (line 1 column 8)");
    }

    #[test]
    fn char_slicing() {
        assert_eq!(take_chars("héllo", 2), "hé");
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(slice_chars("abcdef", 1, 3), "bc");
    }
}

/// `String(value)` for a JSON value.
pub fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(_) => stringify(value),
        Value::Array(items) => items.iter().map(|v| if v.is_null() { String::new() } else { js_string(v) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}
