use crate::common::*;
use goon::embedder::{create_embedder, EmbedderOptions};
use goon::rag::{Embedder, HashEmbedder};

const KEY: &str = "sk-supersecret-embedder-key";

fn opts(provider: &str, model: Option<&str>, base_url: Option<&str>, api_key: Option<&str>) -> EmbedderOptions {
    EmbedderOptions { provider: provider.into(), model: model.map(str::to_string), base_url: base_url.map(str::to_string), api_key: api_key.map(str::to_string), timeout_ms: None, batch_size: None }
}

fn texts(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn hash_provider_is_byte_for_byte_the_hash_embedder() {
    let embedder = create_embedder(opts("hash", None, None, None)).unwrap();
    let input = texts(&["export function alpha() { return 1; }", "beta gamma beta"]);
    let vectors = embedder.embed(input.clone(), None).await.unwrap();
    let reference = HashEmbedder.embed(input, None).await.unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0].len(), 384);
    assert_eq!(vectors, reference);
    let ignored = create_embedder(opts("hash", Some("ignored"), None, Some(KEY))).unwrap();
    assert_eq!(ignored.embed(texts(&["x"]), None).await.unwrap(), HashEmbedder.embed(texts(&["x"]), None).await.unwrap());
}

#[tokio::test]
async fn openai_provider_batches_requests_sends_the_bearer_header_and_maps_by_index() {
    let p = provider(|body, _| json_response(200, json!({ "data": body["input"].as_array().unwrap().iter().enumerate().map(|(i, _)| json!({ "index": i, "embedding": [1, 0, 0] })).collect::<Vec<_>>() }))).await;
    let mut o = opts("openai", Some("text-embedding-3-small"), Some(&p.url), Some(KEY));
    o.batch_size = Some(2);
    let vectors = create_embedder(o).unwrap().embed(texts(&["a", "b", "c"]), None).await.unwrap();
    assert_eq!(vectors.len(), 3);
    assert_eq!(vectors[2], vec![1.0, 0.0, 0.0]);
    assert_eq!(p.count(), 2);
    assert_eq!(p.path(0), "/v1/embeddings");
    assert_eq!(p.header(0, "authorization"), Some(format!("Bearer {KEY}")));
    assert_eq!(p.requests()[0], json!({ "model": "text-embedding-3-small", "input": ["a", "b"] }));
    assert_eq!(p.requests()[1], json!({ "model": "text-embedding-3-small", "input": ["c"] }));
}

#[tokio::test]
async fn openai_provider_wrong_vector_count_is_a_hard_error() {
    let p = provider(|_, _| json_response(200, json!({ "data": [{ "index": 0, "embedding": [1] }] }))).await;
    let error = create_embedder(opts("openai", Some("m"), Some(&p.url), None)).unwrap().embed(texts(&["a", "b"]), None).await.unwrap_err();
    assert_match(&error.to_string(), "vectors for 2 input");
}

#[tokio::test]
async fn ollama_provider_posts_to_api_embed_and_never_forwards_the_key() {
    let p = provider(|_, _| json_response(200, json!({ "embeddings": [[0, 1], [1, 1]] }))).await;
    let vectors = create_embedder(opts("ollama", Some("nomic-embed-text"), Some(&p.origin()), Some(KEY))).unwrap().embed(texts(&["x", "y"]), None).await.unwrap();
    assert_eq!(vectors, vec![vec![0.0, 1.0], vec![1.0, 1.0]]);
    assert_eq!(p.count(), 1);
    assert_eq!(p.path(0), "/api/embed");
    assert_eq!(p.header(0, "authorization"), None);
    assert!(!format!("{:?}", p.heads.lock().unwrap()).contains(KEY));
    assert_eq!(p.requests()[0], json!({ "model": "nomic-embed-text", "input": ["x", "y"] }));
    assert_eq!(embedder::DEFAULT_OLLAMA_BASE_URL, "http://127.0.0.1:11434");
}

#[tokio::test]
async fn errors_never_leak_the_api_key() {
    let failing = provider(|_, _| json_response(401, json!({ "error": format!("bad key {KEY}") }))).await;
    let error = create_embedder(opts("openai", Some("m"), Some(&failing.url), Some(KEY))).unwrap().embed(texts(&["a"]), None).await.unwrap_err().to_string();
    assert!(!error.contains(KEY), "{error}");
    assert_match(&error, r"\[REDACTED\]");
    assert_match(&error, "status 401");

    let networked = create_embedder(opts("openai", Some("m"), Some("http://127.0.0.1:9/v1"), Some(KEY))).unwrap().embed(texts(&["a"]), None).await.unwrap_err().to_string();
    assert!(!networked.contains(KEY), "{networked}");

    let garbage = provider(|_, _| MockResponse { status: 200, content_type: "text/html", headers: vec![], body: format!("<html>{KEY}</html>"), delay_ms: 0 }).await;
    let non_json = create_embedder(opts("openai", Some("m"), Some(&garbage.url), Some(KEY))).unwrap().embed(texts(&["a"]), None).await.unwrap_err().to_string();
    assert!(!non_json.contains(KEY), "{non_json}");
}

#[test]
fn factory_fails_closed_on_missing_model_or_base_url() {
    let no_model = create_embedder(opts("openai", None, Some("https://api.example.test/v1"), None)).err().unwrap().to_string();
    assert_match(&no_model, "requires embedderModel");
    let no_base = create_embedder(opts("openai", Some("m"), None, None)).err().unwrap().to_string();
    assert_match(&no_base, "requires embedderBaseUrl");
    assert!(create_embedder(opts("ollama", Some("m"), None, None)).is_ok());
}

#[test]
fn config_embedder_provider_defaults_to_hash_and_env_overrides_apply() {
    let f = fixture();
    let _env = fixture_env(&f);
    std::fs::create_dir_all(f.config.join("goon")).unwrap();
    std::fs::write(f.config.join("goon").join("config.json"), "{}").unwrap();
    let defaults = config::load_config(&Map::new()).unwrap();
    assert_eq!(defaults.embedder_provider, "hash");
    assert_eq!(defaults.embedder_model, None);
    std::env::set_var("GOON_EMBEDDER_PROVIDER", "ollama");
    std::env::set_var("GOON_EMBEDDER_MODEL", "nomic-embed-text");
    let from_env = config::load_config(&Map::new()).unwrap();
    assert_eq!(from_env.embedder_provider, "ollama");
    assert_eq!(from_env.embedder_model.as_deref(), Some("nomic-embed-text"));
    assert!(!from_env.memory_enabled);
    std::env::remove_var("GOON_EMBEDDER_PROVIDER");
    std::env::remove_var("GOON_EMBEDDER_MODEL");
}
