use crate::orchestration;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Bro tools (orchestration)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ExecParams {
    /// Task instruction for the agent
    pub(crate) prompt: String,
    /// Named bro instance to target. Bare names must be unique across live
    /// teams; use `team::bro` to disambiguate.
    #[serde(default)]
    pub(crate) bro: Option<String>,
    /// Raw provider for ad-hoc tasks
    #[serde(default)]
    pub(crate) provider: Option<String>,
    /// Working directory (absolute path)
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    pub(crate) allow_recursion: Option<bool>,
    /// Per-dispatch allow patterns merged on top of global+project+brofile.
    /// Use to tighten or open the tool surface for this one invocation.
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    pub(crate) allow_tools: Option<Vec<String>>,
    /// Per-dispatch disallow patterns merged on top of global+project+brofile.
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    pub(crate) disallow_tools: Option<Vec<String>>,
    /// MCP tool surface name. When set, the dispatch evaluates the named
    /// surface against the routing packet store and restricts the spawned
    /// agent's tool catalog accordingly.
    #[serde(default)]
    pub(crate) surface: Option<String>,
    /// Override the brofile's `coerce_workspace` setting for this dispatch.
    /// When true, injects the workspace-tools appendix into the ambient
    /// prefix. When false or absent, defers to the brofile setting.
    #[serde(default)]
    pub(crate) coerce_workspace: Option<bool>,
    /// Runtime allocation tier key. When set, dispatch resolves a pooled
    /// provider/account/model/effort lane before spawning.
    #[serde(default)]
    pub(crate) tier: Option<String>,
    /// Named tier ladder for at_least/bounded tier modes.
    #[serde(default)]
    pub(crate) tier_ladder: Option<String>,
    /// Tier mode: exact, at_least, or bounded. Defaults to exact.
    #[serde(default)]
    pub(crate) tier_mode: Option<String>,
    /// Lower tier bound for bounded mode.
    #[serde(default)]
    pub(crate) min_tier: Option<String>,
    /// Upper tier bound for bounded mode.
    #[serde(default)]
    pub(crate) max_tier: Option<String>,
    /// Named provider/account pool.
    #[serde(default)]
    pub(crate) pool_name: Option<String>,
    /// Provider aliases that narrow the selected pool.
    #[serde(default)]
    pub(crate) pool_providers: Option<Vec<String>>,
    /// Hard provider pin for runtime allocation.
    #[serde(default)]
    pub(crate) pin_provider: Option<String>,
    /// Hard account pin for runtime allocation.
    #[serde(default)]
    pub(crate) pin_account: Option<String>,
    /// Hard model pin for runtime allocation.
    #[serde(default)]
    pub(crate) pin_model: Option<String>,
    /// Hard effort pin for runtime allocation.
    #[serde(default)]
    pub(crate) pin_effort: Option<String>,
    /// Soft provider preference for runtime allocation scoring.
    #[serde(default)]
    pub(crate) prefer_provider: Option<String>,
    /// Explicit hard capability requirements.
    #[serde(default)]
    pub(crate) capabilities: Option<Vec<String>>,
    /// Mark the allocation as resumable/durable.
    #[serde(default)]
    pub(crate) durable: Option<bool>,
    /// Named policy or inline policy object. V1 records this in traces
    /// and uses availability scoring.
    #[serde(default)]
    pub(crate) selection_policy: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ResumeParams {
    /// Follow-up instruction
    pub(crate) prompt: String,
    /// Named bro instance to resume. Bare names must be unique across live
    /// teams; use `team::bro` to disambiguate.
    #[serde(default)]
    pub(crate) bro: Option<String>,
    /// Session ID from a prior task (requires provider)
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    /// Provider (required with session_id)
    #[serde(default)]
    pub(crate) provider: Option<String>,
    /// Working directory
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    pub(crate) allow_recursion: Option<bool>,
    /// Per-dispatch allow/disallow overlays for this resume only.
    #[serde(default)]
    pub(crate) allow_tools: Option<Vec<String>>,
    /// Accepts canonical MCP patterns (`mcp__blackbox__bro_*`) and the
    /// surfaced dotted form (`mcp__blackbox__.bro_*`).
    #[serde(default)]
    pub(crate) disallow_tools: Option<Vec<String>>,
    /// MCP tool surface name. When set, the resumed agent's tool catalog
    /// is restricted according to the named surface's routing packet.
    #[serde(default)]
    pub(crate) surface: Option<String>,
    /// Override the brofile's `coerce_workspace` setting for this resume.
    /// When true, injects the workspace-tools appendix into the ambient
    /// prefix. When false or absent, defers to the brofile setting.
    #[serde(default)]
    pub(crate) coerce_workspace: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyExecParams {
    /// Project root / scope Badgey should consult against.
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Initial charter or question for the Badgey instance.
    #[serde(default)]
    pub(crate) brief: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyResumeParams {
    /// Badgey instance id returned by badgey_exec.
    pub(crate) badgey_id: String,
    /// User turn, or a wrapper-direct command such as "dismiss".
    pub(crate) prompt: String,
    /// Max seconds to wait for the underlying provider turn.
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyAskParams {
    /// Badgey instance id returned by badgey_exec.
    pub(crate) badgey_id: String,
    /// Question to ask the existing Badgey instance.
    pub(crate) question: String,
    /// Max seconds to wait for the underlying provider turn.
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyDismissParams {
    /// Badgey instance id returned by badgey_exec.
    pub(crate) badgey_id: String,
    /// Optional close reason written to the thread of record.
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyStatusParams {
    /// Badgey instance id. If omitted, returns the active list summary.
    #[serde(default)]
    pub(crate) badgey_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyListParams {
    /// Include dismissed instances. Default false.
    #[serde(default)]
    pub(crate) include_dismissed: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyScoutParams {
    /// Badgey instance id returned by badgey_exec.
    pub(crate) badgey_id: String,
    /// Focused scout charter.
    pub(crate) charter: String,
    /// Max seconds to wait for the charter-authoring turn.
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyCollectParams {
    /// Scout id to collect, or omit to list scout/sub-bro events for a Badgey instance.
    #[serde(default)]
    pub(crate) scout_id: Option<String>,
    /// Badgey instance id returned by badgey_exec.
    #[serde(default)]
    pub(crate) badgey_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyTriageInboxParams {
    /// Project path or registered scope. Defaults to current working directory.
    #[serde(default)]
    pub(crate) scope: Option<String>,
    /// Optional ISO timestamp lower bound.
    #[serde(default)]
    pub(crate) since: Option<String>,
    /// Existing Badgey instance to attach proposal context to.
    #[serde(default)]
    pub(crate) badgey_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyProposalsListParams {
    /// Badgey instance id (`bg-<8hex>-<8hex>`) whose proposals to list.
    pub(crate) badgey_id: String,
    /// Optional ISO timestamp lower bound on `created_at`. Returns
    /// proposals at or after this moment.
    #[serde(default)]
    pub(crate) since: Option<String>,
    /// When true, exclude terminal-state proposals (Applied / Failed).
    /// Defaults to false (returns all states).
    #[serde(default)]
    pub(crate) only_pending: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyCloseLoopsParams {
    /// Window in days. Default 14.
    #[serde(default)]
    pub(crate) window_days: Option<u64>,
    /// Optional project filter.
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WaitParams {
    /// Task ID from exec or resume
    pub(crate) task_id: String,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct WhenParams {
    /// Team name — waits on each member's most recent task
    #[serde(default)]
    pub(crate) team: Option<String>,
    /// Explicit list of task IDs
    #[serde(default)]
    pub(crate) task_ids: Option<Vec<String>>,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BroadcastParams {
    /// Team name
    pub(crate) team: String,
    /// Prompt sent to every member
    pub(crate) prompt: String,
    /// Working directory override
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    pub(crate) allow_recursion: Option<bool>,
    /// Per-dispatch allow/disallow overlays applied to every member.
    #[serde(default)]
    pub(crate) allow_tools: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct StatusParams {
    /// Task ID to check
    pub(crate) task_id: String,
    /// Number of recent events to include (default: 0)
    #[serde(default)]
    pub(crate) tail: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ReportParams {
    /// Task ID to attach the report to.
    pub(crate) task_id: String,
    /// Short human-readable progress report.
    pub(crate) message: String,
    /// Optional blocker, handoff need, or requested input.
    #[serde(default)]
    pub(crate) needs: Option<String>,
    /// Optional structured payload for workflow hooks or richer agent state.
    #[serde(default)]
    pub(crate) data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AllocatorStatusParams {
    /// Optional project path whose .bro/allocator.json overlay should be included.
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AllocatorTraceParams {
    /// Selection trace id returned by allocated bro_exec responses.
    pub(crate) selection_trace_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct DashboardParams {
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) team: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CouncilListParams {
    /// Filter to councils whose `project` matches this exact path.
    #[serde(default)]
    pub(crate) project: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CouncilOpenParams {
    /// Council ID (e.g. `council-7f01324e`).
    pub(crate) id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CouncilPostsParams {
    pub(crate) id: String,
    /// Return only posts with `sequence > since_seq`. Default 0 (all).
    #[serde(default)]
    pub(crate) since_seq: Option<u64>,
    /// Cap the response (default 100, max 1000).
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct CancelParams {
    /// Task ID to cancel
    pub(crate) task_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct PruneParams {
    /// Status to prune (failed, completed, cancelled). Defaults to
    /// "failed" — the only status that's almost always safe to drop
    /// without further filtering. Running tasks are never pruned.
    #[serde(default)]
    pub(crate) status: Option<String>,
    /// Optional provider filter (claude, codex, copilot, gemini, vibe).
    #[serde(default)]
    pub(crate) provider: Option<String>,
    /// Drop tasks that started more than this many hours ago.
    #[serde(default)]
    pub(crate) older_than_hours: Option<u64>,
    /// Dry-run: report what would be pruned without removing.
    /// Defaults to false — bro_prune is the explicit pruning verb.
    #[serde(default)]
    pub(crate) dry_run: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentListParams {
    #[serde(default)]
    pub(crate) include_superseded: Option<bool>,
    #[serde(default)]
    pub(crate) cost_class: Option<String>,
    #[serde(default)]
    pub(crate) provenance_kind: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentGetParams {
    pub(crate) name: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentDescribeParams {
    pub(crate) agent: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentDispatchParams {
    pub(crate) agent: String,
    #[schemars(with = "serde_json::Map<String, serde_json::Value>")]
    pub(crate) args: serde_json::Value,
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    #[serde(default)]
    pub(crate) bro: Option<String>,
    #[serde(default)]
    pub(crate) ambient: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) caller_provider: Option<String>,
    #[serde(default)]
    pub(crate) caller_session_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AgentSearchParams {
    pub(crate) query: String,
    #[serde(default)]
    pub(crate) limit: Option<u64>,
    #[serde(default)]
    pub(crate) cost_class: Option<String>,
    #[serde(default)]
    pub(crate) provenance_kind: Option<String>,
    #[serde(default)]
    pub(crate) exclude_anti_pattern_matches: Option<bool>,
    #[serde(default)]
    pub(crate) include_vectors: Option<bool>,
    #[serde(default)]
    pub(crate) query_vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentVectorPlan {
    pub(crate) search: Option<orchestration::agents::registry::AgentVectorSearch>,
    pub(crate) route: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BrofileParams {
    /// Operation: create, list, get, delete, set_account, list_accounts,
    /// set_provider_default, get_provider_default, list_provider_defaults,
    /// clear_provider_default
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) account: Option<String>,
    #[serde(default)]
    pub(crate) lens: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) effort: Option<String>,
    #[serde(default)]
    pub(crate) env: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub(crate) scope: Option<String>,
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    /// Persona-bound allow/disallow patterns embedded in the brofile.
    /// Apply at every dispatch using this brofile, between project
    /// mcp.json and per-dispatch ExecParams overrides.
    #[serde(default)]
    pub(crate) allow_tools: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) disallow_tools: Option<Vec<String>>,
    /// When true, inject the workspace-tools appendix into every dispatch
    /// using this brofile. Default off (absent / false).
    #[serde(default)]
    pub(crate) coerce_workspace: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BroSlackBindParams {
    /// Operation: bind, unbind, list, lookup
    pub(crate) action: String,
    /// Slack workspace id (T-prefix). Required for bind/unbind/lookup.
    #[serde(default)]
    pub(crate) team_id: Option<String>,
    /// Slack channel id (C-prefix). Required for bind/unbind/lookup.
    /// Channel ids are stable across renames; channel names are not —
    /// always bind by id.
    #[serde(default)]
    pub(crate) channel_id: Option<String>,
    /// Optional human-readable channel name (e.g. `transcript-search`)
    /// stored alongside the binding for display only. Never used as a
    /// lookup key.
    #[serde(default)]
    pub(crate) channel_name: Option<String>,
    /// Project to bind. Accepts an absolute path, an 8-hex
    /// project_id from the registry, or the canonical_path of a
    /// registered project. Required for `bind`.
    #[serde(default)]
    pub(crate) project: Option<String>,
    /// Optional list filter — return bindings only for this project.
    #[serde(default)]
    pub(crate) project_filter: Option<String>,
    /// Optional list filter — return bindings only for this team.
    #[serde(default)]
    pub(crate) team_filter: Option<String>,
    /// Optional bbox_user attribution captured on bind.
    #[serde(default)]
    pub(crate) registered_by: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct EnsureBadgeyForChannelParams {
    /// Slack workspace id (T-prefix). Required.
    pub(crate) team_id: String,
    /// Slack channel id (C-prefix). Required. Must already have a
    /// binding via `bro_slack_bind action=bind`.
    pub(crate) channel_id: String,
    /// Optional override for the project scope. When absent the bound
    /// project_dir is used. Useful for one-off triage calls against a
    /// project the channel isn't bound to yet.
    #[serde(default)]
    pub(crate) scope_override: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SlackProposalLinkLookupParams {
    /// Slack workspace id (T-prefix).
    pub(crate) team_id: String,
    /// Slack channel id (C-prefix) the proposal was posted in.
    pub(crate) channel_id: String,
    /// Slack message ts of the posted proposal (== reaction's item_ts
    /// or thread reply's thread_ts).
    pub(crate) msg_ts: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyApplyProposalParams {
    /// Badgey instance id (`bg-<8hex>-<8hex>`) that owns the
    /// proposal.
    pub(crate) badgey_id: String,
    /// Proposal id within that instance (e.g. `P-3`).
    pub(crate) proposal_id: String,
    /// When true, retry a proposal currently in Failed state.
    /// Default false — unretried Failed proposals are rejected.
    #[serde(default)]
    pub(crate) retry_failed: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyProposalBeginApplyParams {
    /// Badgey instance id (`bg-<8hex>-<8hex>`).
    pub(crate) badgey_id: String,
    /// Proposal id within that instance.
    pub(crate) proposal_id: String,
    /// When true, retry a proposal currently in Failed state.
    #[serde(default)]
    pub(crate) retry_failed: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct BadgeyProposalCompleteApplyParams {
    /// Badgey instance id.
    pub(crate) badgey_id: String,
    /// Proposal id.
    pub(crate) proposal_id: String,
    /// Outcome of the dispatched work as observed by the caller.
    /// Maps to TaskStatus serialization: `completed` / `failed` /
    /// `cancelled`. Workflow callers pass the actor's
    /// `actor_results.<NodeId>.status` here. `timed_out` is treated
    /// as failure.
    pub(crate) outcome: String,
    /// Task id of the dispatched work, when applicable. For
    /// redispatch_task this is the `actor_results.<NodeId>.taskId`;
    /// for artifact installs it is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) task_id: Option<String>,
    /// Artifact reference for artifact-install proposals
    /// (`<kind>:<name>@<version>`). Omitted for redispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) artifact_ref: Option<String>,
    /// One-line summary of the work performed (typically the
    /// actor's last assistant message snippet, or the install
    /// metadata). Stored on the proposal's audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct SlackProposalLinkRecordParams {
    /// Slack workspace id (T-prefix).
    pub(crate) team_id: String,
    /// Slack channel id (C-prefix) the proposal was posted to.
    pub(crate) channel_id: String,
    /// Slack message ts of the posted proposal. Doubles as thread
    /// root for in-thread replies.
    pub(crate) msg_ts: String,
    /// BadgeyProposalStore id of the proposal this Slack message
    /// represents.
    pub(crate) proposal_id: String,
    /// Optional BadgeyInstance id (`bg-<8hex>-<8hex>`) that owns the
    /// proposal. Required for the apply/refine hook to resolve back
    /// to a real `(BadgeyId, proposal_id)` pair.
    #[serde(default)]
    pub(crate) instance_id: Option<String>,
    /// Optional bro/Claude session id of the agent that authored the
    /// proposal, for thread-reply refinement loops.
    #[serde(default)]
    pub(crate) authoring_session_id: Option<String>,
    /// Project this proposal scopes to. Stored on the link record so
    /// consumers don't need a second lookup against the channel
    /// binding.
    pub(crate) project_dir: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct TeamParams {
    /// Operation: save_template, list_templates, delete_template, create, list, dissolve, roster
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) members: Option<Vec<TeamMemberSlot>>,
    #[serde(default)]
    pub(crate) template: Option<String>,
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    #[serde(default)]
    pub(crate) scope: Option<String>,
    #[serde(default)]
    pub(crate) cancel_running: Option<bool>,
    #[serde(default)]
    pub(crate) advisor: Option<AdvisorSpecParams>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct TeamMemberSlot {
    pub(crate) brofile: String,
    #[serde(default)]
    pub(crate) alias: Option<String>,
    #[serde(default)]
    pub(crate) count: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AdvisorSpecParams {
    /// Special brofile designated as the advisor for this team.
    pub(crate) brofile: String,
    /// Optional advisor alias; defaults to the brofile name.
    #[serde(default)]
    pub(crate) alias: Option<String>,
    /// One-sentence or short-paragraph charter for the advisor.
    pub(crate) charter: String,
    /// Optional extra context that should stay hot across advisor rounds.
    #[serde(default)]
    pub(crate) context: Option<String>,
    /// Halt / escalate conditions the advisor should watch for.
    #[serde(default)]
    pub(crate) halt_conditions: Option<Vec<String>>,
    /// Exit conditions the advisor should watch for.
    #[serde(default)]
    pub(crate) exit_conditions: Option<Vec<String>>,
    /// Optional compiled packet ID the advisor can use mechanically.
    #[serde(default)]
    pub(crate) packet_id: Option<String>,
    /// Wait behavior for internal advisor rounds.
    #[serde(default)]
    pub(crate) mode: Option<orchestration::team::AdvisorMode>,
    /// Optional timeout for internal advisor init/resume waits.
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AdvisorMemberCheckpoint {
    pub(crate) bro: Option<String>,
    pub(crate) task_id: String,
    pub(crate) status: String,
    pub(crate) timed_out: bool,
    pub(crate) keep_going: Option<String>,
    pub(crate) result_snippet: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct AdvisorNoteSummary {
    pub(crate) dispute_count: usize,
    pub(crate) assumption_count: usize,
    pub(crate) surprise_count: usize,
    pub(crate) followup_count: usize,
    pub(crate) blocked_count: usize,
    pub(crate) learned_count: usize,
    pub(crate) done_count: usize,
    pub(crate) recent_unresolved: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AdvisorCheckpoint {
    pub(crate) wait_kind: String,
    pub(crate) team_name: String,
    pub(crate) teamplate: String,
    pub(crate) monitored_task_ids: Vec<String>,
    pub(crate) packet_id: Option<String>,
    pub(crate) total_count: usize,
    pub(crate) completed_count: usize,
    pub(crate) failed_count: usize,
    pub(crate) cancelled_count: usize,
    pub(crate) timed_out_count: usize,
    pub(crate) running_count: usize,
    pub(crate) dispute_count: usize,
    pub(crate) assumption_count: usize,
    pub(crate) surprise_count: usize,
    pub(crate) followup_count: usize,
    pub(crate) blocked_count: usize,
    pub(crate) learned_count: usize,
    pub(crate) done_count: usize,
    pub(crate) members: Vec<AdvisorMemberCheckpoint>,
    pub(crate) notes: AdvisorNoteSummary,
}

// ---------------------------------------------------------------------------
// Atom read tools
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AtomListParams {
    #[serde(default)]
    pub(crate) include_superseded: Option<bool>,
    #[serde(default)]
    pub(crate) cost_class: Option<String>,
    #[serde(default)]
    pub(crate) provenance_kind: Option<String>,
    #[serde(default)]
    pub(crate) subcontract: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AtomGetParams {
    pub(crate) name: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AtomDescribeParams {
    pub(crate) atom: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct AtomSearchParams {
    pub(crate) query: String,
    #[serde(default)]
    pub(crate) limit: Option<u64>,
    #[serde(default)]
    pub(crate) cost_class: Option<String>,
    #[serde(default)]
    pub(crate) provenance_kind: Option<String>,
    #[serde(default)]
    pub(crate) subcontract: Option<String>,
    #[serde(default)]
    pub(crate) exclude_anti_pattern_matches: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct AtomInvokeParams {
    pub(crate) atom: String,
    #[serde(default)]
    #[schemars(with = "std::collections::BTreeMap<String, serde_json::Value>")]
    pub(crate) args: serde_json::Value,
    #[serde(default)]
    pub(crate) project_dir: Option<String>,
    #[serde(default)]
    pub(crate) owner: Option<String>,
    #[serde(default)]
    pub(crate) parent_invocation_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct AtomStatusParams {
    pub(crate) invocation_id: String,
    #[serde(default)]
    pub(crate) owner: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct AtomResumeParams {
    pub(crate) invocation_id: String,
    pub(crate) prompt: String,
    #[serde(default)]
    pub(crate) owner: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct AtomDelegateParams {
    pub(crate) invocation_id: String,
    pub(crate) grant_to: String,
    #[serde(default)]
    pub(crate) owner: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_invoke_args_schema_is_object() {
        let schema = serde_json::to_value(rmcp::schemars::schema_for!(AtomInvokeParams)).unwrap();
        let args_schema = &schema["properties"]["args"];
        assert_eq!(
            args_schema["type"], "object",
            "atom_invoke.args must be advertised as an object, not a string"
        );
    }
}
