use super::body_page::{json_body_page, validate_detail};
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
use serde_json::{Value, json};

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
        let (project, resolved_project_id) =
            match p.project.clone().filter(|s| !s.trim().is_empty()) {
                Some(raw) => match self.resolve_project_write_scope_with_id(&raw) {
                    Ok((scope, resolved_project_id, _write_dir)) => (scope, resolved_project_id),
                    Err(error) => {
                        return Self::err_text(&format!(
                            "whiteboard_open project authority failed: {error:#}"
                        ));
                    }
                },
                None => (String::new(), None),
            };
        let domain = p.domain.clone().unwrap_or_else(|| "facilitation".into());
        if let Err(e) = self.state.whiteboards.open(
            &p.board_id,
            &p.topic,
            &project,
            resolved_project_id.as_deref(),
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
        description = "Read a bounded visible-board preview (up to five posts, annotations, and votes), with truthful visible counts. Blind hides peer posts; debate specialists see only own or related annotations and own votes. Select post_id to focus one visible post. Read detail=full for exact filtered JSON body pages; continue with cursor=body.next_cursor and parse concatenated body.text. Unknown or invisible post ids return not found."
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
        match state_projection(&board_arc.read(), &p) {
            Ok(value) => Self::ok_json(&value),
            Err(error) => Self::err_text(&format!("whiteboard_state: {error}")),
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
        description = "Auto-detect conflicts between posts on a board. Returns three kinds: `direct_overlap` (same target_file + identical target_location), `cascade_collision` (post A cascades to post B's direct target), `severity_disagreement` (same finding_ref, distinct severities). Available in any phase past blind. Default returns at most ten conflict previews and the total count. detail=full returns exact JSON body pages; follow body.next_cursor."
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
        let full = match validate_detail(p.detail.as_deref(), p.cursor.as_deref(), p.body_limit) {
            Ok(full) => full,
            Err(error) => return Self::err_text(&error.to_string()),
        };
        let conflicts = whiteboards::detect_conflicts(&board);
        let exact = json!({"board_id":board.id, "phase":board.phase, "post_count":board.posts.len(), "conflict_count":conflicts.len(), "conflicts":conflicts});
        if full {
            return match json_body_page(
                &format!("whiteboard-conflicts:{}:{}", p.board_id, p.agent_name),
                &exact,
                p.cursor.as_deref(),
                p.body_limit,
            ) {
                Ok(body) => {
                    Self::ok_json(&json!({"board_id":board.id, "phase":board.phase, "body":body}))
                }
                Err(error) => Self::err_text(&error.to_string()),
            };
        }
        let mut preview = exact.clone();
        preview["conflicts"] = json!(
            exact["conflicts"]
                .as_array()
                .unwrap()
                .iter()
                .take(10)
                .map(compact_record)
                .collect::<Vec<_>>()
        );
        preview["preview"] = json!(true);
        preview["returned"] = json!(preview["conflicts"].as_array().unwrap().len());
        preview["detail_hint"] = json!(
            "Read whiteboard_conflicts with the same board_id and agent_name, detail=full; concatenate body.text pages via cursor=body.next_cursor, then parse JSON."
        );
        Self::ok_json(&preview)
    }

    #[tool(
        name = "whiteboard_summarize",
        description = "Summarize only the requesting agent's visible evidence: exact counts and readiness, with bounded post-standing, vote-tally, and agent previews. Gate counts remain numeric and complete for that visible scope. detail=full returns the complete visible summary as JSON body pages; follow body.next_cursor. Hidden peer evidence never contributes counts or ids."
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
        match summary_projection(&board_arc.read(), &p) {
            Ok(value) => Self::ok_json(&value),
            Err(error) => Self::err_text(&format!("whiteboard_summarize: {error}")),
        }
    }

    #[tool(
        name = "whiteboard_archive",
        description = "Archive the board (facilitator/operator role, same authority as a phase transition). Resolve phase only, unless force=true, the abandon path for boards stranded mid-phase by a failed arc. Removes the board from active deliberation and returns archive summary statistics."
    )]
    async fn whiteboard_archive(
        &self,
        Parameters(p): Parameters<WhiteboardArchiveParams>,
    ) -> CallToolResult {
        match self
            .state
            .whiteboards
            .archive(&p.board_id, &p.agent_name, p.force)
        {
            Ok(summary) => Self::ok_json(&serde_json::to_value(&summary).unwrap_or_default()),
            Err(e) => Self::err_text(&format!("whiteboard_archive: {e}")),
        }
    }
}

