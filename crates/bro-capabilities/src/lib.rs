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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefactorPlanHandle {
    pub id: String,
}

#[async_trait]
pub trait RefactorCapability: Send + Sync {
    async fn plan_refactor(&self, request: RefactorRequest)
    -> CapabilityResult<RefactorPlanHandle>;
}
