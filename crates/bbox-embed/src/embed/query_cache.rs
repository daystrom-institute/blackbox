//! Process-wide memoization of query embeddings.
//!
//! Query strings repeat heavily across searches (agents re-running the same
//! probe, retry loops, multi-route fan-out), and the embed call is a blocking
//! HTTP round-trip per route. The cache is keyed by (provider, model, query)
//! so a route change or model bump misses naturally; vectors for the same
//! provider+model are identical regardless of which bucket routed there.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result};
use parking_lot::Mutex;

use super::{Bucket, EmbeddingProvider, EmbeddingRouter};

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
    let key = format!("{}:{}:{}", route.provider_id, route.model, query);
    embed_query_cached_with(&key, || {
        let provider = router.route_for(bucket, project_id)?;
        let started = Instant::now();
        let vector = embed_single_blocking(provider.as_ref(), query)?;
        tracing::debug!(
            target: "blackbox::embed",
            provider = %route.provider_id,
            model = %route.model,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "query embed cache miss"
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
    let texts = vec![query.to_string()];
    let vectors = match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(provider.embed_batch(&texts))),
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new().context("creating embedding runtime")?;
            runtime.block_on(provider.embed_batch(&texts))
        }
    }?;
    vectors
        .into_iter()
        .next()
        .context("embedding provider returned no query vector")
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
}