// Tool projections are deliberately separate from the workflow's full board
// template scope. Resolve visibility before counting or choosing any preview.
fn visible_board(
    board: &whiteboards::Board,
    agent_name: &str,
) -> anyhow::Result<whiteboards::Board> {
    let view = whiteboards::filter_for_agent(board, agent_name)?;
    let mut visible = board.clone();
    visible.posts = view.posts;
    visible.annotations = view.annotations;
    visible.votes = view.votes;
    Ok(visible)
}

fn compact_record(value: &Value) -> Value {
    if serde_json::to_vec(value)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
        <= 800
    {
        return value.clone();
    }
    let Some(object) = value.as_object() else {
        return json!({"preview":true});
    };
    let mut out = serde_json::Map::new();
    let mut omitted = Vec::new();
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "id" | "post_id" | "agent" | "type" | "severity" | "vote" | "result" | "resolves"
        ) || serde_json::to_vec(value)
            .map(|s| s.len())
            .unwrap_or(usize::MAX)
            <= 128
        {
            out.insert(key.clone(), value.clone());
        } else {
            omitted.push(key);
            if matches!(key.as_str(), "body" | "title" | "reason")
                && let Some(text) = value.as_str()
            {
                out.insert(
                    format!("{key}_preview"),
                    json!(text.chars().take(120).collect::<String>()),
                );
            }
        }
    }
    out.insert("omitted_fields".into(), json!(omitted));
    out.insert("preview".into(), json!(true));
    Value::Object(out)
}

fn state_projection(
    board: &whiteboards::Board,
    p: &WhiteboardStateParams,
) -> anyhow::Result<Value> {
    let full = validate_detail(p.detail.as_deref(), p.cursor.as_deref(), p.body_limit)?;
    let mut view = whiteboards::filter_for_agent(board, &p.agent_name)?;
    if let Some(post_id) = p.post_id.as_deref() {
        if !view.posts.iter().any(|post| post.id == post_id) {
            anyhow::bail!("post not found in the visible board");
        }
        view.posts.retain(|post| post.id == post_id);
        view.annotations
            .retain(|annotation| annotation.post_id == post_id);
        view.votes.retain(|vote| vote.post_id == post_id);
    }
    view.post_count = view.posts.len();
    view.annotation_count = view.annotations.len();
    view.vote_count = view.votes.len();
    let mut exact = serde_json::to_value(&view)?;
    // Ticking display age must not invalidate a stable evidence cursor.
    exact.as_object_mut().unwrap().remove("phase_age_secs");
    exact.as_object_mut().unwrap().remove("phase_age_warning");
    exact
        .as_object_mut()
        .unwrap()
        .remove("ready_for_transition");
    let mut out = json!({"id":view.id, "phase":view.phase, "phase_age_secs":view.phase_age_secs, "ready_for_transition":view.ready_for_transition});
    if full {
        out["body"] = json_body_page(
            &format!(
                "whiteboard-state:{}:{}:{:?}",
                p.board_id, p.agent_name, p.post_id
            ),
            &exact,
            p.cursor.as_deref(),
            p.body_limit,
        )?;
        return Ok(out);
    }
    out["topic"] = json!(view.topic.chars().take(240).collect::<String>());
    out["post_count"] = json!(view.post_count);
    out["annotation_count"] = json!(view.annotation_count);
    out["vote_count"] = json!(view.vote_count);
    for key in ["posts", "annotations", "votes"] {
        out[key] = json!(
            exact[key]
                .as_array()
                .unwrap()
                .iter()
                .take(5)
                .map(compact_record)
                .collect::<Vec<_>>()
        );
    }
    out["preview"] = json!(true);
    out["detail_hint"] = json!(
        "Read whiteboard_state with the same board_id, agent_name and optional post_id, detail=full; concatenate body.text pages via cursor=body.next_cursor, then parse JSON. Counts describe visible evidence; preview arrays may omit rows or fields."
    );
    Ok(out)
}

