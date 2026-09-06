//! Generic consultant MCP surface (dissolution Phase 3).
//!
//! Consumer-agnostic entry points for the workflow-facing proposal
//! lifecycle: list, one-shot apply, and the split begin/complete apply
//! pair. Each tool resolves `consumer` through the code-owned registry
//! (`orchestration::consultant::consumers`) and enforces that consumer's id
//! prefix before delegating. The `badgey_*` proposal tools are the pinned
//! shims for `consumer="badgey"` and share the same bounded proposal projection.
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

/// Shared transport projection for the generic and Badgey-pinned proposal reads.
pub(super) fn proposal_response_page(
    store: &crate::orchestration::consultant::proposals::ProposalStore,
    instance: &ConsultantId,
    options: &crate::orchestration::consultant::proposals::ProposalReadOptions,
    mut envelope: Value,
    detail: Option<&str>,
    cursor: Option<&str>,
    body_limit: Option<usize>,
) -> anyhow::Result<Value> {
    let full = crate::tools::body_page::validate_detail(detail, cursor, body_limit)?;
    if full {
        let record = store.exact_response_row(instance, options)?;
        let proposal_id = options.proposal_id.as_deref().unwrap();
        let scope = format!(
            "proposal:{}:{proposal_id}:events={}",
            instance.as_str(),
            options.include_events
        );
        envelope["proposal_id"] = json!(proposal_id);
        envelope["body"] =
            crate::tools::body_page::json_body_page(&scope, &record, cursor, body_limit)?;
        return Ok(envelope);
    }
    store.response_page(instance, options, envelope).map_err(|error| {
        if options.proposal_id.is_some() && error.to_string().contains("error.collection_row_too_large") {
            anyhow::anyhow!("error.proposal_body_requires_paging: exact proposal exceeds the response budget; retry the same proposal_id with detail=full, concatenate body.text pages, and continue with body.next_cursor as cursor")
        } else { error }
    })
}

