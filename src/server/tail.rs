use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State as AxumState};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
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

#[derive(Debug, Deserialize)]
pub(crate) struct TranscriptHistoryQuery {
    #[serde(default)]
    from_cursor: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

const DEFAULT_TRANSCRIPT_HISTORY_LIMIT: usize = 100;
const MAX_TRANSCRIPT_HISTORY_LIMIT: usize = 500;
const FOCUSED_TRANSCRIPT_SNAPSHOT_MEMORY_EVENT_LIMIT: usize = 200;

fn wire_status(status: orchestration::TaskStatus) -> bro_protocol::TaskStatus {
    match status {
        orchestration::TaskStatus::Running => bro_protocol::TaskStatus::Running,
        orchestration::TaskStatus::Completed => bro_protocol::TaskStatus::Completed,
        orchestration::TaskStatus::Failed => bro_protocol::TaskStatus::Failed,
        orchestration::TaskStatus::Cancelled => bro_protocol::TaskStatus::Cancelled,
    }
}

fn response_error(status: StatusCode, error: impl Into<String>) -> Response {
    (
        status,
        axum::Json(serde_json::json!({ "error": error.into() })),
    )
        .into_response()
}

fn transcript_file_for_session(state: &SharedState, session_id: &str) -> Option<String> {
    if session_id.is_empty() || session_id == "pending" {
        return None;
    }
    let config = state.idx.read().reindex_config();
    index::find_session_file(session_id, &config.roots, config.codex_root.as_deref())
        .map(|path| path.to_string_lossy().into_owned())
}

fn focused_transcript_snapshot(
    state: &SharedState,
    task_id: &str,
) -> Result<bro_protocol::FocusedTranscriptSnapshotV1, Response> {
    let task = state.task_store.read().get(task_id).ok_or_else(|| {
        response_error(StatusCode::NOT_FOUND, format!("unknown task id: {task_id}"))
    })?;

    let (
        task_id,
        raw_session_id,
        provider,
        status,
        live_cursor,
        memory_start_cursor,
        next_memory_cursor,
        memory_events,
    ) = {
        let inner = task.inner.lock();
        let event_count = inner.events.len();
        let memory_start_index =
            event_count.saturating_sub(FOCUSED_TRANSCRIPT_SNAPSHOT_MEMORY_EVENT_LIMIT);
        (
            inner.id.clone(),
            inner.session_id.clone(),
            inner.provider,
            inner.status,
            inner.live_cursor,
            memory_start_index as u64,
            event_count as u64,
            inner.events[memory_start_index..].to_vec(),
        )
    };

    let session_id = (raw_session_id != "pending" && !raw_session_id.is_empty())
        .then(|| bro_core::SessionId::new(raw_session_id));
    let history_jsonl_path = session_id
        .as_ref()
        .and_then(|sid| transcript_file_for_session(state, sid.as_str()));
    let events = memory_events
        .into_iter()
        .enumerate()
        .map(
            |(cursor, event)| bro_protocol::FocusedTranscriptMemoryEventV1 {
                cursor: memory_start_cursor + cursor as u64,
                event,
            },
        )
        .collect::<Vec<_>>();

    Ok(bro_protocol::FocusedTranscriptSnapshotV1 {
        task_id: bro_core::TaskId::new(task_id),
        session_id,
        provider,
        status: wire_status(status),
        live_cursor,
        memory_start_cursor,
        next_memory_cursor,
        events,
        history_jsonl_path,
    })
}

fn focused_live_payload_after_snapshot(
    event: &orchestration::tail::TailEvent,
    task_id: &str,
    snapshot_live_cursor: u64,
) -> Option<bro_protocol::FocusedTranscriptLiveEventV1> {
    if event.task_id() != task_id {
        return None;
    }
    let cursor = event.cursor();
    if cursor <= snapshot_live_cursor {
        return None;
    }
    let event_value = match event {
        orchestration::tail::TailEvent::TaskEvent { event, .. } => event.clone(),
        other => serde_json::to_value(other).unwrap_or_else(|_| serde_json::json!({})),
    };
    Some(bro_protocol::FocusedTranscriptLiveEventV1 {
        task_id: bro_core::TaskId::new(task_id.to_string()),
        cursor,
        event: event_value,
    })
}

pub(crate) async fn focused_transcript_stream_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Path(task_id): Path<String>,
) -> Response {
    let mut rx = state.tail_tx.subscribe();
    let snapshot = match focused_transcript_snapshot(&state, &task_id) {
        Ok(snapshot) => snapshot,
        Err(resp) => return resp,
    };
    let snapshot_live_cursor = snapshot.live_cursor;

    let stream = async_stream::stream! {
        match Event::default().event("snapshot").json_data(&snapshot) {
            Ok(event) => yield Ok::<Event, std::convert::Infallible>(event),
            Err(err) => {
                tracing::warn!("failed to serialize focused transcript snapshot: {err}");
                return;
            }
        }

        loop {
            match rx.recv().await {
                Ok(event) => {
                    let Some(payload) = focused_live_payload_after_snapshot(
                        &event,
                        &task_id,
                        snapshot_live_cursor,
                    ) else {
                        continue;
                    };
                    match Event::default().event("event").json_data(&payload) {
                        Ok(event) => yield Ok::<Event, std::convert::Infallible>(event),
                        Err(err) => tracing::warn!("failed to serialize focused transcript live event: {err}"),
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("focused transcript subscriber for {task_id} lagged by {n} events; signaling resync");
                    let payload = serde_json::json!({
                        "reason": "lagged",
                        "skipped": n,
                        "task_id": task_id,
                    });
                    match Event::default().event("resync").json_data(&payload) {
                        Ok(event) => yield Ok::<Event, std::convert::Infallible>(event),
                        Err(err) => tracing::warn!("failed to serialize focused transcript resync event: {err}"),
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).into_response()
}

pub(crate) async fn focused_transcript_history_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Path(task_id): Path<String>,
    Query(query): Query<TranscriptHistoryQuery>,
) -> Response {
    let task = match state.task_store.read().get(&task_id) {
        Some(task) => task,
        None => {
            return response_error(StatusCode::NOT_FOUND, format!("unknown task id: {task_id}"));
        }
    };
    let session_id = {
        let inner = task.inner.lock();
        if inner.session_id.is_empty() || inner.session_id == "pending" {
            return response_error(
                StatusCode::NOT_FOUND,
                format!("task {task_id} has no resolved provider session id"),
            );
        }
        inner.session_id.clone()
    };
    let Some(path) = transcript_file_for_session(&state, &session_id) else {
        return response_error(
            StatusCode::NOT_FOUND,
            format!("no provider transcript file found for session {session_id}"),
        );
    };

    let from_cursor = query.from_cursor.unwrap_or(0);
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TRANSCRIPT_HISTORY_LIMIT)
        .clamp(1, MAX_TRANSCRIPT_HISTORY_LIMIT);
    match read_history_page(&task_id, &session_id, &path, from_cursor, limit) {
        Ok(page) => axum::Json(page).into_response(),
        Err(err) => response_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

fn read_history_page(
    task_id: &str,
    session_id: &str,
    path: &str,
    from_cursor: u64,
    limit: usize,
) -> Result<bro_protocol::TranscriptHistoryPageV1, String> {
    let file = File::open(path).map_err(|err| format!("open transcript file {path}: {err}"))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut byte_offset = 0_u64;
    let mut next_cursor = from_cursor;
    let mut reached_end = true;

    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|err| format!("read transcript file {path}: {err}"))?;
        let cursor = idx as u64;
        let line_offset = byte_offset;
        byte_offset = byte_offset
            .saturating_add(line.len() as u64)
            .saturating_add(1);
        if cursor < from_cursor {
            continue;
        }
        if events.len() >= limit {
            reached_end = false;
            break;
        }
        let event = serde_json::from_str::<Value>(&line)
            .map_err(|err| format!("parse transcript file {path} line {}: {err}", idx + 1))?;
        next_cursor = cursor + 1;
        events.push(bro_protocol::TranscriptHistoryEventV1 {
            cursor,
            byte_offset: line_offset,
            event,
        });
    }

    Ok(bro_protocol::TranscriptHistoryPageV1 {
        task_id: bro_core::TaskId::new(task_id.to_string()),
        session_id: bro_core::SessionId::new(session_id.to_string()),
        history_jsonl_path: path.to_string(),
        from_cursor,
        limit,
        next_cursor,
        reached_end,
        events,
    })
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
    use crate::index::TranscriptIndex;
    use crate::orchestration::TaskStatus;
    use crate::orchestration::tail::TailEvent;
    use axum::body::to_bytes;
    use axum::http::Request;
    use axum::routing::get;
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;
    use tower::ServiceExt;

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

    #[test]
    fn focused_transcript_snapshot_then_cursor_has_no_gap_or_dup_at_boundary() {
        let task_id = "task-boundary";
        let snapshot_live_cursor = 7;

        let duplicate = TailEvent::TaskProgress {
            task_id: task_id.to_string(),
            cursor: snapshot_live_cursor,
            activity: "already snapped".to_string(),
        };
        assert!(
            focused_live_payload_after_snapshot(&duplicate, task_id, snapshot_live_cursor)
                .is_none()
        );

        let next = TailEvent::TaskProgress {
            task_id: task_id.to_string(),
            cursor: snapshot_live_cursor + 1,
            activity: "first streamed".to_string(),
        };
        let payload = focused_live_payload_after_snapshot(&next, task_id, snapshot_live_cursor)
            .expect("cursor after snapshot should stream");
        assert_eq!(payload.cursor, snapshot_live_cursor + 1);

        let wrong_task = TailEvent::TaskProgress {
            task_id: "other-task".to_string(),
            cursor: snapshot_live_cursor + 2,
            activity: "not focused".to_string(),
        };
        assert!(
            focused_live_payload_after_snapshot(&wrong_task, task_id, snapshot_live_cursor)
                .is_none()
        );
    }

    #[test]
    fn focused_live_payload_after_snapshot_forwards_inner_task_event() {
        let task_id = "task-envelope";
        let snapshot_live_cursor = 11;
        let inner_event = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text", "text": "live text"}]
            }
        });

        let stale = TailEvent::TaskEvent {
            task_id: task_id.to_string(),
            cursor: snapshot_live_cursor,
            event: inner_event.clone(),
        };
        assert!(
            focused_live_payload_after_snapshot(&stale, task_id, snapshot_live_cursor).is_none()
        );

        let live = TailEvent::TaskEvent {
            task_id: task_id.to_string(),
            cursor: snapshot_live_cursor + 1,
            event: inner_event.clone(),
        };
        let payload = focused_live_payload_after_snapshot(&live, task_id, snapshot_live_cursor)
            .expect("fresh task event should stream");

        assert_eq!(payload.task_id.as_str(), task_id);
        assert_eq!(payload.cursor, snapshot_live_cursor + 1);
        assert_eq!(payload.event, inner_event);
    }

    #[test]
    fn focused_transcript_snapshot_bounds_memory_events_with_absolute_cursors() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        let task =
            crate::orchestration::test_task("task-many-events", TaskStatus::Running, Provider::Glm);
        {
            let mut inner = task.inner.lock();
            for idx in 0..1000_u64 {
                inner.events.push(serde_json::json!({
                    "type": "provider_event",
                    "idx": idx,
                }));
            }
        }
        state
            .task_store
            .write()
            .insert("task-many-events".to_string(), task)
            .unwrap();

        let snapshot = focused_transcript_snapshot(&state, "task-many-events").unwrap();

        assert_eq!(
            snapshot.events.len(),
            FOCUSED_TRANSCRIPT_SNAPSHOT_MEMORY_EVENT_LIMIT
        );
        assert!(snapshot.events.len() <= FOCUSED_TRANSCRIPT_SNAPSHOT_MEMORY_EVENT_LIMIT);
        assert_eq!(snapshot.memory_start_cursor, 800);
        assert_eq!(snapshot.next_memory_cursor, 1000);
        assert_eq!(snapshot.events[0].cursor, snapshot.memory_start_cursor);
        assert_eq!(snapshot.events[0].event["idx"], 800);
        let last = snapshot.events.last().expect("snapshot has tail events");
        assert_eq!(last.cursor, snapshot.next_memory_cursor - 1);
        assert_eq!(last.event["idx"], 999);
    }

    #[tokio::test]
    async fn focused_transcript_stream_endpoint_is_mounted_and_yields_task_events() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        let task =
            crate::orchestration::test_task("task-stream", TaskStatus::Running, Provider::Glm);
        task.inner.lock().events.push(serde_json::json!({
            "type": "provider_event",
            "text": "hello stream"
        }));
        state
            .task_store
            .write()
            .insert("task-stream".to_string(), task)
            .unwrap();

        let app = axum::Router::new()
            .route(
                "/control/transcript/{task_id}/stream",
                get(focused_transcript_stream_handler),
            )
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/control/transcript/task-stream/stream")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let mut stream = response.into_body().into_data_stream();
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
            .await
            .expect("stream should yield initial snapshot")
            .expect("stream should have a first chunk")
            .expect("first chunk should be ok");
        let text = std::str::from_utf8(&chunk).unwrap();
        assert!(text.contains("event: snapshot"), "{text}");
        assert!(text.contains("task-stream"), "{text}");
        assert!(text.contains("hello stream"), "{text}");
    }

    #[tokio::test]
    async fn focused_transcript_history_reads_provider_transcript_file_page() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let transcript_dir = root.join("claude").join("projects").join("test-project");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript_path = transcript_dir.join("sess-history.jsonl");
        std::fs::write(
            &transcript_path,
            concat!(
                "{\"type\":\"first\"}\n",
                "{\"type\":\"second\"}\n",
                "{\"type\":\"third\"}\n"
            ),
        )
        .unwrap();

        let state = Arc::new(SharedState::for_test(&root));
        let index = TranscriptIndex::open_or_create(
            &root.join("index-with-transcript-root"),
            vec![("test-account".to_string(), root.join("claude"))],
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
        )
        .unwrap();
        *state.idx.write() = index;

        let task =
            crate::orchestration::test_task("task-history", TaskStatus::Completed, Provider::Glm);
        task.inner.lock().session_id = "sess-history".to_string();
        state
            .task_store
            .write()
            .insert("task-history".to_string(), task)
            .unwrap();

        let response = focused_transcript_history_handler(
            AxumState(state),
            Path("task-history".to_string()),
            Query(TranscriptHistoryQuery {
                from_cursor: Some(1),
                limit: Some(1),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let page: bro_protocol::TranscriptHistoryPageV1 = serde_json::from_slice(&body).unwrap();
        assert_eq!(page.task_id.as_str(), "task-history");
        assert_eq!(page.session_id.as_str(), "sess-history");
        assert_eq!(page.from_cursor, 1);
        assert_eq!(page.next_cursor, 2);
        assert!(!page.reached_end);
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].cursor, 1);
        assert_eq!(page.events[0].event["type"], "second");
        assert!(page.history_jsonl_path.ends_with("sess-history.jsonl"));
    }
}
