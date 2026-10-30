//! Shared redaction helpers. Event logs, memory indexes and RAG chunks all pass through here so a
//! credential never reaches disk or another process. False positives only cost a `[REDACTED]`.

use regex::Regex;
use serde_json::{Map, Value};
use std::sync::OnceLock;

const MIN_SECRET_LENGTH: usize = 6;

fn secret_env_key() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|_)(?:API_KEY|APIKEY|TOKEN|SECRET|PASSWORD)$").unwrap())
}

fn sensitive_key() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(api[_-]?key|apikey|token|secret|password|authorization|access[_-]?token|refresh[_-]?token|client[_-]?secret)$").unwrap())
}

fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=-]+").unwrap())
}

fn json_key_value_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?i)("(?:(?:api|access|refresh|client)[_-]?(?:key|token|secret)|token|secret|password|authorization)"\s*:\s*)("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')"#).unwrap())
}

fn key_value_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)((?:api[_-]?key|apikey|access[_-]?token|refresh[_-]?token|client[_-]?secret|token|secret|password|authorization)\s*[:=]\s*)(?:"[^"]*"|'[^']*'|[^\s"',}]+)"#).unwrap()
    })
}

/// Values of environment variables that look like credentials, longest first.
pub fn collect_secrets_from(env: impl IntoIterator<Item = (String, String)>) -> Vec<String> {
    let mut secrets: Vec<String> = vec![];
    for (key, value) in env {
        if value.chars().count() < MIN_SECRET_LENGTH {
            continue;
        }
        if secret_env_key().is_match(&key) && !secrets.contains(&value) {
            secrets.push(value);
        }
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets
}

pub fn collect_secrets() -> Vec<String> {
    collect_secrets_from(std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))))
}

fn normalize_secrets<'a>(secrets: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for secret in secrets {
        // Explicit secrets are redacted regardless of length; only env auto-collection uses the
        // length threshold (a stray `TOKEN=1` must not mangle every log line).
        if !secret.is_empty() && !out.contains(secret) {
            out.push(secret.clone());
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out
}

/// Replaces known literals first, then bearer tokens, then JSON and plain `key=value` pairs.
pub fn redact_secrets(text: &str, secrets: &[String]) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = text.to_string();
    for secret in normalize_secrets(secrets.iter()) {
        if out.contains(&secret) {
            out = out.replace(&secret, "[REDACTED]");
        }
    }
    out = bearer_re().replace_all(&out, "Bearer [REDACTED]").to_string();
    out = json_key_value_re().replace_all(&out, "${1}\"[REDACTED]\"").to_string();
    out = key_value_re().replace_all(&out, "${1}[REDACTED]").to_string();
    out
}

/// Deep copy with credential-looking keys and string values redacted.
pub fn redact_object(value: &Value, secrets: &[String]) -> Value {
    let known = normalize_secrets(secrets.iter());
    fn walk(current: &Value, known: &[String], depth: usize) -> Value {
        if depth > 32 {
            return current.clone();
        }
        match current {
            Value::String(s) => Value::String(redact_secrets(s, known)),
            Value::Array(items) => Value::Array(items.iter().map(|v| walk(v, known, depth + 1)).collect()),
            Value::Object(map) => {
                let mut out = Map::new();
                for (key, child) in map {
                    if sensitive_key().is_match(key) {
                        out.insert(key.clone(), Value::String("[REDACTED]".into()));
                    } else {
                        out.insert(key.clone(), walk(child, known, depth + 1));
                    }
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
    walk(value, &known, 0)
}

const SENSITIVE_BASENAMES: &[&str] = &[".npmrc", ".netrc", ".git-credentials", ".pypirc", ".pgpass", ".htpasswd", ".dockercfg", "credentials", "credentials.json", "service-account.json"];
const SENSITIVE_SEGMENTS: &[&str] = &[".ssh", ".aws", ".gnupg", "gcloud", "secrets", ".kube", ".docker", ".azure"];

/// True for paths whose contents must never be logged, indexed or sent to a model.
pub fn is_sensitive_path(file_path: &str) -> bool {
    let clean = file_path.replace('\\', "/");
    let segments: Vec<String> = clean.split('/').filter(|s| !s.is_empty()).map(str::to_lowercase).collect();
    let Some(base) = segments.last() else {
        return false;
    };
    if SENSITIVE_BASENAMES.contains(&base.as_str()) {
        return true;
    }
    if base.starts_with(".env") {
        return true;
    }
    static KEY_EXT: OnceLock<Regex> = OnceLock::new();
    if KEY_EXT.get_or_init(|| Regex::new(r"(?i)\.(?:pem|key|p12|pfx|jks|keystore|ppk|kdbx)$").unwrap()).is_match(base) {
        return true;
    }
    static ID_KEY: OnceLock<Regex> = OnceLock::new();
    if ID_KEY.get_or_init(|| Regex::new(r"(?i)^id_(?:rsa|dsa|ecdsa|ed25519)(?:\.|$)").unwrap()).is_match(base) {
        return true;
    }
    segments[..segments.len() - 1].iter().any(|s| SENSITIVE_SEGMENTS.contains(&s.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_literals_bearer_and_pairs() {
        let secrets = vec!["sk-live-123456".to_string()];
        let text = redact_secrets("key sk-live-123456 Authorization: Bearer abc.def token=xyz {\"password\": \"p\"}", &secrets);
        assert!(!text.contains("sk-live"));
        assert!(!text.contains("abc.def"));
        assert!(text.contains("token=[REDACTED]"));
        assert!(text.contains("\"password\": \"[REDACTED]\""));
    }

    #[test]
    fn redacts_object_keys() {
        let value = redact_object(&json!({"apiKey": "x", "nested": {"text": "Bearer abc"}}), &[]);
        assert_eq!(value, json!({"apiKey": "[REDACTED]", "nested": {"text": "Bearer [REDACTED]"}}));
    }

    #[test]
    fn sensitive_paths() {
        for path in [".env", "a/.ssh/id_rsa", "x.pem", ".pypirc", "a/.kube/config", "release.keystore"] {
            assert!(is_sensitive_path(path), "{path}");
        }
        assert!(!is_sensitive_path("src/keys.ts"));
    }

    #[test]
    fn collects_env_secrets() {
        let secrets = collect_secrets_from(vec![("MY_API_KEY".into(), "abcdefgh".into()), ("TOKEN".into(), "1".into()), ("PATH".into(), "/usr/bin".into())]);
        assert_eq!(secrets, vec!["abcdefgh".to_string()]);
    }
}