#[tool_router(router = consultant_tools)]
impl BlackboxServer {
    #[tool(
        name = "consultant_proposals_list",
        description = "List consultant proposal summaries by numeric id (default 20, maximum 100). Continue with next_after as after and the returned through bound, keeping since/only_pending unchanged. No drafts or history in list pages. proposal_id reads one exact draft; include_events=true adds transition history. Exact reads cannot combine since/only_pending/limit/after/through. detail=full with proposal_id returns lossless JSON body pages; continue body.next_cursor as cursor (body_limit up to 4096). Ordinary small exact reads retain proposals[0]."
    )]
    pub(crate) async fn consultant_proposals_list(
        &self,
        Parameters(p): Parameters<ConsultantProposalsListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("consultant_proposals_list", move || {
            let id = resolve(&p.consumer, &p.consultant_id).map_err(|e| anyhow::anyhow!(e))?;
            let page = proposal_response_page(
                &server.state.consultant_proposals,
                &id,
                &p.read_options(),
                json!({"consumer": p.consumer, "consultant_id": p.consultant_id}),
                p.detail.as_deref(),
                p.cursor.as_deref(),
                p.body_limit,
            )?;
            Ok(serde_json::to_string_pretty(&page)?)
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
    async fn exact_proposal_body_pages_reconstruct_unicode_and_reject_stale_or_foreign_cursors() {
        use crate::orchestration::consultant::types::ProposalState;
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: ConsultantId = "bg-0123abcd-4567ef89".parse().unwrap();
        let other: ConsultantId = "bg-11111111-22222222".parse().unwrap();
        let draft = json!({"headline":"large proposal", "evidence":"\u{0001}界\n".repeat(8000)});
        server
            .state
            .consultant_proposals
            .create(&id, "packet", draft.clone(), None)
            .unwrap();
        server
            .state
            .consultant_proposals
            .create(&other, "packet", draft.clone(), None)
            .unwrap();
        let basic = json!({"consumer":"badgey", "consultant_id":id.as_str(), "proposal_id":"P-1"});
        let result = server
            .consultant_proposals_list(Parameters(serde_json::from_value(basic.clone()).unwrap()))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(extract_text(&result).contains("proposal_body_requires_paging"));
        let mut args = basic;
        args["detail"] = json!("full");
        let mut reconstructed = String::new();
        let mut first_cursor = None;
        loop {
            let result = server
                .consultant_proposals_list(Parameters(
                    serde_json::from_value(args.clone()).unwrap(),
                ))
                .await;
            assert_ne!(result.is_error, Some(true), "{result:?}");
            let page: Value = serde_json::from_str(&extract_text(&result)).unwrap();
            assert!(serde_json::to_vec(&page["body"]).unwrap().len() <= 4096);
            reconstructed.push_str(page["body"]["text"].as_str().unwrap());
            let Some(cursor) = page["body"]["next_cursor"].as_str() else {
                break;
            };
            if first_cursor.is_none() {
                first_cursor = Some(cursor.to_owned());
            }
            args["cursor"] = json!(cursor);
        }
        let record: Value = serde_json::from_str(&reconstructed).unwrap();
        assert_eq!(record["draft"], draft);
        args["cursor"] = json!(first_cursor.clone().unwrap());
        args["consultant_id"] = json!(other.as_str());
        let foreign = server
            .consultant_proposals_list(Parameters(serde_json::from_value(args.clone()).unwrap()))
            .await;
        assert_eq!(foreign.is_error, Some(true));
        args["consultant_id"] = json!(id.as_str());
        server
            .state
            .consultant_proposals
            .transition(
                &id,
                "P-1",
                ProposalState::Pending,
                ProposalState::Failed,
                None,
            )
            .unwrap();
        let stale = server
            .consultant_proposals_list(Parameters(serde_json::from_value(args).unwrap()))
            .await;
        assert_eq!(stale.is_error, Some(true));
        assert!(extract_text(&stale).contains("restart without cursor"));
    }

    #[tokio::test]
    async fn shipped_proposal_workflows_follow_pages_and_expand_each_draft_without_side_effects() {
        use crate::workflow::context::{ArcContext, resolve_arg_value};
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: ConsultantId = "bg-0123abcd-4567ef89".parse().unwrap();
        for n in 1..=25 {
            server
                .state
                .consultant_proposals
                .create(
                    &id,
                    "packet",
                    json!({"headline": format!("proposal {n}"), "body": format!("draft {n}")}),
                    None,
                )
                .unwrap();
        }
        let parent: Value = serde_json::from_str(include_str!(
            "../../../system-defaults/badgey/workflows/badgey-triage-channel-arc.json"
        ))
        .unwrap();
        let child: Value = serde_json::from_str(include_str!(
            "../../../system-defaults/badgey/workflows/badgey-slack-emit-proposal-arc.json"
        ))
        .unwrap();
        crate::workflow::compile(serde_json::from_value(parent.clone()).unwrap()).unwrap();
        crate::workflow::compile(serde_json::from_value(child.clone()).unwrap()).unwrap();
        let packet: crate::packets::CompileParams = serde_json::from_str(include_str!(
            "../../../system-defaults/badgey/packets/hook-route-proposal-page.json"
        ))
        .unwrap();
        server.state.packets.read().compile(&packet).unwrap();
        let mut context = ArcContext::default();
        context.vars.insert("badgey_id".into(), json!(id.as_str()));
        context.meta.started_at = "2020-01-01T00:00:00Z".into();
        let mut node = "ListProposals".to_owned();
        let mut read_ids = Vec::new();
        let mut pages = 0;
        loop {
            assert!(pages < 3, "shipped gate loop failed to terminate");
            let arguments = &parent["nodes"][&node]["on_enter"][0]["args"]["arguments"];
            let params =
                serde_json::from_value(resolve_arg_value(&context, arguments).unwrap()).unwrap();
            let response = server.consultant_proposals_list(Parameters(params)).await;
            assert_ne!(response.is_error, Some(true), "{response:?}");
            let page: Value = serde_json::from_str(&extract_text(&response)).unwrap();
            context
                .vars
                .insert("proposals_response".into(), page.clone());
            for summary in page["proposals"].as_array().unwrap() {
                assert!(summary.get("draft").is_none());
                let mut child_context = context.clone();
                child_context
                    .vars
                    .insert("proposal".into(), summary.clone());
                let arguments = &child["nodes"]["ReadProposal"]["on_enter"][0]["args"]["arguments"];
                let params =
                    serde_json::from_value(resolve_arg_value(&child_context, arguments).unwrap())
                        .unwrap();
                let response = server.consultant_proposals_list(Parameters(params)).await;
                assert_ne!(response.is_error, Some(true), "{response:?}");
                let exact: Value = serde_json::from_str(&extract_text(&response)).unwrap();
                child_context.vars.insert("proposal_response".into(), exact);
                let expanded = resolve_arg_value(
                    &child_context,
                    &child["nodes"]["ReadProposal"]["on_enter"][1]["args"]["value"],
                )
                .unwrap();
                assert!(expanded["draft"]["body"].is_string());
                assert_eq!(expanded["id"], summary["id"]);
                read_ids.push(expanded["id"].as_str().unwrap().to_owned());
            }
            pages += 1;
            let gate = parent["nodes"]["ForeachPostProposal"]["gate"]
                .as_str()
                .unwrap();
            let verdict = server
                .apply_workflow_gate_entity(
                    gate,
                    &context.flatten_for_gate("ForeachPostProposal"),
                    "ForeachPostProposal",
                )
                .unwrap()
                .unwrap();
            node = parent["nodes"]["ForeachPostProposal"]["next"]["cases"][verdict]
                .as_str()
                .unwrap()
                .to_owned();
            if node == "Done" {
                break;
            }
        }
        assert_eq!(pages, 2);
        assert_eq!(
            read_ids,
            (1..=25).map(|n| format!("P-{n}")).collect::<Vec<_>>()
        );
        assert_eq!(
            parent["nodes"]["ForeachPostProposal"]["foreach"]["on_item_failure"],
            "collect_then_halt"
        );
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
                detail: None,
                cursor: None,
                body_limit: None,
                limit: None,
                after: None,
                through: None,
                proposal_id: None,
                include_events: false,
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
