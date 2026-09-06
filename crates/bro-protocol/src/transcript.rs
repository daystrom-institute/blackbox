//! Transcript view DTOs.
//!
//! The parsed, render-ready shape of a live agent transcript (§2 transcript
//! DTOs, consumers: client + harness + daemon). The *parser* that derives these
//! from the raw stream-json buffer is client-side logic; only the data shape
//! lives at the contract bottom so the thin cockpit can render it without
//! linking the daemon.

/// One rendered item in the verbose inline transcript (§5.4). The fleet layer
/// owns this model rather than reusing `transcripts/types.rs` so the live
/// cockpit view stays decoupled from the stored-transcript schema.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    /// An operator steer / reply (`▌ you ›`).
    UserSteer(String),
    /// Assistant prose (rendered as markdown).
    AssistantText(String),
    /// Extended-thinking block (`✻`, dim).
    Thinking(String),
    /// A tool call with its raw JSON arguments (`⏺ name`).
    ToolCall { name: String, args: String },
    /// A tool result. `tool` is the originating tool name (correlated by
    /// tool_use_id) so the renderer can show change-making tools (Edit/MCP)
    /// verbosely while suppressing noisy output (Bash). `is_error` renders red.
    ToolResult {
        tool: Option<String>,
        content: String,
        is_error: bool,
        /// The window-0 diagnostics rider split off the tool body, when the
        /// harness appended one (Rust file edits). Rendered distinctly.
        rider: Option<String>,
    },
    /// The builtin `report` tool's status line (`◆`, §2.2).
    Report { message: String, needs_input: bool },
    /// Current shared TodoWrite state, parsed from the `todo_write` tool result.
    TodoState(TodoState),
    /// A `/compact` or auto-compaction boundary divider (§2.4).
    CompactBoundary { trigger: String },
    /// End-of-turn footer with usage/cost.
    TurnFooter {
        num_turns: Option<u64>,
        cost_usd: Option<f64>,
        /// Cache-inclusive input tokens of the last model request, from
        /// `result.last_turn_input_tokens`. Never cumulative `usage`.
        input_tokens: Option<u64>,
        /// Current compaction threshold for the session's model, from the
        /// harness-side `CompactionPolicy::threshold`. The percentage bar
        /// is `input_tokens / compaction_threshold * 100`.
        compaction_threshold: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TodoState {
    pub total: usize,
    pub completed: usize,
    pub items: Vec<TodoItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub status: TodoItemStatus,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoItemStatus {
    Pending,
    InProgress,
    Completed,
}
