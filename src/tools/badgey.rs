#![allow(clippy::too_many_arguments)]

mod lifecycle;
mod proposals;
mod reports;
use crate::orchestration;
use crate::server::state::BlackboxServer;
use crate::tools::bro_params::{
    BadgeyApplyProposalParams, BadgeyAskParams, BadgeyCloseLoopsParams, BadgeyCollectParams,
    BadgeyDismissParams, BadgeyExecParams, BadgeyListParams, BadgeyProposalBeginApplyParams,
    BadgeyProposalCompleteApplyParams, BadgeyProposalsListParams, BadgeyResumeParams,
    BadgeyScoutParams, BadgeyStatusParams, BadgeyTriageInboxParams, EnsureBadgeyForChannelParams,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::{Value, json};

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

#[cfg(test)]
mod tests {
    // Async env-mutating tests hold the std `test_env_lock` across `.await` on
    // purpose (env must stay set while the awaited code reads it); #[tokio::test]
    // is single-threaded so this can't deadlock the runtime.
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use crate::orchestration::providers::Provider;
    use crate::server::state::SharedState;
    use crate::tools::badgey_adapter::{
        BadgeyAgentAdapter, recover_badgey_non_terminal_state, restore_badgey_registry_from_notes,
    };
    use crate::tools::bro_params::AgentDispatchParams;
    use crate::{artifacts, notes, threads};
    use std::sync::Arc;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn save_badgey_test_brofile(tmp: &tempfile::TempDir) {
        let _ = orchestration::brofile::save_brofile(
            &orchestration::brofile::Brofile {
                name: "badgey-persona".to_string(),
                provider: Provider::Brodex,
                account: None,
                lens: Some("Badgey test lens".to_string()),
                model: None,
                effort: None,
                filters: Some(orchestration::mcp::McpFilters {
                    allow: Vec::new(),
                    disallow: vec!["mcp__blackbox__bro_*".to_string()],
                }),
                surface: None,
                coerce_workspace: None,
                runtime: None,
                context: None,
                code_mode: None,
                service_tier: None,
            },
            "global",
            &tmp.path().join("bro"),
            None,
        );
    }

    fn fake_codex_bin(tmp: &tempfile::TempDir, session_id: &str) -> String {
        let path = tmp.path().join("fake-codex");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}'\n",
                serde_json::json!({
                    "type": "thread.started",
                    "thread_id": session_id,
                })
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path.to_string_lossy().into_owned()
    }

    async fn codex_bin_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }
    // Flaky under the full parallel suite: badgey_exec dispatches Brodex
    // in-process (no live API in tests), and whether it publishes a session id
    // before the dispatch Fails is a race that loses under load. Passes in
    // isolation. Needs a deterministic harness mock-transport (post-codexification
    // the mockable codex-CLI subprocess path is gone) — tracked as follow-up.
    #[ignore = "needs harness mock-transport for deterministic in-process dispatch"]
    #[tokio::test]
    async fn badgey_lifecycle_tools_write_thread_events() {
        let _env = crate::util::test_env_lock();
        let _env_guard = codex_bin_test_guard().await;
        let prior_bin = std::env::var("CODEX_BIN").ok();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("CODEX_BIN", fake_codex_bin(&tmp, "codex-session-test"));
        }
        save_badgey_test_brofile(&tmp);
        let server = test_server(&tmp);

        let exec = server
            .badgey_exec(Parameters(BadgeyExecParams {
                project_dir: Some(tmp.path().to_string_lossy().into_owned()),
                brief: Some("answer graph questions".to_string()),
            }))
            .await;
        assert_ne!(exec.is_error, Some(true), "{}", extract_text(&exec));
        let exec_body: Value = serde_json::from_str(&extract_text(&exec)).unwrap();
        let badgey_id = exec_body["badgey_id"].as_str().unwrap().to_string();
        let task_id = exec_body["task_id"].as_str().unwrap().to_string();
        let thread_id = exec_body["thread_id"].as_str().unwrap().to_string();
        // Brodex dispatches in-process (bro-harness), so the session id is a
        // daemon-minted uuid — not a value injected by a mocked codex CLI
        // subprocess (that path is gone post-codexification). Assert it is
        // present and non-empty, and thread the real value through the
        // event-content checks below.
        let session_id = exec_body["session_id"].as_str().unwrap().to_string();
        assert!(!session_id.is_empty(), "exec must publish a session id");
        assert!(server.state.task_store.read().get(&task_id).is_some());

        let resume = server
            .badgey_resume(Parameters(BadgeyResumeParams {
                badgey_id: badgey_id.clone(),
                prompt: "ping".to_string(),
                timeout_seconds: Some(2.0),
            }))
            .await;
        assert_ne!(resume.is_error, Some(true), "{}", extract_text(&resume));

        let dismiss = server.badgey_dismiss(Parameters(BadgeyDismissParams {
            badgey_id: badgey_id.clone(),
            reason: Some("done".to_string()),
        }));
        assert_ne!(dismiss.is_error, Some(true), "{}", extract_text(&dismiss));

        let notes = server.state.notes.read();
        let bodies: Vec<_> = notes
            .all()
            .iter()
            .filter(|note| note.thread_id.as_deref() == Some(thread_id.as_str()))
            .map(|note| note.body.as_str())
            .collect();
        // The exec event records the real (uuid) provider session id. The turn
        // event is NOT asserted: it requires a completed provider turn, which an
        // in-process Brodex dispatch can't produce in a unit test (no live API),
        // so only the deterministic lifecycle events (exec, dismiss) are checked.
        let expected_exec = format!(r#""provider_session_id":"{session_id}""#);
        assert!(
            bodies
                .iter()
                .any(|body| body.contains(r#""event":"exec""#) && body.contains(&expected_exec)),
            "exec event with the published session id must be written"
        );
        assert!(
            bodies
                .iter()
                .any(|body| body.contains(r#""event":"dismiss""#))
        );

        match prior_bin {
            Some(value) => unsafe { std::env::set_var("CODEX_BIN", value) },
            None => unsafe { std::env::remove_var("CODEX_BIN") },
        }
    }

    // Flaky under the full parallel suite for the same reason as
    // badgey_lifecycle_tools_write_thread_events: the in-process Brodex dispatch
    // Fails (no live API) and racing session-id publication loses under load.
    // Passes in isolation; needs a harness mock-transport — tracked as follow-up.
    #[ignore = "needs harness mock-transport for deterministic in-process dispatch"]
    #[tokio::test]
    async fn badgey_agent_dispatch_routes_through_wrapper_adapter() {
        let _env = crate::util::test_env_lock();
        let _env_guard = codex_bin_test_guard().await;
        let prior_bin = std::env::var("CODEX_BIN").ok();
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("CODEX_BIN", fake_codex_bin(&tmp, "codex-session-test"));
        }
        save_badgey_test_brofile(&tmp);
        let server = test_server(&tmp);
        server
            .state
            .agent_adapter_registry
            .write()
            .register(std::sync::Arc::new(BadgeyAgentAdapter {
                state: server.state.clone(),
            }));
        server
            .state
            .artifacts
            .read()
            .install_value(
                artifacts::ArtifactKind::Agent,
                "badgey.json".into(),
                &serde_json::json!({
                    "kind": "agent",
                    "name": "badgey",
                    "version": 1,
                    "manifest": {
                        "description": "Badgey test agent.",
                        "dispatch_adapter": "badgey",
                        "brofile_ref": "badgey-persona"
                    }
                }),
                None,
                None,
                None,
            )
            .unwrap();

        let result = server
            .bro_agent_dispatch(Parameters(AgentDispatchParams {
                agent: "badgey".into(),
                args: serde_json::json!({"prompt": "advise"}),
                cwd: Some(tmp.path().to_string_lossy().into_owned()),
                bro: None,
                ambient: None,
                caller_provider: None,
                caller_session_id: None,
                runtime: None,
            }))
            .await;
        assert_ne!(result.is_error, Some(true), "{}", extract_text(&result));
        let body: Value = serde_json::from_str(&extract_text(&result)).unwrap();
        // The badgey persona brofile dispatches Brodex in-process (serializes
        // "brodex"; "codex" is only a deserialize alias), and the session id is
        // a daemon-minted uuid rather than a mocked codex-CLI value.
        assert_eq!(body["session"]["provider"], "brodex");
        assert!(
            body["session"]["session_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "dispatch must publish a session id: {body}"
        );
        assert_eq!(body["resolved_brofile"], "badgey-persona");
        let disallow = body["merged_filters"]["disallow"].as_array().unwrap();
        assert!(
            disallow
                .iter()
                .any(|p| p.as_str() == Some("mcp__blackbox__bro_exec")),
            "recursive dispatch should remain denied: {disallow:?}"
        );
        assert!(
            !disallow
                .iter()
                .any(|p| p.as_str() == Some("mcp__blackbox__bro_report")),
            "bro_report should remain available for telemetry: {disallow:?}"
        );
        assert_eq!(server.state.badgey_registry.list().len(), 1);

        match prior_bin {
            Some(value) => unsafe { std::env::set_var("CODEX_BIN", value) },
            None => unsafe { std::env::remove_var("CODEX_BIN") },
        }
    }

    #[tokio::test]
    async fn badgey_post_processor_records_emit_proposal_actions_once() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
        let instance = orchestration::badgey::registry::BadgeyInstance::new(
            id.clone(),
            orchestration::badgey::types::BadgeyScope {
                project_id: tmp.path().to_string_lossy().into_owned(),
                initial_brief: None,
            },
            Provider::Brodex,
            "codex-session-3".to_string(),
            "thread-00000001".to_string(),
        );
        server
            .state
            .badgey_registry
            .register(instance.clone())
            .unwrap();
        let action_id = uuid::Uuid::new_v4().to_string();
        server
            .state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind: "followup".to_string(),
                body: serde_json::json!({
                    "event": "bg-action-emit-proposal",
                    "action_id": action_id,
                    "kind": "agent",
                    "draft": {"source": "draft-agent.json", "name": "draft-agent"}
                })
                .to_string(),
                task_id: None,
                session_id: Some("codex-session-3".to_string()),
                project: Some(tmp.path().to_string_lossy().into_owned()),
                thread_id: Some("thread-00000001".to_string()),
                provider: Some("codex".to_string()),
                bro: Some("badgey".to_string()),
            })
            .unwrap();

        let results = server
            .badgey_post_process_turn(&instance, "1970-01-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["proposal_id"], "P-1");
        let proposals = server.state.badgey_proposals.list_by_instance(&id).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(
            proposals[0].kind,
            orchestration::badgey::types::ProposalKind::Agent
        );
        assert!(matches!(
            server
                .state
                .badgey_journal
                .list_non_terminal()
                .unwrap()
                .as_slice(),
            []
        ));

        let replay = server
            .badgey_post_process_turn(&instance, "1970-01-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(
            server
                .state
                .badgey_proposals
                .list_by_instance(&id)
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn badgey_post_processor_marks_bad_actions_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
        let instance = orchestration::badgey::registry::BadgeyInstance::new(
            id.clone(),
            orchestration::badgey::types::BadgeyScope {
                project_id: tmp.path().to_string_lossy().into_owned(),
                initial_brief: None,
            },
            Provider::Brodex,
            "codex-session-4".to_string(),
            "thread-00000001".to_string(),
        );
        server
            .state
            .badgey_registry
            .register(instance.clone())
            .unwrap();
        let action_id = uuid::Uuid::new_v4().to_string();
        server
            .state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind: "followup".to_string(),
                body: serde_json::json!({
                    "event": "bg-action-emit-proposal",
                    "action_id": action_id,
                    "draft": {"source": "missing-kind.json"}
                })
                .to_string(),
                task_id: None,
                session_id: Some("codex-session-4".to_string()),
                project: Some(tmp.path().to_string_lossy().into_owned()),
                thread_id: Some("thread-00000001".to_string()),
                provider: Some("codex".to_string()),
                bro: Some("badgey".to_string()),
            })
            .unwrap();

        let results = server
            .badgey_post_process_turn(&instance, "1970-01-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(results[0]["status"], "failed");
        let failure_notes: Vec<_> = server
            .state
            .notes
            .read()
            .all()
            .iter()
            .filter(|note| note.body.contains("bg-action-failed"))
            .cloned()
            .collect();
        assert_eq!(failure_notes.len(), 1);
    }

    #[tokio::test]
    async fn badgey_apply_and_reject_commands_update_proposal_store() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
        let instance = orchestration::badgey::registry::BadgeyInstance::new(
            id.clone(),
            orchestration::badgey::types::BadgeyScope {
                project_id: tmp.path().to_string_lossy().into_owned(),
                initial_brief: None,
            },
            Provider::Brodex,
            "codex-session-5".to_string(),
            "thread-00000001".to_string(),
        );
        server.state.badgey_registry.register(instance).unwrap();
        let draft_path = tmp.path().join("new-bro.json");
        std::fs::write(
            &draft_path,
            serde_json::json!({
                "name": "new-bro",
                "version": 1,
                "provider": "codex"
            })
            .to_string(),
        )
        .unwrap();
        let apply_proposal = server
            .state
            .badgey_proposals
            .create(
                &id,
                orchestration::badgey::types::ProposalKind::Brofile,
                serde_json::json!({"source": draft_path.to_string_lossy()}),
                None,
            )
            .unwrap();
        let reject_proposal = server
            .state
            .badgey_proposals
            .create(
                &id,
                orchestration::badgey::types::ProposalKind::Agent,
                serde_json::json!({"source": "not-used.json"}),
                None,
            )
            .unwrap();

        let apply = server
            .badgey_resume(Parameters(BadgeyResumeParams {
                badgey_id: id.to_string(),
                prompt: format!("apply {}", apply_proposal.id),
                timeout_seconds: None,
            }))
            .await;
        assert_ne!(apply.is_error, Some(true), "{}", extract_text(&apply));
        assert!(
            orchestration::brofile::resolve_brofile("new-bro", &tmp.path().join("bro"), None)
                .is_some()
        );
        assert_eq!(
            server
                .state
                .badgey_proposals
                .get(&id, &apply_proposal.id)
                .unwrap()
                .unwrap()
                .state,
            orchestration::badgey::types::ProposalState::Applied
        );

        let reject = server
            .badgey_resume(Parameters(BadgeyResumeParams {
                badgey_id: id.to_string(),
                prompt: format!("reject {}", reject_proposal.id),
                timeout_seconds: None,
            }))
            .await;
        assert_ne!(reject.is_error, Some(true), "{}", extract_text(&reject));
        assert_eq!(
            server
                .state
                .badgey_proposals
                .get(&id, &reject_proposal.id)
                .unwrap()
                .unwrap()
                .state,
            orchestration::badgey::types::ProposalState::Failed
        );
    }

    #[test]
    fn badgey_restart_replay_restores_registry_from_thread_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
        let thread_result = server
            .state
            .threads
            .write()
            .thread(&threads::ThreadParams {
                action: "open".to_string(),
                name: Some(format!("badgey:{id}")),
                id: None,
                topic: Some("Badgey replay".to_string()),
                project: Some(tmp.path().to_string_lossy().into_owned()),
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("work_item".to_string()),
                origin: None,
            })
            .unwrap();
        // Test setup is sync-only; thread persistence is write-behind here.
        server.state.threads_persister.request();
        let thread_id = server
            .badgey_thread_id_from_open_result(&thread_result)
            .unwrap();
        let scope = orchestration::badgey::types::BadgeyScope {
            project_id: tmp.path().to_string_lossy().into_owned(),
            initial_brief: Some("replay".to_string()),
        };
        server
            .state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind: "learned".to_string(),
                body: serde_json::to_string(&orchestration::badgey::events::ThreadEvent::Exec {
                    brofile_version: "badgey-persona".to_string(),
                    scope: scope.clone(),
                    charter: "replay".to_string(),
                    provider: Provider::Brodex,
                    provider_session_id: "codex-session-6".to_string(),
                })
                .unwrap(),
                task_id: None,
                session_id: Some("codex-session-6".to_string()),
                project: Some(scope.project_id.clone()),
                thread_id: Some(thread_id.clone()),
                provider: Some("codex".to_string()),
                bro: Some("badgey".to_string()),
            })
            .unwrap();

        restore_badgey_registry_from_notes(&server.state);
        let restored = server.state.badgey_registry.get(&id).unwrap();
        assert_eq!(restored.thread_of_record_id, thread_id);
        assert_eq!(restored.provider_session_id, "codex-session-6");
    }

    #[test]
    fn badgey_restart_replay_skips_unobserved_pending_session() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
        let thread_result = server
            .state
            .threads
            .write()
            .thread(&threads::ThreadParams {
                action: "open".to_string(),
                name: Some(format!("badgey:{id}")),
                id: None,
                topic: Some("Badgey pending replay".to_string()),
                project: Some(tmp.path().to_string_lossy().into_owned()),
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("work_item".to_string()),
                origin: None,
            })
            .unwrap();
        // Test setup is sync-only; thread persistence is write-behind here.
        server.state.threads_persister.request();
        let thread_id = server
            .badgey_thread_id_from_open_result(&thread_result)
            .unwrap();
        let scope = orchestration::badgey::types::BadgeyScope {
            project_id: tmp.path().to_string_lossy().into_owned(),
            initial_brief: Some("pending replay".to_string()),
        };
        server
            .state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind: "learned".to_string(),
                body: serde_json::to_string(&orchestration::badgey::events::ThreadEvent::Exec {
                    brofile_version: "badgey-persona".to_string(),
                    scope: scope.clone(),
                    charter: "pending replay".to_string(),
                    provider: Provider::Brodex,
                    provider_session_id: "pending".to_string(),
                })
                .unwrap(),
                task_id: None,
                session_id: None,
                project: Some(scope.project_id.clone()),
                thread_id: Some(thread_id),
                provider: Some("codex".to_string()),
                bro: Some("badgey".to_string()),
            })
            .unwrap();
        // Test setup is sync-only; thread persistence is write-behind here.
        server.state.threads_persister.request();

        restore_badgey_registry_from_notes(&server.state);
        assert!(server.state.badgey_registry.get(&id).is_err());
        assert!(server.state.notes.read().all().iter().any(|note| {
            note.kind == notes::NoteKind::Surprise
                && note
                    .body
                    .contains("badgey_restore_skipped_unobserved_session")
        }));
    }

    #[test]
    fn badgey_collect_waits_for_done_note_after_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
        let instance = orchestration::badgey::registry::BadgeyInstance::new(
            id.clone(),
            orchestration::badgey::types::BadgeyScope {
                project_id: tmp.path().to_string_lossy().into_owned(),
                initial_brief: None,
            },
            Provider::Brodex,
            "codex-session-7".to_string(),
            "thread-00000001".to_string(),
        );
        server
            .state
            .badgey_registry
            .register(instance.clone())
            .unwrap();
        server
            .badgey_write_event(
                &instance,
                orchestration::badgey::events::ThreadEvent::SubbroSpawned {
                    task_id: "task-1".to_string(),
                    scout_id: "scout-1".to_string(),
                    charter: "look".to_string(),
                },
                Some("task-1".to_string()),
            )
            .unwrap();
        server
            .badgey_write_event(
                &instance,
                orchestration::badgey::events::ThreadEvent::SubbroSpawned {
                    task_id: "task-2".to_string(),
                    scout_id: "scout-1".to_string(),
                    charter: "compare".to_string(),
                },
                Some("task-2".to_string()),
            )
            .unwrap();

        let walking = server
            .badgey_collect_internal(Some("scout-1"), Some(id.as_str()))
            .unwrap();
        assert_eq!(walking["status"], "still_walking");

        server
            .state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind: "done".to_string(),
                body: serde_json::json!({
                    "scout_id": "scout-1",
                    "verdict": "found",
                    "summary": "done"
                })
                .to_string(),
                task_id: Some("task-1".to_string()),
                session_id: Some("codex-session-7".to_string()),
                project: Some(tmp.path().to_string_lossy().into_owned()),
                thread_id: Some("thread-00000001".to_string()),
                provider: Some("codex".to_string()),
                bro: Some("badgey-scout".to_string()),
            })
            .unwrap();
        let done = server
            .badgey_collect_internal(Some("scout-1"), Some(id.as_str()))
            .unwrap();
        assert_eq!(done["status"], "still_walking");

        server
            .state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind: "done".to_string(),
                body: serde_json::json!({
                    "scout_id": "scout-1",
                    "verdict": "found",
                    "summary": "done 2"
                })
                .to_string(),
                task_id: Some("task-2".to_string()),
                session_id: Some("codex-session-7".to_string()),
                project: Some(tmp.path().to_string_lossy().into_owned()),
                thread_id: Some("thread-00000001".to_string()),
                provider: Some("codex".to_string()),
                bro: Some("badgey-scout".to_string()),
            })
            .unwrap();
        let done = server
            .badgey_collect_internal(Some("scout-1"), Some(id.as_str()))
            .unwrap();
        assert_eq!(done["status"], "done");
    }

    #[test]
    fn badgey_triage_attached_to_instance_stores_redispatch_proposals() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
        let instance = orchestration::badgey::registry::BadgeyInstance::new(
            id.clone(),
            orchestration::badgey::types::BadgeyScope {
                project_id: tmp.path().to_string_lossy().into_owned(),
                initial_brief: None,
            },
            Provider::Brodex,
            "codex-session-8".to_string(),
            "thread-00000001".to_string(),
        );
        server.state.badgey_registry.register(instance).unwrap();
        let thread_result = server
            .state
            .threads
            .write()
            .thread(&threads::ThreadParams {
                action: "open".to_string(),
                name: Some("stale-work".to_string()),
                id: None,
                topic: Some("stale work".to_string()),
                project: Some(tmp.path().to_string_lossy().into_owned()),
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: None,
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("work_item".to_string()),
                origin: None,
            })
            .unwrap();
        let thread_id = server
            .badgey_thread_id_from_open_result(&thread_result)
            .unwrap();

        let triage = server
            .badgey_triage_inbox_internal(
                Some(tmp.path().to_string_lossy().into_owned()),
                None,
                Some(id.to_string()),
            )
            .unwrap();
        assert_eq!(triage["proposal_sheet"]["proposals"][0]["stored"], true);
        let proposals = server.state.badgey_proposals.list_by_instance(&id).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(
            proposals[0].kind,
            orchestration::badgey::types::ProposalKind::RedispatchTask
        );
        assert_eq!(
            proposals[0].idempotency_key.as_deref(),
            Some(format!("triage:{thread_id}").as_str())
        );
    }

    #[test]
    fn badgey_startup_recovery_fails_orphaned_non_terminal_state() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let action_id = orchestration::badgey::types::ActionId::new_v4();
        server
            .state
            .badgey_journal
            .record_seen(
                action_id.clone(),
                "bg-action-spawn-subbro".to_string(),
                serde_json::json!({"event": "bg-action-spawn-subbro"}),
            )
            .unwrap();
        let id: orchestration::badgey::types::BadgeyId = "bg-0123abcd-4567ef89".parse().unwrap();
        let proposal = server
            .state
            .badgey_proposals
            .create(
                &id,
                orchestration::badgey::types::ProposalKind::RedispatchTask,
                serde_json::json!({"prompt": "retry"}),
                Some("redispatch-task-1".to_string()),
            )
            .unwrap();
        server
            .state
            .badgey_proposals
            .transition(
                &id,
                &proposal.id,
                orchestration::badgey::types::ProposalState::Pending,
                orchestration::badgey::types::ProposalState::Applying,
                None,
            )
            .unwrap();

        recover_badgey_non_terminal_state(&server.state);
        assert!(matches!(
            server
                .state
                .badgey_journal
                .get(&action_id)
                .unwrap()
                .unwrap()
                .state,
            orchestration::badgey::types::ActionJournalState::Failed { .. }
        ));
        assert_eq!(
            server
                .state
                .badgey_proposals
                .get(&id, &proposal.id)
                .unwrap()
                .unwrap()
                .state,
            orchestration::badgey::types::ProposalState::Failed
        );
    }
}
