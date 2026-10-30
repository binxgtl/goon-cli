//! Error text that is safe to show: the provider key is never echoed and messages are bounded.

/// `safeError(error, apiKey)`: redacts the key and caps the message at 2000 characters.
pub fn safe_error(message: &str, api_key: Option<&str>) -> String {
    let mut text = message.to_string();
    if let Some(key) = api_key {
        if !key.is_empty() {
            text = text.split(key).collect::<Vec<_>>().join("[REDACTED]");
        }
    }
    crate::jsutil::take_chars(&text, 2000).to_string()
}

/// Formats an `anyhow::Error` (top-level message only, like `error.message`).
pub fn message_of(error: &anyhow::Error) -> String {
    error.to_string()
}

pub fn safe_anyhow(error: &anyhow::Error, api_key: Option<&str>) -> String {
    safe_error(&message_of(error), api_key)
}
