#![allow(clippy::too_many_arguments)]

mod lifecycle;
mod proposals;
mod reports;
use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::badgey_tools()
}

#[tool_router(router = badgey_tools)]
impl BlackboxServer {
    #[tool(
        name = "badgey_exec",
        description = "Start a Badgey consultant instance for a project scope and return its badgey_id, provider session, task, and thread-of-record ids."
    )]
    pub(crate) async fn badgey_exec(
        &self,
        Parameters(p): Parameters<BadgeyExecParams>,
    ) -> CallToolResult {
        match self
            .badgey_exec_internal(p.project_dir, p.brief, Some("agent:badgey@v1".to_string()))
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_resume",
        description = "Send a turn to an existing Badgey instance. Mechanical commands such as `dismiss` are handled by the wrapper before provider resume."
    )]
    pub(crate) async fn badgey_resume(
        &self,
        Parameters(p): Parameters<BadgeyResumeParams>,
    ) -> CallToolResult {
        match self
            .badgey_resume_internal(&p.badgey_id, &p.prompt, p.timeout_seconds)
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_ask",
        description = "Question-shaped alias for badgey_resume."
    )]
    pub(crate) async fn badgey_ask(
        &self,
        Parameters(p): Parameters<BadgeyAskParams>,
    ) -> CallToolResult {
        match self
            .badgey_resume_internal(&p.badgey_id, &p.question, p.timeout_seconds)
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_dismiss",
        description = "Dismiss a Badgey instance, drain queued turns, write a dismiss event, and resolve its thread of record."
    )]
    pub(crate) fn badgey_dismiss(
        &self,
        Parameters(p): Parameters<BadgeyDismissParams>,
    ) -> CallToolResult {
        match self.badgey_dismiss_internal(&p.badgey_id, p.reason) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_status",
        description = "Inspect one Badgey instance, including queue status and proposals; without badgey_id, returns active instances."
    )]
    pub(crate) fn badgey_status(
        &self,
        Parameters(p): Parameters<BadgeyStatusParams>,
    ) -> CallToolResult {
        match self.badgey_status_internal(p.badgey_id.as_deref()) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_list",
        description = "List Badgey instances and their thread/session bindings."
    )]
    pub(crate) fn badgey_list(
        &self,
        Parameters(p): Parameters<BadgeyListParams>,
    ) -> CallToolResult {
        match self.badgey_list_internal(p.include_dismissed.unwrap_or(false)) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_scout",
        description = "Ask Badgey to author scout sub-charters for a focused question; wrapper post-processing dispatches emitted scout actions."
    )]
    pub(crate) async fn badgey_scout(
        &self,
        Parameters(p): Parameters<BadgeyScoutParams>,
    ) -> CallToolResult {
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(id) => id,
            Err(err) => return Self::err_text(&err),
        };
        let instance = match self.state.badgey_registry.get(&id) {
            Ok(instance) => instance,
            Err(err) => return Self::err_text(&err.to_string()),
        };
        let scout_id = format!("scout-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        if let Err(err) = self.badgey_write_event(
            &instance,
            orchestration::badgey::events::ThreadEvent::ScoutDispatched {
                scout_id: scout_id.clone(),
                scout_thread_id: instance.thread_of_record_id.clone(),
                charters: vec![p.charter.clone()],
            },
            None,
        ) {
            return Self::err_text(&err);
        }
        let prompt = format!(
            "Scout mode. Use scout_id={scout_id}. Author wrapper-mediated sub-bro charters for this question and emit bg-action-spawn-subbro notes with this scout_id as needed.\n\nCharter: {}",
            p.charter
        );
        match self
            .badgey_resume_internal(&p.badgey_id, &prompt, p.timeout_seconds)
            .await
        {
            Ok(mut value) => {
                value["scout_id"] = Value::String(scout_id);
                value["scout_thread_id"] = Value::String(instance.thread_of_record_id);
                Self::ok_json(&value)
            }
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_collect",
        description = "Collect scout/sub-bro events for a Badgey instance or scout id."
    )]
    pub(crate) fn badgey_collect(
        &self,
        Parameters(p): Parameters<BadgeyCollectParams>,
    ) -> CallToolResult {
        match self.badgey_collect_internal(p.scout_id.as_deref(), p.badgey_id.as_deref()) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_triage_inbox",
        description = "Produce a Badgey-shaped inbox triage proposal sheet for stale/open work in a scope."
    )]
    pub(crate) fn badgey_triage_inbox(
        &self,
        Parameters(p): Parameters<BadgeyTriageInboxParams>,
    ) -> CallToolResult {
        match self.badgey_triage_inbox_internal(p.scope, p.since, p.badgey_id) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_close_loops",
        description = "Classify dispatched tasks without done notes; never synthesizes executor done notes."
    )]
    pub(crate) fn badgey_close_loops(
        &self,
        Parameters(p): Parameters<BadgeyCloseLoopsParams>,
    ) -> CallToolResult {
        match self.badgey_close_loops_internal(p.window_days, p.project_dir) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_proposals_list",
        description = "List BadgeyProposal records owned by an instance. Returns full proposal objects (id, kind, state, draft, created_at, updated_at, events, applied_task_id) sorted by proposal_id number. Optional `since` filter (ISO timestamp) restricts to proposals created at or after that moment — useful for reading proposals emitted by the most recent Badgey turn. Used by the per-channel triage workflow's ForeachPostProposal node to iterate proposals freshly emitted by the synthesis turn."
    )]
    pub(crate) fn badgey_proposals_list(
        &self,
        Parameters(p): Parameters<BadgeyProposalsListParams>,
    ) -> CallToolResult {
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(parsed) => parsed,
            Err(e) => return Self::err_text(&e),
        };
        let proposals = match self.state.badgey_proposals.list_by_instance(&id) {
            Ok(v) => v,
            Err(e) => return Self::err_text(&format!("listing proposals: {e}")),
        };
        let filtered: Vec<_> = proposals
            .into_iter()
            .filter(|proposal| {
                p.since
                    .as_deref()
                    .is_none_or(|since| proposal.created_at.as_str() >= since)
            })
            .filter(|proposal| p.only_pending != Some(true) || !proposal.is_terminal())
            .collect();
        Self::ok_json(&json!({
            "badgey_id": p.badgey_id,
            "since": p.since,
            "count": filtered.len(),
            "proposals": filtered,
        }))
    }

    #[tool(
        name = "badgey_ensure_for_channel",
        description = "Get-or-create the system Badgey instance that authors triage briefs for a Slack-bound project. Reads the (team_id, channel_id) binding to resolve the project scope, looks up the binding's badgey_id; if absent or the instance has been dismissed, exec a fresh Badgey instance, persist its id back on the binding, and return it. Used by the per-channel triage workflow's EnsureInstance node."
    )]
    pub(crate) async fn badgey_ensure_for_channel(
        &self,
        Parameters(p): Parameters<EnsureBadgeyForChannelParams>,
    ) -> CallToolResult {
        if p.team_id.trim().is_empty() {
            return Self::err_text("team_id is required");
        }
        if p.channel_id.trim().is_empty() {
            return Self::err_text("channel_id is required");
        }
        let binding = match self
            .state
            .slack_channel_bindings
            .lookup(&p.team_id, &p.channel_id)
        {
            Some(b) => b,
            None => {
                return Self::err_text(&format!(
                    "no binding for team={} channel={} — run bro_slack_bind first",
                    p.team_id, p.channel_id
                ));
            }
        };
        let scope = p
            .scope_override
            .clone()
            .unwrap_or_else(|| binding.project_dir.clone());

        // Resume existing instance when present + still active.
        if let Some(ref bid) = binding.badgey_id {
            if let Ok(parsed) = bid.parse::<orchestration::badgey::types::BadgeyId>() {
                match self.state.badgey_registry.get(&parsed) {
                    Ok(instance) => {
                        return Self::ok_json(&json!({
                            "badgey_id": bid,
                            "thread_id": instance.thread_of_record_id,
                            "project_id": instance.scope.project_id,
                            "session_id": instance.provider_session_id,
                            "created": false,
                        }));
                    }
                    Err(e) => {
                        tracing::info!(
                            badgey_id = %bid,
                            "ensure_badgey_for_channel: stored badgey unusable ({e}) — creating fresh"
                        );
                    }
                }
            }
        }

        // Create a new instance and persist its id back on the binding.
        let initial_brief = format!(
            "Slack daily-brief triage agent for #{} (project: {}). \
             Operate in triage + corpus-mining mode: classify stale work-items, \
             score graph-edge meatiness, dispatch focused scouts when warranted, \
             and synthesize structured proposals for review.",
            binding.channel_name.as_deref().unwrap_or(&p.channel_id),
            scope,
        );
        let exec_result = match self
            .badgey_exec_internal(
                Some(scope.clone()),
                Some(initial_brief),
                Some(format!("badgey-slack-{}", p.channel_id)),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return Self::err_text(&format!("badgey_exec failed: {e}")),
        };
        let new_badgey_id = match exec_result.get("badgey_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Self::err_text("badgey_exec didn't return a badgey_id"),
        };
        if let Err(e) = self.state.slack_channel_bindings.set_badgey_id(
            &p.team_id,
            &p.channel_id,
            Some(new_badgey_id.clone()),
        ) {
            tracing::warn!(
                badgey_id = %new_badgey_id,
                "ensure_badgey_for_channel: persisting badgey_id on binding failed: {e}"
            );
        }
        Self::ok_json(&json!({
            "badgey_id": new_badgey_id,
            "thread_id": exec_result.get("thread_id"),
            "project_id": exec_result.get("project_id"),
            "session_id": exec_result.get("session_id"),
            "task_id": exec_result.get("task_id"),
            "created": true,
        }))
    }

    #[tool(
        name = "badgey_apply_proposal",
        description = "Apply a stored BadgeyProposal — drives the wrapper's full apply path: state-machine transition (Pending/Failed → Applying), kind-specific dispatch (artifact_promotion → bbox_artifact_install; redispatch_task → spawn_privileged_task with the proposal's prompt; workflow_install/agent_install/packet_install → matching artifact install), record applied_task_id, transition (Applying → Applied | Failed). Returns the apply result with status. One-shot wrapper — for the Slack-reaction flow prefer the split `badgey_proposal_begin_apply` + `badgey_proposal_complete_apply` pair so the workflow engine tracks the dispatched bro natively as an actor node."
    )]
    pub(crate) async fn badgey_apply_proposal(
        &self,
        Parameters(p): Parameters<BadgeyApplyProposalParams>,
    ) -> CallToolResult {
        // Always return Ok with explicit `status` + `summary` fields.
        //
        // status is one of:
        //   "applied"         — fresh apply succeeded
        //   "already_applied" — proposal was already in Applied state
        //   "failed"          — apply path raised
        //   "bad_input"       — badgey_id couldn't parse
        //
        // summary is a one-line human-readable description that the
        // Slack-emit summary template can interpolate without
        // worrying about which fields are present per kind/outcome:
        //   applied (RedispatchTask):  "dispatched task `<task_id>`"
        //   applied (artifact_*):      "installed `<artifact_ref>`"
        //   already_applied:           "already applied (prior task `<id>`)"
        //   failed / bad_input:        "<error>"
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "status": "bad_input",
                    "error": e.clone(),
                    "summary": e,
                    "badgey_id": p.badgey_id,
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
                }
                Self::ok_json(&value)
            }
            Err(e) => Self::ok_json(&json!({
                "status": "failed",
                "error": e.clone(),
                "summary": e,
                "badgey_id": p.badgey_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }

    #[tool(
        name = "badgey_proposal_begin_apply",
        description = "Phase 1 of the split apply path. Transitions a proposal Pending|Failed → Applying and returns dispatch parameters (prompt + brofile + label for redispatch_task; artifact_kind + source + version for artifact installs). Does NOT spawn the bro or install the artifact — the workflow caller does that via an actor node or `bbox_artifact_install` mcp_call, then calls `badgey_proposal_complete_apply` with the outcome. Lets the engine track the dispatched work natively (actor task lifecycle, retries, gates) instead of opaquely spawning behind a wrapper."
    )]
    pub(crate) async fn badgey_proposal_begin_apply(
        &self,
        Parameters(p): Parameters<BadgeyProposalBeginApplyParams>,
    ) -> CallToolResult {
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "outcome": "rejected",
                    "reason": "bad_input",
                    "error": e.clone(),
                    "badgey_id": p.badgey_id,
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
                "badgey_id": p.badgey_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }

    #[tool(
        name = "badgey_proposal_complete_apply",
        description = "Phase 2 of the split apply path. Given the outcome of the dispatched work (passed in `outcome`: `completed` / `failed` / `cancelled` / `timed_out`), transitions the proposal Applying → Applied or Applying → Failed and writes the audit decision. Always returns `{status: applied|failed, ...}` so the workflow's PostOutcome node can read the final state and pick the badge."
    )]
    pub(crate) async fn badgey_proposal_complete_apply(
        &self,
        Parameters(p): Parameters<BadgeyProposalCompleteApplyParams>,
    ) -> CallToolResult {
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "status": "failed",
                    "error": e.clone(),
                    "badgey_id": p.badgey_id,
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
                "badgey_id": p.badgey_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }
}