fn summary_projection(
    board: &whiteboards::Board,
    p: &WhiteboardSummarizeParams,
) -> anyhow::Result<Value> {
    let full = validate_detail(p.detail.as_deref(), p.cursor.as_deref(), p.body_limit)?;
    let age = whiteboards::filter_for_agent(board, &p.agent_name)?.phase_age_secs;
    let ready = board.ready_for_transition(age);
    let board = visible_board(board, &p.agent_name)?;
    let agent_name = &p.agent_name;
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
                        "has_posted": if board.phase != whiteboards::Phase::Blind || name == agent_name { Some(posted.contains(name.as_str())) } else { None },
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
    let exact = serde_json::json!({
        "board_id": board.id,
        "topic": board.topic,
        "phase": board.phase.as_str(),
        "post_count": board.posts.len(),
        "posts_by_type": posts_by_type,
        "annotation_count": board.annotations.len(),
        "vote_count": board.votes.len(),
        "vote_tally": board.vote_tally(),
        "conflict_count": conflicts.len(),
        "challenge_count": challenges,
        // Informational only: debate no longer gates on this reaching 0;
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
    });
    if full {
        return Ok(
            json!({"board_id":board.id, "phase":board.phase, "phase_age_secs":age, "ready_for_transition":ready,
            "body":json_body_page(&format!("whiteboard-summary:{}:{}", p.board_id, p.agent_name), &exact, p.cursor.as_deref(), p.body_limit)?}),
        );
    }
    let mut out = exact;
    out["phase_age_secs"] = json!(age);
    out["ready_for_transition"] = json!(ready);
    out["topic"] = json!(board.topic.chars().take(240).collect::<String>());
    for key in ["surviving_post_ids", "excluded_post_ids"] {
        let values = out[key].as_array().unwrap();
        out[key] = json!(values.iter().take(20).collect::<Vec<_>>());
    }
    for key in ["agents", "vote_tally"] {
        let values = out[key].as_object().unwrap();
        let preview = values
            .iter()
            .take(10)
            .map(|(name, value)| (name.clone(), compact_record(value)))
            .collect::<serde_json::Map<_, _>>();
        out[key] = json!(preview);
    }
    out["preview"] = json!(true);
    out["detail_hint"] = json!(
        "Read whiteboard_summarize with the same board_id and agent_name, detail=full; concatenate body.text pages via cursor=body.next_cursor, then parse JSON. Scalar counts are complete for visible evidence; arrays and maps are previews."
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn board() -> whiteboards::Board {
        whiteboards::Board {
            id: "board-example".into(),
            topic: "Review the proposed change".into(),
            project: String::new(),
            project_id: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            phase: whiteboards::Phase::Blind,
            phase_history: Vec::new(),
            agents: ["alice", "bob", "facilitator"]
                .into_iter()
                .map(|name| {
                    (
                        name.into(),
                        whiteboards::Agent {
                            role: if name == "facilitator" {
                                whiteboards::Role::Facilitator
                            } else {
                                whiteboards::Role::Specialist
                            },
                            domain: "review".into(),
                            registered_at: "2026-01-01T00:00:00Z".into(),
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
            posts: ["alice", "bob"]
                .into_iter()
                .enumerate()
                .map(|(index, agent)| whiteboards::Post {
                    id: format!("post-{index}"),
                    agent: agent.into(),
                    post_type: whiteboards::PostType::Claim,
                    title: format!("{agent} claim"),
                    body: format!("{agent} private evidence ").repeat(1000),
                    target_file: None,
                    target_location: None,
                    severity: None,
                    finding_refs: Vec::new(),
                    cascade_targets: Vec::new(),
                    posted_at: "2026-01-01T00:00:00Z".into(),
                })
                .collect(),
            annotations: Vec::new(),
            votes: Vec::new(),
            arc_thread_id: None,
        }
    }

    fn state_params() -> WhiteboardStateParams {
        WhiteboardStateParams {
            board_id: "board-example".into(),
            agent_name: "alice".into(),
            detail: None,
            cursor: None,
            body_limit: None,
            post_id: None,
        }
    }

    fn summary_params(agent: &str) -> WhiteboardSummarizeParams {
        WhiteboardSummarizeParams {
            board_id: "board-example".into(),
            agent_name: agent.into(),
            detail: None,
            cursor: None,
            body_limit: None,
        }
    }

    #[test]
    fn whiteboard_previews_and_summaries_do_not_reveal_hidden_posts_or_counts() {
        let board = board();
        let state = state_projection(&board, &state_params()).unwrap();
        assert_eq!(state["post_count"], 1);
        assert_eq!(state["posts"].as_array().unwrap().len(), 1);
        assert!(!state.to_string().contains("bob private"));
        let summary = summary_projection(&board, &summary_params("alice")).unwrap();
        assert_eq!(summary["post_count"], 1);
        assert_eq!(summary["unvalidated_count"], 1);
        assert_eq!(summary["surviving_post_ids"], json!(["post-0"]));
        assert!(summary["agents"]["bob"]["has_posted"].is_null());
        let mut params = state_params();
        params.post_id = Some("post-1".into());
        let invisible = state_projection(&board, &params).unwrap_err().to_string();
        params.post_id = Some("missing".into());
        assert_eq!(
            invisible,
            state_projection(&board, &params).unwrap_err().to_string()
        );
    }

    #[test]
    fn whiteboard_debate_summary_keeps_specialist_visibility_and_facilitator_gate_counts() {
        let mut board = board();
        board.phase = whiteboards::Phase::Debate;
        board.annotations.push(whiteboards::Annotation {
            id: "hidden-challenge".into(),
            post_id: "post-1".into(),
            agent: "bob".into(),
            annotation_type: whiteboards::AnnotationType::Challenge,
            body: "peer-only argument".into(),
            result: None,
            resolves: None,
            posted_at: "2026-01-01T00:00:00Z".into(),
        });
        board.votes.push(whiteboards::Vote {
            post_id: "post-1".into(),
            agent: "bob".into(),
            vote: whiteboards::VoteValue::Reject,
            reason: None,
            at: "2026-01-01T00:00:00Z".into(),
        });
        let specialist = summary_projection(&board, &summary_params("alice")).unwrap();
        assert_eq!(specialist["unresolved_challenges"], 0);
        assert_eq!(specialist["annotation_count"], 0);
        assert_eq!(specialist["vote_count"], 0);
        let facilitator = summary_projection(&board, &summary_params("facilitator")).unwrap();
        assert_eq!(facilitator["unresolved_challenges"], 1);
        assert_eq!(facilitator["vote_count"], 1);
    }

    #[test]
    fn whiteboard_full_pages_are_exact_filtered_and_recheck_reader_scope() {
        let board = board();
        let mut params = state_params();
        params.detail = Some("full".into());
        let first = state_projection(&board, &params).unwrap();
        let first_cursor = first["body"]["next_cursor"].as_str().unwrap().to_string();
        let mut text = String::new();
        loop {
            let page = state_projection(&board, &params).unwrap();
            text.push_str(page["body"]["text"].as_str().unwrap());
            params.cursor = page["body"]["next_cursor"].as_str().map(str::to_string);
            if params.cursor.is_none() {
                break;
            }
        }
        let exact: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(exact["posts"][0]["body"], board.posts[0].body);
        assert_eq!(exact["post_count"], 1);
        assert!(!text.contains("bob private"));
        params.cursor = Some(first_cursor);
        params.agent_name = "bob".into();
        assert!(state_projection(&board, &params).is_err());
        params.agent_name = "unregistered".into();
        assert!(state_projection(&board, &params).is_err());
    }

    #[test]
    fn whiteboard_large_preview_is_explicit_and_fits_mirrored_transport() {
        let mut board = board();
        let mut template = board.posts[0].clone();
        template.body = "\u{0001}🦀".repeat(9000);
        template.finding_refs = (0..500).map(|index| format!("finding-{index}")).collect();
        board.posts = (0..30)
            .map(|index| {
                let mut post = template.clone();
                post.id = format!("post-{index}");
                post
            })
            .collect();
        let state = state_projection(&board, &state_params()).unwrap();
        assert_eq!(state["post_count"], 30);
        assert_eq!(state["posts"].as_array().unwrap().len(), 5);
        assert_eq!(state["preview"], true);
        assert!(
            state["posts"][0]["omitted_fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "finding_refs")
        );
        let envelope = json!({"content":[{"type":"text", "text":serde_json::to_string_pretty(&state).unwrap()}], "structuredContent":state});
        assert!(serde_json::to_vec(&envelope).unwrap().len() < 80 * 1024);
    }
}
