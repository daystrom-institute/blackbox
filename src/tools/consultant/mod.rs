//! Generic consultant MCP surface (dissolution Phase 3).
//!
//! Consumer-agnostic entry points for the workflow-facing proposal
//! lifecycle: list, one-shot apply, and the split begin/complete apply
//! pair. Each tool resolves `consumer` through the code-owned registry
//! (`orchestration::consultant::consumers`) and enforces that consumer's id
//! prefix before delegating. The `badgey_*` proposal tools are the pinned
//! shims for `consumer="badgey"` and keep their wire format unchanged.
//!
//! The conversational lifecycle (exec/resume/dismiss) stays consumer-prefixed
//! until the turn-loop runtime moves out of `tools/badgey` — see
//! design/orchestration/agents/consultant-runtime.md §5 Phase 3.

use crate::orchestration::consultant::consumers;
use crate::orchestration::consultant::descriptor::ConsumerDescriptor;
use crate::orchestration::consultant::types::ConsultantId;
use crate::server::state::BlackboxServer;
use crate::tools::bro_params::{
    ConsultantApplyProposalParams, ConsultantProposalBeginApplyParams,
    ConsultantProposalCompleteApplyParams, ConsultantProposalsListParams,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::{Value, json};

mod lifecycle;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::consultant_tools()
}

fn resolve(consumer: &str, consultant_id: &str) -> Result<ConsultantId, String> {
    let descriptor: &ConsumerDescriptor = consumers::lookup(consumer).ok_or_else(|| {
        format!(
            "error.bad_input(code=unknown_consumer): no consultant consumer '{consumer}' (known: {})",
            consumers::names().join(", ")
        )
    })?;
    descriptor
        .parse_id(consultant_id)
        .map_err(|e| format!("error.bad_input(code=invalid_consultant_id): {e}"))
}

