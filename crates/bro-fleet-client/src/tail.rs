//! Client-local fleet tail events.
//!
//! The cockpit's `FleetOrchestrator` owns a local `broadcast` channel that the
//! status poller emits terminal-transition events on; the TUI subscribes and
//! flashes them. This is NOT the daemon's `/tail` event type (which is shared
//! across the daemon and carries more variants) — the fleet client only needs
//! the three terminal transitions, so it keeps its own lean enum.

#[derive(Debug, Clone)]
pub enum TailEvent {
    TaskCompleted {
        task_id: String,
        elapsed: String,
        cost: Option<f64>,
        source_session: String,
        task_kind: Option<String>,
    },
    TaskFailed {
        task_id: String,
        elapsed: String,
        error: String,
    },
    TaskCancelled {
        task_id: String,
        elapsed: String,
    },
}

impl TailEvent {
    pub fn task_id(&self) -> &str {
        match self {
            TailEvent::TaskCompleted { task_id, .. }
            | TailEvent::TaskFailed { task_id, .. }
            | TailEvent::TaskCancelled { task_id, .. } => task_id,
        }
    }
}
