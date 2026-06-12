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

use crate::orchestration::providers::Provider;
use crate::server::BlackboxServer;
use crate::server::state::SharedState;
use crate::tools::bro_helpers::split_csv;
use crate::tools::bro_params::AgentVectorPlan;
use crate::{embed, index, orchestration, vectors};

const TASK_BRO_REF_CACHE_CAPACITY: usize = 512;
const TASK_BRO_REF_MISS_TTL: Duration = Duration::from_secs(5);
const SESSION_FILE_CACHE_CAPACITY: usize = 512;
const SESSION_FILE_CACHE_MISS_TTL: Duration = Duration::from_secs(5);

static TASK_BRO_REF_CACHE: OnceLock<Mutex<TaskBroRefCache>> = OnceLock::new();
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

type TaskBroRefCache = TimedLookupCache<orchestration::team::BroRef>;
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

fn task_bro_ref_cache() -> &'static Mutex<TaskBroRefCache> {
    TASK_BRO_REF_CACHE.get_or_init(|| {
        Mutex::new(TaskBroRefCache::new(
            TASK_BRO_REF_CACHE_CAPACITY,
            TASK_BRO_REF_MISS_TTL,
        ))
    })
}

fn session_file_cache() -> &'static Mutex<SessionFileCache> {
    SESSION_FILE_CACHE.get_or_init(|| {
        Mutex::new(SessionFileCache::new(
            SESSION_FILE_CACHE_CAPACITY,
            SESSION_FILE_CACHE_MISS_TTL,
        ))
    })
}

async fn cached_bro_ref_for_task(
    task_id: &str,
    store_dir: &StdPath,
) -> Option<orchestration::team::BroRef> {
    resolve_task_bro_ref_cached(
        task_bro_ref_cache(),
        task_id,
        store_dir,
        |task_id, store_dir| orchestration::team::find_bro_ref_for_task(&task_id, &store_dir),
    )
    .await
}

async fn resolve_task_bro_ref_cached<F>(
    cache: &Mutex<TaskBroRefCache>,
    task_id: &str,
    store_dir: &StdPath,
    resolver: F,
) -> Option<orchestration::team::BroRef>
where
    F: FnOnce(String, PathBuf) -> Option<orchestration::team::BroRef> + Send + 'static,
{
    let now = Instant::now();
    if let Some(cached) = {
        let mut cache = cache.lock().expect("task bro ref cache poisoned");
        cache.get(task_id, now)
    } {
        return cached;
    }

    let lookup_task_id = task_id.to_string();
    let cache_task_id = lookup_task_id.clone();
    let log_task_id = lookup_task_id.clone();
    let lookup_store_dir = store_dir.to_path_buf();
    let resolved =
        match tokio::task::spawn_blocking(move || resolver(lookup_task_id, lookup_store_dir)).await
        {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!(
                    task_id = %log_task_id,
                    error = %err,
                    "task bro ref resolution failed"
                );
                None
            }
        };

    {
        let mut cache = cache.lock().expect("task bro ref cache poisoned");
        cache.insert(cache_task_id, resolved.clone(), Instant::now());
    }

    resolved
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

