use std::collections::{HashMap, VecDeque};
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Query, State as AxumState};
use axum::response::sse::{Event, Sse};
use futures::Stream;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::index;
use crate::orchestration::providers::Provider;
use crate::server::state::SharedState;
use crate::tools::bro_helpers::split_csv;

const SESSION_FILE_CACHE_CAPACITY: usize = 512;
const SESSION_FILE_CACHE_MISS_TTL: Duration = Duration::from_secs(5);

static SESSION_FILE_CACHE: OnceLock<Mutex<SessionFileCache>> = OnceLock::new();

#[derive(Debug, Clone)]
enum CachedLookup<T> {
    Hit(T),
    Miss { expires_at: Instant },
}

#[derive(Debug)]
struct TimedLookupCache<T> {
    capacity: usize,
    miss_ttl: Duration,
    entries: HashMap<String, CachedLookup<T>>,
    order: VecDeque<String>,
}

type SessionFileCache = TimedLookupCache<String>;

impl<T: Clone> TimedLookupCache<T> {
    fn new(capacity: usize, miss_ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            miss_ttl,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &str, now: Instant) -> Option<Option<T>> {
        match self.entries.get(key) {
            Some(CachedLookup::Hit(value)) => Some(Some(value.clone())),
            Some(CachedLookup::Miss { expires_at }) if *expires_at > now => Some(None),
            Some(CachedLookup::Miss { .. }) => {
                self.entries.remove(key);
                self.order.retain(|existing| existing != key);
                None
            }
            None => None,
        }
    }

    fn insert(&mut self, key: String, value: Option<T>, now: Instant) {
        if !self.entries.contains_key(&key) {
            while self.entries.len() >= self.capacity {
                let Some(oldest) = self.order.pop_front() else {
                    break;
                };
                self.entries.remove(&oldest);
            }
            self.order.push_back(key.clone());
        }

        let entry = match value {
            Some(value) => CachedLookup::Hit(value),
            None => CachedLookup::Miss {
                expires_at: now + self.miss_ttl,
            },
        };
        self.entries.insert(key, entry);
    }
}

fn session_file_cache() -> &'static Mutex<SessionFileCache> {
    SESSION_FILE_CACHE.get_or_init(|| {
        Mutex::new(SessionFileCache::new(
            SESSION_FILE_CACHE_CAPACITY,
            SESSION_FILE_CACHE_MISS_TTL,
        ))
    })
}

async fn cached_session_file_for_event(
    session_id: &str,
    roots: &[(String, PathBuf)],
    codex_root: Option<&StdPath>,
) -> Option<String> {
    resolve_session_file_cached(
        session_file_cache(),
        session_id,
        roots,
        codex_root,
        |session_id, roots, codex_root| {
            index::find_session_file(&session_id, &roots, codex_root.as_deref())
                .map(|path| path.to_string_lossy().into_owned())
        },
    )
    .await
}

async fn resolve_session_file_cached<F>(
    cache: &Mutex<SessionFileCache>,
    session_id: &str,
    roots: &[(String, PathBuf)],
    codex_root: Option<&StdPath>,
    resolver: F,
) -> Option<String>
where
    F: FnOnce(String, Vec<(String, PathBuf)>, Option<PathBuf>) -> Option<String> + Send + 'static,
{
    let now = Instant::now();
    if let Some(cached) = {
        let mut cache = cache.lock().expect("session file cache poisoned");
        cache.get(session_id, now)
    } {
        return cached;
    }

    let lookup_session_id = session_id.to_string();
    let cache_session_id = lookup_session_id.clone();
    let log_session_id = lookup_session_id.clone();
    let lookup_roots = roots.to_vec();
    let lookup_codex_root = codex_root.map(StdPath::to_path_buf);
    let resolved = match tokio::task::spawn_blocking(move || {
        resolver(lookup_session_id, lookup_roots, lookup_codex_root)
    })
    .await
    {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(
                session_id = %log_session_id,
                error = %err,
                "session file resolution failed"
            );
            None
        }
    };

    {
        let mut cache = cache.lock().expect("session file cache poisoned");
        cache.insert(cache_session_id, resolved.clone(), Instant::now());
    }

    resolved
}

// ---------------------------------------------------------------------------
// Tail SSE endpoint (outside MCP)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct TailQuery {
    /// Comma-separated bro names (the brofile a named dispatch used).
    /// Accepts legacy `bro=`.
    #[serde(default, alias = "bro")]
    bros: Option<String>,
    /// Comma-separated session IDs — matches events by their task's session_id.
    #[serde(default, alias = "session")]
    sessions: Option<String>,
    /// Comma-separated provider names. Accepts legacy `provider=`.
    #[serde(default, alias = "provider")]
    providers: Option<String>,
}