#[tool_router(router = consultant_tools)]
impl BlackboxServer {
    #[tool(
        name = "consultant_proposals_list",
        description = "List proposal records owned by a consultant instance of any registered consumer. Returns full proposal objects (id, kind, state, draft, created_at, updated_at, events, applied_task_id) sorted by proposal_id number. Optional `since` filter (ISO timestamp) restricts to proposals created at or after that moment. Consumer-agnostic equivalent of `badgey_proposals_list`."
    )]
    pub(crate) async fn consultant_proposals_list(
        &self,
        Parameters(p): Parameters<ConsultantProposalsListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("consultant_proposals_list", move || {
            let id = resolve(&p.consumer, &p.consultant_id).map_err(|e| anyhow::anyhow!(e))?;
            let proposals = server
                .state
                .consultant_proposals
                .list_by_instance(&id)
                .map_err(|e| anyhow::anyhow!("listing proposals: {e}"))?;
            let filtered: Vec<_> = proposals
                .into_iter()
                .filter(|proposal| {
                    p.since
                        .as_deref()
                        .is_none_or(|since| proposal.created_at.as_str() >= since)
                })
                .filter(|proposal| p.only_pending != Some(true) || !proposal.is_terminal())
                .collect();
            Ok(serde_json::to_string_pretty(&json!({
                "consumer": p.consumer,
                "consultant_id": p.consultant_id,
                "since": p.since,
                "count": filtered.len(),
                "proposals": filtered,
            }))?)
        })
        .await
    }

    #[tool(
        name = "consultant_apply_proposal",
        description = "Apply a stored consultant proposal for any registered consumer — state-machine transition (Pending/Failed → Applying), kind-specific dispatch (artifact kinds → bbox_artifact_install; redispatch_task → privileged task spawn), record applied_task_id, transition (Applying → Applied | Failed). One-shot wrapper; workflow callers that want the engine to track the dispatched work natively should use the split `consultant_proposal_begin_apply` + `consultant_proposal_complete_apply` pair. Consumer-agnostic equivalent of `badgey_apply_proposal`."
    )]
    pub(crate) async fn consultant_apply_proposal(
        &self,
        Parameters(p): Parameters<ConsultantApplyProposalParams>,
    ) -> CallToolResult {
        // Same contract as badgey_apply_proposal: always Ok with explicit
        // `status` (applied | already_applied | failed | bad_input) and a
        // one-line `summary` so workflow templates can interpolate blindly.
        let id = match resolve(&p.consumer, &p.consultant_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "status": "bad_input",
                    "error": e.clone(),
                    "summary": e,
                    "consumer": p.consumer,
                    "consultant_id": p.consultant_id,
                }));
            }
        };
        let result = self
            .badgey_apply_proposal_internal(&id, &p.proposal_id, p.retry_failed.unwrap_or(false))
            .await;
        match result {
            Ok(mut value) => {
                let already = value
                    .get("already_applied")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let status = if already {
                    "already_applied"
                } else {
                    "applied"
                };
                let summary = if already {
                    let prior = value
                        .get("prior_task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if prior.is_empty() {
                        "already applied".to_string()
                    } else {
                        format!("already applied (prior task `{prior}`)")
                    }
                } else if let Some(task_id) = value.get("task_id").and_then(Value::as_str) {
                    format!("dispatched task `{task_id}`")
                } else if let Some(artifact_ref) = value.get("artifact_ref").and_then(Value::as_str)
                {
                    format!("installed `{artifact_ref}`")
                } else {
                    "applied".to_string()
                };
                if let Some(obj) = value.as_object_mut() {
                    obj.entry("status".to_string())
                        .or_insert_with(|| Value::String(status.into()));
                    obj.insert("summary".into(), Value::String(summary));
                    obj.insert("consumer".into(), Value::String(p.consumer.clone()));
                }
                Self::ok_json(&value)
            }
            Err(e) => Self::ok_json(&json!({
                "status": "failed",
                "error": e.clone(),
                "summary": e,
                "consumer": p.consumer,
                "consultant_id": p.consultant_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }

    #[tool(
        name = "consultant_proposal_begin_apply",
        description = "Phase 1 of the consumer-agnostic split apply path. Transitions a proposal Pending|Failed → Applying and returns dispatch parameters (prompt + brofile + label for redispatch_task; artifact_kind + source + version for artifact installs). Does NOT spawn the bro or install the artifact — the workflow caller does that via an actor node or `bbox_artifact_install` mcp_call, then calls `consultant_proposal_complete_apply` with the outcome. Consumer-agnostic equivalent of `badgey_proposal_begin_apply`."
    )]
    pub(crate) async fn consultant_proposal_begin_apply(
        &self,
        Parameters(p): Parameters<ConsultantProposalBeginApplyParams>,
    ) -> CallToolResult {
        let id = match resolve(&p.consumer, &p.consultant_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "outcome": "rejected",
                    "reason": "bad_input",
                    "error": e.clone(),
                    "consumer": p.consumer,
                    "consultant_id": p.consultant_id,
                }));
            }
        };
        match self
            .badgey_proposal_begin_apply_internal(
                &id,
                &p.proposal_id,
                p.retry_failed.unwrap_or(false),
            )
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(e) => Self::ok_json(&json!({
                "outcome": "rejected",
                "reason": "internal_error",
                "error": e.clone(),
                "consumer": p.consumer,
                "consultant_id": p.consultant_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }

    #[tool(
        name = "consultant_proposal_complete_apply",
        description = "Phase 2 of the consumer-agnostic split apply path. Given the outcome of the dispatched work (`completed` / `failed` / `cancelled` / `timed_out`), transitions the proposal Applying → Applied or Applying → Failed and writes the audit decision. Always returns `{status: applied|failed, ...}` so the workflow's outcome node can read the final state. Consumer-agnostic equivalent of `badgey_proposal_complete_apply`."
    )]
    pub(crate) async fn consultant_proposal_complete_apply(
        &self,
        Parameters(p): Parameters<ConsultantProposalCompleteApplyParams>,
    ) -> CallToolResult {
        let id = match resolve(&p.consumer, &p.consultant_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "status": "failed",
                    "error": e.clone(),
                    "consumer": p.consumer,
                    "consultant_id": p.consultant_id,
                }));
            }
        };
        match self
            .badgey_proposal_complete_apply_internal(
                &id,
                &p.proposal_id,
                &p.outcome,
                p.task_id.as_deref(),
                p.artifact_ref.as_deref(),
                p.summary.as_deref(),
            )
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(e) => Self::ok_json(&json!({
                "status": "failed",
                "error": e.clone(),
                "consumer": p.consumer,
                "consultant_id": p.consultant_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::state::SharedState;
    use std::sync::Arc;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn unknown_consumer_is_rejected() {
        let err = resolve("nonexistent", "bg-3f7a91c4-91ff04cc").unwrap_err();
        assert!(err.contains("unknown_consumer"), "{err}");
        assert!(err.contains("badgey"), "should list known consumers: {err}");
    }

    #[test]
    fn wrong_prefix_for_consumer_is_rejected() {
        let err = resolve("badgey", "xx-3f7a91c4-91ff04cc").unwrap_err();
        assert!(err.contains("invalid_consultant_id"), "{err}");
    }

    #[tokio::test]
    async fn apply_with_unknown_consumer_returns_bad_input_status() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let result = server
            .consultant_apply_proposal(Parameters(ConsultantApplyProposalParams {
                consumer: "nonexistent".to_string(),
                consultant_id: "bg-3f7a91c4-91ff04cc".to_string(),
                proposal_id: "P-1".to_string(),
                retry_failed: None,
            }))
            .await;
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["status"], "bad_input");
        assert_eq!(body["consumer"], "nonexistent");
    }

    #[tokio::test]
    async fn proposals_list_reads_badgey_consumer_store() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: ConsultantId = "bg-0123abcd-4567ef89".parse().unwrap();
        server
            .state
            .consultant_proposals
            .create(&id, "packet", serde_json::json!({"v": 1}), None)
            .unwrap();
        let result = server
            .consultant_proposals_list(Parameters(ConsultantProposalsListParams {
                consumer: "badgey".to_string(),
                consultant_id: id.as_str().to_string(),
                since: None,
                only_pending: Some(true),
            }))
            .await;
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["count"], 1);
        assert_eq!(body["proposals"][0]["kind"], "packet");
        assert_eq!(body["consumer"], "badgey");
    }
}