pub(crate) fn resolve_agent_vector_search(
    query: &str,
    supplied_query_vector: Option<&[f32]>,
) -> AgentVectorPlan {
    #[cfg(test)]
    if supplied_query_vector.is_none() {
        return AgentVectorPlan {
            search: None,
            route: None,
            error: Some("live query embedding disabled in unit tests".into()),
        };
    }
    let route = match embed::EmbeddingRouter::load_default()
        .and_then(|router| router.route(embed::Bucket::AgentManifest, None))
    {
        Ok(route) => route.vector_route_id(),
        Err(err) => {
            return AgentVectorPlan {
                search: None,
                route: None,
                error: Some(format!("agent_manifest route unavailable: {err}")),
            };
        }
    };
    let Some(metrics) = vectors::metrics().get(&route).cloned() else {
        return AgentVectorPlan {
            search: None,
            route: Some(route),
            error: Some("agent_manifest vector partition has no active records".into()),
        };
    };
    if metrics.active_count == 0 {
        return AgentVectorPlan {
            search: None,
            route: Some(route),
            error: Some("agent_manifest vector partition has no active records".into()),
        };
    }
    let query_vector = match supplied_query_vector {
        Some(vector) => vector.to_vec(),
        None => match BlackboxServer::embed_agent_query(query) {
            Ok(vector) => vector,
            Err(err) => {
                return AgentVectorPlan {
                    search: None,
                    route: Some(route),
                    error: Some(format!("agent_manifest query embedding failed: {err}")),
                };
            }
        },
    };
    if query_vector.len() != metrics.dims {
        return AgentVectorPlan {
            search: None,
            route: Some(route),
            error: Some(format!(
                "query vector dims {} do not match agent_manifest partition dims {}",
                query_vector.len(),
                metrics.dims
            )),
        };
    }
    AgentVectorPlan {
        search: Some(orchestration::agents::registry::AgentVectorSearch {
            route: route.clone(),
            query_vector,
        }),
        route: Some(route),
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Tail SSE endpoint (outside MCP)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct TailQuery {
    /// Comma-separated team names — union of members. Accepts legacy `team=`.
    #[serde(default, alias = "team")]
    teams: Option<String>,
    /// Comma-separated bro names. Accepts legacy `bro=`.
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

    let wanted_teams = split_csv(&query.teams);
    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();
    let no_selectors = wanted_teams.is_empty()
        && wanted_bros.is_empty()
        && wanted_sessions.is_empty()
        && wanted_providers.is_empty();
    let store_dir = state.store_dir.clone();

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
                    let bro_ref = cached_bro_ref_for_task(tid, &store_dir).await;

                    // Effective selector + label resolution. Team-based lookup
                    // (find_bro_ref_for_task) wins when the dispatching path
                    // attributed via task_history. Otherwise fall back to the
                    // task's `bro_label` — set during dispatch so brofile-only
                    // workflow nodes (implementer / advisor) and ensemble
                    // members with duplicate-name brofiles surface in tail
                    // instead of being anonymous.
                    let (effective_member, effective_team, effective_label) = match &bro_ref {
                        Some(r) => {
                            let label = format!("{}::{}", r.team_name, r.member_name);
                            (Some(r.member_name.clone()), Some(r.team_name.clone()), Some(label))
                        }
                        None => {
                            let label = task_bro_label.clone();
                            let (team, member) = match label.as_deref() {
                                Some(s) => match s.split_once("::") {
                                    Some((t, m)) => (Some(t.to_string()), Some(m.to_string())),
                                    None => (None, Some(s.to_string())),
                                },
                                None => (None, None),
                            };
                            (member, team, label)
                        }
                    };

                    // Provider is a filter that applies on top of the selector
                    // union. Bros/sessions/teams are OR'd together: match ANY
                    // specified selector across them; a category being empty
                    // means it contributes no matches (but also doesn't reject).
                    let provider_ok = wanted_providers.is_empty()
                        || task_provider.map(|p| wanted_providers.contains(&p)).unwrap_or(false);
                    let selectors_specified = !wanted_bros.is_empty()
                        || !wanted_sessions.is_empty()
                        || !wanted_teams.is_empty();
                    let selector_match = if !selectors_specified {
                        true
                    } else {
                        let bro_m = match (&effective_member, &effective_label) {
                            (Some(m), Some(l)) => wanted_bros
                                .iter()
                                .any(|w| w == m || w == l),
                            (Some(m), None) => wanted_bros.iter().any(|w| w == m),
                            (None, Some(l)) => wanted_bros.iter().any(|w| w == l),
                            _ => false,
                        };
                        let session_m = task_session_id.as_deref()
                            .map(|s| wanted_sessions.iter().any(|w| w == s))
                            .unwrap_or(false);
                        let team_m = match &effective_team {
                            Some(t) => wanted_teams.iter().any(|w| w == t),
                            None => false,
                        };
                        bro_m || session_m || team_m
                    };
                    if !(no_selectors || (provider_ok && selector_match)) {
                        continue;
                    }

                    let mut evt_json = serde_json::to_value(&event).unwrap_or_default();
                    if let Some(member) = &effective_member {
                        evt_json["bro_name"] = Value::String(member.clone());
                    }
                    if let Some(label) = &effective_label {
                        evt_json["bro_selector"] = Value::String(label.clone());
                    }
                    if let Some(team) = &effective_team {
                        evt_json["team_name"] = Value::String(team.clone());
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

    #[tokio::test]
    async fn task_bro_ref_cache_hit_avoids_resolution() {
        let cache = Mutex::new(TaskBroRefCache::new(8, Duration::from_millis(10)));
        let calls = Arc::new(AtomicUsize::new(0));
        let expected = orchestration::team::BroRef {
            team_name: "team-a".to_string(),
            member_name: "bro-a".to_string(),
        };

        let first = resolve_task_bro_ref_cached(&cache, "task-cache", StdPath::new("/unused"), {
            let calls = calls.clone();
            let expected = expected.clone();
            move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Some(expected)
            }
        })
        .await;
        assert_eq!(first, Some(expected.clone()));

        let second = resolve_task_bro_ref_cached(&cache, "task-cache", StdPath::new("/unused"), {
            let calls = calls.clone();
            move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            }
        })
        .await;
        assert_eq!(second, Some(expected));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn task_bro_ref_cache_miss_ttl_allows_late_upgrade() {
        let cache = Mutex::new(TaskBroRefCache::new(8, Duration::from_millis(5)));
        let calls = Arc::new(AtomicUsize::new(0));
        let upgraded = orchestration::team::BroRef {
            team_name: "team-late".to_string(),
            member_name: "bro-late".to_string(),
        };

        let initial = resolve_task_bro_ref_cached(&cache, "task-late", StdPath::new("/unused"), {
            let calls = calls.clone();
            move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                None
            }
        })
        .await;
        assert_eq!(initial, None);

        let cached_miss =
            resolve_task_bro_ref_cached(&cache, "task-late", StdPath::new("/unused"), {
                let calls = calls.clone();
                let upgraded = upgraded.clone();
                move |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Some(upgraded)
                }
            })
            .await;
        assert_eq!(cached_miss, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(10)).await;

        let upgraded_result =
            resolve_task_bro_ref_cached(&cache, "task-late", StdPath::new("/unused"), {
                let calls = calls.clone();
                let upgraded = upgraded.clone();
                move |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Some(upgraded)
                }
            })
            .await;
        assert_eq!(upgraded_result, Some(upgraded));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn task_bro_ref_cache_evicts_oldest_entry_when_bounded() {
        let mut cache = TaskBroRefCache::new(2, Duration::from_secs(1));
        let now = Instant::now();
        let bro_a = orchestration::team::BroRef {
            team_name: "team-a".to_string(),
            member_name: "bro-a".to_string(),
        };
        let bro_b = orchestration::team::BroRef {
            team_name: "team-b".to_string(),
            member_name: "bro-b".to_string(),
        };
        let bro_c = orchestration::team::BroRef {
            team_name: "team-c".to_string(),
            member_name: "bro-c".to_string(),
        };

        cache.insert("task-a".to_string(), Some(bro_a), now);
        cache.insert("task-b".to_string(), Some(bro_b.clone()), now);
        cache.insert("task-c".to_string(), Some(bro_c.clone()), now);

        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.get("task-a", now), None);
        assert_eq!(cache.get("task-b", now), Some(Some(bro_b)));
        assert_eq!(cache.get("task-c", now), Some(Some(bro_c)));
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
