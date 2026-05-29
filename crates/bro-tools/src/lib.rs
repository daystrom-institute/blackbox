//! `bro-tools` — provider-agnostic tool implementations and the `Tool`
//! abstraction used by the Anthropic harness (`bro-harness`).
//!
//! See `design/orchestration/anthropic-harness.md`. The split mirrors
//! daystrom-mk2's `Daystrom.Worker/Tools` (workspace tools) and pg_recon's
//! `WebToolFunctions` (web tools), Rust-native: typed inputs derive
//! `schemars::JsonSchema` instead of C# reflection-based schema generation.
//!
//! Nothing here knows about Anthropic, providers, or the stream-json wire
//! format — those live in `bro-harness`.

pub mod safety;
pub mod shell;
pub mod todo;
pub mod tool;
pub mod web;
pub mod workspace;

pub use safety::SafetyPolicy;
pub use shell::{ShellKill, ShellList, ShellPoll, ShellRun, ShellSessions};
pub use todo::{TodoItem, TodoList, TodoStatus, TodoWrite};
pub use tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};

use std::sync::Arc;

/// The built-in tool set the harness registers by default. The web-search
/// tool is intentionally absent: it is a provider-executed server-side tool
/// (verified for GLM + DeepSeek), passed through by the harness rather than
/// implemented here. A client-side search fallback can be added later behind
/// config for providers that lack a server-side tool.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    use workspace::*;
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(FileRead),
        Arc::new(SmartRead),
        Arc::new(FileEdit),
        Arc::new(FileWrite),
        Arc::new(ListDir),
        Arc::new(ContentSearch),
        Arc::new(Glob),
        Arc::new(crate::shell::ShellRun),
        Arc::new(crate::shell::ShellPoll),
        Arc::new(crate::shell::ShellKill),
        Arc::new(crate::shell::ShellList),
        Arc::new(GitStatus),
        Arc::new(GitLog),
        Arc::new(GitDiff),
        Arc::new(GitShow),
        Arc::new(GitCommit),
        Arc::new(web::WebFetch),
        Arc::new(crate::todo::TodoWrite),
    ];
    tools
}
