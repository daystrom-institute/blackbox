//! Backend model catalog: the provider's own context window, hard maximum,
//! and auto-compaction limit per model, as codex reads them from its `models`
//! endpoint. The harness's static compaction table stays the fallback for
//! transports and models the catalog does not cover.
//!
//! The catalog distinguishes the window a client manages to (`context_window`)
//! from the hard ceiling the backend accepts (`max_context_window`). A session
//! can run well past the first without rejection, so every ratio the harness
//! publishes names the first as the target and the second as the ceiling.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelLimits {
    pub slug: String,
    /// The window the client manages to (codex `context_window`).
    pub context_window: Option<u64>,
    /// The hard maximum the backend accepts (codex `max_context_window`).
    pub max_context_window: Option<u64>,
    /// Explicit auto-compaction limit, when the catalog publishes one.
    pub auto_compact_token_limit: Option<u64>,
    /// Percent of the window inference may use (codex default 95).
    pub effective_context_window_percent: u64,
}

impl ModelLimits {
    /// codex `resolved_context_window`.
    pub fn target_window(&self) -> Option<u64> {
        self.context_window.or(self.max_context_window)
    }

    /// codex `auto_compact_token_limit`: 90% of the target window, capped by
    /// an explicit catalog limit when there is one.
    pub fn auto_compact_limit(&self) -> Option<u64> {
        let from_window = self.target_window().map(|window| window / 10 * 9);
        match (from_window, self.auto_compact_token_limit) {
            (Some(window), Some(explicit)) => Some(window.min(explicit)),
            (Some(window), None) => Some(window),
            (None, explicit) => explicit,
        }
    }
}

/// Parse a `models` response or codex's `models_cache.json` (same envelope).
pub(super) fn parse_catalog(value: &Value) -> Vec<ModelLimits> {
    value
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let slug = model["slug"].as_str().filter(|slug| !slug.is_empty())?;
            Some(ModelLimits {
                slug: slug.to_owned(),
                context_window: model["context_window"].as_u64(),
                max_context_window: model["max_context_window"].as_u64(),
                auto_compact_token_limit: model["auto_compact_token_limit"].as_u64(),
                effective_context_window_percent: model["effective_context_window_percent"]
                    .as_u64()
                    .unwrap_or(95),
            })
        })
        .collect()
}

/// `{backend}/models` for a `{backend}/responses` endpoint. `None` when the
/// endpoint is not shaped that way.
pub(super) fn models_endpoint(http_endpoint: &str) -> Option<String> {
    http_endpoint
        .strip_suffix("/responses")
        .map(|base| format!("{base}/models"))
}

/// The backend keys catalog visibility on the requesting client's version.
/// Mirror the version codex last recorded in its own cache so both clients
/// see the same catalog; an explicit override wins.
const DEFAULT_CLIENT_VERSION: &str = "0.154.0";

fn client_version(cached: Option<&Value>) -> String {
    super::session_var("BRO_HARNESS_CODEX_CLIENT_VERSION")
        .or_else(|| {
            cached
                .and_then(|cache| cache["client_version"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| DEFAULT_CLIENT_VERSION.to_owned())
}

pub(super) const CACHE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Codex's own `models_cache.json` under `codex_home`, when younger than
/// `max_age`. Read-only: the harness never writes codex's cache.
pub(super) async fn cached_catalog(codex_home: &Path, max_age: Duration) -> Option<Value> {
    let path = codex_home.join("models_cache.json");
    let modified = tokio::fs::metadata(&path).await.ok()?.modified().ok()?;
    if modified.elapsed().ok()? > max_age {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn fetch_catalog(
    http: &reqwest::Client,
    endpoint: &str,
    headers: Vec<(&'static str, String)>,
    client_version: &str,
) -> Result<Vec<ModelLimits>> {
    let mut request = http
        .get(format!("{endpoint}?client_version={client_version}"))
        .timeout(Duration::from_secs(10))
        .header("accept", "application/json");
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request.send().await.context("models request")?;
    let status = response.status();
    let text = response.text().await.context("read models response")?;
    anyhow::ensure!(
        status.is_success(),
        "models {status}: {}",
        text.chars().take(200).collect::<String>()
    );
    let value: Value = serde_json::from_str(&text).context("parse models response")?;
    Ok(parse_catalog(&value))
}

/// Best-effort catalog for a ChatGPT-backed session: codex's fresh cache
/// first, then the backend. Failure means an empty catalog and the built-in
/// window table, never a failed session.
pub(super) async fn load(
    http: &reqwest::Client,
    http_endpoint: &str,
    headers: Vec<(&'static str, String)>,
    codex_home: &Path,
) -> Vec<ModelLimits> {
    let Some(endpoint) = models_endpoint(http_endpoint) else {
        return Vec::new();
    };
    let cached = cached_catalog(codex_home, CACHE_MAX_AGE).await;
    if let Some(cache) = &cached {
        let models = parse_catalog(cache);
        if !models.is_empty() {
            return models;
        }
    }
    match fetch_catalog(http, &endpoint, headers, &client_version(cached.as_ref())).await {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => {
            tracing::warn!("model catalog is empty; using the built-in window table");
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(
                "model catalog unavailable ({error:#}); using the built-in window table"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> Value {
        json!({"models":[
            {"slug":"gpt-6-astra","context_window":272000,"max_context_window":872000,
             "auto_compact_token_limit":null,"effective_context_window_percent":95},
            {"slug":"capped","context_window":400000,"auto_compact_token_limit":250000},
            {"slug":"max-only","max_context_window":1000000},
            {"slug":"","context_window":1},
            {"context_window":2}
        ]})
    }

    #[test]
    fn parse_keeps_named_models_and_derives_codex_limits() {
        let models = parse_catalog(&catalog());
        assert_eq!(models.len(), 3);
        let astra = &models[0];
        assert_eq!(astra.target_window(), Some(272_000));
        assert_eq!(astra.auto_compact_limit(), Some(244_800));
        assert_eq!(astra.max_context_window, Some(872_000));
        assert_eq!(astra.effective_context_window_percent, 95);
        assert_eq!(models[1].auto_compact_limit(), Some(250_000));
        assert_eq!(models[2].target_window(), Some(1_000_000));
        assert_eq!(models[2].auto_compact_limit(), Some(900_000));
    }

    #[test]
    fn models_endpoint_derives_from_the_responses_endpoint() {
        assert_eq!(
            models_endpoint("https://chatgpt.com/backend-api/codex/responses").as_deref(),
            Some("https://chatgpt.com/backend-api/codex/models")
        );
        assert_eq!(models_endpoint("https://api.example.invalid/v1"), None);
    }

    #[tokio::test]
    async fn cached_catalog_is_read_only_while_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(cached_catalog(&root, CACHE_MAX_AGE).await.is_none());
        std::fs::write(
            root.join("models_cache.json"),
            json!({"client_version":"0.154.0","models":catalog()["models"]}).to_string(),
        )
        .unwrap();
        let cache = cached_catalog(&root, CACHE_MAX_AGE).await.unwrap();
        assert_eq!(client_version(Some(&cache)), "0.154.0");
        assert_eq!(parse_catalog(&cache).len(), 3);
        assert!(cached_catalog(&root, Duration::ZERO).await.is_none());
        assert_eq!(
            std::fs::read_to_string(root.join("models_cache.json"))
                .unwrap()
                .len(),
            json!({"client_version":"0.154.0","models":catalog()["models"]})
                .to_string()
                .len()
        );
    }
}
