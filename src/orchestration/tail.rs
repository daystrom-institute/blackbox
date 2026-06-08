use serde::Serialize;

use super::providers::Provider;

// ---------------------------------------------------------------------------
// Tail events — broadcast to SSE subscribers
// ---------------------------------------------------------------------------

// Variants intentionally share the `Task` prefix: the serde tag field is
// `type` with snake_case values ("task_started", "task_completed", …)
// which is what SSE consumers match on. Dropping the prefix would change
// the wire format.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TailEvent {
    TaskStarted {
        cursor: u64,
        task_id: String,
        provider: Provider,
        bro_name: Option<String>,
    },
    TaskProgress {
        cursor: u64,
        task_id: String,
        activity: String,
    },
    TaskCompleted {
        cursor: u64,
        task_id: String,
        elapsed: String,
        cost: Option<f64>,
        /// Session ID of the completed bro task. Carried into the
        /// `task-completed` routing signal so auto-digest-arc can read
        /// the session transcript without a separate lookup.
        source_session: String,
        /// Brofile / team context label from `TaskInner.bro_label`.
        /// Populated as `task_kind` in the routing entity so downstream
        /// packets can filter by bro type (executor vs. ensemble, etc.).
        task_kind: Option<String>,
    },
    TaskFailed {
        cursor: u64,
        task_id: String,
        elapsed: String,
        error: String,
    },
    TaskCancelled {
        cursor: u64,
        task_id: String,
        elapsed: String,
    },
}

impl TailEvent {
    pub fn task_id(&self) -> &str {
        match self {
            TailEvent::TaskStarted { task_id, .. }
            | TailEvent::TaskProgress { task_id, .. }
            | TailEvent::TaskCompleted { task_id, .. }
            | TailEvent::TaskFailed { task_id, .. }
            | TailEvent::TaskCancelled { task_id, .. } => task_id,
        }
    }

    pub fn cursor(&self) -> u64 {
        match self {
            TailEvent::TaskStarted { cursor, .. }
            | TailEvent::TaskProgress { cursor, .. }
            | TailEvent::TaskCompleted { cursor, .. }
            | TailEvent::TaskFailed { cursor, .. }
            | TailEvent::TaskCancelled { cursor, .. } => *cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tail_event_task_id() {
        let evt = TailEvent::TaskProgress {
            cursor: 1,
            task_id: "my-task".into(),
            activity: "working".into(),
        };
        assert_eq!(evt.task_id(), "my-task");
        assert_eq!(evt.cursor(), 1);
    }

    #[test]
    fn test_tail_event_serialization() {
        let evt = TailEvent::TaskFailed {
            cursor: 2,
            task_id: "t2".into(),
            elapsed: "5s".into(),
            error: "spawn error".into(),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"type\":\"task_failed\""));
        assert!(json.contains("\"cursor\":2"));
        assert!(json.contains("spawn error"));
    }
}
