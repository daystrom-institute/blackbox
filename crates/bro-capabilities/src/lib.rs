//! Capability traits supplied to in-process bro sessions.
//!
//! The traits live below both implementers: the daemon can implement them, the
//! harness can consume them, and absence remains a fail-closed runtime choice.

use async_trait::async_trait;
use bro_core::{AtomRef, BroError};
use serde::{Deserialize, Serialize};

pub type CapabilityResult<T> = Result<T, BroError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomInvocation {
    pub atom: AtomRef,
    pub input_json: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtomOutput {
    pub output_json: serde_json::Value,
}

#[async_trait]
pub trait AtomCapability: Send + Sync {
    async fn invoke_atom(&self, invocation: AtomInvocation) -> CapabilityResult<AtomOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusLookup {
    pub query: String,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusHit {
    pub id: String,
    pub text: String,
}

#[async_trait]
pub trait CorpusCapability: Send + Sync {
    async fn search_corpus(&self, lookup: CorpusLookup) -> CapabilityResult<Vec<CorpusHit>>;
}

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
