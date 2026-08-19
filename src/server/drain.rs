//! Convergence drain gate: admission drain mode + orchestration activity probe.
//!
//! Converging or cycling the daemon while operator sessions are mid bro-wave
//! (running tasks, `bro_wait` / `bro_when_all` long-polls, workflow arcs in
//! flight) sandbags live orchestration state. This module gives the converge
//! path two things:
//!
//! 1. A cheap, machine-readable **activity probe** (`GET
//!    /admin/orchestration-activity`) that reports running bro tasks,
//!    in-flight workflow arcs, active long-poll waiters, and recent
//!    orchestration writes (threads / notes / knowledge).
//! 2. An operator-togglable **admission drain** (`GET|POST /admin/drain`).
//!    While draining, fresh dispatches (`bro_exec` and every path that funnels
//!    through `dispatch_fresh_bro_task`, plus top-level workflow arc starts)
//!    are refused with a retryable `error.maintenance_pending`; in-flight work
//!    continues and `bro_resume` of existing sessions stays allowed.
//!
//! The drain flag is persisted as `<store_dir>/maintenance-drain.json` so a
//! daemon crash or restart mid-window does not silently reopen admission.
//! Startup behavior is explicit: if the marker exists the daemon boots
//! draining and logs a warning naming the clear path (`POST /admin/drain
//! {"draining": false}`); the converge wrapper's last step clears it.
//!
//! The wrapper is `scripts/converge-gate`; call order is gate, converge,
//! clear drain (see the script header).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::SharedState;
use crate::orchestration::TaskStatus;

/// File name of the persisted drain marker under the bro store dir.
pub(crate) const DRAIN_MARKER_FILE: &str = "maintenance-drain.json";

/// Stable error code prefix used on every refusal. Callers match on this.
pub(crate) const MAINTENANCE_PENDING_CODE: &str = "error.maintenance_pending";

/// Default look-back for the "recent orchestration writes" section.
pub(crate) const DEFAULT_WRITES_WINDOW_MINUTES: u64 = 10;
const MAX_WRITES_WINDOW_MINUTES: u64 = 24 * 60;

/// Persisted drain record. Absent file == not draining.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct DrainRecord {
    /// Always `true` on disk; the file's presence is the flag, the field
    /// keeps the JSON self-describing.
    pub(crate) draining: bool,
    pub(crate) set_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) set_by: Option<String>,
}

/// In-memory mirror of the persisted drain marker.
///
/// Reads are lock-cheap (one `RwLock` read of an `Option`); every fresh
/// dispatch consults it. Writes are rare (operator toggles) and go through
/// [`DrainState::set`] / [`DrainState::clear`], which persist first and only
/// then flip memory so a crash between the two leaves the durable side ahead
/// of the volatile side, never behind it.
pub(crate) struct DrainState {
    marker_path: PathBuf,
    record: RwLock<Option<DrainRecord>>,
}

