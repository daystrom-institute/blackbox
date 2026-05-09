use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::config_tools()
}

#[tool_router(router = config_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_mcp",
        description = "Manage MCP servers + tool filters for dispatched bros."
    )]
    pub(crate) fn bro_mcp(
        &self,
        Parameters(p): Parameters<orchestration::mcp::McpToolParams>,
    ) -> CallToolResult {
        Self::run("bro_mcp", || orchestration::mcp::handle(&p))
    }

    #[tool(
        name = "bro_slack_bind",
        description = "Bind a Slack channel to a bbox project. The binding scopes inbound Slack→badgey activity to a single project and gives the daily-triage cron a per-channel home for proposal posts. Channel id (C-prefix) is the stable lookup key; rename-safe. Actions: bind, unbind, list, lookup. Project accepts absolute path or 8-hex project_id from the registry."
    )]
    pub(crate) fn bro_slack_bind(
        &self,
        Parameters(p): Parameters<BroSlackBindParams>,
    ) -> CallToolResult {
        let store = &self.state.slack_channel_bindings;
        match p.action.as_str() {
            "bind" => {
                let team_id = match p.team_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(t) => t.to_string(),
                    None => return Self::err_text("team_id is required"),
                };
                let channel_id = match p.channel_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(c) => c.to_string(),
                    None => return Self::err_text("channel_id is required"),
                };
                let project_input = match p.project.as_deref().filter(|s| !s.is_empty()) {
                    Some(pr) => pr.to_string(),
                    None => return Self::err_text("project is required"),
                };
                let registry = self.state.projects.read();
                let records = registry.list();
                // Mirror ProjectRegistry's symlink-resolving behavior so
                // bind accepts aliases the registry already collapsed at
                // register time. Without this, a user who registered
                // `/repo/foo` (resolved target) but binds the symlink
                // `/home/me/foo` would see project_id=None and miss
                // rename/preflight scoping later. Refuse paths that
                // exist on disk but are non-directories (file, etc.)
                // — those would silently bind nonsense.
                let canonical_input = match entity_ref::canonical_input_path(&project_input) {
                    Ok(p) => Some(p.to_string_lossy().into_owned()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => {
                        return Self::err_text(&format!(
                            "project '{project_input}' is not a usable directory: {e}"
                        ));
                    }
                };
                let resolved = records.iter().find(|r| {
                    r.canonical_path == project_input
                        || r.project_id == project_input
                        || canonical_input
                            .as_deref()
                            .is_some_and(|c| c == r.canonical_path)
                });
                let (project_dir, project_id) = match resolved {
                    Some(rec) => (rec.canonical_path.clone(), Some(rec.project_id.clone())),
                    None => {
                        let path_buf = std::path::PathBuf::from(&project_input);
                        if !path_buf.is_absolute() {
                            return Self::err_text(&format!(
                                "project '{project_input}' is not registered and not an absolute path; register via bbox_project_register first"
                            ));
                        }
                        // Store the canonicalized form when fs resolution
                        // succeeded (symlink-stable storage); fall back
                        // to the operator's literal absolute path when
                        // it doesn't exist yet on disk.
                        let stored = canonical_input.unwrap_or_else(|| project_input.clone());
                        (stored, None)
                    }
                };
                drop(registry);
                // Preserve any badgey_id from a prior bind so re-binding
                // an already-active channel doesn't orphan its system
                // BadgeyInstance. The instance lifecycle is unbind-only;
                // rebind updates project/name/path in place.
                let prior_badgey_id = store
                    .lookup(&team_id, &channel_id)
                    .and_then(|b| b.badgey_id);
                let binding = slack_channel_bindings::ChannelBinding {
                    team_id: team_id.clone(),
                    channel_id: channel_id.clone(),
                    channel_name: p.channel_name.clone(),
                    project_dir: project_dir.clone(),
                    project_id: project_id.clone(),
                    registered_at: util::now_iso(),
                    registered_by: p.registered_by.clone(),
                    badgey_id: prior_badgey_id,
                };
                if let Err(e) = store.bind(binding.clone()) {
                    return Self::err_text(&format!("bind failed: {e}"));
                }
                Self::ok_json(&json!({
                    "bound": true,
                    "binding": binding,
                }))
            }
            "unbind" => {
                let team_id = match p.team_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(t) => t,
                    None => return Self::err_text("team_id is required"),
                };
                let channel_id = match p.channel_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(c) => c,
                    None => return Self::err_text("channel_id is required"),
                };
                match store.unbind(team_id, channel_id) {
                    Ok(Some(prior)) => {
                        // Dismiss the system Badgey instance that was
                        // serving this channel, if any. Best-effort:
                        // dismissal failure (e.g. instance already gone)
                        // is logged but doesn't fail the unbind.
                        let mut dismissed_badgey: Option<String> = None;
                        if let Some(ref bid) = prior.badgey_id {
                            if let Ok(parsed) =
                                bid.parse::<orchestration::badgey::types::BadgeyId>()
                            {
                                match self.state.badgey_registry.dismiss(&parsed) {
                                    Ok(_) => dismissed_badgey = Some(bid.clone()),
                                    Err(e) => tracing::warn!(
                                        badgey_id = %bid,
                                        "unbind: dismissing system Badgey instance failed: {e}"
                                    ),
                                }
                            }
                        }
                        Self::ok_json(&json!({
                            "unbound": true,
                            "prior": prior,
                            "dismissed_badgey": dismissed_badgey,
                        }))
                    }
                    Ok(None) => Self::ok_json(&json!({"unbound": false})),
                    Err(e) => Self::err_text(&format!("unbind failed: {e}")),
                }
            }
            "list" => {
                let bindings = store.list(p.team_filter.as_deref(), p.project_filter.as_deref());
                Self::ok_json(&json!({"bindings": bindings}))
            }
            "lookup" => {
                let team_id = match p.team_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(t) => t,
                    None => return Self::err_text("team_id is required"),
                };
                let channel_id = match p.channel_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(c) => c,
                    None => return Self::err_text("channel_id is required"),
                };
                match store.lookup(team_id, channel_id) {
                    Some(b) => Self::ok_json(&json!({"found": true, "binding": b})),
                    None => Self::ok_json(&json!({"found": false})),
                }
            }
            other => Self::err_text(&format!("Unknown bro_slack_bind action: {other}")),
        }
    }

    #[tool(
        name = "bro_slack_link_record",
        description = "Record a SlackProposalLink mapping a posted Slack message back to its BadgeyProposal. Called by the per-channel triage workflow's emit-proposal subworkflow after chat.postMessage so inbound reactions/replies can resolve back to (BadgeyId, proposal_id) and the apply/refine hooks fire."
    )]
    pub(crate) fn bro_slack_link_record(
        &self,
        Parameters(p): Parameters<SlackProposalLinkRecordParams>,
    ) -> CallToolResult {
        if p.team_id.trim().is_empty()
            || p.channel_id.trim().is_empty()
            || p.msg_ts.trim().is_empty()
            || p.proposal_id.trim().is_empty()
            || p.project_dir.trim().is_empty()
        {
            return Self::err_text(
                "team_id, channel_id, msg_ts, proposal_id, and project_dir are all required",
            );
        }
        let link = slack_proposal_links::SlackProposalLink {
            team_id: p.team_id,
            channel_id: p.channel_id,
            msg_ts: p.msg_ts.clone(),
            proposal_id: p.proposal_id.clone(),
            instance_id: p.instance_id,
            authoring_session_id: p.authoring_session_id,
            version: 1,
            project_dir: p.project_dir,
            posted_at: util::now_iso(),
        };
        match self.state.slack_proposal_links.record(link) {
            Ok(()) => Self::ok_json(&json!({
                "recorded": true,
                "msg_ts": p.msg_ts,
                "proposal_id": p.proposal_id,
            })),
            Err(e) => Self::err_text(&format!("recording slack proposal link failed: {e}")),
        }
    }

    #[tool(
        name = "bro_slack_link_lookup",
        description = "Resolve a Slack message ts back to its SlackProposalLink (proposal_id, instance_id, project_dir, version, posted_at). Used by the apply/refine workflows that fire on `:white_check_mark:` reactions and in-thread replies — they need the (BadgeyId, proposal_id) pair from the link to call badgey_apply_proposal or bro_resume. Returns {found: false} for messages that aren't a posted proposal (e.g. random check on an unrelated message) so workflows can no-op cleanly."
    )]
    pub(crate) fn bro_slack_link_lookup(
        &self,
        Parameters(p): Parameters<SlackProposalLinkLookupParams>,
    ) -> CallToolResult {
        if p.team_id.trim().is_empty()
            || p.channel_id.trim().is_empty()
            || p.msg_ts.trim().is_empty()
        {
            return Self::err_text("team_id, channel_id, and msg_ts are all required");
        }
        match self
            .state
            .slack_proposal_links
            .lookup_by_msg(&p.team_id, &p.channel_id, &p.msg_ts)
        {
            Some(link) => Self::ok_json(&json!({"found": true, "link": link})),
            None => Self::ok_json(&json!({"found": false})),
        }
    }
}
