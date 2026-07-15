//! Stable context-window lineage and read-only remaining-capacity tool.

use std::sync::Mutex;

use async_trait::async_trait;
use bro_tools::{Tool, ToolCx, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const GET_CONTEXT_REMAINING: &str = "get_context_remaining";
const CONTEXT_WINDOW_STATE_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextWindowLineage {
    pub v: u16,
    pub first_window_id: String,
    #[serde(default)]
    pub previous_window_id: Option<String>,
    pub current_window_id: String,
    pub ordinal: u64,
}

impl ContextWindowLineage {
    fn fresh() -> Self {
        let current = uuid::Uuid::new_v4().to_string();
        Self {
            v: CONTEXT_WINDOW_STATE_VERSION,
            first_window_id: current.clone(),
            previous_window_id: None,
            current_window_id: current,
            ordinal: 0,
        }
    }

    fn from_side(value: &Value) -> Self {
        serde_json::from_value::<Self>(value.clone())
            .ok()
            .filter(|lineage| {
                lineage.v == CONTEXT_WINDOW_STATE_VERSION
                    && !lineage.first_window_id.is_empty()
                    && !lineage.current_window_id.is_empty()
            })
            .unwrap_or_else(Self::fresh)
    }

    fn advance(&mut self) -> ContextWindowTransition {
        let previous = self.current_window_id.clone();
        let current = uuid::Uuid::new_v4().to_string();
        self.previous_window_id = Some(previous.clone());
        self.current_window_id = current.clone();
        self.ordinal = self.ordinal.saturating_add(1);
        ContextWindowTransition {
            first_window_id: self.first_window_id.clone(),
            previous_window_id: previous,
            current_window_id: current,
            ordinal: self.ordinal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextWindowTransition {
    pub first_window_id: String,
    pub previous_window_id: String,
    pub current_window_id: String,
    pub ordinal: u64,
}

#[derive(Debug)]
struct ContextWindowState {
    lineage: ContextWindowLineage,
    window_tokens: Option<u64>,
    used_tokens: Option<u64>,
}

/// Shared by the loop and the model tool. The tool can only inspect this
/// worker-local state; all mutations remain owned by the turn loop.
pub struct ContextWindowTracker {
    state: Mutex<ContextWindowState>,
}

impl ContextWindowTracker {
    pub fn restore(value: &Value, window_tokens: Option<u64>) -> Self {
        Self {
            state: Mutex::new(ContextWindowState {
                lineage: ContextWindowLineage::from_side(value),
                window_tokens,
                // Provider usage is not stored. A resumed session reports null
                // until the next model call supplies a fresh measurement.
                used_tokens: None,
            }),
        }
    }

    pub fn set_window_tokens(&self, window_tokens: Option<u64>) {
        if let Ok(mut state) = self.state.lock() {
            state.window_tokens = window_tokens;
        }
    }

    pub fn observe_used_tokens(&self, used_tokens: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.used_tokens = Some(used_tokens);
        }
    }

    pub fn observe_projected_tokens(&self, projected_tokens: u64) {
        if let Ok(mut state) = self.state.lock()
            && state.used_tokens.is_some()
        {
            state.used_tokens = Some(projected_tokens);
        }
    }

    pub fn invalidate_measurement(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.used_tokens = None;
        }
    }

    pub fn advance(&self) -> ContextWindowTransition {
        let mut state = self.state.lock().expect("context-window state poisoned");
        state.used_tokens = None;
        state.lineage.advance()
    }

    pub fn tokens_left(&self) -> Option<u64> {
        let state = self.state.lock().ok()?;
        Some(state.window_tokens?.saturating_sub(state.used_tokens?))
    }

    pub fn lineage(&self) -> ContextWindowLineage {
        self.state
            .lock()
            .expect("context-window state poisoned")
            .lineage
            .clone()
    }

    pub fn to_side(&self) -> Value {
        serde_json::to_value(self.lineage()).unwrap_or(Value::Null)
    }
}

pub struct GetContextRemainingTool {
    tracker: std::sync::Arc<ContextWindowTracker>,
}

impl GetContextRemainingTool {
    pub fn new(tracker: std::sync::Arc<ContextWindowTracker>) -> Self {
        Self { tracker }
    }
}

#[async_trait]
impl Tool for GetContextRemainingTool {
    fn name(&self) -> &str {
        GET_CONTEXT_REMAINING
    }

    fn description(&self) -> &str {
        "Return the estimated tokens remaining in the active model context window. Returns null until provider usage is available."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolResult {
        ToolResult::Json(json!({"tokens_left": self.tracker.tokens_left()}))
    }

    fn annotations(&self) -> bro_tools::ToolAnnotations {
        bro_tools::ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn lineage_restores_and_advances_without_resetting_first_identity() {
        let tracker = ContextWindowTracker::restore(&Value::Null, Some(200_000));
        let first = tracker.lineage();
        let transition = tracker.advance();
        assert_eq!(transition.first_window_id, first.first_window_id);
        assert_eq!(transition.previous_window_id, first.current_window_id);
        assert_ne!(transition.current_window_id, transition.previous_window_id);
        assert_eq!(transition.ordinal, 1);

        let restored = ContextWindowTracker::restore(&tracker.to_side(), Some(200_000));
        assert_eq!(restored.lineage(), tracker.lineage());
        assert_eq!(restored.tokens_left(), None);
    }

    #[tokio::test]
    async fn remaining_tool_is_read_only_and_returns_null_until_measured() {
        let tracker = Arc::new(ContextWindowTracker::restore(&Value::Null, Some(100)));
        let tool = GetContextRemainingTool::new(tracker.clone());
        let cx = ToolCx {
            invocation_id: None,
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(Default::default()),
            tool_arg_defaults: Arc::new(Default::default()),
            shell_env: Arc::new(Default::default()),
        };
        let ToolResult::Json(before) = tool.call(json!({}), &cx).await else {
            panic!("expected json result")
        };
        assert!(before["tokens_left"].is_null());

        tracker.observe_used_tokens(45);
        let ToolResult::Json(after) = tool.call(json!({}), &cx).await else {
            panic!("expected json result")
        };
        assert_eq!(after["tokens_left"], 55);
        assert!(tool.annotations().read_only);
    }
}