impl DrainState {
    /// Load the drain marker from `store_dir` (blocking fs; boot-time only).
    pub(crate) fn open(store_dir: &Path) -> Self {
        let marker_path = store_dir.join(DRAIN_MARKER_FILE);
        let record = match std::fs::read_to_string(&marker_path) {
            Ok(raw) => match serde_json::from_str::<DrainRecord>(&raw) {
                Ok(rec) if rec.draining => Some(rec),
                Ok(_) => None,
                Err(err) => {
                    // A corrupt marker still means "someone asked for a
                    // drain": fail closed and keep admission drained until an
                    // operator clears it explicitly.
                    tracing::warn!(
                        target: "blackbox::drain",
                        path = %marker_path.display(),
                        error = %err,
                        "drain marker unreadable; booting in drain mode (fail closed)"
                    );
                    Some(DrainRecord {
                        draining: true,
                        set_at: crate::util::now_iso(),
                        reason: Some(format!("unreadable drain marker: {err}")),
                        set_by: Some("startup".to_string()),
                    })
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                tracing::warn!(
                    target: "blackbox::drain",
                    path = %marker_path.display(),
                    error = %err,
                    "drain marker unreadable; booting in drain mode (fail closed)"
                );
                Some(DrainRecord {
                    draining: true,
                    set_at: crate::util::now_iso(),
                    reason: Some(format!("unreadable drain marker: {err}")),
                    set_by: Some("startup".to_string()),
                })
            }
        };
        if let Some(rec) = &record {
            tracing::warn!(
                target: "blackbox::drain",
                set_at = %rec.set_at,
                reason = rec.reason.as_deref().unwrap_or(""),
                "admission DRAIN persisted across startup: fresh dispatches are refused until an operator clears it (POST /admin/drain {{\"draining\": false}} or scripts/converge-gate --clear)"
            );
        }
        Self {
            marker_path,
            record: RwLock::new(record),
        }
    }

    /// Test / in-memory constructor: no marker on disk, not draining.
    #[cfg(test)]
    pub(crate) fn in_memory(store_dir: &Path) -> Self {
        Self {
            marker_path: store_dir.join(DRAIN_MARKER_FILE),
            record: RwLock::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn marker_path(&self) -> &Path {
        &self.marker_path
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.record.read().is_some()
    }

    pub(crate) fn current(&self) -> Option<DrainRecord> {
        self.record.read().clone()
    }

    /// Enter drain mode. Persists the marker (atomic rename) before flipping
    /// the in-memory flag. Idempotent: re-setting keeps the original
    /// `set_at` unless the caller supplies a new reason.
    ///
    /// Blocking fs: callers on the tokio runtime wrap this in
    /// `spawn_blocking`.
    pub(crate) fn set(
        &self,
        reason: Option<String>,
        set_by: Option<String>,
    ) -> std::io::Result<DrainRecord> {
        let existing = self.record.read().clone();
        let record = match existing {
            Some(mut rec) => {
                if reason.is_some() {
                    rec.reason = reason;
                }
                if set_by.is_some() {
                    rec.set_by = set_by;
                }
                rec
            }
            None => DrainRecord {
                draining: true,
                set_at: crate::util::now_iso(),
                reason,
                set_by,
            },
        };
        write_marker_atomic(&self.marker_path, &record)?;
        *self.record.write() = Some(record.clone());
        Ok(record)
    }

    /// Leave drain mode. Removes the marker before clearing memory; a
    /// missing marker is not an error (idempotent clear).
    pub(crate) fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.marker_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        *self.record.write() = None;
        Ok(())
    }

    /// JSON view for the probe and the `/admin/drain` GET.
    pub(crate) fn snapshot(&self) -> Value {
        match self.current() {
            Some(rec) => json!({
                "draining": true,
                "set_at": rec.set_at,
                "reason": rec.reason,
                "set_by": rec.set_by,
                "marker_path": self.marker_path.display().to_string(),
            }),
            None => json!({
                "draining": false,
                "marker_path": self.marker_path.display().to_string(),
            }),
        }
    }

    /// The refusal returned to a fresh dispatch while draining. `None` when
    /// admission is open. The text is stable-prefixed
    /// (`error.maintenance_pending`) and names the window plus the retry
    /// contract so an orchestrator can back off instead of treating it as a
    /// hard failure.
    pub(crate) fn admission_refusal(&self, what: &str) -> Option<String> {
        let rec = self.current()?;
        Some(refusal_message(&rec, what))
    }
}

fn refusal_message(rec: &DrainRecord, what: &str) -> String {
    let reason = rec
        .reason
        .as_deref()
        .map(|r| format!(" reason: {r};"))
        .unwrap_or_default();
    format!(
        "{MAINTENANCE_PENDING_CODE}: {what} refused, the daemon is draining for a maintenance window (set {set_at};{reason} retryable=true). New dispatches are refused until the window clears; in-flight work continues and bro_resume of existing sessions is still allowed. Check GET /admin/drain and retry after `draining` is false.",
        set_at = rec.set_at,
    )
}

fn write_marker_atomic(path: &Path, record: &DrainRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// Long-poll waiter registry
// ---------------------------------------------------------------------------

/// One active long-poll (`bro_wait`, `bro_when_all`, `bro_when_any`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LongPollWaiter {
    pub(crate) id: u64,
    pub(crate) tool: &'static str,
    pub(crate) task_ids: Vec<String>,
    pub(crate) started_at_ms: u64,
}

/// Registry of in-flight long-poll waiters. Registration returns an RAII
/// guard so a waiter is removed on every exit path (return, timeout, client
/// disconnect cancelling the future).
#[derive(Default)]
pub(crate) struct LongPollRegistry {
    next_id: AtomicU64,
    waiters: Mutex<HashMap<u64, LongPollWaiter>>,
}

impl LongPollRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        tool: &'static str,
        task_ids: Vec<String>,
    ) -> LongPollGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.waiters.lock().insert(
            id,
            LongPollWaiter {
                id,
                tool,
                task_ids,
                started_at_ms: crate::orchestration::now_ms(),
            },
        );
        LongPollGuard {
            registry: Arc::clone(self),
            id,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.waiters.lock().len()
    }

    pub(crate) fn snapshot(&self) -> Vec<LongPollWaiter> {
        let mut rows: Vec<LongPollWaiter> = self.waiters.lock().values().cloned().collect();
        rows.sort_by_key(|w| w.id);
        rows
    }
}

