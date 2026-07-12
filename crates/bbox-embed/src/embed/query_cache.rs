//! Process-wide memoization of query embeddings.
//!
//! Query strings repeat heavily across searches (agents re-running the same
//! probe, retry loops, multi-route fan-out), and the embed call is a blocking
//! HTTP round-trip per route. The cache is keyed by the exact query encoder
//! (provider, query_model, dim, dtype, query) so a route change, model bump,
//! or dtype change misses naturally; vectors for the same encoder are
//! identical regardless of which bucket routed there. Compatibility families
//! govern which partitions a cached vector may SEARCH, never cache identity —
//! two same-family query models still produce different vectors.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result};
use parking_lot::Mutex;

use super::{
    Bucket, EmbedInput, EmbedInputType, EmbeddingProvider, EmbeddingRouter, VisualRouteMeta,
};

const CACHE_CAP: usize = 256;

struct CacheEntry {
    vector: Vec<f32>,
    last_used: u64,
}

#[derive(Default)]
struct QueryEmbedCache {
    entries: HashMap<String, CacheEntry>,
    tick: u64,
}

impl QueryEmbedCache {
    fn get(&mut self, key: &str) -> Option<Vec<f32>> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(key).map(|entry| {
            entry.last_used = tick;
            entry.vector.clone()
        })
    }

    fn insert(&mut self, key: String, vector: Vec<f32>) {
        self.tick += 1;
        if self.entries.len() >= CACHE_CAP && !self.entries.contains_key(&key) {
            let evict = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            if let Some(evict) = evict {
                self.entries.remove(&evict);
            }
        }
        let last_used = self.tick;
        self.entries.insert(key, CacheEntry { vector, last_used });
    }
}

static QUERY_EMBED_CACHE: OnceLock<Mutex<QueryEmbedCache>> = OnceLock::new();

fn cache() -> &'static Mutex<QueryEmbedCache> {
    QUERY_EMBED_CACHE.get_or_init(Mutex::default)
}

/// Embed a single query string through the provider routed for `bucket`,
/// memoized process-wide. Blocking: drives the provider's async embed call to
/// completion on the current runtime (or a throwaway one), so call this from
/// blocking-pool / sync contexts only.
pub fn embed_query_cached(
    router: &EmbeddingRouter,
    bucket: Bucket,
    project_id: Option<&str>,
    query: &str,
) -> Result<Vec<f32>> {
    let route = router.route(bucket, project_id)?;
    let key = format!(
        "{}:{}:{}:{}:{}",
        route.provider_id,
        route.query_model,
        route.dimensions,
        route.output_dtype.as_str(),
        query
    );
    embed_query_cached_with(&key, || {
        let provider = router.route_for(bucket, project_id)?;
        let started = Instant::now();
        let vector = embed_single_blocking(provider.as_ref(), query)?;
        tracing::debug!(
            target: "blackbox::embed",
            provider = %route.provider_id,
            model = %route.query_model,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "query embed cache miss"
        );
        Ok(vector)
    })
}

/// Embed a single query string through a visual route (`[embed.routes.visual]`,
/// chunk-kind-keyed — see `EmbeddingRouter::visual_route`), memoized process-
/// wide through the same cache `embed_query_cached` uses. Visual routes have
/// no `Bucket`, so this mirrors `embed_query_cached`'s shape but derives the
/// cache key from a caller-supplied `VisualRouteMeta` (from
/// `EmbeddingRouter::configured_visual_routes` / `visual_route`) instead of a
/// `Route` — cheap on a cache hit, no provider constructed. `kind` is any
/// configured chunk kind that resolves to `meta`'s partition (used only to
/// build the provider via `visual_provider` on a miss); visual routes are
/// always symmetric (no separate query_model), so `meta.document_model`
/// stands in for the query-side model in the key, matching
/// `VisualRouteMeta::vector_route_id`'s inputs.
pub fn embed_query_cached_visual(
    router: &EmbeddingRouter,
    meta: &VisualRouteMeta,
    kind: &str,
    query: &str,
) -> Result<Vec<f32>> {
    let key = format!(
        "{}:{}:{}:{}:{}",
        meta.provider_id,
        meta.document_model,
        meta.dimensions,
        meta.output_dtype.as_str(),
        query
    );
    embed_query_cached_with(&key, || {
        let provider = router
            .visual_provider(kind)?
            .with_context(|| format!("visual route `{kind}` has no configured provider"))?;
        let started = Instant::now();
        let vector = embed_single_blocking(provider.as_ref(), query)?;
        tracing::debug!(
            target: "blackbox::embed",
            provider = %meta.provider_id,
            model = %meta.document_model,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "visual query embed cache miss"
        );
        Ok(vector)
    })
}

/// Cache shell around an embed closure; the closure runs only on a miss.
/// Split out so tests can count invocations without a live provider.
fn embed_query_cached_with(
    key: &str,
    embed: impl FnOnce() -> Result<Vec<f32>>,
) -> Result<Vec<f32>> {
    if let Some(vector) = cache().lock().get(key) {
        tracing::debug!(target: "blackbox::embed", "query embed cache hit");
        return Ok(vector);
    }
    let vector = embed()?;
    cache().lock().insert(key.to_string(), vector.clone());
    Ok(vector)
}

