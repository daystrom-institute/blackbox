//! Shared wire DTOs for daemon, harness, and thin clients.
//!
//! The contract crate is the schema. Transports may be stdio, in-process calls,
//! or a future socket, but the payloads live here.

use bro_core::{BroError, Origin, Provider, SessionId, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod dispatch;
mod transcript;

pub use dispatch::{CloseoutErrorClass, CloseoutHooksWire, CloseoutOutcome, CloseoutPhase, CloseoutRequest, DispatchSpec, PhaseResult, ResumeSpec, SERVICE_TIER_DEFAULT, SERVICE_TIER_PRIORITY};
pub use transcript::{TodoItem, TodoItemStatus, TodoState, TranscriptItem};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum SessionCommand {
    UserTurn { text: String },
    Interrupt,
    SetModel { model: String },
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    /// A terminal status is one the task will not leave on its own — the
    /// process has exited (cleanly, in error, or by cancellation). `Pending`
    /// and `Running` are live.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub task_id: TaskId,
    pub session_id: Option<SessionId>,
    pub status: TaskStatus,
    pub last_message: Option<String>,
    pub error: Option<BroError>,
    /// Source of the dispatch (Slice 1b). Defaults to `Unknown` on
    /// snapshots serialized before the field existed.
    #[serde(default)]
    pub origin: Origin,
    /// Concrete cockpit-managed worktree root for this task, when the task's cwd
    /// is under one of the daemon-recognized managed worktree roots.
    #[serde(default)]
    pub managed_worktree: Option<String>,
    /// True when a workflow or atom owns this task's lifecycle.
    #[serde(default)]
    pub workflow_owned: bool,
    /// True when the latest terminal result was interrupted by operator
    /// control, rather than naturally completed.
    #[serde(default)]
    pub interrupted: bool,
}

/// Summary DTO for one fleet task, projected by the daemon for the
/// roster snapshot endpoint (Slice 1a of
/// design/fleet-tui/daemon-roster-and-tail-unification.md §3 item 1).
///
/// Contract: the DTO carries NO event payloads. Roster traffic is a
/// summary plane — a verbose unfocused agent's transcript must not be
/// able to balloon the snapshot, and the 80KB MCP-cap truncation class
/// (cf. cf87a52) cannot occur on this path because the events field
/// is simply absent. A test in this crate asserts the DTO serializes
/// without an `events` field.
///
/// Field provenance is intentionally explicit:
/// - `task_id` / `status` / `provider` / `cost` / `turns` / `cwd` /
///   `label` / `session_id` / `last_message_snippet` are all already
///   on `TaskInner`; the daemon reads them under the per-task lock
///   and projects directly.
/// - `name` is the daemon-owned display name. Fresh dispatch defaults it
///   from the first user prompt; later rename flows may overwrite it.
/// - `model` is the resolved dispatch model persisted on `TaskInner`, with
///   event-buffer scraping retained only as a load-time fallback for legacy
///   tasks that predate the dispatch-time cache.
/// - `report` is a bounded teaser of the latest `bro_report` message.
/// - `last_event_at` is **not** a stored field — the daemon has no
///   per-event arrival stamp on V1. The handler derives it from
///   `max(started_at, completed_at)` (the only wall-clock fields on
///   `TaskInner`); for terminal tasks this is the completion time,
///   for live tasks this is the spawn time. Slice 2 will revisit this
///   if a per-event arrival stamp becomes available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RosterSummaryV1 {
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub provider: Provider,
    pub cost: Option<f64>,
    pub turns: Option<u64>,
    pub cwd: Option<String>,
    pub label: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub session_id: Option<SessionId>,
    pub last_message_snippet: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub report: Option<String>,
    pub last_event_at: Option<u64>,
    /// Source of the dispatch (Slice 1b). Lets the fleet roster tab Fleet
    /// vs Dispatched vs Workflow vs Atom without re-deriving from labels.
    #[serde(default)]
    pub origin: Origin,
    /// Concrete cockpit-managed worktree root for this task, when present.
    #[serde(default)]
    pub managed_worktree: Option<String>,
    /// True when a workflow or atom owns this task's lifecycle.
    #[serde(default)]
    pub workflow_owned: bool,
    /// Spawn-time wall-clock millis (wave 7c). `RosterView` is the
    /// per-task summary plane; the dashboard consumer needs both
    /// `last_event_at` and the spawn time to recompute the
    /// `elapsed` field the legacy `bro_dashboard` returned (terminal
    /// tasks: `last_event_at - started_at`; live tasks: `now -
    /// started_at`). Cheap additive field — set once at dispatch
    /// from `TaskInner.started_at`.
    #[serde(default)]
    pub started_at: Option<u64>,
    /// Agent attribution separate from `label` (wave 7c). The
    /// dashboard needs to surface both `agentLabel` and `broLabel`
    /// distinctly, but `label` already collapses them via
    /// `bro_label.or(agent_label)`. Carrying both keeps the
    /// projection lossless without forcing the dashboard back into
    /// a per-task inner lock.
    #[serde(default)]
    pub agent_label: Option<String>,
    /// Full structured `bro_report` (wave 7c). `report` is a bounded
    /// 80-char teaser for the fleet row UI; consumers that need the
    /// full object (`message` / `needs` / `data` / `reportedAt` /
    /// `reportedAgo`) read it from this field. Additive+optional —
    /// omitted for tasks that never called `bro_report`.
    #[serde(default)]
    pub report_full: Option<BroReportV1>,
    /// True when the latest terminal result was interrupted by operator
    /// control. Additive marker layered on top of `status=cancelled`.
    #[serde(default)]
    pub interrupted: bool,
}

