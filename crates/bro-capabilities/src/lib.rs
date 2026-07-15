//! Capability traits supplied to in-process bro sessions.
//!
//! The traits live below both implementers: the daemon can implement them, the
//! harness can consume them, and absence remains a fail-closed runtime choice.

use async_trait::async_trait;
use bro_core::{AtomRef, AttemptId, BroError};
use serde::{Deserialize, Serialize};

mod execution;

pub use execution::{
    AttemptOutcome, AttemptState, ExecutionAccepted, ExecutionCodeMode, ExecutionDirective,
    ExecutionDirectiveCadence, ExecutionDispatchContext, ExecutionKind, ExecutionRequest,
    ExecutionScope, ExecutionServiceTier, ExecutionToolPolicy, WorkingSetIntent,
};

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

#[async_trait]
pub trait RefactorCapability: Send + Sync {
    /// Produce a dry-run plan, store it host-side, and return a handle. The
    /// full plan JSON is *not* returned — it is dereferenced on demand via
    /// [`RefactorCapability::materialize_plan`].
    async fn plan_refactor(&self, request: RefactorRequest)
    -> CapabilityResult<RefactorPlanHandle>;

    /// Materialize a previously produced plan by its handle id. Errors if the
    /// id is unknown (e.g. evicted, or never produced on this host).
    async fn materialize_plan(&self, id: String) -> CapabilityResult<serde_json::Value>;
}

/// Operational intent requests a concrete attempt only through this narrow,
/// idempotent contract. Implementations may be an in-memory adapter during
/// migration or a fleet RPC client in production.
#[async_trait]
pub trait ExecutionCapability: Send + Sync {
    async fn request_execution(
        &self,
        request: ExecutionRequest,
    ) -> CapabilityResult<ExecutionAccepted>;

    async fn attempt_outcome(&self, attempt_id: AttemptId) -> CapabilityResult<AttemptOutcome>;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bro_core::{AttemptId, OperationId, Provider, SessionId, TaskId};
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeExecutionService {
        accepted: Mutex<HashMap<String, (ExecutionRequest, ExecutionAccepted)>>,
    }

    #[async_trait]
    impl ExecutionCapability for FakeExecutionService {
        async fn request_execution(
            &self,
            request: ExecutionRequest,
        ) -> CapabilityResult<ExecutionAccepted> {
            let mut accepted = self.accepted.lock().await;
            if let Some((prior_request, prior)) = accepted.get(&request.idempotency_key) {
                if prior_request != &request {
                    return Err(BroError::new(
                        "execution.idempotency_conflict",
                        "idempotency key was already used for a different execution request",
                    ));
                }
                let mut prior = prior.clone();
                prior.deduplicated = true;
                return Ok(prior);
            }
            let ordinal = accepted.len() + 1;
            let result = ExecutionAccepted {
                operation_id: request.operation_id.clone(),
                attempt_id: AttemptId::new(format!("attempt-{ordinal}")),
                task_id: TaskId::new(format!("task-{ordinal}")),
                session_id: SessionId::new(format!("session-{ordinal}")),
                deduplicated: false,
            };
            accepted.insert(request.idempotency_key.clone(), (request, result.clone()));
            Ok(result)
        }

        async fn attempt_outcome(&self, attempt_id: AttemptId) -> CapabilityResult<AttemptOutcome> {
            Ok(AttemptOutcome {
                attempt_id,
                state: AttemptState::Accepted,
                result: serde_json::Value::Null,
            })
        }
    }

    fn execution_request() -> ExecutionRequest {
        ExecutionRequest {
            operation_id: OperationId::new("operation-1"),
            idempotency_key: "stable-key".to_string(),
            kind: ExecutionKind::Fresh {
                prompt: "probe".to_string(),
            },
            provider: Provider::Glm,
            model: "glm-test".to_string(),
            effort: Some("high".to_string()),
            service_tier: ExecutionServiceTier::Priority,
            code_mode: Some(ExecutionCodeMode::Only),
            dispatch_context: Some(ExecutionDispatchContext {
                persona: Some("executor".to_string()),
                ..ExecutionDispatchContext::default()
            }),
            working_set: WorkingSetIntent::Scratch,
            shell_env: Default::default(),
            tool_policy: ExecutionToolPolicy::default(),
            system_prompt: None,
            output_schema: None,
            labels: Default::default(),
        }
    }

    #[tokio::test]
    async fn execution_contract_deduplicates_by_idempotency_key() {
        let service = FakeExecutionService::default();
        let request = execution_request();
        let first = service.request_execution(request.clone()).await.unwrap();
        let second = service.request_execution(request).await.unwrap();
        assert!(!first.deduplicated);
        assert!(second.deduplicated);
        assert_eq!(first.attempt_id, second.attempt_id);
        assert_eq!(service.accepted.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn concurrent_duplicate_execution_requests_create_one_attempt() {
        let service = FakeExecutionService::default();
        let request = execution_request();
        let (left, right) = tokio::join!(
            service.request_execution(request.clone()),
            service.request_execution(request)
        );
        let left = left.unwrap();
        let right = right.unwrap();
        assert_eq!(left.attempt_id, right.attempt_id);
        assert_ne!(left.deduplicated, right.deduplicated);
        assert_eq!(service.accepted.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn reused_key_with_different_payload_is_a_typed_conflict() {
        let service = FakeExecutionService::default();
        let request = execution_request();
        service.request_execution(request.clone()).await.unwrap();
        let mut conflict = request;
        conflict.kind = ExecutionKind::Resume {
            session_id: SessionId::new("other-session"),
            prompt: "different work".to_string(),
        };
        let error = service.request_execution(conflict).await.unwrap_err();
        assert_eq!(error.code, "execution.idempotency_conflict");
        assert_eq!(service.accepted.lock().await.len(), 1);
    }
}