fn embed_single_blocking(provider: &dyn EmbeddingProvider, query: &str) -> Result<Vec<f32>> {
    let inputs = vec![EmbedInput::Text(query.to_string())];
    let outputs = match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(provider.embed_batch(&inputs, EmbedInputType::Query))
        }),
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new().context("creating embedding runtime")?;
            runtime.block_on(provider.embed_batch(&inputs, EmbedInputType::Query))
        }
    }?;
    outputs
        .into_iter()
        .next()
        .context("embedding provider returned no query vector")?
        .into_single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn second_lookup_for_same_key_skips_embed() {
        let calls = AtomicUsize::new(0);
        let embed = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![0.25, 0.75])
        };
        let key = "test-provider:test-model:query-cache-hit-test";
        let first = embed_query_cached_with(key, embed).unwrap();
        let second = embed_query_cached_with(key, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![9.9, 9.9])
        })
        .unwrap();
        assert_eq!(first, vec![0.25, 0.75]);
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn embed_failure_is_not_cached() {
        let key = "test-provider:test-model:query-cache-error-test";
        let err = embed_query_cached_with(key, || anyhow::bail!("provider down"));
        assert!(err.is_err());
        let recovered = embed_query_cached_with(key, || Ok(vec![1.0])).unwrap();
        assert_eq!(recovered, vec![1.0]);
    }

    #[test]
    fn eviction_keeps_recently_used_entries() {
        let mut cache = QueryEmbedCache::default();
        for i in 0..CACHE_CAP {
            cache.insert(format!("evict-test:{i}"), vec![i as f32]);
        }
        // Touch the first entry so it is the most recently used.
        assert!(cache.get("evict-test:0").is_some());
        cache.insert("evict-test:overflow".into(), vec![-1.0]);
        assert_eq!(cache.entries.len(), CACHE_CAP);
        assert!(cache.entries.contains_key("evict-test:0"));
        assert!(cache.entries.contains_key("evict-test:overflow"));
    }

    /// `embed_query_cached_visual` (the visual-route analog of
    /// `embed_query_cached`, used by hybrid search's query-side lane): a
    /// repeat query for the same visual partition must hit the process
    /// cache, not re-bill the provider. Network-free: the provider talks to
    /// a local loopback mock, never a real endpoint.
    #[tokio::test(flavor = "multi_thread")]
    async fn embed_query_cached_visual_hits_provider_once_then_caches() {
        use axum::{Json, Router, routing::post};
        use std::sync::Arc;
        use tokio::net::TcpListener;

        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/multimodalembeddings",
                post(move |Json(_body): Json<serde_json::Value>| {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        let embedding = vec![0.5_f32; 256];
                        Json(serde_json::json!({"data": [{"embedding": embedding}]}))
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        // Scoped so the lock guard drops before the `.await`s below
        // (clippy::await_holding_lock) — the variable name is unique to
        // this test, so no other test reads or writes it once set.
        // SAFETY: mutation happens under test_env_lock().
        {
            let _env = bbox_util::util::test_env_lock();
            unsafe {
                std::env::set_var("BBOX_QUERY_CACHE_VISUAL_TEST_KEY", "test-key");
            }
        }
        let router = EmbeddingRouter::from_toml_str(&format!(
            r#"
[embed.providers.voyage_visual]
type = "voyage_multimodal"
api_key_env = "BBOX_QUERY_CACHE_VISUAL_TEST_KEY"
output_dimension = 256
endpoint = "http://{addr}/v1/multimodalembeddings"

[embed.routes.visual]
pdf_figure = "voyage_visual"
"#
        ))
        .unwrap();
        let meta = router.visual_route("pdf_figure").unwrap().unwrap();

        let first = embed_query_cached_visual(
            &router,
            &meta,
            "pdf_figure",
            "embed_query_cached_visual_hits_provider_once_then_caches query",
        )
        .unwrap();
        let second = embed_query_cached_visual(
            &router,
            &meta,
            "pdf_figure",
            "embed_query_cached_visual_hits_provider_once_then_caches query",
        )
        .unwrap();

        {
            let _env = bbox_util::util::test_env_lock();
            unsafe {
                std::env::remove_var("BBOX_QUERY_CACHE_VISUAL_TEST_KEY");
            }
        }

        assert_eq!(first, vec![0.5_f32; 256]);
        assert_eq!(second, first);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second lookup must be a cache hit, not a second provider call"
        );
    }

    /// A visual query embed failure (unreachable provider) must surface as
    /// an `Err` the caller can degrade, and must not poison the cache —
    /// mirrors `embed_failure_is_not_cached` for the bucket-keyed path.
    #[tokio::test(flavor = "multi_thread")]
    async fn embed_query_cached_visual_failure_is_not_cached() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.providers.voyage_visual]
type = "voyage_multimodal"
api_key_env = "BBOX_QUERY_CACHE_VISUAL_UNREACHABLE_KEY"
output_dimension = 256
endpoint = "http://127.0.0.1:9/v1/multimodalembeddings"

[embed.routes.visual]
pdf_figure = "voyage_visual"
"#,
        )
        .unwrap();
        let meta = router.visual_route("pdf_figure").unwrap().unwrap();

        let err = embed_query_cached_visual(
            &router,
            &meta,
            "pdf_figure",
            "embed_query_cached_visual_failure_is_not_cached query",
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
