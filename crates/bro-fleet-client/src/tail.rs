//! Client-local fleet tail events.
//!
//! The cockpit's `FleetOrchestrator` owns a local `broadcast` channel that the
//! TUI subscribes to for wakeups/status flashes. This is NOT the daemon's `/tail`
//! event type. Roster changes are payload-free because daemon roster traffic is
//! summary-only; focused transcript streaming is a later slice.

#[derive(Debug, Clone)]
pub enum TailEvent {
    RosterChanged,
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
    pub fn task_id(&self) -> Option<&str> {
        match self {
            TailEvent::RosterChanged => None,
            TailEvent::TaskCompleted { task_id, .. }
            | TailEvent::TaskFailed { task_id, .. }
            | TailEvent::TaskCancelled { task_id, .. } => Some(task_id),
        }
    }
}
