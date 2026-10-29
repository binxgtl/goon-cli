//! Embedder seam: `hash` (offline default), OpenAI-compatible `/embeddings` and Ollama
//! `/api/embed`. The API key only ever goes into the `Authorization` header of `openai` requests;
//! error messages are scrubbed of it.

use crate::rag::{Embedder, HashEmbedder};
use anyhow::{anyhow, bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_EMBEDDER_BATCH_SIZE: usize = 64;
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

fn scrub(text: &str, secret: Option<&str>) -> String {
    let clean = match secret.filter(|s| !s.is_empty()) {
        Some(s) => text.replace(s, "[REDACTED]"),
        None => text.to_string(),
    };
    crate::jsutil::take_chars(&clean, 200).to_string()
}

fn vector_of(value: &Value, source: &str) -> Result<Vec<f64>> {
    let items = value.as_array().ok_or_else(|| anyhow!("{source} returned a malformed embedding vector."))?;
    items.iter().map(|v| v.as_f64().filter(|f| f.is_finite()).ok_or_else(|| anyhow!("{source} returned a malformed embedding vector."))).collect()
}

#[derive(Clone)]
pub struct RemoteEmbedder {
    pub name: &'static str,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout_ms: u64,
    pub batch_size: usize,
}

impl RemoteEmbedder {
    async fn post(&self, body: Value, cancel: Option<CancellationToken>) -> Result<Value> {
        let path = if self.name == "openai" { "/embeddings" } else { "/api/embed" };
        let mut request = crate::providers::http_client().post(format!("{}{path}", self.base_url)).header("content-type", "application/json").timeout(Duration::from_millis(self.timeout_ms)).body(crate::jsutil::stringify(&body));
        if let Some(k) = &self.api_key {
            request = request.header("authorization", format!("Bearer {k}"));
        }
        let cancel = cancel.unwrap_or_default();
        let response = tokio::select! {
            r = request.send() => r.map_err(|e| anyhow!("{} embedder request failed: {}", self.name, scrub(&e.to_string(), self.api_key.as_deref())))?,
            _ = cancel.cancelled() => return Err(crate::harness::Aborted::default().into()),
        };
        let status = response.status();
        let raw = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let body = scrub(&raw, self.api_key.as_deref());
            bail!("{} embedder request failed with status {}: {}", self.name, status.as_u16(), if body.is_empty() { "(no body)".to_string() } else { body });
        }
        serde_json::from_str(&raw).map_err(|_| anyhow!("{} embedder returned non-JSON output: {}", self.name, scrub(&raw, self.api_key.as_deref())))
    }

    async fn embed_batch(&self, texts: &[String], cancel: Option<CancellationToken>) -> Result<Vec<Vec<f64>>> {
        let payload = self.post(json!({ "model": self.model, "input": texts }), cancel).await?;
        if self.name == "openai" {
            let data = payload.get("data").and_then(Value::as_array);
            let Some(data) = data.filter(|d| d.len() == texts.len()) else {
                bail!("openai embedder returned {} vectors for {} input(s).", payload.get("data").and_then(Value::as_array).map(|d| d.len().to_string()).unwrap_or_else(|| "no".into()), texts.len());
            };
            let mut vectors = vec![vec![]; texts.len()];
            for (pos, entry) in data.iter().enumerate() {
                let index = entry.get("index").and_then(Value::as_u64).map(|i| i as usize).filter(|i| *i < texts.len()).unwrap_or(pos);
                vectors[index] = vector_of(entry.get("embedding").unwrap_or(&Value::Null), "openai embedder")?;
            }
            return Ok(vectors);
        }
        let embeddings = payload.get("embeddings").and_then(Value::as_array);
        let Some(embeddings) = embeddings.filter(|e| e.len() == texts.len()) else {
            bail!("ollama embedder returned {} vectors for {} input(s).", payload.get("embeddings").and_then(Value::as_array).map(|d| d.len().to_string()).unwrap_or_else(|| "no".into()), texts.len());
        };
        embeddings.iter().map(|v| vector_of(v, "ollama embedder")).collect()
    }
}

impl Embedder for RemoteEmbedder {
    fn embed(&self, texts: Vec<String>, cancel: Option<CancellationToken>) -> BoxFuture<'static, Result<Vec<Vec<f64>>>> {
        let me = self.clone();
        Box::pin(async move {
            let mut out = vec![];
            for chunk in texts.chunks(me.batch_size.max(1)) {
                out.extend(me.embed_batch(chunk, cancel.clone()).await?);
            }
            Ok(out)
        })
    }
}

pub struct EmbedderOptions {
    pub provider: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout_ms: Option<u64>,
    pub batch_size: Option<usize>,
}

pub fn create_embedder(options: EmbedderOptions) -> Result<Arc<dyn Embedder>> {
    let provider = if options.provider.is_empty() { "hash".to_string() } else { options.provider.clone() };
    if provider == "hash" {
        return Ok(Arc::new(HashEmbedder));
    }
    let model = options.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).ok_or_else(|| anyhow!("embedderProvider \"{provider}\" requires embedderModel to be set."))?.to_string();
    let base = options.base_url.clone().or_else(|| (provider == "ollama").then(|| DEFAULT_OLLAMA_BASE_URL.to_string())).unwrap_or_default();
    let base = base.trim_end_matches('/').to_string();
    if base.is_empty() {
        bail!("embedderProvider \"openai\" requires embedderBaseUrl (or the chat baseUrl) to be set.");
    }
    let name: &'static str = if provider == "openai" { "openai" } else { "ollama" };
    Ok(Arc::new(RemoteEmbedder {
        name,
        model,
        base_url: base,
        api_key: if provider == "openai" { options.api_key.filter(|k| !k.is_empty()) } else { None },
        timeout_ms: options.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).max(1000),
        batch_size: options.batch_size.unwrap_or(DEFAULT_EMBEDDER_BATCH_SIZE).max(1),
    }))
}

/// The chat key only goes to an embedder on the same origin as the chat endpoint.
pub fn embedder_api_key(embedder_base_url: Option<&str>, chat_base_url: Option<&str>, chat_key: Option<&str>) -> Option<String> {
    if let Ok(dedicated) = std::env::var("GOON_EMBEDDER_API_KEY") {
        if !dedicated.trim().is_empty() {
            return Some(dedicated.trim().to_string());
        }
    }
    let key = chat_key.filter(|k| !k.is_empty())?;
    let Some(emb) = embedder_base_url else { return Some(key.to_string()) };
    let origin = |u: &str| url::Url::parse(u).ok().map(|u| u.origin().ascii_serialization());
    match (origin(emb), chat_base_url.and_then(origin)) {
        (Some(a), Some(b)) if a == b => Some(key.to_string()),
        _ => None,
    }
}
