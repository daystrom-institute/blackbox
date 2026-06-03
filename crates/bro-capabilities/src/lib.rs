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
