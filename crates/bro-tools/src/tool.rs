//! The `Tool` abstraction. Ported from daystrom's `McpToolDefinition` /
//! `ToolResult`, Rust-native.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Grammar-constrained freeform authoring surface for transports that support
/// it. Tools still expose [`Tool::input_schema`] as the JSON-function fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeformGrammar {
    pub syntax: String,
    pub definition: String,
}

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
    /// Per-turn capture of file mutations: file-mutating tools push an
    /// [`crate::edits::EditEvent`] (pre-image + pre/post sha256 + path) here
    /// at mutation time, and the edit loop drains the sink at the batch
    /// boundary to feed edit diagnostics. Ephemeral — never persisted to
    /// `side`.
    pub edits: Arc<std::sync::Mutex<crate::edits::EditSink>>,
    /// Per-session daemon-supplied identity/config env. This is intentionally
    /// separate from process env so shell children do not inherit credentials.
    /// Tools that expose it must redact sensitive values.
    pub session_env: Arc<BTreeMap<String, String>>,
    /// Host-supplied, NON-SECRET env overlay for shell children (e.g. a
    /// project's `RUSTC_WRAPPER=sccache` from fleet.json `project_dispatch`).
    /// Deliberately a separate lane from `session_env`: credentials stay out
    /// of shells, build env stays out of transports. Model-supplied per-call
    /// `env` still wins over this overlay.
    pub shell_env: Arc<BTreeMap<String, String>>,
    /// Per-session host-supplied tool arg default/pin table. Like
    /// `session_env`, this is daemon-trusted context and is never inherited by
    /// shell children.
    pub tool_arg_defaults: Arc<crate::tool_defaults::ToolArgDefaults>,
}

/// Run a synchronous, I/O-heavy tool body on tokio's blocking pool.
///
/// In-process harness loops share the host daemon's runtime; a sync fs walk
/// or child-process wait inside an async `call` body blocks a runtime worker
/// for its full duration (measured 1–90ms worker polls under streaming agent
/// load — blackbox concurrency-model §4.6). Tool bodies that are pure sync
/// work (tree walks, capped reads, sync git capture) go through this; bodies
/// already on `tokio::fs`/`tokio::process` stay direct.
pub async fn call_blocking(f: impl FnOnce() -> ToolResult + Send + 'static) -> ToolResult {
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => ToolResult::Error(format!("tool execution task failed: {e}")),
    }
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
    fn freeform_grammar(&self) -> Option<FreeformGrammar> {
        None
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult;
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default()
    }
    /// Code-mode projection override. When set, the tool is installed in the
    /// cell as `<namespace>.<method>(...)` — a nested namespace global beside
    /// `tools` — instead of a flat `tools.*` property. Dispatch is unchanged:
    /// the cell call routes through the seam by canonical [`Tool::name`]
    /// (conventionally `"<namespace>.<method>"`), honoring the same filter.
    /// Domain bindings (`code.*`, `lsp.*`, …) set this; ordinary tools don't.
    fn namespace_binding(&self) -> Option<(String, String)> {
        None
    }
}

/// Derive a JSON Schema object for a typed tool input.
pub fn schema_for<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}