/// Structured form of a `bro_report` payload, projected into the
/// roster summary so dashboard consumers can render the full report
/// object without re-locking the per-task inner mutex (wave 7c).
///
/// The field names match `orchestration::BroReport::to_json()`:
/// `message`, `needs` (when set), `data` (when set), `reportedAt`,
/// `reportedAgo`. `reportedAgo` is computed at projection time
/// against the daemon's wall clock, identical to the
/// `BroReport::to_json()` contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BroReportV1 {
    pub message: String,
    #[serde(default)]
    pub needs: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    pub reported_at: u64,
    pub reported_ago: String,
}

/// Wire envelope for the `GET /control/roster` snapshot. The
/// monotonic `version` field is the daemon roster generation. A client
/// fetches this snapshot, then consumes `RosterDelta` events with
/// `seq > version`; if it observes a gap or receives a resync signal,
/// it re-fetches the snapshot and resumes from the new version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RosterSnapshotV1 {
    pub version: u64,
    pub tasks: Vec<RosterSummaryV1>,
}

/// Versioned roster membership/summary delta for
/// `GET /control/roster/stream`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RosterDelta {
    Added { seq: u64, task: RosterSummaryV1 },
    Updated { seq: u64, task: RosterSummaryV1 },
    Removed { seq: u64, task_id: TaskId },
}

impl RosterDelta {
    pub fn seq(&self) -> u64 {
        match self {
            RosterDelta::Added { seq, .. }
            | RosterDelta::Updated { seq, .. }
            | RosterDelta::Removed { seq, .. } => *seq,
        }
    }
}

/// One raw provider event from the task's current in-memory event buffer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusedTranscriptMemoryEventV1 {
    /// Zero-based cursor within the daemon's current in-memory event window.
    pub cursor: u64,
    pub event: Value,
}

/// Initial SSE payload for `GET /control/transcript/{task_id}/stream`.
///
/// `live_cursor` is the daemon tail cursor through which the snapshot is
/// current. The stream sends only live events with `cursor > live_cursor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusedTranscriptSnapshotV1 {
    pub task_id: TaskId,
    pub session_id: Option<SessionId>,
    pub provider: Provider,
    pub status: TaskStatus,
    pub live_cursor: u64,
    pub memory_start_cursor: u64,
    pub next_memory_cursor: u64,
    pub events: Vec<FocusedTranscriptMemoryEventV1>,
    pub history_jsonl_path: Option<String>,
}

/// One live tail event for a focused transcript subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusedTranscriptLiveEventV1 {
    pub task_id: TaskId,
    pub cursor: u64,
    pub event: Value,
}