pub(crate) async fn tail_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<TailQuery>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = state.tail_tx.subscribe();
    let config = state.idx.read().reindex_config();

    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();
    let no_selectors =
        wanted_bros.is_empty() && wanted_sessions.is_empty() && wanted_providers.is_empty();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let tid = event.task_id();
                    let (task_provider, task_session_id, task_bro_label) = {
                        let store = state.task_store.read();
                        store.get(tid)
                            .map(|t| {
                                let inner = t.inner.lock();
                                (
                                    Some(inner.provider),
                                    Some(inner.session_id.clone()),
                                    inner.bro_label.clone(),
                                )
                            })
                            .unwrap_or((None, None, None))
                    };
                    // Provider is a filter that applies on top of the selector
                    // union. Bros and sessions are OR'd together: match ANY
                    // specified selector across them; a category being empty
                    // means it contributes no matches (but also doesn't reject).
                    let provider_ok = wanted_providers.is_empty()
                        || task_provider.map(|p| wanted_providers.contains(&p)).unwrap_or(false);
                    let selectors_specified = !wanted_bros.is_empty() || !wanted_sessions.is_empty();
                    let selector_match = if !selectors_specified {
                        true
                    } else {
                        let bro_m = task_bro_label
                            .as_deref()
                            .is_some_and(|label| wanted_bros.iter().any(|w| w == label));
                        let session_m = task_session_id.as_deref()
                            .map(|s| wanted_sessions.iter().any(|w| w == s))
                            .unwrap_or(false);
                        bro_m || session_m
                    };
                    if !(no_selectors || (provider_ok && selector_match)) {
                        continue;
                    }

                    let mut evt_json = serde_json::to_value(&event).unwrap_or_default();
                    if let Some(label) = &task_bro_label {
                        evt_json["bro_name"] = Value::String(label.clone());
                        evt_json["bro_selector"] = Value::String(label.clone());
                    }
                    if let Some(ref sid) = task_session_id {
                        if sid.as_str() != "pending" {
                            evt_json["session_id"] = Value::String(sid.clone());
                            if let Some(path) = cached_session_file_for_event(
                                sid,
                                &config.roots,
                                config.codex_root.as_deref(),
                            )
                            .await
                            {
                                evt_json["jsonl_path"] = Value::String(path);
                            }
                        }
                    }
                    let data = serde_json::to_string(&evt_json).unwrap_or_default();
                    yield Ok(Event::default().data(data));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("tail subscriber lagged by {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
}

pub(crate) async fn roster_stream_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = state.roster_tx.subscribe();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(delta) => {
                    let event_name = match &delta {
                        bro_protocol::RosterDelta::Added { .. } => "added",
                        bro_protocol::RosterDelta::Updated { .. } => "updated",
                        bro_protocol::RosterDelta::Removed { .. } => "removed",
                    };
                    match Event::default().event(event_name).json_data(&delta) {
                        Ok(event) => yield Ok::<Event, std::convert::Infallible>(event),
                        Err(err) => tracing::warn!("failed to serialize roster delta: {err}"),
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("roster subscriber lagged by {n} events; signaling resync");
                    let payload = serde_json::json!({
                        "reason": "lagged",
                        "skipped": n,
                    });
                    match Event::default().event("resync").json_data(&payload) {
                        Ok(event) => yield Ok::<Event, std::convert::Infallible>(event),
                        Err(err) => tracing::warn!("failed to serialize roster resync event: {err}"),
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn timed_lookup_cache_evicts_oldest_entry_when_bounded() {
        let mut cache = SessionFileCache::new(2, Duration::from_secs(1));
        let now = Instant::now();

        cache.insert("sess-a".to_string(), Some("/a.jsonl".to_string()), now);
        cache.insert("sess-b".to_string(), Some("/b.jsonl".to_string()), now);
        cache.insert("sess-c".to_string(), Some("/c.jsonl".to_string()), now);

        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.get("sess-a", now), None);
        assert_eq!(cache.get("sess-b", now), Some(Some("/b.jsonl".to_string())));
        assert_eq!(cache.get("sess-c", now), Some(Some("/c.jsonl".to_string())));
    }

    #[tokio::test]
    async fn session_file_cache_hit_avoids_resolution() {
        let cache = Mutex::new(SessionFileCache::new(8, Duration::from_millis(10)));
        let calls = Arc::new(AtomicUsize::new(0));
        let expected = "/tmp/sess-cache.jsonl".to_string();

        let first = resolve_session_file_cached(&cache, "sess-cache", &[], None, {
            let calls = calls.clone();
            let expected = expected.clone();
            move |_, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(expected)
            }
        })
        .await;
        assert_eq!(first, Some(expected.clone()));

        let second = resolve_session_file_cached(&cache, "sess-cache", &[], None, {
            let calls = calls.clone();
            move |_, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            }
        })
        .await;
        assert_eq!(second, Some(expected));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
