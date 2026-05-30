//! The `Tool` abstraction. Ported from daystrom's `McpToolDefinition` /
//! `ToolResult`, Rust-native.

use async_trait::async_trait;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Execution context handed to every tool call.
#[derive(Clone)]
pub struct ToolCx {
    /// Worktree root — the cwd the harness was spawned in. All file/shell/git
    /// operations are scoped to (and validated against) this root.
    pub root: PathBuf,
    /// Shared safety policy (command denylist + sensitive-file guard).
    pub safety: Arc<crate::safety::SafetyPolicy>,
    /// Shared HTTP client for web tools.
    pub http: reqwest::Client,
    /// Durable, loop-level todo list. Seeded from the persisted session `side`
    /// cell at start, flushed back at turn end, so it survives `exec → resume`.
    /// Shared behind `Arc<Mutex<_>>` like the registry's cross-turn state.
    pub todos: Arc<std::sync::Mutex<crate::todo::TodoList>>,
    /// Live shell sessions (shell_run/shell_poll). In-memory, single-`run()`
    /// lifetime — holds running OS children, so it is NOT persisted to `side`.
    pub shell_sessions: Arc<std::sync::Mutex<crate::shell::ShellSessions>>,
    /// Clipboard registers: named, server-held value cells the `clip_*` tools
    /// produce/consume, and the substrate other tools chain through (the ref
    /// ABI). Durable across `exec → resume` via the `side` cell, like `todos`.
    pub clipboard: Arc<std::sync::Mutex<crate::clipboard::Registers>>,
    /// Per-turn capture of file mutations: file-mutating tools push an
    /// [`crate::edits::EditEvent`] (pre-image + pre/post sha256 + path) here
    /// at mutation time, and the edit loop drains the sink after each
    /// dispatch to feed window-0 diagnostics. Ephemeral — never persisted to
    /// `side`.
    pub edits: Arc<std::sync::Mutex<crate::edits::EditSink>>,
}

/// Result of a tool call. Maps onto the content of an Anthropic
/// `tool_result` block; `Error` sets `is_error: true`.
#[derive(Debug, Clone)]
pub enum ToolResult {
    Text(String),
    Json(Value),
    Error(String),
}

impl ToolResult {
    pub fn is_error(&self) -> bool {
        matches!(self, ToolResult::Error(_))
    }

    /// Render to `(content_string, is_error)` for a `tool_result` block.
    pub fn into_content(self) -> (String, bool) {
        match self {
            ToolResult::Text(t) => (t, false),
            ToolResult::Json(v) => (
                serde_json::to_string(&v).unwrap_or_else(|_| v.to_string()),
                false,
            ),
            ToolResult::Error(e) => (e, true),
        }
    }

    /// Convenience for `anyhow`-returning tool bodies.
    pub fn from_result<T: Into<Value>>(r: anyhow::Result<T>) -> ToolResult {
        match r {
            Ok(v) => ToolResult::Json(v.into()),
            Err(e) => ToolResult::Error(format!("{e:#}")),
        }
    }
}

/// MCP-style safety hints surfaced to the model/harness.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolAnnotations {
    pub read_only: bool,
    pub destructive: bool,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the input object. Derive from a typed input with
    /// [`schema_for`].
    fn input_schema(&self) -> Value;
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult;
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
    }
}

/// Derive a JSON Schema object for a typed tool input.
pub fn schema_for<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}
