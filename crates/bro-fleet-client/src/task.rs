//! Client-side task mirror.
//!
//! A lean local copy of the fields the fleet cockpit reads, fed by the daemon
//! status poller. This is NOT the daemon's `Task` (the daemon owns the real
//! execution engine); it is the client's view of a dispatched agent, persisted
//! to the cockpit's own `bro_home/fleet` store so historical sessions survive a
//! reload. Status is typed directly on the wire `bro_protocol::TaskStatus`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use bro_core::Provider;
use bro_protocol::TaskStatus;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Human-readable elapsed time between `started_at` and `completed_at` (or now).
pub fn format_elapsed(started_at: u64, completed_at: Option<u64>) -> String {
    let end = completed_at.unwrap_or_else(now_ms);
    let ms = end.saturating_sub(started_at);
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

/// Live mirror of a dispatched fleet agent. Only the fields the cockpit renders
/// (via `AgentHandle::snapshot`) are kept; the daemon owns everything else.
pub struct TaskInner {
    pub id: String,
    pub provider: Provider,
    pub session_id: String,
    pub events: Vec<Value>,
    pub last_assistant_message: Option<String>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub stderr: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub cwd: Option<String>,
    /// Durable roster display name (mirrors the daemon's `bro_label`).
    pub bro_label: Option<String>,
    /// Loaded/orphaned tasks are marked recoverable so the cockpit can
    /// distinguish a resumable interruption from ordinary terminal state.
    pub recoverable: bool,
    /// Wall-clock (ms) of the last observed stream event ("last interaction").
    /// Stamped by the status poller when the event count grows.
    pub last_event_at_ms: Option<u64>,
    /// The model pinned at dispatch time - survives cockpit reload even when
    /// stream events lack a top-level `model` field (e.g. brodex/Responses).
    pub model: Option<String>,
}

pub struct Task {
    pub inner: Mutex<TaskInner>,
    pub notify: Arc<Notify>,
    /// Wall-clock (ms) the status poller last completed a poll cycle (success OR
    /// handled error) for this task — a liveness heartbeat, distinct from
    /// `last_event_at_ms` (which only advances on new daemon events). The
    /// poller-supervisor watches this: if a non-terminal task's heartbeat goes
    /// stale, its poller has silently wedged and the supervisor respawns it.
    /// Seeded to "now" at construction so a freshly-attached poller gets a grace
    /// window before the supervisor would consider it stalled.
    pub last_poll_ms: AtomicU64,
}

impl Task {
    pub fn id(&self) -> String {
        self.inner.lock().id.clone()
    }

    /// Stamp the poll-liveness heartbeat (called by the poller each cycle).
    pub fn mark_polled(&self) {
        self.last_poll_ms
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Milliseconds since the last poll-liveness heartbeat.
    pub fn since_last_poll_ms(&self) -> u64 {
        now_ms().saturating_sub(self.last_poll_ms.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// On-disk record. A subset of the daemon's persisted shape — serde ignores the
/// daemon-only fields in any pre-existing `tasks.json`, so a cockpit store
/// written by an older in-process fleet still loads.
#[derive(Serialize, Deserialize)]
struct PersistedTask {
    id: String,
    provider: Provider,
    session_id: String,
    #[serde(default)]
    events: Vec<Value>,
    #[serde(default)]
    last_assistant_message: Option<String>,
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    num_turns: Option<u64>,
    #[serde(default)]
    stderr: String,
    status: TaskStatus,
    started_at: u64,
    #[serde(default)]
    completed_at: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    bro_label: Option<String>,
    #[serde(default)]
    recoverable: bool,
    #[serde(default)]
    model: Option<String>,
}

/// The cockpit's task store: the agents it has dispatched or reloaded.
pub struct TaskStore {
    tasks: HashMap<String, Arc<Task>>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }

    pub fn insert(&mut self, id: String, task: Arc<Task>) {
        self.tasks.insert(id, task);
    }

    pub fn all_tasks(&self) -> Vec<Arc<Task>> {
        self.tasks.values().cloned().collect()
    }

    /// Drop entries failing the predicate (e.g. a forgotten task). Returns the
    /// IDs removed for caller reporting.
    pub fn retain_drop<F>(&mut self, mut keep: F) -> Vec<String>
    where
        F: FnMut(&Task) -> bool,
    {
        let mut dropped = Vec::new();
        self.tasks.retain(|id, t| {
            if keep(t) {
                true
            } else {
                dropped.push(id.clone());
                false
            }
        });
        dropped
    }

    /// Flush every task to `store_dir/tasks.json` (atomic rename) so a later
    /// cockpit launch reloads these sessions.
    pub fn persist_all_events(&self, store_dir: &std::path::Path) {
        let records: Vec<PersistedTask> = self
            .tasks
            .values()
            .map(|t| {
                let inner = t.inner.lock();
                PersistedTask {
                    id: inner.id.clone(),
                    provider: inner.provider,
                    session_id: inner.session_id.clone(),
                    events: inner.events.clone(),
                    last_assistant_message: inner.last_assistant_message.clone(),
                    cost_usd: inner.cost_usd,
                    num_turns: inner.num_turns,
                    stderr: inner.stderr.chars().take(2000).collect(),
                    status: inner.status,
                    started_at: inner.started_at,
                    completed_at: inner.completed_at,
                    cwd: inner.cwd.clone(),
                    bro_label: inner.bro_label.clone(),
                    recoverable: inner.recoverable,
                    model: inner.model.clone(),
                }
            })
            .collect();

        let file = store_dir.join("tasks.json");
        let tmp = store_dir.join("tasks.json.tmp");
        if let Ok(data) = serde_json::to_string(&records) {
            let _ = std::fs::create_dir_all(store_dir);
            if std::fs::write(&tmp, &data).is_ok() {
                let _ = std::fs::rename(&tmp, &file);
            }
        }
    }

    /// Load persisted sessions. A `Running` task from a prior launch is flipped
    /// to `Failed` + `recoverable` (the cockpit shows it as Interrupted, §5);
    /// `ttl_ms` evicts records older than the cutoff (the cockpit passes
    /// `u64::MAX` — manual cleanup, no TTL).
    pub fn load(store_dir: &std::path::Path, ttl_ms: u64) -> Self {
        let file = store_dir.join("tasks.json");
        let mut store = Self::new();
        let data = match std::fs::read_to_string(&file) {
            Ok(d) => d,
            Err(_) => return store,
        };
        let records: Vec<PersistedTask> = match serde_json::from_str(&data) {
            Ok(r) => r,
            Err(_) => return store,
        };
        let cutoff = now_ms().saturating_sub(ttl_ms);
        for mut rec in records {
            if rec.started_at < cutoff {
                continue;
            }
            if rec.status == TaskStatus::Running || rec.status == TaskStatus::Pending {
                rec.status = TaskStatus::Failed;
                rec.completed_at = Some(now_ms());
                rec.recoverable = true;
            }
            let task = Arc::new(Task {
                inner: Mutex::new(TaskInner {
                    id: rec.id.clone(),
                    provider: rec.provider,
                    session_id: rec.session_id,
                    events: rec.events,
                    last_assistant_message: rec.last_assistant_message,
                    cost_usd: rec.cost_usd,
                    num_turns: rec.num_turns,
                    stderr: rec.stderr,
                    status: rec.status,
                    started_at: rec.started_at,
                    completed_at: rec.completed_at,
                    cwd: rec.cwd,
                    bro_label: rec.bro_label,
                    recoverable: rec.recoverable,
                    last_event_at_ms: None,
                    model: rec.model,
                }),
                notify: Arc::new(Notify::new()),
                last_poll_ms: AtomicU64::new(now_ms()),
            });
            store.tasks.insert(rec.id, task);
        }
        store
    }
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}
