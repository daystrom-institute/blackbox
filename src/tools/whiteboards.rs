use crate::routing;
use crate::server::BlackboxServer;
use crate::server::dispatch::dispatch_routing_verdict_direct;
use crate::tools::bro_runtime_params::{
    WhiteboardAnnotateParams, WhiteboardArchiveParams, WhiteboardConflictsParams,
    WhiteboardOpenParams, WhiteboardPostParams, WhiteboardRegisterParams, WhiteboardStateParams,
    WhiteboardSummarizeParams, WhiteboardTransitionParams, WhiteboardVoteParams,
};
use crate::whiteboards;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::whiteboards_tools()
}

#[tool_router(router = whiteboards_tools)]
impl BlackboxServer {
    // ── Whiteboard tools — multi-agent deliberation surface ─────

    #[tool(
        name = "whiteboard_open",
        description = "Open a new whiteboard for structured deliberation. The board collects posts (blind phase), annotations (validate/debate phases), and votes (debate phase) from registered agents, advanced through phases by a facilitator-or-operator role. Returns when the board is created and the opener is registered as facilitator. Idempotent re-open against an existing id is rejected — use whiteboard_state to inspect."
    )]
    pub(crate) async fn whiteboard_open(
        &self,
        Parameters(p): Parameters<WhiteboardOpenParams>,
    ) -> CallToolResult {
        // Boards key by the registered base project (inbox scoping) — a
        // worktree opener's path must not scope the board to an ephemeral
        // checkout.
        let project = match p.project.clone().filter(|s| !s.trim().is_empty()) {
            Some(raw) => self.resolve_project_write_scope(&raw).0,
            None => String::new(),
        };
        let domain = p.domain.clone().unwrap_or_else(|| "facilitation".into());
        if let Err(e) = self.state.whiteboards.open(
            &p.board_id,
            &p.topic,
            &project,
            p.arc_thread_id.as_deref(),
            &p.opened_by,
        ) {
            return Self::err_text(&format!("whiteboard_open: {e}"));
        }
        if let Err(e) = self.state.whiteboards.register(
            &p.board_id,
            &p.opened_by,
            whiteboards::Role::Facilitator,
            &domain,
        ) {
            return Self::err_text(&format!("whiteboard_open register opener: {e}"));
        }
        Self::ok_json(&serde_json::json!({
            "status": "opened",
            "board_id": p.board_id,
            "topic": p.topic,
            "phase": "blind",
            "facilitator": p.opened_by,
        }))
    }

    #[tool(
        name = "whiteboard_register",
        description = "Register an agent on an existing board. Idempotent — re-registration with the same name is a no-op. Roles: `specialist` (post + annotate + vote), `facilitator` (transition + post + annotate + vote), `operator` (same powers as facilitator; convention is for human / external Claude joiners)."
    )]
    async fn whiteboard_register(
        &self,
        Parameters(p): Parameters<WhiteboardRegisterParams>,
    ) -> CallToolResult {
        let role = match p.role.as_str() {
            "specialist" => whiteboards::Role::Specialist,
            "facilitator" => whiteboards::Role::Facilitator,
            "operator" => whiteboards::Role::Operator,
            other => {
                return Self::err_text(&format!(
                    "whiteboard_register: unknown role '{other}' (use specialist / facilitator / operator)"
                ));
            }
        };
        match self
            .state
            .whiteboards
            .register(&p.board_id, &p.agent_name, role, &p.domain)
        {
            Ok(()) => Self::ok_json(&serde_json::json!({
                "status": "registered",
                "board_id": p.board_id,
                "agent_name": p.agent_name,
                "role": p.role,
            })),
            Err(e) => Self::err_text(&format!("whiteboard_register: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_post",
        description = "Post a structured claim/proposal/concern to a whiteboard during its blind phase. Type one of: proposal, claim, concern, informational. Optional fields target_file / target_location / severity / finding_refs / cascade_targets enable conflict detection downstream."
    )]
    async fn whiteboard_post(
        &self,
        Parameters(p): Parameters<WhiteboardPostParams>,
    ) -> CallToolResult {
        let post_type = match p.post_type.as_str() {
            "proposal" => whiteboards::PostType::Proposal,
            "claim" => whiteboards::PostType::Claim,
            "concern" => whiteboards::PostType::Concern,
            "informational" => whiteboards::PostType::Informational,
            other => {
                return Self::err_text(&format!(
                    "whiteboard_post: unknown type '{other}' (use proposal / claim / concern / informational)"
                ));
            }
        };
        let severity = match p.severity.as_deref() {
            Some("critical") => Some(whiteboards::Severity::Critical),
            Some("high") => Some(whiteboards::Severity::High),
            Some("medium") => Some(whiteboards::Severity::Medium),
            Some("low") => Some(whiteboards::Severity::Low),
            Some(other) => {
                return Self::err_text(&format!("whiteboard_post: unknown severity '{other}'"));
            }
            None => None,
        };
        match self.state.whiteboards.post(
            &p.board_id,
            &p.agent_name,
            post_type,
            &p.title,
            &p.body,
            p.target_file.as_deref(),
            p.target_location.as_deref(),
            severity,
            p.finding_refs.unwrap_or_default(),
            p.cascade_targets.unwrap_or_default(),
        ) {
            Ok(post_id) => Self::ok_json(&serde_json::json!({
                "status": "posted",
                "board_id": p.board_id,
                "post_id": post_id,
            })),
            Err(e) => Self::err_text(&format!("whiteboard_post: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_state",
        description = "Read board state filtered for the requesting agent. Phaser-style visibility: blind phase shows only own posts; later phases reveal full board. Includes phase, phase_age_secs, ready_for_transition advisory flag, post / annotation / vote arrays scoped to what this agent should see."
    )]
    async fn whiteboard_state(
        &self,
        Parameters(p): Parameters<WhiteboardStateParams>,
    ) -> CallToolResult {
        let board_arc = match self.state.whiteboards.get(&p.board_id) {
            Some(b) => b,
            None => {
                return Self::err_text(&format!(
                    "whiteboard_state: board '{}' does not exist",
                    p.board_id
                ));
            }
        };
        let view = whiteboards::filter_for_agent(&board_arc.read(), &p.agent_name);
        match view {
            Ok(v) => Self::ok_json(&serde_json::to_value(&v).unwrap_or_default()),
            Err(e) => Self::err_text(&format!("whiteboard_state: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_annotate",
        description = "Annotate a post during the validate or debate phase. Validate phase accepts only `validation` (with required `result`: confirmed / refuted / inconclusive). Debate phase accepts `challenge`, `corroborate`, or `resolve` (resolve must reference a challenge id via `resolves`; a post owner may resolve another agent's challenge on their own post)."
    )]
    async fn whiteboard_annotate(
        &self,
        Parameters(p): Parameters<WhiteboardAnnotateParams>,
    ) -> CallToolResult {
        let ann = match p.annotation_type.as_str() {
            "challenge" => whiteboards::AnnotationType::Challenge,
            "corroborate" => whiteboards::AnnotationType::Corroborate,
            "resolve" => whiteboards::AnnotationType::Resolve,
            "validation" => whiteboards::AnnotationType::Validation,
            other => {
                return Self::err_text(&format!("whiteboard_annotate: unknown type '{other}'"));
            }
        };
        let result = match p.result.as_deref() {
            Some("confirmed") => Some(whiteboards::ValidationResult::Confirmed),
            Some("refuted") => Some(whiteboards::ValidationResult::Refuted),
            Some("inconclusive") => Some(whiteboards::ValidationResult::Inconclusive),
            Some(other) => {
                return Self::err_text(&format!("whiteboard_annotate: unknown result '{other}'"));
            }
            None => None,
        };
        match self.state.whiteboards.annotate(
            &p.board_id,
            &p.agent_name,
            &p.post_id,
            ann,
            &p.body,
            result,
            p.resolves.as_deref(),
        ) {
            Ok(ann_id) => Self::ok_json(&serde_json::json!({
                "status": "annotated",
                "board_id": p.board_id,
                "annotation_id": ann_id,
                "post_id": p.post_id,
            })),
            Err(e) => Self::err_text(&format!("whiteboard_annotate: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_vote",
        description = "Cast an advisory vote on a post during the debate phase. One vote per agent per post — re-vote replaces. Vote: accept, reject, or defer."
    )]
    async fn whiteboard_vote(
        &self,
        Parameters(p): Parameters<WhiteboardVoteParams>,
    ) -> CallToolResult {
        let v = match p.vote.as_str() {
            "accept" => whiteboards::VoteValue::Accept,
            "reject" => whiteboards::VoteValue::Reject,
            "defer" => whiteboards::VoteValue::Defer,
            other => return Self::err_text(&format!("whiteboard_vote: unknown vote '{other}'")),
        };
        match self.state.whiteboards.vote(
            &p.board_id,
            &p.agent_name,
            &p.post_id,
            v,
            p.reason.as_deref(),
        ) {
            Ok(replaced) => Self::ok_json(&serde_json::json!({
                "status": if replaced { "vote_replaced" } else { "voted" },
                "board_id": p.board_id,
                "post_id": p.post_id,
                "vote": p.vote,
            })),
            Err(e) => Self::err_text(&format!("whiteboard_vote: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_transition",
        description = "Advance the board to a new phase. Facilitator or operator role required. Sequence: blind → read → validate → debate → resolve → archived; read → debate and validate → resolve are legal skips. Transition emits a `board-transitioned` signal correlated to (board_id, target_phase) so any wait node observing the board resumes."
    )]
    pub(crate) async fn whiteboard_transition(
        &self,
        Parameters(p): Parameters<WhiteboardTransitionParams>,
    ) -> CallToolResult {
        let target = match p.target_phase.as_str() {
            "read" => whiteboards::Phase::Read,
            "validate" => whiteboards::Phase::Validate,
            "debate" => whiteboards::Phase::Debate,
            "resolve" => whiteboards::Phase::Resolve,
            "archived" => whiteboards::Phase::Archived,
            other => {
                return Self::err_text(&format!(
                    "whiteboard_transition: unknown target_phase '{other}'"
                ));
            }
        };
        let result = self.state.whiteboards.transition(
            &p.board_id,
            &p.agent_name,
            target,
            p.summary.as_deref(),
        );
        match result {
            Ok((from, to)) => {
                // Fire the routed signal so wait_for_phase nodes resume.
                let state = self.state.clone();
                let board_id = p.board_id.clone();
                let from_str = from.as_str().to_string();
                let to_str = to.as_str().to_string();
                tokio::spawn(async move {
                    let entity = serde_json::json!({
                        "board_id": board_id,
                        "from_phase": from_str,
                        "to_phase": to_str,
                    });
                    let mut correlate = serde_json::Map::new();
                    correlate.insert("board".into(), serde_json::json!(board_id));
                    correlate.insert("phase".into(), serde_json::json!(to_str));
                    let verdict = routing::RoutingVerdict::SignalArc {
                        signal: "board-transitioned".into(),
                        correlate,
                        payload: Some(entity.clone()),
                    };
                    let _ = dispatch_routing_verdict_direct(
                        state.clone(),
                        "whiteboard",
                        verdict,
                        entity,
                    )
                    .await;
                    // Emit whiteboard.phase_changed system event. Observation-only.
                    let mut correlation = serde_json::Map::new();
                    correlation.insert("board_id".into(), serde_json::json!(board_id));
                    let draft = crate::system_events::SystemEventDraft {
                        kind: crate::system_events::types::SystemEventKind::WhiteboardPhaseChanged,
                        producer: "whiteboard.transition".to_string(),
                        project: None,
                        principal: None,
                        subject: None,
                        correlation,
                        causation_id: None,
                        payload: serde_json::json!({
                            "board_id": board_id,
                            "from_phase": from_str,
                            "to_phase": to_str,
                        }),
                    };
                    if let Err(e) = state.system_events.emit(draft).await {
                        tracing::warn!("whiteboard.phase_changed system event emit failed: {e:#}");
                    }
                });
                Self::ok_json(&serde_json::json!({
                    "status": "transitioned",
                    "board_id": p.board_id,
                    "from": from.as_str(),
                    "to": to.as_str(),
                }))
            }
            Err(e) => Self::err_text(&format!("whiteboard_transition: {e}")),
        }
    }

    #[tool(
        name = "whiteboard_conflicts",
        description = "Auto-detect conflicts between posts on a board. Returns three kinds: `direct_overlap` (same target_file + identical target_location), `cascade_collision` (post A cascades to post B's direct target), `severity_disagreement` (same finding_ref, distinct severities). Available in any phase past blind."
    )]
    async fn whiteboard_conflicts(
        &self,
        Parameters(p): Parameters<WhiteboardConflictsParams>,
    ) -> CallToolResult {
        let board_arc = match self.state.whiteboards.get(&p.board_id) {
            Some(b) => b,
            None => {
                return Self::err_text(&format!(
                    "whiteboard_conflicts: board '{}' does not exist",
                    p.board_id
                ));
            }
        };
        let board = board_arc.read();
        if !board.agents.contains_key(&p.agent_name) {
            return Self::err_text(&format!(
                "agent '{}' not registered on board '{}'",
                p.agent_name, p.board_id
            ));
        }
        if board.phase == whiteboards::Phase::Blind {
            return Self::err_text("whiteboard_conflicts: not available in blind phase");
        }
        let conflicts = whiteboards::detect_conflicts(&board);
        Self::ok_json(&serde_json::json!({
            "phase": board.phase.as_str(),
            "post_count": board.posts.len(),
            "conflict_count": conflicts.len(),
            "conflicts": conflicts,
        }))
    }

    #[tool(
        name = "whiteboard_summarize",
        description = "Condensed board summary without full post bodies. Returns counts per type, vote tally per post, conflict count, unresolved-challenge count, agent status (has_posted), phase age, ready_for_transition advisory."
    )]
    async fn whiteboard_summarize(
        &self,
        Parameters(p): Parameters<WhiteboardSummarizeParams>,
    ) -> CallToolResult {
        let board_arc = match self.state.whiteboards.get(&p.board_id) {
            Some(b) => b,
            None => {
                return Self::err_text(&format!(
                    "whiteboard_summarize: board '{}' does not exist",
                    p.board_id
                ));
            }
        };
        let board = board_arc.read();
        if !board.agents.contains_key(&p.agent_name) {
            return Self::err_text(&format!(
                "agent '{}' not registered on board '{}'",
                p.agent_name, p.board_id
            ));
        }
        let phase_age = chrono::DateTime::parse_from_rfc3339(
            &board
                .phase_history
                .last()
                .map(|h| h.at.clone())
                .unwrap_or_default(),
        )
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(0);
        let mut posts_by_type = std::collections::BTreeMap::<&str, u32>::new();
        for post in &board.posts {
            let key = match post.post_type {
                whiteboards::PostType::Proposal => "proposal",
                whiteboards::PostType::Claim => "claim",
                whiteboards::PostType::Concern => "concern",
                whiteboards::PostType::Informational => "informational",
            };
            *posts_by_type.entry(key).or_default() += 1;
        }
        let posted: std::collections::HashSet<&str> =
            board.posts.iter().map(|p| p.agent.as_str()).collect();
        let agents_status: serde_json::Map<String, serde_json::Value> = board
            .agents
            .iter()
            .map(|(name, info)| {
                (
                    name.clone(),
                    serde_json::json!({
                        "role": match info.role {
                            whiteboards::Role::Specialist => "specialist",
                            whiteboards::Role::Facilitator => "facilitator",
                            whiteboards::Role::Operator => "operator",
                        },
                        "domain": info.domain,
                        "has_posted": posted.contains(name.as_str()),
                    }),
                )
            })
            .collect();
        let conflicts = if board.phase == whiteboards::Phase::Blind {
            Vec::new()
        } else {
            whiteboards::detect_conflicts(&board)
        };
        let challenges = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == whiteboards::AnnotationType::Challenge)
            .count();
        let resolved: std::collections::HashSet<&str> = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == whiteboards::AnnotationType::Resolve)
            .filter_map(|a| a.resolves.as_deref())
            .collect();
        let unresolved_challenges = board
            .annotations
            .iter()
            .filter(|a| a.annotation_type == whiteboards::AnnotationType::Challenge)
            .filter(|c| !resolved.contains(c.id.as_str()))
            .count();
        let vs = board.validation_summary();
        Self::ok_json(&serde_json::json!({
            "board_id": board.id,
            "topic": board.topic,
            "phase": board.phase.as_str(),
            "phase_age_secs": phase_age,
            "ready_for_transition": board.ready_for_transition(phase_age),
            "post_count": board.posts.len(),
            "posts_by_type": posts_by_type,
            "annotation_count": board.annotations.len(),
            "vote_count": board.votes.len(),
            "vote_tally": board.vote_tally(),
            "conflict_count": conflicts.len(),
            "challenge_count": challenges,
            // Informational only — debate no longer gates on this reaching 0;
            // surviving unresolved challenges flow into the plan's Contradictions.
            "unresolved_challenges": unresolved_challenges,
            // Validator-driven exclusion teeth + review coverage.
            "surviving_post_ids": vs.surviving_post_ids,
            "excluded_post_ids": vs.excluded_post_ids,
            "confirmed_count": vs.confirmed_count,
            "refuted_count": vs.refuted_count,
            "inconclusive_count": vs.inconclusive_count,
            "unvalidated_count": vs.unvalidated_count,
            "unreviewed_post_count": vs.unreviewed_post_count,
            "agents": agents_status,
        }))
    }

    #[tool(
        name = "whiteboard_archive",
        description = "Archive the board. Resolve phase only. Strips active state, moves to `<store>/whiteboards/archive/<id>.json`, returns summary statistics."
    )]
    async fn whiteboard_archive(
        &self,
        Parameters(p): Parameters<WhiteboardArchiveParams>,
    ) -> CallToolResult {
        match self.state.whiteboards.archive(&p.board_id, &p.agent_name) {
            Ok(summary) => Self::ok_json(&serde_json::to_value(&summary).unwrap_or_default()),
            Err(e) => Self::err_text(&format!("whiteboard_archive: {e}")),
        }
    }
}
