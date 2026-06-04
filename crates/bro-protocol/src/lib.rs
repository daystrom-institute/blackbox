//! Shared wire DTOs for daemon, harness, and thin clients.
//!
//! The contract crate is the schema. Transports may be stdio, in-process calls,
//! or a future socket, but the payloads live here.

use bro_core::{BroError, SessionId, TaskId};
use serde::{Deserialize, Serialize};

mod dispatch;
mod transcript;

pub use dispatch::{DispatchSpec, ResumeSpec};
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
}
