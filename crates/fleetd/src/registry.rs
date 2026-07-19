//! The in-memory session registry.
//!
//! Deliberately in-memory only. fleetd persists nothing beyond the children it
//! supervises and the event logs those children write themselves: an accepted
//! v1 limitation is that a fleetd restart kills its children, so there is
//! nothing a durable registry could usefully re-adopt.
//!
//! What the registry DOES survive is a daemon disconnect. Children keep
//! running, relaying pauses (the durable event log is the backlog), entries
//! stay, and the next accepted connection can ask for
//! [`Registry::summaries`] and re-adopt session by session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bro_protocol::{SessionState, SessionSummary};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::spawn::WorkerKill;

/// One supervised session.
#[derive(Clone)]
pub struct SessionEntry {
    pub session_id: String,
    pub task_id: String,
    pub pid: Option<u32>,
    pub state: SessionState,
    /// Highest event seq observed on this session's stdout, relayed or not.
    pub last_seq: Option<u64>,
    /// Highest seq the daemon has acknowledged durably ingesting.
    pub acked_seq: Option<u64>,
    pub event_log_path: PathBuf,
    pub exit_code: Option<i32>,
    pub control: mpsc::UnboundedSender<Value>,
    pub killer: Arc<WorkerKill>,
}

impl SessionEntry {
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            pid: self.pid,
            state: self.state,
            last_seq: self.last_seq,
            event_log_path: self.event_log_path.clone(),
            exit_code: self.exit_code,
        }
    }

    /// A terminal session whose events the daemon has fully acknowledged is
    /// safe to forget: nothing else will ever be said about it.
    pub fn is_fully_acked_terminal(&self) -> bool {
        if self.state != SessionState::Exited {
            return false;
        }
        match (self.last_seq, self.acked_seq) {
            (None, _) => true,
            (Some(last), Some(acked)) => acked >= last,
            (Some(_), None) => false,
        }
    }
}

