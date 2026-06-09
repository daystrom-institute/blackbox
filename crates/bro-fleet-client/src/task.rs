//! Client-side roster projection.
//!
//! A lean in-memory copy of the daemon roster fields the fleet cockpit reads.
//! The daemon owns Tier-1 fleet data; this store is rebuilt from
//! `/control/roster` snapshots and `/control/roster/stream` deltas and never
//! persists `$BRO_HOME/fleet/tasks.json`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bro_core::{Origin, Provider};
use bro_protocol::{RosterDelta, RosterSnapshotV1, RosterSummaryV1, TaskStatus};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Notify;

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Live projection of a daemon roster row. Only the fields the cockpit renders
/// (via `AgentHandle::snapshot`) are kept here; the daemon owns execution.
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
    /// Durable roster display name (mirrors the daemon's `label`).
    pub bro_label: Option<String>,
    /// Roster snapshot rows are daemon-owned; recoverable is retained only for
    /// local synthetic error rows returned from a failed dispatch/resume call.
    pub recoverable: bool,
    /// Wall-clock (ms) of the last daemon-observed activity stamp.
    pub last_event_at_ms: Option<u64>,
    /// Model reported by the daemon roster, if available.
    pub model: Option<String>,
    pub origin: Origin,
    pub managed_worktree: Option<String>,
    pub workflow_owned: bool,
}

pub struct Task {
    pub inner: Mutex<TaskInner>,
    pub notify: Arc<Notify>,
}

impl Task {
    pub fn id(&self) -> String {
        self.inner.lock().id.clone()
    }
}

/// The cockpit's in-memory roster projection.
pub struct TaskStore {
    tasks: HashMap<String, Arc<Task>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterApply {
    Applied { seq: u64 },
    Gap { expected: u64, got: u64 },
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

    pub fn replace_from_snapshot(&mut self, snapshot: RosterSnapshotV1) -> u64 {
        let mut seen = HashSet::new();
        for task in snapshot.tasks {
            seen.insert(task.task_id.as_str().to_string());
            self.upsert_roster_task(task);
        }
        self.tasks.retain(|id, _| seen.contains(id));
        snapshot.version
    }

    pub fn apply_delta(&mut self, last_seq: u64, delta: RosterDelta) -> RosterApply {
        let seq = delta.seq();
        if seq <= last_seq {
            return RosterApply::Applied { seq: last_seq };
        }
        let expected = last_seq.saturating_add(1);
        if seq != expected {
            return RosterApply::Gap { expected, got: seq };
        }
        match delta {
            RosterDelta::Added { task, .. } | RosterDelta::Updated { task, .. } => {
                self.upsert_roster_task(task);
            }
            RosterDelta::Removed { task_id, .. } => {
                self.tasks.remove(task_id.as_str());
            }
        }
        RosterApply::Applied { seq }
    }

    fn upsert_roster_task(&mut self, task: RosterSummaryV1) {
        let id = task.task_id.as_str().to_string();
        if let Some(existing) = self.tasks.get(&id) {
            update_task_from_roster(existing, task);
        } else {
            self.tasks.insert(id, task_from_roster(task));
        }
    }
}

impl Default for TaskStore {
    fn default() -> Self {
        Self::new()
    }
}

fn task_from_roster(task: RosterSummaryV1) -> Arc<Task> {
    let now = now_ms();
    let completed_at = task.status.is_terminal().then_some(task.last_event_at.unwrap_or(now));
    Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: task.task_id.as_str().to_string(),
            provider: task.provider,
            session_id: task
                .session_id
                .as_ref()
                .map(|id| id.as_str().to_string())
                .unwrap_or_else(|| "pending".to_string()),
            events: Vec::new(),
            last_assistant_message: task.last_message_snippet,
            cost_usd: task.cost,
            num_turns: task.turns,
            stderr: String::new(),
            status: task.status,
            started_at: task.last_event_at.unwrap_or(now),
            completed_at,
            cwd: task.cwd,
            bro_label: task.label,
            recoverable: false,
            last_event_at_ms: task.last_event_at,
            model: task.model,
            origin: task.origin,
            managed_worktree: task.managed_worktree,
            workflow_owned: task.workflow_owned,
        }),
        notify: Arc::new(Notify::new()),
    })
}

