//! `todo_write` — a durable, loop-level plan/checklist the agent maintains
//! across turns. Mirrors Claude Code's `TodoWrite` and Codex's `update_plan`.
//!
//! The list is a single shared cell (`ToolCx.todos`) the harness seeds from the
//! persisted session `side` blob at start and flushes back at turn end, so it
//! survives `exec → resume`. The tool fully replaces
//! the list on each call (idempotent set-semantics), which is how both peer
//! harnesses model it — the model re-emits the whole list with updated
//! statuses rather than patching individual items.

use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoItem {
    /// Short imperative description of the step. (`content` is also accepted as
    /// an alias for Claude Code TodoWrite parity.)
    #[serde(alias = "content", alias = "description")]
    pub task: String,
    pub status: TodoStatus,
}

/// The shared, mutable todo list. Hangs off `ToolCx` behind an `Arc<Mutex<_>>`,
/// the same pattern the registry uses for its cross-turn `activated` set.
#[derive(Debug, Clone, Default)]
pub struct TodoList {
    pub items: Vec<TodoItem>,
}

impl TodoList {
    /// Seed from the persisted `side.todos` value. Tolerant: malformed or
    /// absent state yields an empty list rather than failing a resume.
    pub fn from_side(v: &Value) -> Self {
        serde_json::from_value::<Vec<TodoItem>>(v.clone())
            .map(|items| TodoList { items })
            .unwrap_or_default()
    }

    /// Serialize for the `side.todos` slot.
    pub fn to_side(&self) -> Value {
        serde_json::to_value(&self.items).unwrap_or(Value::Null)
    }

    fn render(&self) -> String {
        if self.items.is_empty() {
            return "(todo list is empty)".to_string();
        }
        self.items
            .iter()
            .map(|it| {
                let mark = match it.status {
                    TodoStatus::Pending => "[ ]",
                    TodoStatus::InProgress => "[~]",
                    TodoStatus::Completed => "[x]",
                };
                format!("{mark} {}", it.task)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Deserialize, JsonSchema)]
struct TodoWriteInput {
    /// The full todo list. Replaces the previous list entirely — include every
    /// item each call, with its current status.
    items: Vec<TodoItem>,
}

/// Unit struct; the shared list lives on `ToolCx.todos`, read in `call` exactly
/// as the file/shell tools read `cx.root` / `cx.safety`.
pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }
    fn description(&self) -> &str {
        "Record or update your task checklist for multi-step work. Pass the FULL list each call (it replaces the previous one); each item is {task, status: pending|in_progress|completed}. Keep at most one item in_progress. The list persists across turns and resumes."
    }
    fn input_schema(&self) -> Value {
        schema_for::<TodoWriteInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        // Not a filesystem mutation; cheap bookkeeping. Neither read_only nor
        // destructive in the worktree sense.
        ToolAnnotations::default()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: TodoWriteInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };

        let in_progress = args
            .items
            .iter()
            .filter(|it| it.status == TodoStatus::InProgress)
            .count();
        if in_progress > 1 {
            return ToolResult::Error(format!(
                "at most one item may be in_progress; got {in_progress}"
            ));
        }

        let list = TodoList { items: args.items };
        let rendered = list.render();
        let total = list.items.len();
        let completed = list
            .items
            .iter()
            .filter(|it| it.status == TodoStatus::Completed)
            .count();

        // Commit to the shared cell; the harness flushes it to `side` at turn end.
        match cx.todos.lock() {
            Ok(mut guard) => *guard = list,
            Err(_) => return ToolResult::Error("todo list lock poisoned".into()),
        }

        ToolResult::Json(json!({
            "ok": true,
            "total": total,
            "completed": completed,
            "list": rendered,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn cx() -> ToolCx {
        ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(crate::shell::ShellSessions::default())),
            promises: Arc::new(Mutex::new(crate::promise::PromiseStore::default())),
            edits: Arc::new(Mutex::new(crate::edits::EditSink::default())),
        }
    }

    #[tokio::test]
    async fn writes_list_into_shared_cell() {
        let c = cx();
        let r = TodoWrite
            .call(
                // mixes canonical `task` with the `content` alias on purpose.
                json!({"items":[
                    {"task":"scaffold","status":"completed"},
                    {"content":"wire it","status":"in_progress"},
                    {"task":"test","status":"pending"}
                ]}),
                &c,
            )
            .await;
        assert!(!r.is_error(), "{r:?}");
        let guard = c.todos.lock().unwrap();
        assert_eq!(guard.items.len(), 3);
        assert_eq!(guard.items[1].status, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn rejects_multiple_in_progress() {
        let c = cx();
        let r = TodoWrite
            .call(
                json!({"items":[
                    {"content":"a","status":"in_progress"},
                    {"content":"b","status":"in_progress"}
                ]}),
                &c,
            )
            .await;
        assert!(r.is_error());
        // The shared cell is left untouched on rejection.
        assert!(c.todos.lock().unwrap().items.is_empty());
    }

    #[test]
    fn side_round_trip() {
        let list = TodoList {
            items: vec![TodoItem {
                task: "x".into(),
                status: TodoStatus::Pending,
            }],
        };
        let side = list.to_side();
        let back = TodoList::from_side(&side);
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].task, "x");
        // Null / garbage tolerated.
        assert!(TodoList::from_side(&Value::Null).items.is_empty());
        assert!(TodoList::from_side(&json!("nope")).items.is_empty());
    }
}
