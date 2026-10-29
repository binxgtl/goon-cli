//! `generateText`-style helpers with the AI SDK retry policy (exponential backoff honoring
//! `retry-after` headers).

use crate::model::{ApiError, GenerateResponse, LanguageModel, ModelRequest};
use std::time::Duration;

fn header_delay_ms(error: &ApiError) -> Option<f64> {
    if let Some(ms) = error.headers.get("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        return Some(ms);
    }
    let raw = error.headers.get("retry-after")?;
    if let Ok(seconds) = raw.trim().parse::<f64>() {
        return Some(seconds * 1000.0);
    }
    let date = chrono::DateTime::parse_from_rfc2822(raw.trim()).ok()?;
    Some((date.timestamp_millis() - crate::jsutil::now_millis()) as f64)
}

/// Delay before the next attempt (AI SDK `retryWithExponentialBackoffRespectingRetryHeaders`).
pub fn retry_delay(error: &ApiError, exponential_ms: f64) -> Duration {
    if let Some(ms) = header_delay_ms(error) {
        if (0.0..60_000.0).contains(&ms) || ms < exponential_ms {
            return Duration::from_millis(ms.max(0.0) as u64);
        }
    }
    Duration::from_millis(exponential_ms as u64)
}

/// Calls `model.generate` with up to `max_retries` retries for retryable API errors.
pub async fn generate_text(model: &dyn LanguageModel, request: ModelRequest, max_retries: u32) -> Result<GenerateResponse, ApiError> {
    let mut delay = 2000.0;
    let mut errors: Vec<ApiError> = vec![];
    loop {
        match model.generate(request.clone()).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                if error.aborted || request.cancel.is_cancelled() {
                    return Err(error);
                }
                let attempts = errors.len() as u32 + 1;
                if max_retries == 0 {
                    return Err(error);
                }
                if !error.retryable {
                    if errors.is_empty() {
                        return Err(error);
                    }
                    return Err(ApiError { message: format!("Failed after {attempts} attempts with non-retryable error: '{}'", error.message), ..error });
                }
                if attempts > max_retries {
                    return Err(ApiError { message: format!("Failed after {attempts} attempts. Last error: {}", error.message), ..error });
                }
                let wait = retry_delay(&error, delay);
                errors.push(error);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = request.cancel.cancelled() => return Err(ApiError::aborted()),
                }
                delay *= 2.0;
            }
        }
    }
}