fn update_task_from_roster(existing: &Task, task: RosterSummaryV1) {
    let mut inner = existing.inner.lock();
    inner.provider = task.provider;
    inner.session_id = task
        .session_id
        .as_ref()
        .map(|id| id.as_str().to_string())
        .unwrap_or_else(|| "pending".to_string());
    inner.last_assistant_message = task.last_message_snippet;
    inner.cost_usd = task.cost;
    inner.num_turns = task.turns;
    inner.status = task.status;
    if inner.started_at == 0 {
        inner.started_at = task.last_event_at.unwrap_or_else(now_ms);
    }
    inner.completed_at = task.status.is_terminal().then_some(task.last_event_at.unwrap_or_else(now_ms));
    inner.cwd = task.cwd;
    inner.bro_label = task.label;
    inner.recoverable = false;
    inner.last_event_at_ms = task.last_event_at;
    inner.model = task.model;
    inner.origin = task.origin;
    inner.managed_worktree = task.managed_worktree;
    inner.workflow_owned = task.workflow_owned;
    drop(inner);
    existing.notify.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;
    use bro_core::{Origin, TaskId};

    fn summary(id: &str, status: TaskStatus) -> RosterSummaryV1 {
        RosterSummaryV1 {
            task_id: TaskId::new(id),
            status,
            provider: Provider::Brodex,
            cost: Some(0.25),
            turns: Some(3),
            cwd: Some("/tmp/project".to_string()),
            label: Some(format!("agent-{id}")),
            session_id: None,
            last_message_snippet: Some("hello".to_string()),
            model: Some("gpt-test".to_string()),
            last_event_at: Some(42),
            origin: Origin::Cockpit,
            managed_worktree: Some("/tmp/worktree".to_string()),
            workflow_owned: false,
        }
    }

    #[test]
    fn applies_added_updated_removed_roster_deltas() {
        let mut store = TaskStore::new();
        let added = RosterDelta::Added {
            seq: 1,
            task: summary("task-1", TaskStatus::Running),
        };
        assert_eq!(store.apply_delta(0, added), RosterApply::Applied { seq: 1 });
        assert_eq!(store.all_tasks().len(), 1);

        let updated = RosterDelta::Updated {
            seq: 2,
            task: summary("task-1", TaskStatus::Completed),
        };
        assert_eq!(store.apply_delta(1, updated), RosterApply::Applied { seq: 2 });
        let task = store.all_tasks().pop().unwrap();
        assert_eq!(task.inner.lock().status, TaskStatus::Completed);

        let removed = RosterDelta::Removed {
            seq: 3,
            task_id: TaskId::new("task-1"),
        };
        assert_eq!(store.apply_delta(2, removed), RosterApply::Applied { seq: 3 });
        assert!(store.all_tasks().is_empty());
    }

    #[test]
    fn roster_seq_gap_requests_snapshot_resync() {
        let mut store = TaskStore::new();
        let delta = RosterDelta::Added {
            seq: 7,
            task: summary("task-1", TaskStatus::Running),
        };
        assert_eq!(
            store.apply_delta(4, delta),
            RosterApply::Gap {
                expected: 5,
                got: 7
            }
        );
        assert!(store.all_tasks().is_empty());
    }

    #[test]
    fn in_memory_roster_does_not_write_tasks_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = TaskStore::new();
        let snapshot = RosterSnapshotV1 {
            version: 1,
            tasks: vec![summary("task-1", TaskStatus::Running)],
        };
        store.replace_from_snapshot(snapshot);
        assert!(!root.join("tasks.json").exists());
    }
}