/// One provider transcript-file JSONL record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptHistoryEventV1 {
    /// Zero-based JSONL record index in the provider transcript file.
    pub cursor: u64,
    pub byte_offset: u64,
    pub event: Value,
}

/// Bounded page from the provider transcript file source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptHistoryPageV1 {
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub history_jsonl_path: String,
    pub from_cursor: u64,
    pub limit: usize,
    pub next_cursor: u64,
    pub reached_end: bool,
    pub events: Vec<TranscriptHistoryEventV1>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RosterSummaryV1 must serialize with the documented fields and
    /// carry no events payload. A regression that adds an `events` field
    /// (even as an empty Vec) reintroduces the 80KB MCP-cap truncation
    /// class (cf87a52) and breaks the contract the snapshot endpoint
    /// advertises to Slice 2's SSE delta stream.
    #[test]
    fn roster_summary_v1_serializes_without_events_field() {
        let summary = RosterSummaryV1 {
            task_id: TaskId::new("task-1"),
            status: TaskStatus::Running,
            provider: Provider::Glm,
            cost: Some(0.42),
            turns: Some(3),
            cwd: Some("/tmp/repo".to_string()),
            managed_worktree: Some("/tmp/repo".to_string()),
            label: Some("team-x::member-y".to_string()),
            name: Some("Inspect the failing roster columns".to_string()),
            session_id: Some(SessionId::new("sess-1")),
            last_message_snippet: Some("Looking at the file…".to_string()),
            model: Some("claude-opus-4-6".to_string()),
            report: Some("Reading roster state".to_string()),
            last_event_at: Some(1_700_000_000_000),
            origin: Origin::AgentDispatch,
            workflow_owned: false,
            started_at: Some(1_700_000_000_000),
            agent_label: Some("team-x::member-y".to_string()),
            report_full: None,
            interrupted: false,
        };
        let value = serde_json::to_value(&summary).unwrap();
        let obj = value.as_object().expect("summary must serialize as object");

        // Every documented field must be present.
        for key in [
            "task_id",
            "status",
            "provider",
            "cost",
            "turns",
            "cwd",
            "managed_worktree",
            "label",
            "name",
            "session_id",
            "last_message_snippet",
            "model",
            "report",
            "last_event_at",
            "origin",
            "workflow_owned",
            "started_at",
            "agent_label",
            "report_full",
            "interrupted",
        ] {
            assert!(
                obj.contains_key(key),
                "RosterSummaryV1 must serialize field `{key}`; got keys: {:?}",
                obj.keys().collect::<Vec<_>>()
            );
        }

        // `origin` is a lowercase variant name — must NOT be the Debug
        // form (`AgentDispatch` becomes `agentdispatch` on the wire).
        assert_eq!(
            obj["origin"].as_str(),
            Some("agentdispatch"),
            "RosterSummaryV1.origin must serialize as lowercase variant"
        );

        // No event-payload field, no Vec/array field that could carry one.
        assert!(
            !obj.contains_key("events"),
            "RosterSummaryV1 must NOT carry an `events` field (regression of cf87a52)"
        );
        assert!(
            !obj.contains_key("recentEvents"),
            "RosterSummaryV1 must NOT carry a `recentEvents` field"
        );
        for (k, v) in obj {
            assert!(
                !v.is_array(),
                "RosterSummaryV1 field `{k}` unexpectedly serialized as array: {v}"
            );
        }
    }

    /// Envelope shape: `{ version, tasks }`. Used by Slice 2's resync
    /// logic, so the field names are part of the contract.
    #[test]
    fn roster_snapshot_v1_envelope_has_version_and_tasks() {
        let snap = RosterSnapshotV1 {
            version: 1_700_000_000_000,
            tasks: vec![],
        };
        let value = serde_json::to_value(&snap).unwrap();
        let obj = value.as_object().unwrap();
        assert!(obj.contains_key("version"));
        assert!(obj.contains_key("tasks"));
        assert_eq!(obj.len(), 2, "envelope should carry exactly `version` and `tasks`");
    }
}