pub(crate) struct LongPollGuard {
    registry: Arc<LongPollRegistry>,
    id: u64,
}

impl Drop for LongPollGuard {
    fn drop(&mut self) {
        self.registry.waiters.lock().remove(&self.id);
    }
}

// ---------------------------------------------------------------------------
// Activity probe
// ---------------------------------------------------------------------------

/// Clamp an operator-supplied look-back window to a sane range.
pub(crate) fn clamp_writes_window_minutes(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_WRITES_WINDOW_MINUTES)
        .min(MAX_WRITES_WINDOW_MINUTES)
}

fn rfc3339_age_secs(ts: &str, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    Some((now - parsed.with_timezone(&chrono::Utc)).num_seconds())
}

/// Build the machine-readable orchestration activity snapshot.
///
/// Cheap by construction: one read of the task store (per-task inner lock
/// only for running tasks), the arc token map, the waiter registry, and a
/// linear pass over the threads / notes / knowledge stores. No I/O.
///
/// `quiescent` is the gate's verdict input and its scope is stated in the
/// payload (`quiescent_scope: "tasks,arcs,waiters"`): no running tasks, no
/// in-flight arcs, no long-poll waiters. Write recency is DELIBERATELY
/// excluded from `quiescent` (a chatty operator session cannot be drained,
/// only observed); it is reported at the same level as
/// `recent_writes_total` and in `recent_writes`, and the converge-gate
/// wrapper enforces the policy for it (blocking by default, tunable with
/// `--writes-window`). Raw consumers must read both fields.
pub(crate) fn orchestration_activity_snapshot(
    state: &SharedState,
    writes_window_minutes: u64,
) -> Value {
    let now_ms = crate::orchestration::now_ms();
    let now = chrono::Utc::now();

    // Running bro tasks.
    let mut running_tasks: Vec<Value> = Vec::new();
    {
        let store = state.task_store.read();
        for task in store.all_tasks() {
            let inner = task.inner.lock();
            if inner.status != TaskStatus::Running {
                continue;
            }
            let age_secs = now_ms.saturating_sub(inner.started_at) / 1000;
            running_tasks.push(json!({
                "task_id": inner.id,
                "session_id": inner.session_id,
                "provider": inner.provider.as_str(),
                "origin": inner.origin.as_wire(),
                "bro": inner.bro_label,
                "agent": inner.agent_label,
                "name": inner.name,
                "cwd": inner.cwd,
                "started_at_ms": inner.started_at,
                "age_secs": age_secs,
            }));
        }
    }
    running_tasks.sort_by(|a, b| {
        b["age_secs"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["age_secs"].as_u64().unwrap_or(0))
    });

    // Workflow arcs in flight: the cancel-token map holds exactly the live
    // arcs (registered at run start, dropped at terminus); the running_arcs
    // snapshot map decorates them with node-level state when present.
    let live_arc_ids: Vec<String> = state.arc_cancel_tokens.read().keys().cloned().collect();
    let arcs_in_flight: Vec<Value> = {
        let snapshots = state.running_arcs.read();
        let by_arc: HashMap<&str, &super::state::ArcSnapshot> =
            snapshots.values().map(|s| (s.arc_id.as_str(), s)).collect();
        let mut rows: Vec<Value> = live_arc_ids
            .iter()
            .map(|arc_id| match by_arc.get(arc_id.as_str()) {
                Some(s) => json!({
                    "arc_id": s.arc_id,
                    "arc_thread_id": s.arc_thread_id,
                    "workflow": s.workflow_name,
                    "workflow_version": s.workflow_version,
                    "status": s.status,
                    "current_node": s.current_node,
                    "in_flight_nodes": s.in_flight_nodes,
                    "started_at": s.started_at,
                    "age_secs": rfc3339_age_secs(&s.started_at, now),
                }),
                None => json!({
                    "arc_id": arc_id,
                    "status": "starting",
                }),
            })
            .collect();
        rows.sort_by(|a, b| {
            a["arc_id"]
                .as_str()
                .unwrap_or("")
                .cmp(b["arc_id"].as_str().unwrap_or(""))
        });
        rows
    };

    // Long-poll waiters.
    let waiters: Vec<Value> = state
        .long_polls
        .snapshot()
        .into_iter()
        .map(|w| {
            json!({
                "id": w.id,
                "tool": w.tool,
                "task_ids": w.task_ids,
                "age_secs": now_ms.saturating_sub(w.started_at_ms) / 1000,
            })
        })
        .collect();

    // Recent orchestration writes.
    let window_secs = (writes_window_minutes as i64).saturating_mul(60);
    let within = |ts: &str| -> bool {
        matches!(rfc3339_age_secs(ts, now), Some(age) if age >= 0 && age <= window_secs)
    };
    let mut thread_rows: Vec<Value> = Vec::new();
    for t in state.threads.read().all() {
        if within(&t.last_activity) {
            thread_rows.push(json!({
                "id": t.id,
                "topic": t.topic,
                "status": t.status,
                "last_activity": t.last_activity,
            }));
        }
    }
    let mut note_rows: Vec<Value> = Vec::new();
    for n in state.notes.read().all() {
        if within(&n.updated_at) {
            note_rows.push(json!({
                "id": n.id,
                "kind": n.kind,
                "updated_at": n.updated_at,
            }));
        }
    }
    let mut knowledge_rows: Vec<Value> = Vec::new();
    for e in state.kb.read().all_entries() {
        if within(&e.updated_at) {
            knowledge_rows.push(json!({
                "id": e.id,
                "title": e.title,
                "updated_at": e.updated_at,
            }));
        }
    }
    let recent_total = thread_rows.len() + note_rows.len() + knowledge_rows.len();

    let quiescent = running_tasks.is_empty() && arcs_in_flight.is_empty() && waiters.is_empty();

    json!({
        "status": "ok",
        "sampled_at": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "drain": state.drain.snapshot(),
        "quiescent": quiescent,
        "quiescent_scope": "tasks,arcs,waiters",
        "recent_writes_total": recent_total,
        "running_tasks": {
            "count": running_tasks.len(),
            "tasks": running_tasks,
        },
        "workflows_in_flight": {
            "count": arcs_in_flight.len(),
            "arcs": arcs_in_flight,
        },
        "long_poll_waiters": {
            "count": waiters.len(),
            "waiters": waiters,
        },
        "recent_writes": {
            "window_minutes": writes_window_minutes,
            "total": recent_total,
            "threads": thread_rows,
            "notes": note_rows,
            "knowledge": knowledge_rows,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_without_marker_is_not_draining() {
        let dir = tempfile::tempdir().unwrap();
        let drain = DrainState::open(dir.path());
        assert!(!drain.is_draining());
        assert!(drain.admission_refusal("bro_exec").is_none());
        assert_eq!(drain.snapshot()["draining"], json!(false));
    }

    #[test]
    fn set_persists_marker_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let drain = DrainState::open(dir.path());
        let rec = drain
            .set(Some("converge 1.2.3".into()), Some("converge-gate".into()))
            .unwrap();
        assert!(rec.draining);
        assert!(drain.is_draining());
        assert!(drain.marker_path().exists());

        // A fresh open (simulating a daemon restart) keeps the drain.
        let reopened = DrainState::open(dir.path());
        assert!(reopened.is_draining());
        let cur = reopened.current().unwrap();
        assert_eq!(cur.reason.as_deref(), Some("converge 1.2.3"));
        assert_eq!(cur.set_by.as_deref(), Some("converge-gate"));
        assert_eq!(cur.set_at, rec.set_at);
    }

    #[test]
    fn set_is_idempotent_and_keeps_original_set_at() {
        let dir = tempfile::tempdir().unwrap();
        let drain = DrainState::open(dir.path());
        let first = drain.set(Some("a".into()), None).unwrap();
        let second = drain.set(None, Some("op".into())).unwrap();
        assert_eq!(first.set_at, second.set_at);
        assert_eq!(second.reason.as_deref(), Some("a"));
        assert_eq!(second.set_by.as_deref(), Some("op"));
    }

    #[test]
    fn clear_removes_marker_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let drain = DrainState::open(dir.path());
        drain.set(None, None).unwrap();
        drain.clear().unwrap();
        assert!(!drain.is_draining());
        assert!(!drain.marker_path().exists());
        drain.clear().unwrap();
        assert!(!DrainState::open(dir.path()).is_draining());
    }

    #[test]
    fn corrupt_marker_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DRAIN_MARKER_FILE), "{not json").unwrap();
        let drain = DrainState::open(dir.path());
        assert!(drain.is_draining());
        let msg = drain.admission_refusal("bro_exec").unwrap();
        assert!(msg.starts_with(MAINTENANCE_PENDING_CODE), "{msg}");
        // Clearing removes the corrupt marker too.
        drain.clear().unwrap();
        assert!(!DrainState::open(dir.path()).is_draining());
    }

    #[test]
    fn refusal_message_is_retryable_and_names_window() {
        let rec = DrainRecord {
            draining: true,
            set_at: "2026-08-18T00:00:00Z".into(),
            reason: Some("converge".into()),
            set_by: None,
        };
        let msg = refusal_message(&rec, "bro_exec");
        assert!(msg.starts_with("error.maintenance_pending: bro_exec refused"));
        assert!(msg.contains("retryable=true"));
        assert!(msg.contains("2026-08-18T00:00:00Z"));
        assert!(msg.contains("reason: converge"));
        assert!(msg.contains("bro_resume"));
    }

    #[test]
    fn long_poll_guard_registers_and_unregisters() {
        let reg = Arc::new(LongPollRegistry::new());
        assert_eq!(reg.len(), 0);
        let g1 = reg.register("bro_wait", vec!["t1".into()]);
        let g2 = reg.register("bro_when_all", vec!["t1".into(), "t2".into()]);
        assert_eq!(reg.len(), 2);
        let snap = reg.snapshot();
        assert_eq!(snap[0].tool, "bro_wait");
        assert_eq!(snap[1].task_ids, vec!["t1".to_string(), "t2".to_string()]);
        drop(g1);
        assert_eq!(reg.len(), 1);
        drop(g2);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn activity_snapshot_reports_scope_and_activity() {
        use crate::orchestration::providers::Provider;
        use crate::orchestration::{TaskStatus, test_task};

        let dir = tempfile::tempdir().unwrap();
        let state = SharedState::for_test(&dir.path().join("bro"));

        // Empty daemon: quiescent, scope stated, nothing recent.
        let snap = orchestration_activity_snapshot(&state, 10);
        assert_eq!(snap["quiescent"], json!(true));
        assert_eq!(snap["quiescent_scope"], json!("tasks,arcs,waiters"));
        assert_eq!(snap["recent_writes_total"], json!(0));
        assert_eq!(snap["drain"]["draining"], json!(false));
        assert_eq!(snap["running_tasks"]["count"], json!(0));
        assert_eq!(snap["workflows_in_flight"]["count"], json!(0));
        assert_eq!(snap["long_poll_waiters"]["count"], json!(0));

        // One running task + one finished task: only the running one counts.
        {
            let mut store = state.task_store.write();
            store
                .insert(
                    "run-1".into(),
                    test_task("run-1", TaskStatus::Running, Provider::Glm),
                )
                .unwrap();
            store
                .insert(
                    "done-1".into(),
                    test_task("done-1", TaskStatus::Completed, Provider::Glm),
                )
                .unwrap();
        }
        // One live arc token and one long-poll waiter.
        let _tok = state.register_arc_cancel_token("arc-live");
        let long_polls = Arc::clone(&state.long_polls);
        let _guard = long_polls.register("bro_wait", vec!["run-1".into()]);
        state.drain.set(Some("converge".into()), None).unwrap();

        let snap = orchestration_activity_snapshot(&state, 10);
        assert_eq!(snap["quiescent"], json!(false));
        assert_eq!(snap["running_tasks"]["count"], json!(1));
        assert_eq!(snap["running_tasks"]["tasks"][0]["task_id"], json!("run-1"));
        assert!(snap["running_tasks"]["tasks"][0]["age_secs"].is_u64());
        assert_eq!(snap["workflows_in_flight"]["count"], json!(1));
        assert_eq!(
            snap["workflows_in_flight"]["arcs"][0]["arc_id"],
            json!("arc-live")
        );
        assert_eq!(snap["long_poll_waiters"]["count"], json!(1));
        assert_eq!(
            snap["long_poll_waiters"]["waiters"][0]["tool"],
            json!("bro_wait")
        );
        assert_eq!(snap["drain"]["draining"], json!(true));
        assert_eq!(snap["drain"]["reason"], json!("converge"));

        // Dropping the waiter and the arc token, and finishing the task,
        // returns to quiescent even though drain is still set.
        drop(_guard);
        state.unregister_arc_cancel_token("arc-live");
        state
            .task_store
            .read()
            .get("run-1")
            .unwrap()
            .inner
            .lock()
            .status = TaskStatus::Completed;
        let snap = orchestration_activity_snapshot(&state, 10);
        assert_eq!(snap["quiescent"], json!(true));
        assert_eq!(snap["drain"]["draining"], json!(true));
    }

    #[test]
    fn writes_window_clamps() {
        assert_eq!(
            clamp_writes_window_minutes(None),
            DEFAULT_WRITES_WINDOW_MINUTES
        );
        assert_eq!(clamp_writes_window_minutes(Some(0)), 0);
        assert_eq!(
            clamp_writes_window_minutes(Some(999_999)),
            MAX_WRITES_WINDOW_MINUTES
        );
    }
}