/// Thread-safe map of live and recently-terminal sessions.
///
/// Every method takes the lock for a short, await-free critical section: the
/// guard is never held across an `.await`, so this stays a plain
/// `std::sync::Mutex` rather than an async one.
#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<String, SessionEntry>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, entry: SessionEntry) {
        self.lock().insert(entry.session_id.clone(), entry);
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.lock().contains_key(session_id)
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every session, ordered by session id so a `Sessions` answer is stable
    /// across calls (a daemon diffing two adoptions should not see churn from
    /// hash-map iteration order).
    pub fn summaries(&self) -> Vec<SessionSummary> {
        let mut summaries: Vec<SessionSummary> =
            self.lock().values().map(SessionEntry::summary).collect();
        summaries.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        summaries
    }

    pub fn summary(&self, session_id: &str) -> Option<SessionSummary> {
        self.lock().get(session_id).map(SessionEntry::summary)
    }

    /// Record the highest seq seen for a session. Monotonic: an out-of-order
    /// or replayed lower seq never walks the high-water mark backwards.
    pub fn note_seq(&self, session_id: &str, seq: u64) {
        if let Some(entry) = self.lock().get_mut(session_id) {
            entry.last_seq = Some(entry.last_seq.map_or(seq, |current| current.max(seq)));
        }
    }

    /// Record the daemon's durable-ingest cursor, then GC the entry if it is
    /// terminal and fully acknowledged.
    pub fn note_ack(&self, session_id: &str, through_seq: u64) {
        let mut sessions = self.lock();
        let Some(entry) = sessions.get_mut(session_id) else {
            return;
        };
        entry.acked_seq = Some(entry.acked_seq.map_or(through_seq, |c| c.max(through_seq)));
        if entry.is_fully_acked_terminal() {
            sessions.remove(session_id);
        }
    }

    pub fn mark_exited(&self, session_id: &str, exit_code: Option<i32>) {
        if let Some(entry) = self.lock().get_mut(session_id) {
            entry.state = SessionState::Exited;
            entry.exit_code = exit_code;
        }
    }

    pub fn control_sender(&self, session_id: &str) -> Option<mpsc::UnboundedSender<Value>> {
        self.lock().get(session_id).map(|e| e.control.clone())
    }

    pub fn killer(&self, session_id: &str) -> Option<Arc<WorkerKill>> {
        self.lock().get(session_id).map(|e| e.killer.clone())
    }

    pub fn event_log_path(&self, session_id: &str) -> Option<PathBuf> {
        self.lock()
            .get(session_id)
            .map(|e| e.event_log_path.clone())
    }

    /// SIGTERM every live child. Used on fleetd shutdown so the accepted v1
    /// limitation ("fleetd's own restart kills its children") is an orderly
    /// signal rather than orphaned processes.
    pub fn kill_all(&self) {
        let killers: Vec<Arc<WorkerKill>> = self
            .lock()
            .values()
            .filter(|entry| entry.state == SessionState::Running)
            .map(|entry| entry.killer.clone())
            .collect();
        for killer in killers {
            killer.kill();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session_id: &str) -> SessionEntry {
        let (control, _rx) = mpsc::unbounded_channel();
        SessionEntry {
            session_id: session_id.to_string(),
            task_id: format!("task-{session_id}"),
            pid: Some(4242),
            state: SessionState::Running,
            last_seq: None,
            acked_seq: None,
            event_log_path: PathBuf::from(format!("/state/{session_id}.events.jsonl")),
            exit_code: None,
            control,
            killer: WorkerKill::new(None),
        }
    }

    #[test]
    fn summaries_are_sorted_and_reflect_state() {
        let registry = Registry::new();
        registry.insert(entry("sess-b"));
        registry.insert(entry("sess-a"));
        let ids: Vec<String> = registry
            .summaries()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, vec!["sess-a", "sess-b"]);

        registry.note_seq("sess-a", 5);
        registry.mark_exited("sess-a", Some(0));
        let summary = registry.summary("sess-a").expect("present");
        assert_eq!(summary.state, SessionState::Exited);
        assert_eq!(summary.exit_code, Some(0));
        assert_eq!(summary.last_seq, Some(5));
    }

    #[test]
    fn note_seq_is_monotonic() {
        let registry = Registry::new();
        registry.insert(entry("sess-a"));
        registry.note_seq("sess-a", 10);
        registry.note_seq("sess-a", 3);
        assert_eq!(registry.summary("sess-a").unwrap().last_seq, Some(10));
    }

    #[test]
    fn unknown_sessions_are_no_ops_not_panics() {
        let registry = Registry::new();
        registry.note_seq("nope", 1);
        registry.note_ack("nope", 1);
        registry.mark_exited("nope", Some(1));
        assert!(registry.is_empty());
        assert!(registry.control_sender("nope").is_none());
        assert!(registry.killer("nope").is_none());
    }

    /// GC is ack-driven and terminal-only: a live session is never dropped no
    /// matter how far the daemon's cursor has advanced.
    #[test]
    fn ack_gcs_only_fully_acked_terminal_sessions() {
        let registry = Registry::new();
        registry.insert(entry("live"));
        registry.note_seq("live", 5);
        registry.note_ack("live", 5);
        assert!(registry.contains("live"), "a running session is never GC'd");

        registry.insert(entry("done"));
        registry.note_seq("done", 5);
        registry.mark_exited("done", Some(0));
        registry.note_ack("done", 4);
        assert!(
            registry.contains("done"),
            "a partially-acked terminal session must stay for replay"
        );
        registry.note_ack("done", 5);
        assert!(
            !registry.contains("done"),
            "fully-acked terminal session is GC'd"
        );
    }

    /// A terminal session that never emitted a seq-carrying event has nothing
    /// left to replay, so any ack clears it.
    #[test]
    fn terminal_session_without_events_is_gcd_on_ack() {
        let registry = Registry::new();
        registry.insert(entry("quiet"));
        registry.mark_exited("quiet", Some(1));
        registry.note_ack("quiet", 0);
        assert!(!registry.contains("quiet"));
    }

    #[test]
    fn kill_all_fires_live_children_once() {
        let registry = Registry::new();
        registry.insert(entry("live"));
        registry.insert(entry("done"));
        registry.mark_exited("done", Some(0));
        let live = registry.killer("live").unwrap();
        let done = registry.killer("done").unwrap();
        registry.kill_all();
        assert!(live.fired());
        assert!(!done.fired(), "an exited session is not signalled again");
    }
}
