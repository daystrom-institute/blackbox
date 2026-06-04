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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefactorRequest {
    pub kind: String,
    pub input_json: serde_json::Value,
}

/// A handle to a refactor plan that stays host-side. Only the id (for later
/// materialization) and a short preview enter the model's context — the §6/§9
/// ref-handle model: large results never cross into the prompt or over a wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefactorPlanHandle {
    pub id: String,
    pub preview: String,
}

/// One host built-in tool invocation: the tool's registered name plus its raw
/// JSON input. This is the generic "invoke a bro-tools built-in by name" seam
/// (`narf-tool-placement.md` §5.1) — one bridge rather than N bespoke traits.
/// The implementer (the harness) owns the `Tool::call` dispatch and the
/// per-session `ToolCx`; bro-script (contract-bottom) can only call down here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub name: String,
    pub input_json: serde_json::Value,
}

/// The result of a host tool call, flattened to the `tool_result` shape: a
/// content string, whether the tool reported an error, and a MIME-ish content
/// type so the caller can decide how to treat the stored bytes. The content is
/// stored host-side as a ref by the runtime; only a bounded envelope crosses
/// into the cell (§9 ref-handle model).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallOutput {
    pub content: String,
    pub is_error: bool,
    pub content_type: String,
}

/// Invoke a host built-in tool by name. The implementer MUST gate the callable
/// set by the same `ToolFilter` as the flat model-facing surface
/// (`narf-tool-placement.md` §4.5 — an unfiltered in-box surface is a
/// deny-bypass), and runs the tool against the per-session execution context.
/// An unknown / denied / unavailable tool fails closed with a [`BroError`].
#[async_trait]
pub trait ToolCapability: Send + Sync {
    async fn call_tool(&self, invocation: ToolInvocation) -> CapabilityResult<ToolCallOutput>;
}

#[async_trait]
pub trait RefactorCapability: Send + Sync {
    /// Produce a dry-run plan, store it host-side, and return a handle. The
    /// full plan JSON is *not* returned — it is dereferenced on demand via
    /// [`RefactorCapability::materialize_plan`].
    async fn plan_refactor(
        &self,
        request: RefactorRequest,
    ) -> CapabilityResult<RefactorPlanHandle>;

    /// Materialize a previously produced plan by its handle id. Errors if the
    /// id is unknown (e.g. evicted, or never produced on this host).
    async fn materialize_plan(&self, id: String) -> CapabilityResult<serde_json::Value>;
}
