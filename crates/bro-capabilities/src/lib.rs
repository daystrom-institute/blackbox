//! The generic host-tool capability seam consumed by code-mode cells.
//!
//! Daemon-owned capabilities (corpus search, atoms, knowledge, ...) reach a
//! harness session through the daemon's server-filtered MCP catalog, never
//! through trait slots here. The seam rule: a harness-side dependency on
//! daemon-side function is either plain MCP or a deliberately-designed typed
//! RPC contract; this crate holds only the harness-internal [`ToolCapability`]
//! seam that projects the already-filtered session tool set into code-mode
//! cells. (The in-process-era `AtomCapability` / `CorpusCapability` trait
//! slots were deleted 2026-07-19 with zero implementers; see
//! design/daemon-runtime/locality-first-decomposition.md section 2.)

use async_trait::async_trait;
use bro_core::BroError;
use serde::{Deserialize, Serialize};

pub type CapabilityResult<T> = Result<T, BroError>;

/// One host built-in tool invocation: the tool's registered name plus its raw
/// JSON input. This is the generic "invoke a bro-tools built-in by name" seam —
/// one bridge rather than N bespoke traits. The implementer (the harness) owns
/// the `Tool::call` dispatch and the per-session `ToolCx`; a code-mode cell's
/// `tools.*` call can only reach down here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub name: String,
    pub input_json: serde_json::Value,
}

/// The result of a host tool call, flattened to the `tool_result` shape: a
/// content string, whether the tool reported an error, and a MIME-ish content
/// type so the caller can decide whether to parse it as JSON or treat it as
/// text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallOutput {
    pub content: String,
    pub is_error: bool,
    pub content_type: String,
}

/// Invoke a host built-in tool by name. The implementer MUST gate the callable
/// set by the same `ToolFilter` as the flat model-facing surface (an unfiltered
/// in-box surface would be a deny-bypass), and runs the tool against the
/// per-session execution context. An unknown / denied / unavailable tool fails
/// closed with a [`BroError`].
#[async_trait]
pub trait ToolCapability: Send + Sync {
    async fn call_tool(&self, invocation: ToolInvocation) -> CapabilityResult<ToolCallOutput>;
}
